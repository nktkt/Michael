//! `tomcatrs-servlet-bridge` — the Rust ↔ JVM bridge of the **Tomcat-RS
//! Compatibility Runtime**.
//!
//! # Why this crate exists
//!
//! Tomcat-RS is an *incremental* Rust rewrite of Apache Tomcat. A from-scratch
//! rewrite cannot run the enormous body of existing Java Servlet / JSP web
//! applications on day one — so the compatibility strategy is to embed a single
//! JVM inside the Rust process and run unmodified Servlets/WARs against it,
//! while everything that *can* be native (connector, HTTP parsing, TLS,
//! routing, session store) stays in Rust.
//!
//! This crate is the architectural heart of that strategy.
//!
//! # Architecture
//!
//! ```text
//!   ┌─────────────────────── Rust process ───────────────────────┐
//!   │                                                            │
//!   │  tomcatrs-coyote        tomcatrs-servlet-bridge             │
//!   │  ┌────────────┐         ┌───────────────────────────────┐  │
//!   │  │ HTTP/1.1   │         │  JvmRuntime                   │  │
//!   │  │ HTTP/2     │ Request │   ├─ JavaVM handle (1 per     │  │
//!   │  │ parser  ───┼────────▶│   │  process, `jvm` feature)  │  │
//!   │  │            │ Handle  │   ├─ webapp registry          │  │
//!   │  │ socket I/O │◀────────┼── │  worker pool (JNI-attached│  │
//!   │  └────────────┘ Response│   │  threads)                 │  │
//!   │                 Handle  │   └─ ServletInvoker           │  │
//!   │                         └───────────────┬───────────────┘  │
//!   └─────────────────────────────────────────┼──────────────────┘
//!                                             │ JNI (lazy)
//!   ┌─────────────────────────────────────────▼──────────────────┐
//!   │  Embedded JVM                                              │
//!   │   org.apache.tomcatrs.bridge.TomcatRsRequestFacade  ──┐    │
//!   │   org.apache.tomcatrs.bridge.TomcatRsResponseFacade   │    │
//!   │   org.apache.tomcatrs.bridge.TomcatRsServletContext   ├──▶ │
//!   │            ▲                                          │    │
//!   │            │ Jakarta Servlet API                      │    │
//!   │   unmodified user Servlet / Filter / JSP ─────────────┘    │
//!   └────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Design principles
//!
//! * **Lazy materialization.** Crossing JNI is expensive. The Rust side parses
//!   the HTTP request once, but headers, parameters and attributes are *not*
//!   pushed into Java. Instead the Java facade classes hold an opaque
//!   `nativeRequestId` and call back into Rust (`native` methods) only for the
//!   values the servlet actually touches. See [`request_facade`].
//!
//! * **Streaming bodies.** The request body is a Rust async reader surfaced to
//!   Java as a `ServletInputStream`; the response is a Java
//!   `ServletOutputStream` that drains into a Rust sink. Neither side buffers
//!   the whole payload. See [`response_facade`].
//!
//! * **Per-worker JNI attach.** JNI requires every thread that touches the JVM
//!   to be *attached* to it. Attaching/detaching per request is costly, so the
//!   bridge owns a fixed worker pool whose threads attach once at start-up and
//!   stay attached for their lifetime. See [`jvm`].
//!
//! * **`AsyncContext`.** Servlet 3.0 async (`request.startAsync()`) means the
//!   request outlives the initial `service()` call. The bridge keeps the Rust
//!   request/response state alive in an [`async_servlet::AsyncContextState`]
//!   until Java calls `AsyncContext.complete()`.
//!
//! # Feature flags
//!
//! | feature   | default | effect                                                   |
//! |-----------|---------|----------------------------------------------------------|
//! | `jvm`     | no      | Links the `jni` crate and compiles the real embedded-JVM implementation. Requires a JDK. |
//!
//! With **default features** the crate builds and tests with *no JDK
//! installed*. [`JvmRuntime`] still exists as a type with the full public API,
//! but [`JvmRuntime::start`] returns [`tomcatrs_core::Error::Bridge`]. The
//! [`NoopServletInvoker`] is fully functional on the default path and answers
//! every invocation with HTTP `501 Not Implemented`.

#![forbid(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

pub mod async_servlet;
pub mod classloader;
pub mod dispatch;
pub mod invoker;
pub mod jni;
pub mod jvm;
pub mod listener;
pub mod registration;
pub mod request_facade;
pub mod response_facade;
pub mod session_bridge;

pub use async_servlet::{AsyncContextState, AsyncState};
pub use classloader::{ClassLoaderFactory, WebappClassLoaderConfig};
pub use dispatch::{BridgeAdapter, ServletDispatch};
pub use invoker::JvmServletInvoker;
pub use jvm::{JvmConfig, JvmRuntime, WebappRuntime};
pub use request_facade::RequestHandle;
pub use response_facade::ResponseHandle;

use async_trait::async_trait;
use tomcatrs_core::{ContextId, WrapperId};

/// Crate version, sourced from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The fully-qualified name of the JAR built from the `java/` scaffold that
/// must be present on the embedded JVM's common classpath.
pub const BRIDGE_JAR_NAME: &str = "tomcatrs-bridge.jar";

/// Outcome of dispatching a request to a servlet.
///
/// Returned by [`ServletInvoker::invoke`]. The connector uses this to decide
/// whether it still owns the response (and may, for example, write an error
/// page) or whether the servlet has already committed bytes to the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvocationResult {
    /// Final HTTP status code the servlet set on the response.
    pub status: u16,
    /// Whether the response has been *committed* — i.e. the status line and
    /// headers have been flushed and can no longer be changed.
    pub committed: bool,
}

impl InvocationResult {
    /// Construct a result.
    pub fn new(status: u16, committed: bool) -> Self {
        Self { status, committed }
    }
}

/// Dispatches an HTTP request to a Java servlet (or a stand-in).
///
/// The connector layer (`tomcatrs-coyote` / `tomcatrs-catalina`) parses the
/// request in Rust, builds a [`RequestHandle`] / [`ResponseHandle`] pair, and
/// hands them to an invoker. Two implementations exist:
///
/// * [`NoopServletInvoker`] — the v0.1.0 default. Requires no JVM and answers
///   `501 Not Implemented`. Used so the whole runtime is exercisable end-to-end
///   before the JVM bridge is enabled.
/// * `JvmServletInvoker` (behind the `jvm` feature, in [`jvm`]) — the real
///   implementation that crosses JNI into the embedded JVM.
#[async_trait]
pub trait ServletInvoker: Send + Sync {
    /// Invoke the servlet identified by `wrapper` within web application
    /// `context`, passing ownership of the request/response handles for the
    /// duration of the call.
    async fn invoke(
        &self,
        context: ContextId,
        wrapper: WrapperId,
        request: RequestHandle,
        response: ResponseHandle,
    ) -> tomcatrs_core::Result<InvocationResult>;
}

/// The v0.1.0 default [`ServletInvoker`]: a no-op that needs no JVM.
///
/// Every invocation drains nothing, writes a short `text/plain` explanatory
/// body to the response sink, and reports HTTP `501 Not Implemented`. This lets
/// the connector, router, and session subsystems be wired up and tested before
/// the embedded JVM is brought online with `--features jvm`.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopServletInvoker;

impl NoopServletInvoker {
    /// HTTP status returned by every invocation.
    pub const STATUS: u16 = 501;

    /// Create a new no-op invoker.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ServletInvoker for NoopServletInvoker {
    async fn invoke(
        &self,
        context: ContextId,
        wrapper: WrapperId,
        request: RequestHandle,
        response: ResponseHandle,
    ) -> tomcatrs_core::Result<InvocationResult> {
        tracing::debug!(
            %context,
            %wrapper,
            request_id = request.id(),
            "NoopServletInvoker: JVM bridge not active, replying 501"
        );

        let body = format!(
            "501 Not Implemented\n\
             \n\
             The Tomcat-RS servlet bridge is running without an embedded JVM.\n\
             Servlet '{wrapper}' in context '{context}' cannot be executed.\n\
             Rebuild tomcatrs-servlet-bridge with `--features jvm` to enable \
             Java servlet support.\n"
        );

        response.set_status(Self::STATUS);
        response.set_header("Content-Type", "text/plain; charset=utf-8");
        response.write_body(body.as_bytes());
        let committed = response.commit();

        Ok(InvocationResult::new(Self::STATUS, committed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request_facade::RequestParts;

    fn handles() -> (RequestHandle, ResponseHandle) {
        let req = RequestHandle::new(RequestParts {
            method: "GET".into(),
            uri: "/app/hello".into(),
            ..RequestParts::default()
        });
        let resp = ResponseHandle::new();
        (req, resp)
    }

    #[test]
    fn noop_invoker_replies_501() {
        let invoker = NoopServletInvoker::new();
        let (req, resp) = handles();
        let sink = resp.sink_handle();

        let result = futures_lite_block_on(invoker.invoke(
            "/app".to_string(),
            "hello".to_string(),
            req,
            resp,
        ))
        .expect("noop invoker never errors");

        assert_eq!(result.status, 501);
        assert_eq!(result.status, NoopServletInvoker::STATUS);
        assert!(result.committed);

        let written = sink.snapshot();
        assert_eq!(written.status, Some(501));
        assert!(written.committed);
        let body = String::from_utf8(written.body).unwrap();
        assert!(body.contains("501 Not Implemented"));
        assert!(body.contains("--features jvm"));
    }

    #[test]
    fn invocation_result_is_copy() {
        let a = InvocationResult::new(200, true);
        let b = a;
        assert_eq!(a, b);
    }

    /// Minimal single-future executor so the tests need no async runtime
    /// dependency. The futures produced by [`NoopServletInvoker`] never yield
    /// to a real reactor, so a busy poll is sufficient and terminates
    /// immediately.
    fn futures_lite_block_on<F: std::future::Future>(mut fut: F) -> F::Output {
        use std::pin::Pin;
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

        fn noop_raw_waker() -> RawWaker {
            fn no_op(_: *const ()) {}
            fn clone(_: *const ()) -> RawWaker {
                noop_raw_waker()
            }
            let vtable = &RawWakerVTable::new(clone, no_op, no_op, no_op);
            RawWaker::new(std::ptr::null(), vtable)
        }

        let waker = unsafe { Waker::from_raw(noop_raw_waker()) };
        let mut cx = Context::from_waker(&waker);
        // SAFETY: `fut` is owned and never moved again after this shadowing.
        let mut fut = unsafe { Pin::new_unchecked(&mut fut) };
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(out) => return out,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }
}
