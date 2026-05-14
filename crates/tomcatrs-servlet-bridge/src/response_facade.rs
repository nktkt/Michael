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

    /// Append body bytes — the Rust side of `ServletOutputStream.write`.
    /// Writing body bytes does *not* itself commit the response in this model;
    /// the connector decides when to flush. Bytes written after an explicit
    /// [`ResponseHandle::commit`] are still appended (the stream stays open),
    /// but headers can no longer change.
    pub fn write_body(&self, bytes: &[u8]) {
        let mut s = self.state.lock().expect("response state mutex poisoned");
        s.body.extend_from_slice(bytes);
    }

    /// Commit the response: freeze the status line and headers. Idempotent.
    /// Returns the committed flag (always `true` afterwards), mirroring what
    /// the connector reports in [`crate::InvocationResult`].
    pub fn commit(&self) -> bool {
        let mut s = self.state.lock().expect("response state mutex poisoned");
        s.committed = true;
        if s.status.is_none() {
            s.status = Some(200);
        }
        true
    }

    /// Whether the response has been committed.
    pub fn is_committed(&self) -> bool {
        self.state
            .lock()
            .expect("response state mutex poisoned")
            .committed
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
}
