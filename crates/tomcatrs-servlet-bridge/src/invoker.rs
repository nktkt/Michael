//! The real [`ServletInvoker`]: [`JvmServletInvoker`], which crosses JNI into
//! the embedded JVM (under `--features jvm`) or degrades to an honest stub
//! (default features).
//!
//! # Where this sits
//!
//! ```text
//!   connector ──Request──▶ JvmServletInvoker::invoke_coyote
//!                              │  from_coyote
//!                              ▼
//!                          RequestHandle / ResponseHandle
//!                              │  invoke()
//!                              ▼
//!        ┌──────────── #[cfg(feature = "jvm")] ───────────┐
//!        │  register handles in the jni HANDLE_REGISTRY   │
//!        │  JvmRuntime::with_env(|env| { … })             │
//!        │    look up WebappRuntime for the context       │
//!        │    instantiate / fetch the servlet             │
//!        │    build TomcatRsRequestFacade / …ResponseFacade│
//!        │    ServletDispatcher.dispatch(servlet, req,res) │
//!        │  read the ResponseHandle back, unregister      │
//!        └────────────────────────────────────────────────┘
//!        ┌──────────── #[cfg(not(feature = "jvm"))] ──────┐
//!        │  write a 501 explanatory body, commit, return  │
//!        └────────────────────────────────────────────────┘
//!                              │  into_coyote
//!                              ▼
//!   connector ◀──Response──────┘
//! ```
//!
//! # Two builds, one type
//!
//! [`JvmServletInvoker`] has the *same* public API in both builds. With default
//! features it is fully functional: every invocation answers HTTP
//! `501 Not Implemented` with an explanatory body, so the whole connector →
//! router → invoker → response path is exercised by `cargo test` with no JDK
//! present. With `--features jvm` the same calls reach a live servlet over JNI.
//!
//! # Sibling-module APIs used by the `jvm` path
//!
//! The `jvm`-feature dispatch path is written against APIs owned by other
//! modules this wave:
//!
//! * [`crate::jni`] — the process-global handle registry, used as
//!   `register_request(RequestHandle)` / `register_response(ResponseHandle)`
//!   (the native id is read off the handle itself via
//!   [`RequestHandle::native_id`] / [`ResponseHandle::native_id`]) and
//!   `unregister_request(i64)` / `unregister_response(i64)`.
//! * [`crate::jvm::JvmRuntime`] — `with_env(FnOnce(&mut JNIEnv) -> Result<R>)
//!   -> Result<R>` is the single JNI funnel, `webapp(&ContextId) ->
//!   Option<Arc<WebappRuntime>>` resolves the per-context runtime, and
//!   `WebappRuntime::servlet(&str) -> Option<ServletInstanceHandle>` hands back
//!   the servlet instance whose `global_ref()` is passed to the Java
//!   `ServletDispatcher`.
//!
//! These call sites are all behind `#[cfg(feature = "jvm")]` and so are *not*
//! compiled by the default-feature build/test used to verify this module; if a
//! signature drifts during integration, only `invoke_impl` needs touching.

use std::sync::Arc;

use async_trait::async_trait;
use tomcatrs_core::{ContextId, Result, WrapperId};

use crate::jvm::JvmRuntime;
use crate::request_facade::RequestHandle;
use crate::response_facade::ResponseHandle;
use crate::{InvocationResult, ServletInvoker};

/// HTTP status the no-JVM stub path answers every invocation with.
pub const STUB_STATUS: u16 = 501;

/// A [`ServletInvoker`] backed by the embedded JVM owned by a [`JvmRuntime`].
///
/// Construct one with [`JvmServletInvoker::new`], passing the shared
/// [`JvmRuntime`]. The invoker holds only an `Arc` clone, so it is cheap to
/// hand to every connector worker.
#[derive(Clone)]
pub struct JvmServletInvoker {
    runtime: Arc<JvmRuntime>,
}

impl std::fmt::Debug for JvmServletInvoker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JvmServletInvoker")
            .field("jvm_feature", &cfg!(feature = "jvm"))
            .finish()
    }
}

impl JvmServletInvoker {
    /// Wrap a shared [`JvmRuntime`] in a servlet invoker.
    pub fn new(runtime: Arc<JvmRuntime>) -> Self {
        Self { runtime }
    }

    /// The [`JvmRuntime`] this invoker dispatches into.
    pub fn runtime(&self) -> &Arc<JvmRuntime> {
        &self.runtime
    }

    /// Convenience round-trip wrapper: marshal a connector
    /// [`tomcatrs_coyote::Request`] into a [`RequestHandle`], invoke the
    /// servlet, and marshal the [`ResponseHandle`] back into a
    /// [`tomcatrs_coyote::Response`].
    ///
    /// This is the call the connector's `Adapter` ultimately makes;
    /// [`ServletInvoker::invoke`] itself stays handle-based so the bridge can
    /// also drive it from the async re-dispatch path.
    pub async fn invoke_coyote(
        &self,
        context: ContextId,
        wrapper: WrapperId,
        request: &tomcatrs_coyote::Request,
    ) -> Result<tomcatrs_coyote::Response> {
        let req = RequestHandle::from_coyote(request);
        let resp = ResponseHandle::new();
        // Keep a clone so we can read the response back after `invoke`
        // consumes the handle; both clones share the same underlying state.
        let resp_for_wire = resp.clone();

        self.invoke(context, wrapper, req, resp).await?;

        Ok(resp_for_wire.into_coyote())
    }
}

#[async_trait]
impl ServletInvoker for JvmServletInvoker {
    async fn invoke(
        &self,
        context: ContextId,
        wrapper: WrapperId,
        request: RequestHandle,
        response: ResponseHandle,
    ) -> Result<InvocationResult> {
        invoke_impl(&self.runtime, context, wrapper, request, response).await
    }
}

// ---------------------------------------------------------------------------
// Real implementation — only compiled with `--features jvm`.
// ---------------------------------------------------------------------------
#[cfg(feature = "jvm")]
async fn invoke_impl(
    runtime: &Arc<JvmRuntime>,
    context: ContextId,
    wrapper: WrapperId,
    request: RequestHandle,
    response: ResponseHandle,
) -> Result<InvocationResult> {
    use tomcatrs_core::Error;

    // Guard against a servlet being dispatched twice on the same handle.
    if request.mark_dispatched() {
        return Err(Error::bridge(format!(
            "request {} already dispatched to a servlet",
            request.id()
        )));
    }

    // 1. Register the handles so the `NativeRequest` / `NativeResponse` JNI
    //    callbacks can resolve the opaque ids the Java facades carry. The
    //    registry is owned by `crate::jni`; the ids are the handles' own
    //    `native_id()` values.
    let request_id = request.native_id();
    let response_id = response.native_id();
    crate::jni::register_request(request.clone());
    crate::jni::register_response(response.clone());

    // Ensure we always unregister, even on an early return / JNI error.
    struct Unregister {
        request_id: i64,
        response_id: i64,
    }
    impl Drop for Unregister {
        fn drop(&mut self) {
            crate::jni::unregister_request(self.request_id);
            crate::jni::unregister_response(self.response_id);
        }
    }
    let _cleanup = Unregister {
        request_id,
        response_id,
    };

    // 2. Resolve the JVM-side runtime (class loader, servlet cache) for the
    //    web application this request was routed to, and the servlet instance.
    let webapp = match runtime.webapp(&context) {
        Some(w) => w,
        None => {
            return Ok(fallback_response(
                &context,
                &wrapper,
                &response,
                Error::bridge(format!("no webapp registered for context {context}")),
            ));
        }
    };
    let servlet = match webapp.servlet(&wrapper) {
        Some(s) => s,
        None => {
            return Ok(fallback_response(
                &context,
                &wrapper,
                &response,
                Error::bridge(format!("servlet {wrapper} not registered in {context}")),
            ));
        }
    };

    // 3. Cross into the JVM on an attached worker thread and dispatch.
    let dispatch = runtime.with_env(|env| -> Result<()> {
        // Build the Java-side facade objects, passing the opaque native ids.
        // Their accessors call back into the natives registered by `crate::jni`.
        let req_facade = env
            .new_object(
                "org/apache/tomcatrs/bridge/TomcatRsRequestFacade",
                "(J)V",
                &[jni::objects::JValue::Long(request_id)],
            )
            .map_err(|e| Error::bridge(format!("constructing request facade failed: {e}")))?;
        let res_facade = env
            .new_object(
                "org/apache/tomcatrs/bridge/TomcatRsResponseFacade",
                "(J)V",
                &[jni::objects::JValue::Long(response_id)],
            )
            .map_err(|e| Error::bridge(format!("constructing response facade failed: {e}")))?;

        // Hand off to the Java `ServletDispatcher` helper, which performs the
        // `servlet.service(req, res)` call (and the filter chain) inside the
        // webapp's class loader.
        env.call_static_method(
            "org/apache/tomcatrs/bridge/ServletDispatcher",
            "dispatch",
            "(Ljakarta/servlet/Servlet;Ljakarta/servlet/ServletRequest;\
             Ljakarta/servlet/ServletResponse;)V",
            &[
                jni::objects::JValue::Object(servlet.global_ref().as_obj()),
                jni::objects::JValue::Object(&req_facade),
                jni::objects::JValue::Object(&res_facade),
            ],
        )
        .map_err(|e| Error::bridge(format!("ServletDispatcher.dispatch failed: {e}")))?;

        Ok(())
    });

    match dispatch {
        Ok(()) => {
            // 3. Read the response back from the (shared) handle and finish it.
            let snap = response.complete();
            Ok(InvocationResult::new(
                snap.effective_status(),
                snap.committed,
            ))
        }
        // The JVM isn't actually available, or `with_env` itself failed: fall
        // back gracefully rather than dropping the connection.
        Err(e) => Ok(fallback_response(&context, &wrapper, &response, e)),
    }
    // `_cleanup` drops here, unregistering both handles.
}

/// On a JVM dispatch failure, write a `502 Bad Gateway` explanatory body so the
/// connector still has a coherent response to serialize.
#[cfg(feature = "jvm")]
fn fallback_response(
    context: &str,
    wrapper: &str,
    response: &ResponseHandle,
    error: tomcatrs_core::Error,
) -> InvocationResult {
    tracing::error!(%context, %wrapper, %error, "JVM servlet dispatch failed; serving 502");
    const STATUS: u16 = 502;
    let body = format!(
        "502 Bad Gateway\n\n\
         The Tomcat-RS servlet bridge could not dispatch servlet '{wrapper}' \
         in context '{context}' to the embedded JVM.\n\
         Cause: {error}\n"
    );
    if !response.is_committed() {
        response.set_status(STATUS);
        response.set_header("Content-Type", "text/plain; charset=utf-8");
    }
    response.write_body(body.as_bytes());
    let committed = response.commit();
    InvocationResult::new(response.effective_status(), committed)
}

// ---------------------------------------------------------------------------
// Stub implementation — compiled with default features (no JDK required).
// ---------------------------------------------------------------------------
#[cfg(not(feature = "jvm"))]
async fn invoke_impl(
    _runtime: &Arc<JvmRuntime>,
    context: ContextId,
    wrapper: WrapperId,
    request: RequestHandle,
    response: ResponseHandle,
) -> Result<InvocationResult> {
    Ok(stub_invoke(&context, &wrapper, &request, &response))
}

/// The no-JVM dispatch logic, factored out of [`invoke_impl`] so it can be unit
/// tested without an `Arc<JvmRuntime>` — which is impossible to construct on
/// the default-feature build, where the stub `JvmRuntime` is uninhabited.
///
/// Writes a `501 Not Implemented` `text/plain` body to `response`, commits it,
/// and reports the outcome. Also marks `request` as dispatched, so the full
/// handle lifecycle is exercised exactly as it would be on the real path.
#[cfg(not(feature = "jvm"))]
fn stub_invoke(
    context: &str,
    wrapper: &str,
    request: &RequestHandle,
    response: &ResponseHandle,
) -> InvocationResult {
    let _ = request.mark_dispatched();

    tracing::debug!(
        %context,
        %wrapper,
        request_id = request.id(),
        "JvmServletInvoker: built without --features jvm, replying 501"
    );

    let body = format!(
        "501 Not Implemented\n\n\
         The Tomcat-RS servlet bridge was compiled without the embedded JVM.\n\
         Servlet '{wrapper}' in context '{context}' cannot be executed.\n\
         Rebuild tomcatrs-servlet-bridge with `--features jvm` to enable \
         Java servlet support.\n"
    );

    response.set_status(STUB_STATUS);
    response.set_header("Content-Type", "text/plain; charset=utf-8");
    response.write_body(body.as_bytes());
    let committed = response.commit();

    InvocationResult::new(STUB_STATUS, committed)
}

#[cfg(all(test, not(feature = "jvm")))]
mod tests {
    use super::*;
    use crate::request_facade::RequestParts;
    use bytes::Bytes;

    fn coyote_request() -> tomcatrs_coyote::Request {
        tomcatrs_coyote::Request {
            method: "GET".into(),
            uri: "/app/hello".into(),
            path: "/app/hello".into(),
            query: None,
            version: "HTTP/1.1".into(),
            headers: vec![("Host".into(), "localhost".into())],
            body: Bytes::new(),
            peer_addr: "127.0.0.1:8080".parse().unwrap(),
        }
    }

    #[test]
    fn stub_invoke_replies_501_with_body() {
        let req = RequestHandle::new(RequestParts {
            method: "GET".into(),
            uri: "/app/hello".into(),
            ..RequestParts::default()
        });
        let resp = ResponseHandle::new();

        let result = stub_invoke("/app", "hello", &req, &resp);

        assert_eq!(result.status, 501);
        assert_eq!(result.status, STUB_STATUS);
        assert!(result.committed);
        // The request handle went through the same dispatch bookkeeping.
        assert!(req.is_dispatched());

        let snap = resp.snapshot();
        assert_eq!(snap.status, Some(501));
        assert!(snap.committed);
        let body = String::from_utf8(snap.body).unwrap();
        assert!(body.contains("501 Not Implemented"));
        assert!(body.contains("--features jvm"));
        assert!(body.contains("hello"));
        assert!(body.contains("/app"));
        assert_eq!(
            resp.header("content-type").as_deref(),
            Some("text/plain; charset=utf-8")
        );
    }

    /// Mirrors what [`JvmServletInvoker::invoke_coyote`] does internally —
    /// `from_coyote` → dispatch → `into_coyote` — without needing an
    /// `Arc<JvmRuntime>` (uninhabited on the default build).
    #[test]
    fn stub_invoke_coyote_round_trip_marshalling() {
        let raw = coyote_request();
        let req = RequestHandle::from_coyote(&raw);
        let resp = ResponseHandle::new();
        let resp_for_wire = resp.clone();

        let result = stub_invoke("/app", "hello", &req, &resp);
        assert_eq!(result.status, 501);

        let response = resp_for_wire.into_coyote();
        assert_eq!(response.status, 501);
        assert_eq!(
            response.header("content-type"),
            Some("text/plain; charset=utf-8")
        );
        let body = String::from_utf8(response.body.to_vec()).unwrap();
        assert!(body.contains("501 Not Implemented"));
    }
}
