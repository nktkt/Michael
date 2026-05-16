//! Rust-side request state and the contract with the Java request facade.
//!
//! # The lazy-materialization contract
//!
//! When the connector parses an HTTP request it builds a [`RequestParts`] and
//! wraps it in a [`RequestHandle`]. The handle is given a process-unique
//! [`RequestHandle::id`] (`nativeRequestId`). Only that integer crosses the JNI
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
//! # Public surface
//!
//! Two shapes of accessor coexist on [`RequestHandle`], on purpose:
//!
//! * **Rust-ergonomic** — [`RequestHandle::id`] (`u64`),
//!   [`RequestHandle::read_body`] (returns [`Bytes`]). These are what the
//!   existing [`crate::jni`] glue and [`crate::async_servlet`] are written
//!   against.
//! * **JNI-shaped** — [`RequestHandle::native_id`] (`i64`, the literal `jlong`
//!   the Java facade stores), [`RequestHandle::read_body_into`] (fills a caller
//!   `&mut [u8]` and returns a `usize` count, mirroring
//!   `ServletInputStream.read(byte[])`). These are what the
//!   [`crate::invoker::JvmServletInvoker`] marshalling layer uses.
//!
//! Both views address the *same* underlying state; they are kept separate only
//! so neither caller has to convert at every call site.
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

    /// All header names, in arrival order (original casing preserved).
    ///
    /// Backs the Java `HttpServletRequest.getHeaderNames()` enumeration.
    pub fn header_names(&self) -> Vec<String> {
        self.headers.iter().map(|(k, _)| k.clone()).collect()
    }

    /// The value of the `Content-Length` header parsed as a `u64`, if present
    /// and well-formed.
    pub fn content_length(&self) -> Option<u64> {
        self.header("content-length")
            .and_then(|v| v.trim().parse().ok())
    }
}

/// The streaming request body, surfaced to Java as a `ServletInputStream`.
///
/// In v1.0.0 the body is modelled as a buffered [`Bytes`] cursor: the
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
    /// Build a buffered body from anything convertible into [`Bytes`], with the
    /// read cursor at the start.
    pub fn buffered(data: impl Into<Bytes>) -> Self {
        RequestBody::Buffered {
            data: data.into(),
            position: 0,
        }
    }

    /// Total length of the body, regardless of how much has been consumed.
    pub fn len(&self) -> usize {
        match self {
            RequestBody::Empty => 0,
            RequestBody::Buffered { data, .. } => data.len(),
        }
    }

    /// Whether the body carries no bytes at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

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

    /// Read into a caller-provided buffer, advancing the cursor. Returns the
    /// number of bytes copied — `0` at end-of-stream — exactly like
    /// `java.io.InputStream.read(byte[])` (modulo the `-1` sentinel, which the
    /// JNI glue substitutes).
    pub fn read_into(&mut self, buf: &mut [u8]) -> usize {
        let chunk = self.read(buf.len());
        buf[..chunk.len()].copy_from_slice(&chunk);
        chunk.len()
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

    /// Build a [`RequestHandle`] from a connector-produced
    /// [`tomcatrs_coyote::Request`].
    ///
    /// This is the canonical entry point for the [`crate::invoker`] layer: the
    /// connector parses the wire request once, and this copies the eagerly
    /// available metadata into a [`RequestParts`] while the body is moved in as
    /// a buffered [`RequestBody`] cursor (the `Bytes` clone is cheap — it is
    /// reference-counted, not copied).
    ///
    /// The handle receives a fresh, process-unique `nativeRequestId`.
    pub fn from_coyote(req: &tomcatrs_coyote::Request) -> Self {
        let scheme = req
            .header("x-forwarded-proto")
            .map(str::to_owned)
            .unwrap_or_else(|| "http".to_string());
        // The Servlet spec is explicit: `getRequestURI()` returns the path
        // portion only — no query string. Coyote's `Request.uri` carries
        // the raw request target (may include `?query`), and `Request.path`
        // is the already-normalised path component. Prefer the normalised
        // path here so `HttpServletRequest.getRequestURI()` returns
        // `/hello` rather than `/hello?name=Spring` (the latter routes to
        // a "no static resource hello?name=Spring" 404 in Spring MVC).
        let path_only = if !req.path.is_empty() {
            req.path.clone()
        } else {
            // Fall back to splitting the raw URI on the first `?` so the
            // facade still behaves correctly for callers that build a
            // Request without setting `path` (unit tests, JNI tests).
            req.uri.split('?').next().unwrap_or(&req.uri).to_string()
        };
        let parts = RequestParts {
            method: req.method.clone(),
            uri: path_only,
            query: req.query.clone(),
            protocol: req.version.clone(),
            headers: req.headers.clone(),
            remote_addr: req.peer_addr.to_string(),
            scheme,
        };
        let body = if req.body.is_empty() {
            RequestBody::Empty
        } else {
            RequestBody::buffered(req.body.clone())
        };
        Self::with_body(parts, body)
    }

    /// The opaque `nativeRequestId` — the only value that crosses JNI.
    pub fn id(&self) -> u64 {
        self.inner.id
    }

    /// The `nativeRequestId` as the `jlong` (`i64`) the Java facade actually
    /// stores. Convenience for the JNI / invoker marshalling layer.
    pub fn native_id(&self) -> i64 {
        self.inner.id as i64
    }

    /// The eagerly-parsed request metadata.
    pub fn parts(&self) -> &RequestParts {
        &self.inner.parts
    }

    /// HTTP method, upper-cased (`GET`, `POST`, …).
    pub fn method(&self) -> &str {
        &self.inner.parts.method
    }

    /// Request URI including the context path, excluding the query string.
    pub fn request_uri(&self) -> &str {
        &self.inner.parts.uri
    }

    /// Raw query string (without the leading `?`), if any.
    pub fn query_string(&self) -> Option<&str> {
        self.inner.parts.query.as_deref()
    }

    /// HTTP protocol token, e.g. `HTTP/1.1`.
    pub fn protocol(&self) -> &str {
        &self.inner.parts.protocol
    }

    /// Connector-derived scheme (`http` / `https`).
    pub fn scheme(&self) -> &str {
        &self.inner.parts.scheme
    }

    /// Remote peer address as a string, e.g. `203.0.113.7:54321`.
    pub fn remote_addr(&self) -> &str {
        &self.inner.parts.remote_addr
    }

    /// Case-insensitive header lookup. The Java `nativeGetHeader` glue calls
    /// straight through to this.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.inner.parts.header(name)
    }

    /// All header names, in arrival order. Backs `getHeaderNames()`.
    pub fn header_names(&self) -> Vec<String> {
        self.inner.parts.header_names()
    }

    /// Value of the `Content-Length` header as a `u64`, if present and valid.
    pub fn content_length(&self) -> Option<u64> {
        self.inner.parts.content_length()
    }

    /// Read up to `max` more body bytes (the Rust side of
    /// `ServletInputStream.read`). Returns an empty slice at end-of-stream.
    ///
    /// This [`Bytes`]-returning shape is what the existing [`crate::jni`] glue
    /// is written against; [`RequestHandle::read_body_into`] is the
    /// caller-buffer variant used by the invoker.
    pub fn read_body(&self, max: usize) -> Bytes {
        self.inner
            .body
            .lock()
            .expect("request body mutex poisoned")
            .read(max)
    }

    /// Read body bytes into a caller-provided buffer, returning the number of
    /// bytes copied (`0` at end-of-stream).
    ///
    /// This mirrors `java.io.InputStream.read(byte[])` and is the streaming
    /// primitive the JNI / invoker layer drives the request body with.
    pub fn read_body_into(&self, buf: &mut [u8]) -> usize {
        self.inner
            .body
            .lock()
            .expect("request body mutex poisoned")
            .read_into(buf)
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
    use std::net::SocketAddr;

    fn coyote_request() -> tomcatrs_coyote::Request {
        tomcatrs_coyote::Request {
            method: "POST".into(),
            uri: "/app/submit?id=7".into(),
            path: "/app/submit".into(),
            query: Some("id=7".into()),
            version: "HTTP/1.1".into(),
            headers: vec![
                ("Host".into(), "localhost".into()),
                ("Content-Type".into(), "text/plain".into()),
                ("Content-Length".into(), "11".into()),
            ],
            body: Bytes::from_static(b"hello world"),
            peer_addr: "203.0.113.7:54321".parse::<SocketAddr>().unwrap(),
        }
    }

    #[test]
    fn ids_are_unique_and_monotonic() {
        let a = RequestHandle::new(RequestParts::default());
        let b = RequestHandle::new(RequestParts::default());
        assert!(b.id() > a.id());
        assert_eq!(a.native_id(), a.id() as i64);
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
    fn read_body_into_streams_correctly() {
        let req = RequestHandle::with_body(
            RequestParts::default(),
            RequestBody::buffered(Bytes::from_static(b"abcdefgh")),
        );
        let mut buf = [0u8; 3];
        assert_eq!(req.read_body_into(&mut buf), 3);
        assert_eq!(&buf, b"abc");
        assert_eq!(req.read_body_into(&mut buf), 3);
        assert_eq!(&buf, b"def");
        // Final short read: only two bytes left.
        assert_eq!(req.read_body_into(&mut buf), 2);
        assert_eq!(&buf[..2], b"gh");
        // End of stream.
        assert_eq!(req.read_body_into(&mut buf), 0);
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

    #[test]
    fn from_coyote_round_trips_metadata_and_body() {
        let req = RequestHandle::from_coyote(&coyote_request());

        assert_eq!(req.method(), "POST");
        // Servlet spec: getRequestURI() returns the path component only —
        // no query string. (Spring MVC routes the full string verbatim,
        // and treats `/path?query` as a literal pattern → 404.)
        assert_eq!(req.request_uri(), "/app/submit");
        assert_eq!(req.query_string(), Some("id=7"));
        assert_eq!(req.protocol(), "HTTP/1.1");
        assert_eq!(req.remote_addr(), "203.0.113.7:54321");
        assert_eq!(req.scheme(), "http");

        // Headers survive with case-insensitive lookup.
        assert_eq!(req.header("host"), Some("localhost"));
        assert_eq!(req.header("CONTENT-TYPE"), Some("text/plain"));
        assert_eq!(req.content_length(), Some(11));
        assert_eq!(req.header_names().len(), 3);

        // Body round-trips and streams.
        assert_eq!(req.body_remaining(), 11);
        assert_eq!(&req.read_body(11)[..], b"hello world");
        assert_eq!(req.body_remaining(), 0);
    }

    #[test]
    fn from_coyote_empty_body_is_empty() {
        let mut raw = coyote_request();
        raw.body = Bytes::new();
        let req = RequestHandle::from_coyote(&raw);
        assert_eq!(req.body_remaining(), 0);
        assert!(req.read_body(8).is_empty());
    }
}
