//! Rust-side request state and the contract with the Java request facade.
//!
//! # The lazy-materialization contract
//!
//! When the connector parses an HTTP request it builds a [`RequestParts`] and
//! wraps it in a [`RequestHandle`]. The handle is given a process-unique
//! [`RequestHandle::id`] (`nativeRequestId`). Only that `u64` crosses the JNI
//! boundary.
//!
//! On the JVM side, `org.apache.tomcatrs.bridge.TomcatRsRequestFacade`
//! implements `jakarta.servlet.http.HttpServletRequest`. Each accessor
//! (`getHeader`, `getParameter`, `getInputStream`, …) is a thin Java method
//! that calls a `native` method on the companion `NativeRequest` class,
//! passing `nativeRequestId`. The JNI glue in [`crate::jni`] looks the handle
//! up in the [`crate::jvm::JvmRuntime`] request registry and returns just the
//! requested value.
//!
//! This means a servlet that only reads two headers causes exactly two JNI
//! round trips for headers — not a bulk copy of the entire request.
//!
//! # Java facade classes
//!
//! The `java/` directory of this crate contains the scaffold sources, compiled
//! into [`crate::BRIDGE_JAR_NAME`]:
//!
//! * **`TomcatRsRequestFacade`** — `implements HttpServletRequest`. Holds
//!   `long nativeRequestId`. Delegates every getter to `NativeRequest`.
//! * **`NativeRequest`** — package-private holder of the `native` method
//!   declarations (`nativeGetHeader`, `nativeGetMethod`,
//!   `nativeReadBody`, …). Its `static { System.loadLibrary(...) }` is a no-op
//!   because the natives are registered by the embedding Rust process via
//!   `RegisterNatives`, not loaded from a `.so`.
//! * **`TomcatRsServletContext`** — `implements ServletContext`; bridges
//!   context-init parameters and `getRealPath` to the Rust webapp registry.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use dashmap::DashMap;

/// Monotonic source of `nativeRequestId` values.
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// The immutable, eagerly-parsed portion of an HTTP request.
///
/// The connector fills this in once from the wire. Everything here is cheap to
/// hold in Rust; it is *not* pushed across JNI — the Java facade pulls
/// individual fields lazily.
#[derive(Debug, Clone, Default)]
pub struct RequestParts {
    /// HTTP method, upper-cased (`GET`, `POST`, …).
    pub method: String,
    /// Request URI including the context path, excluding the query string.
    pub uri: String,
    /// Raw query string (without the leading `?`), if any.
    pub query: Option<String>,
    /// HTTP protocol token, e.g. `HTTP/1.1`.
    pub protocol: String,
    /// Request headers, in arrival order. Names are case-insensitive per
    /// RFC 9110 — lookups via [`RequestHandle::header`] fold case.
    pub headers: Vec<(String, String)>,
    /// Remote peer address as a string, e.g. `203.0.113.7:54321`.
    pub remote_addr: String,
    /// Scheme derived by the connector (`http` / `https`).
    pub scheme: String,
}

impl RequestParts {
    /// Case-insensitive header lookup over the eagerly-parsed headers.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// The streaming request body, surfaced to Java as a `ServletInputStream`.
///
/// In v0.1.0 the body is modelled as a buffered [`Bytes`] cursor: the
/// connector may hand over an already-read body, or none. The type is
/// deliberately an `enum` so the `jvm` feature can later add a truly streaming
/// variant (an async reader pumped by the worker pool) without changing the
/// public surface of [`RequestHandle`].
#[derive(Debug, Default)]
pub enum RequestBody {
    /// No body (e.g. a `GET` with no payload).
    #[default]
    Empty,
    /// A fully-buffered body together with a read cursor.
    Buffered {
        /// The body bytes.
        data: Bytes,
        /// Number of bytes already consumed by `nativeReadBody` calls.
        position: usize,
    },
}

impl RequestBody {
    /// Bytes not yet consumed.
    pub fn remaining(&self) -> usize {
        match self {
            RequestBody::Empty => 0,
            RequestBody::Buffered { data, position } => data.len().saturating_sub(*position),
        }
    }

    /// Read up to `max` more bytes, advancing the cursor. Returns an empty
    /// slice at end-of-stream. This is the Rust side of the Java
    /// `ServletInputStream.read(byte[])` call.
    pub fn read(&mut self, max: usize) -> Bytes {
        match self {
            RequestBody::Empty => Bytes::new(),
            RequestBody::Buffered { data, position } => {
                let start = *position;
                let end = (start + max).min(data.len());
                *position = end;
                data.slice(start..end)
            }
        }
    }
}

#[derive(Debug)]
struct RequestInner {
    id: u64,
    parts: RequestParts,
    body: std::sync::Mutex<RequestBody>,
    /// Request attributes (`ServletRequest.setAttribute`). Populated lazily,
    /// possibly *from the Java side* — hence interior mutability.
    attributes: DashMap<String, String>,
    /// Set once the request has been dispatched to a servlet.
    dispatched: AtomicBool,
}

/// An opaque, cheaply-cloneable handle to one in-flight HTTP request.
///
/// Cloning shares the same underlying state (the clone carries the same
/// `nativeRequestId`); this is what lets the worker pool, the JNI callbacks and
/// the [`crate::async_servlet::AsyncContextState`] all refer to the same
/// request.
#[derive(Debug, Clone)]
pub struct RequestHandle {
    inner: Arc<RequestInner>,
}

impl RequestHandle {
    /// Wrap freshly-parsed [`RequestParts`] in a new handle with a fresh
    /// `nativeRequestId` and an empty body.
    pub fn new(parts: RequestParts) -> Self {
        Self::with_body(parts, RequestBody::Empty)
    }

    /// Like [`RequestHandle::new`] but with an attached request body.
    pub fn with_body(parts: RequestParts, body: RequestBody) -> Self {
        let id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: Arc::new(RequestInner {
                id,
                parts,
                body: std::sync::Mutex::new(body),
                attributes: DashMap::new(),
                dispatched: AtomicBool::new(false),
            }),
        }
    }

    /// The opaque `nativeRequestId` — the only value that crosses JNI.
    pub fn id(&self) -> u64 {
        self.inner.id
    }

    /// The eagerly-parsed request metadata.
    pub fn parts(&self) -> &RequestParts {
        &self.inner.parts
    }

    /// Case-insensitive header lookup. The Java `nativeGetHeader` glue calls
    /// straight through to this.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.inner.parts.header(name)
    }

    /// Read up to `max` more body bytes (the Rust side of
    /// `ServletInputStream.read`). Returns an empty slice at end-of-stream.
    pub fn read_body(&self, max: usize) -> Bytes {
        self.inner
            .body
            .lock()
            .expect("request body mutex poisoned")
            .read(max)
    }

    /// Bytes of request body not yet consumed.
    pub fn body_remaining(&self) -> usize {
        self.inner
            .body
            .lock()
            .expect("request body mutex poisoned")
            .remaining()
    }

    /// Set a request attribute. Callable from Rust or, via JNI glue, from the
    /// Java `setAttribute`.
    pub fn set_attribute(&self, name: impl Into<String>, value: impl Into<String>) {
        self.inner.attributes.insert(name.into(), value.into());
    }

    /// Retrieve a previously-set request attribute.
    pub fn attribute(&self, name: &str) -> Option<String> {
        self.inner.attributes.get(name).map(|v| v.clone())
    }

    /// Mark the request as dispatched to a servlet; returns the previous value.
    /// Used to guard against double-dispatch.
    pub fn mark_dispatched(&self) -> bool {
        self.inner.dispatched.swap(true, Ordering::SeqCst)
    }

    /// Whether [`RequestHandle::mark_dispatched`] has been called.
    pub fn is_dispatched(&self) -> bool {
        self.inner.dispatched.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_monotonic() {
        let a = RequestHandle::new(RequestParts::default());
        let b = RequestHandle::new(RequestParts::default());
        assert!(b.id() > a.id());
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let req = RequestHandle::new(RequestParts {
            headers: vec![("Content-Type".into(), "text/html".into())],
            ..RequestParts::default()
        });
        assert_eq!(req.header("content-type"), Some("text/html"));
        assert_eq!(req.header("CONTENT-TYPE"), Some("text/html"));
        assert_eq!(req.header("missing"), None);
    }

    #[test]
    fn body_streams_in_chunks() {
        let req = RequestHandle::with_body(
            RequestParts::default(),
            RequestBody::Buffered {
                data: Bytes::from_static(b"hello world"),
                position: 0,
            },
        );
        assert_eq!(req.body_remaining(), 11);
        assert_eq!(&req.read_body(5)[..], b"hello");
        assert_eq!(&req.read_body(99)[..], b" world");
        assert_eq!(req.body_remaining(), 0);
        assert!(req.read_body(1).is_empty());
    }

    #[test]
    fn attributes_round_trip() {
        let req = RequestHandle::new(RequestParts::default());
        assert_eq!(req.attribute("k"), None);
        req.set_attribute("k", "v");
        assert_eq!(req.attribute("k").as_deref(), Some("v"));
    }

    #[test]
    fn dispatch_guard() {
        let req = RequestHandle::new(RequestParts::default());
        assert!(!req.is_dispatched());
        assert!(!req.mark_dispatched());
        assert!(req.mark_dispatched());
        assert!(req.is_dispatched());
    }

    #[test]
    fn clone_shares_state() {
        let req = RequestHandle::new(RequestParts::default());
        let clone = req.clone();
        assert_eq!(req.id(), clone.id());
        req.set_attribute("shared", "yes");
        assert_eq!(clone.attribute("shared").as_deref(), Some("yes"));
    }
}
