//! High-level bridge dispatch layer: the glue that makes the JVM servlet bridge
//! directly pluggable into a `tomcatrs-coyote` connector.
//!
//! # Where this sits
//!
//! `tomcatrs-coyote` parses the wire protocol into a protocol-agnostic
//! [`tomcatrs_coyote::Request`] and hands it to a
//! [`tomcatrs_coyote::Adapter`]. This module provides [`BridgeAdapter`], an
//! `Adapter` implementation that routes each request to a Java servlet (or, on
//! the no-JVM default build, to the [`NoopServletInvoker`] stub) through the
//! bridge.
//!
//! ```text
//!   coyote::Request
//!         │
//!         ▼
//!   BridgeAdapter::service
//!         │  resolve(&Request) ─► Option<(ContextId, WrapperId)>
//!         │        │
//!         │        ├─ None  ─────────────► coyote::Response  (404)
//!         │        │
//!         │        └─ Some(ctx, wrapper)
//!         ▼
//!   ServletDispatch::dispatch
//!         │  build RequestHandle  (lazy-materialization: only metadata,
//!         │                        the body stays a Rust-side cursor)
//!         │  build fresh ResponseHandle
//!         ▼
//!   ServletInvoker::invoke(ctx, wrapper, RequestHandle, ResponseHandle)
//!         │  (jvm feature)  crosses JNI; the Java facade pulls individual
//!         │                 headers/params back through `crate::jni`
//!         │  (default)      NoopServletInvoker writes a 501 body
//!         ▼
//!   ResponseHandle  ──marshal──►  coyote::Response
//! ```
//!
//! # Data-flow and the lazy / streaming design
//!
//! The whole point of the bridge is to cross JNI as rarely as possible. This
//! module is careful to preserve that:
//!
//! * **Lazy materialization.** [`ServletDispatch::dispatch`] turns the coyote
//!   request into a [`RequestHandle`] carrying only the eagerly-parsed
//!   [`RequestParts`](crate::request_facade::RequestParts) (method, URI, query,
//!   headers, …) plus the body as a Rust-side [`RequestBody`] cursor. *Nothing*
//!   is pushed into the JVM here. When the `jvm` feature is enabled the handle
//!   is registered in the [`crate::jni`] `HANDLE_REGISTRY` so the Java facade
//!   classes can call back (`nativeGetHeader`, `nativeReadBody`, …) for exactly
//!   the values the servlet actually touches — one JNI round trip per value,
//!   never a bulk copy.
//!
//! * **Streaming bodies.** The request body is handed over as a
//!   [`RequestBody::Buffered`] cursor that the Java `ServletInputStream` drains
//!   incrementally via `nativeReadBody`; the response is a [`ResponseHandle`]
//!   whose [`ResponseSink`](crate::response_facade::ResponseSink) the Java
//!   `ServletOutputStream` appends into via `nativeWriteBody`. Only at the very
//!   end — after the invoker returns — does [`ServletDispatch`] take a single
//!   [`snapshot`](crate::response_facade::ResponseSink::snapshot) and marshal it
//!   into a [`tomcatrs_coyote::Response`]. In v0.1.0 that snapshot is a full
//!   buffer, but the seam is deliberately a snapshot call so a future truly
//!   streaming connector path can replace it without touching the invoker
//!   contract.
//!
//! # No-JVM build
//!
//! Everything in this module compiles and runs with **default features** (no
//! JDK). On that path [`ServletDispatch`] is constructed around a
//! [`NoopServletInvoker`], so [`ServletDispatch::dispatch`] produces a
//! `501 Not Implemented` response, and [`BridgeAdapter`]'s unresolved-route
//! `404` path is fully exercised.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use tomcatrs_core::{ContextId, WrapperId};

use crate::request_facade::{RequestBody, RequestHandle, RequestParts};
use crate::response_facade::{ResponseHandle, ResponseSnapshot};
use crate::{JvmRuntime, ServletInvoker};

/// An error encountered while marshalling between the coyote wire types and the
/// bridge's [`RequestHandle`] / [`ResponseHandle`] facades.
///
/// Marshalling is intentionally infallible in v0.1.0 — every coyote
/// [`tomcatrs_coyote::Request`] maps cleanly onto a
/// [`RequestParts`] — but the error type exists so that future, stricter
/// validation (e.g. rejecting malformed header names before they reach the JVM)
/// has a home that does not change [`ServletDispatch::dispatch`]'s signature:
/// it already returns [`tomcatrs_core::Result`], and [`MarshallingError`]
/// converts into [`tomcatrs_core::Error::Bridge`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarshallingError {
    /// Human-readable description of what could not be marshalled.
    message: String,
}

impl MarshallingError {
    /// Construct a marshalling error from anything string-like.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The human-readable description.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for MarshallingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "request/response marshalling failed: {}", self.message)
    }
}

impl std::error::Error for MarshallingError {}

impl From<MarshallingError> for tomcatrs_core::Error {
    fn from(err: MarshallingError) -> Self {
        tomcatrs_core::Error::bridge(err.to_string())
    }
}

/// Build the bridge-side [`RequestHandle`] from a coyote
/// [`tomcatrs_coyote::Request`].
///
/// This is the lazy-materialization entry point: it copies only the
/// eagerly-parsed metadata into [`RequestParts`] and hands the body over as a
/// Rust-side [`RequestBody`] cursor. Nothing crosses JNI here.
fn request_handle_from_coyote(req: &tomcatrs_coyote::Request) -> RequestHandle {
    let parts = RequestParts {
        method: req.method.clone(),
        // The bridge facade exposes the normalized, percent-decoded path as the
        // servlet request URI; the raw target is kept by the connector for
        // logging only.
        uri: req.path.clone(),
        query: req.query.clone(),
        protocol: req.version.clone(),
        headers: req.headers.clone(),
        remote_addr: req.peer_addr.to_string(),
        scheme: scheme_for(req),
    };

    if req.body.is_empty() {
        RequestHandle::new(parts)
    } else {
        RequestHandle::with_body(
            parts,
            RequestBody::Buffered {
                data: req.body.clone(),
                position: 0,
            },
        )
    }
}

/// Best-effort scheme derivation for a coyote request.
///
/// The coyote `Request` does not carry the scheme directly (TLS termination is
/// a connector concern), so we honour a forwarding header if present and
/// otherwise default to `http`.
fn scheme_for(req: &tomcatrs_coyote::Request) -> String {
    req.header("x-forwarded-proto")
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "http".to_string())
}

/// Marshal a finished [`ResponseSnapshot`] back into a coyote
/// [`tomcatrs_coyote::Response`].
///
/// Called once, after the invoker returns and the response sink is final. An
/// invoker that never set a status is treated as `200 OK`, matching
/// [`ResponseHandle::commit`](crate::response_facade::ResponseHandle::commit)'s
/// own default.
fn response_from_snapshot(snapshot: ResponseSnapshot) -> tomcatrs_coyote::Response {
    tomcatrs_coyote::Response {
        status: snapshot.status.unwrap_or(200),
        headers: snapshot.headers,
        body: snapshot.body.into(),
    }
}

/// Drives a single request through the JVM servlet bridge.
///
/// A [`ServletDispatch`] pairs a [`ServletInvoker`] (the thing that actually
/// runs — or stubs — the servlet) with the [`JvmRuntime`] it belongs to. It is
/// cheap to clone: both fields are `Arc`s.
///
/// The invoker is held as an `Arc<dyn ServletInvoker>` so a `ServletDispatch`
/// works uniformly with the no-JVM [`NoopServletInvoker`] and, once it lands,
/// the `jvm`-feature `JvmServletInvoker` — no generic parameter leaks into the
/// connector wiring.
///
/// The runtime is held as `Option<Arc<JvmRuntime>>`: on the default (no-JVM)
/// build a [`JvmRuntime`] value cannot exist at all (it is an uninhabited stub
/// — [`JvmRuntime::start`] never returns `Ok`), and even on the `jvm` build the
/// [`NoopServletInvoker`] needs no runtime. `None` is therefore a first-class
/// state, not an error.
#[derive(Clone)]
pub struct ServletDispatch {
    invoker: Arc<dyn ServletInvoker>,
    runtime: Option<Arc<JvmRuntime>>,
}

impl ServletDispatch {
    /// Create a dispatcher around an invoker and the JVM runtime it targets.
    ///
    /// Use this on the `jvm` build once a [`JvmRuntime`] has been started; for
    /// the no-JVM [`NoopServletInvoker`](crate::NoopServletInvoker) path use
    /// [`ServletDispatch::without_runtime`] instead.
    pub fn new(invoker: Arc<dyn ServletInvoker>, runtime: Arc<JvmRuntime>) -> Self {
        Self {
            invoker,
            runtime: Some(runtime),
        }
    }

    /// Create a dispatcher around an invoker with **no** [`JvmRuntime`].
    ///
    /// This is the constructor used by the no-JVM default build (and tests):
    /// the [`NoopServletInvoker`](crate::NoopServletInvoker) never touches a
    /// runtime, so none is required and [`ServletDispatch::runtime`] returns
    /// `None`.
    pub fn without_runtime(invoker: Arc<dyn ServletInvoker>) -> Self {
        Self {
            invoker,
            runtime: None,
        }
    }

    /// The [`JvmRuntime`] this dispatcher targets, if one was supplied.
    ///
    /// Returns `None` for dispatchers built with
    /// [`ServletDispatch::without_runtime`].
    pub fn runtime(&self) -> Option<&Arc<JvmRuntime>> {
        self.runtime.as_ref()
    }

    /// The invoker this dispatcher drives.
    pub fn invoker(&self) -> &Arc<dyn ServletInvoker> {
        &self.invoker
    }

    /// Dispatch one coyote request to the servlet identified by
    /// `(context, wrapper)` and marshal the result back to a coyote response.
    ///
    /// Steps, in order:
    ///
    /// 1. Build a [`RequestHandle`] from the coyote
    ///    [`tomcatrs_coyote::Request`] (lazy-materialization: metadata only,
    ///    body as a Rust cursor). With the `jvm` feature the handle is
    ///    registered in the [`crate::jni`] `HANDLE_REGISTRY` so the Java facade
    ///    can call back for individual values.
    /// 2. Build a fresh [`ResponseHandle`] and keep a
    ///    [`ResponseSink`](crate::response_facade::ResponseSink) observer.
    /// 3. Hand both handles to [`ServletInvoker::invoke`].
    /// 4. After the invoker returns, [`snapshot`](crate::response_facade::ResponseSink::snapshot)
    ///    the sink and marshal it into a [`tomcatrs_coyote::Response`].
    ///
    /// # Errors
    ///
    /// Returns whatever [`ServletInvoker::invoke`] returns on failure (a
    /// [`tomcatrs_core::Error`]). On the no-JVM build the
    /// [`NoopServletInvoker`](crate::NoopServletInvoker) never errors and this
    /// yields a `501` response.
    pub async fn dispatch(
        &self,
        context: ContextId,
        wrapper: WrapperId,
        req: tomcatrs_coyote::Request,
    ) -> tomcatrs_core::Result<tomcatrs_coyote::Response> {
        // 1. coyote::Request -> RequestHandle (lazy: nothing crosses JNI yet).
        let request = request_handle_from_coyote(&req);

        // 2. Fresh ResponseHandle + an observer for the connector to read back.
        let response = ResponseHandle::new();
        let sink = response.sink_handle();

        // With the embedded JVM compiled in, the handles must be registered in
        // the `crate::jni` `HANDLE_REGISTRY` *before* the invoker crosses JNI,
        // so the Java facade classes can resolve `nativeRequestId` /
        // `nativeResponseId` back to these Rust-side handles for their lazy
        // `native` callbacks (`nativeGetHeader`, `nativeReadBody`,
        // `nativeWriteBody`, …). That registration is owned by the
        // `JvmServletInvoker` (it knows the registry handle from the
        // `JvmRuntime`), so `ServletInvoker::invoke` below performs it; on the
        // default build there is no registry and nothing to do here.

        tracing::debug!(
            %context,
            %wrapper,
            request_id = request.id(),
            response_id = response.id(),
            "ServletDispatch: invoking servlet through the bridge"
        );

        // 3. Run the servlet (or the no-JVM stub).
        let outcome = self
            .invoker
            .invoke(context, wrapper, request, response)
            .await?;

        // 4. ResponseHandle (via its sink) -> coyote::Response.
        let snapshot = sink.snapshot();
        debug_assert_eq!(
            snapshot.status.unwrap_or(200),
            outcome.status,
            "invoker-reported status should match the sink"
        );
        Ok(response_from_snapshot(snapshot))
    }
}

impl fmt::Debug for ServletDispatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServletDispatch")
            .field("invoker", &"Arc<dyn ServletInvoker>")
            .field("has_runtime", &self.runtime().is_some())
            .finish()
    }
}

/// A resolver that maps a coyote request onto a `(ContextId, WrapperId)` pair,
/// or `None` when nothing matches (→ `404`).
///
/// The real router (`Mapper`) lives in `tomcatrs-catalina`; this crate takes a
/// callback so the bridge stays free of a dependency on the routing crate (and
/// thus of a dependency cycle).
pub type RouteResolver =
    Arc<dyn Fn(&tomcatrs_coyote::Request) -> Option<(ContextId, WrapperId)> + Send + Sync>;

/// A [`tomcatrs_coyote::Adapter`] that routes requests through the JVM servlet
/// bridge.
///
/// Plug a `BridgeAdapter` straight into a `tomcatrs_coyote::HttpConnector` and
/// the connector will, for every request:
///
/// 1. call the [`RouteResolver`] to find the target `(context, wrapper)`;
/// 2. if resolved, [`ServletDispatch::dispatch`] the request through the
///    bridge;
/// 3. if unresolved, return a plain `404 Not Found`.
///
/// It is cheap to clone (a [`ServletDispatch`] plus an `Arc`'d closure).
#[derive(Clone)]
pub struct BridgeAdapter {
    dispatch: ServletDispatch,
    resolver: RouteResolver,
}

impl BridgeAdapter {
    /// Build an adapter from a [`ServletDispatch`] and a routing callback.
    pub fn new(dispatch: ServletDispatch, resolver: RouteResolver) -> Self {
        Self { dispatch, resolver }
    }

    /// Build an adapter from a [`ServletDispatch`] and a plain closure,
    /// wrapping the closure in the required `Arc` for you.
    pub fn from_fn<F>(dispatch: ServletDispatch, resolver: F) -> Self
    where
        F: Fn(&tomcatrs_coyote::Request) -> Option<(ContextId, WrapperId)> + Send + Sync + 'static,
    {
        Self::new(dispatch, Arc::new(resolver))
    }

    /// The [`ServletDispatch`] this adapter dispatches through.
    pub fn dispatch(&self) -> &ServletDispatch {
        &self.dispatch
    }

    /// Render the `404 Not Found` response returned when the resolver yields
    /// `None`. Kept as a function so the body/headers are defined in one place
    /// and unit-testable.
    fn not_found(req: &tomcatrs_coyote::Request) -> tomcatrs_coyote::Response {
        let body = format!("404 Not Found\n\nNo servlet is mapped to '{}'.\n", req.path);
        let mut resp = tomcatrs_coyote::Response::with_body(404, body);
        resp.set_header("Content-Type", "text/plain; charset=utf-8");
        resp
    }
}

impl fmt::Debug for BridgeAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BridgeAdapter")
            .field("dispatch", &self.dispatch)
            .field("resolver", &"Arc<dyn Fn(..) -> Option<..>>")
            .finish()
    }
}

#[async_trait]
impl tomcatrs_coyote::Adapter for BridgeAdapter {
    async fn service(&self, req: tomcatrs_coyote::Request) -> tomcatrs_coyote::Response {
        match (self.resolver)(&req) {
            Some((context, wrapper)) => {
                tracing::debug!(%context, %wrapper, path = %req.path, "BridgeAdapter: route resolved");
                match self.dispatch.dispatch(context, wrapper, req).await {
                    Ok(response) => response,
                    Err(err) => {
                        // A bridge failure (e.g. JNI error) is surfaced as a
                        // 500 — the connector still owns the socket and must
                        // produce *some* response.
                        tracing::error!(error = %err, "BridgeAdapter: dispatch failed");
                        let mut resp = tomcatrs_coyote::Response::with_body(
                            500,
                            format!("500 Internal Server Error\n\n{err}\n"),
                        );
                        resp.set_header("Content-Type", "text/plain; charset=utf-8");
                        resp
                    }
                }
            }
            None => {
                tracing::debug!(path = %req.path, "BridgeAdapter: no route, replying 404");
                Self::not_found(&req)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoopServletInvoker;
    use bytes::Bytes;

    /// Minimal single-future executor so the tests need no async runtime
    /// dependency — the bridge futures used here never yield to a real
    /// reactor, so a busy poll terminates immediately.
    fn block_on<F: std::future::Future>(mut fut: F) -> F::Output {
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

    fn coyote_request(path: &str) -> tomcatrs_coyote::Request {
        tomcatrs_coyote::Request {
            method: "GET".into(),
            uri: path.into(),
            path: path.into(),
            query: None,
            version: "HTTP/1.1".into(),
            headers: vec![("Host".into(), "localhost".into())],
            body: Bytes::new(),
            peer_addr: "127.0.0.1:54321".parse().unwrap(),
        }
    }

    /// A `ServletDispatch` over the no-JVM stub invoker — the only kind that
    /// can be constructed on the default build.
    fn noop_dispatch() -> ServletDispatch {
        ServletDispatch::without_runtime(Arc::new(NoopServletInvoker::new()))
    }

    #[test]
    fn marshalling_error_converts_to_bridge_error() {
        let err = MarshallingError::new("bad header name");
        assert!(err.to_string().contains("bad header name"));
        let core: tomcatrs_core::Error = err.into();
        match core {
            tomcatrs_core::Error::Bridge(msg) => assert!(msg.contains("bad header name")),
            other => panic!("expected Error::Bridge, got {other:?}"),
        }
    }

    #[test]
    fn request_handle_carries_coyote_metadata_and_body() {
        let mut req = coyote_request("/app/hello");
        req.method = "POST".into();
        req.query = Some("q=1".into());
        req.body = Bytes::from_static(b"payload");

        let handle = request_handle_from_coyote(&req);
        assert_eq!(handle.parts().method, "POST");
        assert_eq!(handle.parts().uri, "/app/hello");
        assert_eq!(handle.parts().query.as_deref(), Some("q=1"));
        assert_eq!(handle.parts().protocol, "HTTP/1.1");
        assert_eq!(handle.parts().remote_addr, "127.0.0.1:54321");
        assert_eq!(handle.parts().scheme, "http");
        // Body is handed over as a streamable Rust-side cursor.
        assert_eq!(handle.body_remaining(), 7);
        assert_eq!(&handle.read_body(7)[..], b"payload");
    }

    #[test]
    fn scheme_honours_forwarded_proto_header() {
        let mut req = coyote_request("/x");
        req.headers
            .push(("X-Forwarded-Proto".into(), "HTTPS".into()));
        assert_eq!(scheme_for(&req), "https");
    }

    #[test]
    fn dispatch_round_trips_coyote_request_to_response() {
        let dispatch = noop_dispatch();
        let req = coyote_request("/app/hello");

        let resp = block_on(dispatch.dispatch("/app".to_string(), "hello".to_string(), req))
            .expect("noop dispatch never errors");

        // The no-JVM stub answers 501 with an explanatory text/plain body.
        assert_eq!(resp.status, 501);
        assert_eq!(
            resp.header("content-type"),
            Some("text/plain; charset=utf-8")
        );
        let body = String::from_utf8(resp.body.to_vec()).unwrap();
        assert!(body.contains("501 Not Implemented"));
        assert!(body.contains("--features jvm"));
    }

    #[test]
    fn bridge_adapter_unresolved_route_replies_404() {
        use tomcatrs_coyote::Adapter;

        let adapter = BridgeAdapter::from_fn(noop_dispatch(), |_req| None);
        let resp = block_on(adapter.service(coyote_request("/nope")));

        assert_eq!(resp.status, 404);
        assert_eq!(
            resp.header("content-type"),
            Some("text/plain; charset=utf-8")
        );
        let body = String::from_utf8(resp.body.to_vec()).unwrap();
        assert!(body.contains("404 Not Found"));
        assert!(body.contains("/nope"));
    }

    #[test]
    fn bridge_adapter_resolved_route_dispatches_through_bridge() {
        use tomcatrs_coyote::Adapter;

        let adapter = BridgeAdapter::from_fn(noop_dispatch(), |req| {
            // Trivial resolver: anything under `/app` maps to the `hello`
            // servlet of the `/app` context.
            req.path
                .starts_with("/app")
                .then(|| ("/app".to_string(), "hello".to_string()))
        });

        // Resolved -> dispatched -> NoopServletInvoker -> 501 with body.
        let resp = block_on(adapter.service(coyote_request("/app/hello")));
        assert_eq!(resp.status, 501);
        let body = String::from_utf8(resp.body.to_vec()).unwrap();
        assert!(body.contains("501 Not Implemented"));
        assert!(body.contains("Servlet 'hello'"));
        assert!(body.contains("context '/app'"));

        // Unresolved sibling path still 404s through the same adapter.
        let resp = block_on(adapter.service(coyote_request("/other")));
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn dispatch_without_runtime_reports_no_runtime() {
        let dispatch = noop_dispatch();
        assert!(dispatch.runtime().is_none());
        // The invoker is still reachable.
        let _ = dispatch.invoker();
    }

    #[test]
    fn adapter_and_dispatch_are_debug() {
        let adapter = BridgeAdapter::from_fn(noop_dispatch(), |_| None);
        let s = format!("{adapter:?}");
        assert!(s.contains("BridgeAdapter"));
        assert!(s.contains("ServletDispatch"));
    }
}
