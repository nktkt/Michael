//! Rust-side response state and the contract with the Java response facade.
//!
//! # The streaming-sink contract
//!
//! Symmetrically to [`crate::request_facade`], the connector creates a
//! [`ResponseHandle`] with a fresh `nativeResponseId` and hands it to the
//! invoker. On the JVM side
//! `org.apache.tomcatrs.bridge.TomcatRsResponseFacade` implements
//! `jakarta.servlet.http.HttpServletResponse`; its `ServletOutputStream`
//! forwards `write(byte[])` calls to a `native` method that appends straight
//! into the Rust [`ResponseSink`]. No full-response buffer is built on the
//! Java heap.
//!
//! Status and headers behave the same way: `setStatus` / `setHeader` are
//! `native` calls that mutate the Rust sink, and become immutable once the
//! response is *committed* (the first body flush, or an explicit `flushBuffer`).
//!
//! # Reading the response back
//!
//! Once the invoker returns, the connector needs the finished response. Three
//! shapes are offered, all reading the same underlying state:
//!
//! * [`ResponseHandle::snapshot`] / [`ResponseSink::snapshot`] — an immutable
//!   [`ResponseSnapshot`] copy (status, headers, body, committed flag).
//! * [`ResponseHandle::into_coyote`] — consume the handle and produce a
//!   [`tomcatrs_coyote::Response`] ready to serialize onto the wire.
//!
//! # Java facade classes
//!
//! Compiled into [`crate::BRIDGE_JAR_NAME`]:
//!
//! * **`TomcatRsResponseFacade`** — `implements HttpServletResponse`. Holds
//!   `long nativeResponseId`. Its `getOutputStream()` returns a
//!   `ServletOutputStream` whose `write` paths call
//!   `NativeResponse.nativeWriteBody`.
//! * **`NativeResponse`** — package-private holder of the `native` method
//!   declarations (`nativeSetStatus`, `nativeAddHeader`, `nativeWriteBody`,
//!   `nativeCommit`, …).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Monotonic source of `nativeResponseId` values.
static NEXT_RESPONSE_ID: AtomicU64 = AtomicU64::new(1);

/// The HTTP status a [`ResponseHandle`] reports until a servlet sets one
/// explicitly, matching the Servlet spec default.
pub const DEFAULT_STATUS: u16 = 200;

/// The mutable Rust-side accumulator behind a [`ResponseHandle`].
///
/// This is the sink that the Java `ServletOutputStream` drains into. It is
/// shared (`Arc<Mutex<…>>`) so the connector can observe the final response
/// after the invoker returns, even though the invoker held the
/// [`ResponseHandle`].
#[derive(Debug, Default)]
struct ResponseState {
    status: Option<u16>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    committed: bool,
}

/// A cheap, cloneable observer of a [`ResponseHandle`]'s underlying state.
///
/// The connector keeps one of these so that, after [`crate::ServletInvoker`]
/// returns, it can [`ResponseSink::snapshot`] the bytes/status/headers the
/// servlet produced and write them to the wire.
#[derive(Debug, Clone)]
pub struct ResponseSink {
    state: Arc<Mutex<ResponseState>>,
}

/// An immutable point-in-time copy of a response, taken by
/// [`ResponseSink::snapshot`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponseSnapshot {
    /// Final status code, if one was set.
    pub status: Option<u16>,
    /// Response headers in insertion order.
    pub headers: Vec<(String, String)>,
    /// Full response body accumulated so far.
    pub body: Vec<u8>,
    /// Whether the response has been committed.
    pub committed: bool,
}

impl ResponseSnapshot {
    /// The effective status code: the explicitly-set one, or
    /// [`DEFAULT_STATUS`] if the servlet never set one.
    pub fn effective_status(&self) -> u16 {
        self.status.unwrap_or(DEFAULT_STATUS)
    }
}

impl ResponseSink {
    /// Take an immutable copy of the current response state.
    pub fn snapshot(&self) -> ResponseSnapshot {
        let s = self.state.lock().expect("response state mutex poisoned");
        ResponseSnapshot {
            status: s.status,
            headers: s.headers.clone(),
            body: s.body.clone(),
            committed: s.committed,
        }
    }

    /// Whether the response has been committed.
    pub fn is_committed(&self) -> bool {
        self.state
            .lock()
            .expect("response state mutex poisoned")
            .committed
    }
}

/// An opaque, cheaply-cloneable handle to one in-flight HTTP response.
///
/// All mutators are `&self`: they are called both from Rust and, via JNI glue,
/// from the Java facade, possibly from a worker thread distinct from the one
/// that created the handle. Mutations after [`ResponseHandle::commit`] are
/// silently ignored — matching the Servlet spec, where writing headers after
/// commit is a no-op rather than an error.
#[derive(Debug, Clone)]
pub struct ResponseHandle {
    id: u64,
    state: Arc<Mutex<ResponseState>>,
}

impl ResponseHandle {
    /// Create a fresh response handle with a new `nativeResponseId`.
    pub fn new() -> Self {
        Self {
            id: NEXT_RESPONSE_ID.fetch_add(1, Ordering::Relaxed),
            state: Arc::new(Mutex::new(ResponseState::default())),
        }
    }

    /// The opaque `nativeResponseId` — the only value that crosses JNI.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The `nativeResponseId` as the `jlong` (`i64`) the Java facade actually
    /// stores. Convenience for the JNI / invoker marshalling layer.
    pub fn native_id(&self) -> i64 {
        self.id as i64
    }

    /// A cloneable observer the connector keeps to read the final response.
    pub fn sink_handle(&self) -> ResponseSink {
        ResponseSink {
            state: Arc::clone(&self.state),
        }
    }

    /// Set the HTTP status code. Ignored if already committed. The Rust side of
    /// the Java `nativeSetStatus`.
    pub fn set_status(&self, status: u16) {
        let mut s = self.state.lock().expect("response state mutex poisoned");
        if !s.committed {
            s.status = Some(status);
        }
    }

    /// The explicitly-set status code, or `None` if a servlet never set one.
    /// Use [`ResponseHandle::effective_status`] for the wire value.
    pub fn status(&self) -> Option<u16> {
        self.state
            .lock()
            .expect("response state mutex poisoned")
            .status
    }

    /// The effective status code: the explicitly-set one, or
    /// [`DEFAULT_STATUS`] if none was set.
    pub fn effective_status(&self) -> u16 {
        self.status().unwrap_or(DEFAULT_STATUS)
    }

    /// Append (or, if already present, replace) a response header. Ignored if
    /// already committed. The Rust side of the Java `nativeSetHeader`.
    pub fn set_header(&self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        let value = value.into();
        let mut s = self.state.lock().expect("response state mutex poisoned");
        if s.committed {
            return;
        }
        if let Some(existing) = s
            .headers
            .iter_mut()
            .find(|(k, _)| k.eq_ignore_ascii_case(&name))
        {
            existing.1 = value;
        } else {
            s.headers.push((name, value));
        }
    }

    /// Append a header, allowing duplicates (the Rust side of the Java
    /// `nativeAddHeader`, used for e.g. multiple `Set-Cookie`s). Ignored if
    /// already committed.
    pub fn add_header(&self, name: impl Into<String>, value: impl Into<String>) {
        let mut s = self.state.lock().expect("response state mutex poisoned");
        if !s.committed {
            s.headers.push((name.into(), value.into()));
        }
    }

    /// Case-insensitive lookup of the first value set for `name`.
    pub fn header(&self, name: &str) -> Option<String> {
        let s = self.state.lock().expect("response state mutex poisoned");
        s.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }

    /// Append body bytes — the Rust side of `ServletOutputStream.write`.
    /// Writing body bytes does *not* itself commit the response in this model;
    /// the connector decides when to flush. Bytes written after an explicit
    /// [`ResponseHandle::commit`] are still appended (the stream stays open),
    /// but headers can no longer change.
    pub fn write_body(&self, bytes: &[u8]) {
        let mut s = self.state.lock().expect("response state mutex poisoned");
        s.body.extend_from_slice(bytes);
    }

    /// Number of body bytes accumulated so far.
    pub fn body_len(&self) -> usize {
        self.state
            .lock()
            .expect("response state mutex poisoned")
            .body
            .len()
    }

    /// Flush the response: the Rust side of `ServletResponse.flushBuffer`.
    ///
    /// In the v1.0.0 buffered model "flushing" simply commits the status line
    /// and headers (there is no incremental wire write yet); it is therefore an
    /// alias for [`ResponseHandle::commit`] kept under the Servlet-API name so
    /// the Java facade reads naturally. Idempotent.
    pub fn flush(&self) -> bool {
        self.commit()
    }

    /// Commit the response: freeze the status line and headers. Idempotent.
    /// Returns the committed flag (always `true` afterwards), mirroring what
    /// the connector reports in [`crate::InvocationResult`].
    pub fn commit(&self) -> bool {
        let mut s = self.state.lock().expect("response state mutex poisoned");
        s.committed = true;
        if s.status.is_none() {
            s.status = Some(DEFAULT_STATUS);
        }
        true
    }

    /// Finish the response: commit it (if not already) and return the resulting
    /// [`InvocationResult`](crate::InvocationResult)-shaped pair via a
    /// [`ResponseSnapshot`].
    ///
    /// This is the natural "the servlet returned, wrap things up" call for the
    /// [`crate::invoker`] layer.
    pub fn complete(&self) -> ResponseSnapshot {
        self.commit();
        self.snapshot()
    }

    /// Whether the response has been committed.
    pub fn is_committed(&self) -> bool {
        self.state
            .lock()
            .expect("response state mutex poisoned")
            .committed
    }

    /// Take an immutable snapshot of the current state without consuming the
    /// handle.
    pub fn snapshot(&self) -> ResponseSnapshot {
        self.sink_handle().snapshot()
    }

    /// Consume the handle and marshal it into a connector-ready
    /// [`tomcatrs_coyote::Response`].
    ///
    /// The status is the [`ResponseHandle::effective_status`] (defaulting to
    /// [`DEFAULT_STATUS`]); headers and body are moved across. This is the
    /// counterpart of [`crate::request_facade::RequestHandle::from_coyote`] and
    /// completes the round trip the [`crate::invoker`] layer performs.
    pub fn into_coyote(self) -> tomcatrs_coyote::Response {
        // If other clones of the handle still exist, fall back to cloning the
        // state rather than panicking on `Arc::try_unwrap`.
        let state = match Arc::try_unwrap(self.state) {
            Ok(mutex) => mutex.into_inner().expect("response state mutex poisoned"),
            Err(shared) => {
                let guard = shared.lock().expect("response state mutex poisoned");
                ResponseState {
                    status: guard.status,
                    headers: guard.headers.clone(),
                    body: guard.body.clone(),
                    committed: guard.committed,
                }
            }
        };
        tomcatrs_coyote::Response {
            status: state.status.unwrap_or(DEFAULT_STATUS),
            headers: state.headers,
            body: bytes::Bytes::from(state.body),
        }
    }
}

impl Default for ResponseHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique() {
        let a = ResponseHandle::new();
        let b = ResponseHandle::new();
        assert_ne!(a.id(), b.id());
        assert_eq!(a.native_id(), a.id() as i64);
    }

    #[test]
    fn status_and_body_visible_through_sink() {
        let resp = ResponseHandle::new();
        let sink = resp.sink_handle();
        resp.set_status(404);
        resp.write_body(b"not found");
        let snap = sink.snapshot();
        assert_eq!(snap.status, Some(404));
        assert_eq!(snap.body, b"not found");
        assert!(!snap.committed);
    }

    #[test]
    fn set_header_replaces_add_header_appends() {
        let resp = ResponseHandle::new();
        resp.set_header("X-A", "1");
        resp.set_header("x-a", "2");
        resp.add_header("Set-Cookie", "a=1");
        resp.add_header("Set-Cookie", "b=2");
        let snap = resp.sink_handle().snapshot();
        let x_a: Vec<_> = snap.headers.iter().filter(|(k, _)| k == "X-A").collect();
        assert_eq!(x_a.len(), 1);
        assert_eq!(x_a[0].1, "2");
        assert_eq!(resp.header("x-a").as_deref(), Some("2"));
        let cookies: Vec<_> = snap
            .headers
            .iter()
            .filter(|(k, _)| k == "Set-Cookie")
            .collect();
        assert_eq!(cookies.len(), 2);
    }

    #[test]
    fn commit_freezes_headers_and_status() {
        let resp = ResponseHandle::new();
        resp.set_status(201);
        assert!(resp.commit());
        assert!(resp.is_committed());
        resp.set_status(500);
        resp.set_header("X-Late", "nope");
        let snap = resp.sink_handle().snapshot();
        assert_eq!(snap.status, Some(201));
        assert!(snap.headers.is_empty());
    }

    #[test]
    fn commit_defaults_status_to_200() {
        let resp = ResponseHandle::new();
        resp.commit();
        assert_eq!(resp.sink_handle().snapshot().status, Some(200));
    }

    #[test]
    fn body_can_still_grow_after_commit() {
        let resp = ResponseHandle::new();
        resp.commit();
        resp.write_body(b"streamed");
        assert_eq!(resp.sink_handle().snapshot().body, b"streamed");
    }

    #[test]
    fn flush_is_commit() {
        let resp = ResponseHandle::new();
        assert!(!resp.is_committed());
        assert!(resp.flush());
        assert!(resp.is_committed());
    }

    #[test]
    fn effective_status_defaults_without_explicit_set() {
        let resp = ResponseHandle::new();
        assert_eq!(resp.status(), None);
        assert_eq!(resp.effective_status(), DEFAULT_STATUS);
        resp.set_status(503);
        assert_eq!(resp.effective_status(), 503);
    }

    #[test]
    fn complete_commits_and_snapshots() {
        let resp = ResponseHandle::new();
        resp.set_status(202);
        resp.write_body(b"accepted");
        let snap = resp.complete();
        assert!(snap.committed);
        assert_eq!(snap.status, Some(202));
        assert_eq!(snap.body, b"accepted");
    }

    #[test]
    fn into_coyote_marshals_status_headers_body() {
        let resp = ResponseHandle::new();
        resp.set_status(418);
        resp.set_header("Content-Type", "text/plain");
        resp.add_header("X-Trace", "abc");
        resp.write_body(b"i am a teapot");

        let coyote = resp.into_coyote();
        assert_eq!(coyote.status, 418);
        assert_eq!(coyote.header("content-type"), Some("text/plain"));
        assert_eq!(coyote.header("x-trace"), Some("abc"));
        assert_eq!(&coyote.body[..], b"i am a teapot");
    }

    #[test]
    fn into_coyote_defaults_status_when_unset() {
        let resp = ResponseHandle::new();
        resp.write_body(b"body only");
        let coyote = resp.into_coyote();
        assert_eq!(coyote.status, DEFAULT_STATUS);
        assert_eq!(&coyote.body[..], b"body only");
    }

    #[test]
    fn into_coyote_works_with_outstanding_clones() {
        let resp = ResponseHandle::new();
        let sink = resp.sink_handle();
        resp.set_status(200);
        resp.write_body(b"shared");
        // `sink` keeps the Arc alive; `into_coyote` must still succeed.
        let coyote = resp.into_coyote();
        assert_eq!(&coyote.body[..], b"shared");
        // The observer still sees the state.
        assert_eq!(sink.snapshot().body, b"shared");
    }
}
