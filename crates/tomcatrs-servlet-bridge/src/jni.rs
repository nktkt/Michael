//! The JNI glue: native methods the Java facade classes call back into.
//!
//! Every `native` method declared on `NativeRequest` / `NativeResponse` (see
//! the `java/` scaffold) is implemented here and wired up at JVM start-up with
//! `JNIEnv::register_native_methods` — *not* loaded from a shared library.
//! This is why the Java `System.loadLibrary(...)` calls are no-ops: the
//! embedding Rust process registers the natives directly.
//!
//! ## Lazy materialization in practice
//!
//! Each native takes a `jlong nativeRequestId` / `nativeResponseId`, looks the
//! handle up in the `HANDLE_REGISTRY`, and returns *only* the one value
//! asked for. A header that the servlet never reads never crosses JNI.
//!
//! ## The handle registry
//!
//! The connector / dispatch layer parses a request in Rust, builds a
//! [`RequestHandle`] / [`ResponseHandle`] pair, and *registers* them here under
//! their `nativeRequestId` / `nativeResponseId` before crossing into Java. The
//! registry — `HANDLE_REGISTRY` — is a process-global `DashMap`. Its
//! `register_*` / `unregister_*` / `lookup_*` API is plain Rust and is
//! available **with or without** the `jvm` feature, so non-JNI code can
//! populate and drain it and so its logic stays unit-testable on a host with no
//! JDK.
//!
//! ## Build configuration
//!
//! The `extern "system"` JNI entry points and the [`register_native_methods`]
//! helper are behind `#[cfg(feature = "jvm")]`; with default features they
//! compile to nothing, so no JDK is needed. The signature tables
//! ([`NATIVE_REQUEST_METHODS`] / [`NATIVE_RESPONSE_METHODS`]) and the registry
//! are always compiled — the tables are the canonical reference for what the
//! Java side declares.

use std::sync::OnceLock;

use dashmap::DashMap;

use crate::request_facade::RequestHandle;
use crate::response_facade::ResponseHandle;

/// The JNI method registration table the Java facade expects, as
/// `(java_name, jni_signature)` pairs. Kept as data (rather than only as code)
/// so it can be asserted against the Java sources in tests without a JDK.
pub const NATIVE_REQUEST_METHODS: &[(&str, &str)] = &[
    ("nativeGetMethod", "(J)Ljava/lang/String;"),
    ("nativeGetRequestUri", "(J)Ljava/lang/String;"),
    ("nativeGetQueryString", "(J)Ljava/lang/String;"),
    ("nativeGetProtocol", "(J)Ljava/lang/String;"),
    ("nativeGetScheme", "(J)Ljava/lang/String;"),
    ("nativeGetRemoteAddr", "(J)Ljava/lang/String;"),
    ("nativeGetHeader", "(JLjava/lang/String;)Ljava/lang/String;"),
    ("nativeGetHeaderNames", "(J)[Ljava/lang/String;"),
    ("nativeGetContentLength", "(J)J"),
    (
        "nativeGetAttribute",
        "(JLjava/lang/String;)Ljava/lang/String;",
    ),
    (
        "nativeSetAttribute",
        "(JLjava/lang/String;Ljava/lang/String;)V",
    ),
    ("nativeReadBody", "(J[BII)I"),
    ("nativeBodyRemaining", "(J)I"),
];

/// The JNI method registration table for the response facade.
pub const NATIVE_RESPONSE_METHODS: &[(&str, &str)] = &[
    ("nativeSetStatus", "(JI)V"),
    (
        "nativeSetHeader",
        "(JLjava/lang/String;Ljava/lang/String;)V",
    ),
    (
        "nativeAddHeader",
        "(JLjava/lang/String;Ljava/lang/String;)V",
    ),
    ("nativeWriteBody", "(J[BII)V"),
    ("nativeFlush", "(J)V"),
    ("nativeCommit", "(J)Z"),
    ("nativeIsCommitted", "(J)Z"),
];

// ---------------------------------------------------------------------------
// Process-global handle registry — always compiled.
// ---------------------------------------------------------------------------

/// One registered request: the live [`RequestHandle`] keyed by its
/// `nativeRequestId`.
///
/// The handle already owns everything the JNI natives need — eagerly parsed
/// [`RequestParts`](crate::request_facade::RequestParts), the streaming body
/// cursor, and the attribute map — and is cheap to clone (it is `Arc`-backed),
/// so the entry simply *holds* it rather than copying a snapshot out of it.
/// Pure-Rust accessors on this struct (e.g. [`RequestEntry::header`]) mirror
/// what the JNI shims expose, which keeps that logic testable without a JVM.
#[derive(Debug, Clone)]
pub struct RequestEntry {
    handle: RequestHandle,
}

impl RequestEntry {
    /// Wrap a live request handle for storage in the registry.
    pub fn new(handle: RequestHandle) -> Self {
        Self { handle }
    }

    /// The wrapped request handle.
    pub fn handle(&self) -> &RequestHandle {
        &self.handle
    }

    /// HTTP method (`GET`, `POST`, …).
    pub fn method(&self) -> &str {
        &self.handle.parts().method
    }

    /// Request URI, excluding the query string.
    pub fn request_uri(&self) -> &str {
        &self.handle.parts().uri
    }

    /// Raw query string without the leading `?`, or `None`.
    pub fn query_string(&self) -> Option<&str> {
        self.handle.parts().query.as_deref()
    }

    /// HTTP protocol token, e.g. `HTTP/1.1`.
    pub fn protocol(&self) -> &str {
        &self.handle.parts().protocol
    }

    /// Connector-derived scheme (`http` / `https`).
    pub fn scheme(&self) -> &str {
        &self.handle.parts().scheme
    }

    /// Remote peer address as a string.
    pub fn remote_addr(&self) -> &str {
        &self.handle.parts().remote_addr
    }

    /// Case-insensitive header lookup — the pure-Rust core of
    /// `NativeRequest.nativeGetHeader`.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.handle.header(name)
    }

    /// All header names, in arrival order, with duplicates preserved — the
    /// pure-Rust core of `NativeRequest.nativeGetHeaderNames`.
    pub fn header_names(&self) -> Vec<String> {
        self.handle
            .parts()
            .headers
            .iter()
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// `Content-Length` parsed from the request headers, or `-1` when the
    /// header is absent or malformed — matching the Servlet API contract for
    /// `getContentLengthLong()`.
    pub fn content_length(&self) -> i64 {
        self.header("Content-Length")
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|n| *n >= 0)
            .unwrap_or(-1)
    }
}

/// One registered response: the live [`ResponseHandle`] keyed by its
/// `nativeResponseId`.
///
/// As with [`RequestEntry`], the handle is `Arc`-backed and already owns the
/// response sink (status, headers, body buffer, committed flag), so the entry
/// holds it directly.
#[derive(Debug, Clone)]
pub struct ResponseEntry {
    handle: ResponseHandle,
}

impl ResponseEntry {
    /// Wrap a live response handle for storage in the registry.
    pub fn new(handle: ResponseHandle) -> Self {
        Self { handle }
    }

    /// The wrapped response handle.
    pub fn handle(&self) -> &ResponseHandle {
        &self.handle
    }
}

/// The process-global registry mapping `nativeRequestId` / `nativeResponseId`
/// values to their Rust-side handles.
///
/// There is exactly one per process. It is populated by the connector /
/// dispatch layer just before a request crosses into Java and drained once the
/// servlet invocation (or its `AsyncContext`) finishes.
#[derive(Debug, Default)]
pub struct HandleRegistry {
    requests: DashMap<i64, RequestEntry>,
    responses: DashMap<i64, ResponseEntry>,
}

impl HandleRegistry {
    /// Register `handle` under its `nativeRequestId`, returning any handle that
    /// was previously registered under the same id (normally `None`).
    pub fn register_request(&self, handle: RequestHandle) -> Option<RequestEntry> {
        let id = handle.id() as i64;
        self.requests.insert(id, RequestEntry::new(handle))
    }

    /// Remove and return the request registered under `id`, if any.
    pub fn unregister_request(&self, id: i64) -> Option<RequestEntry> {
        self.requests.remove(&id).map(|(_, entry)| entry)
    }

    /// Look the request `id` up, returning a clone of its entry.
    pub fn lookup_request(&self, id: i64) -> Option<RequestEntry> {
        self.requests.get(&id).map(|e| e.clone())
    }

    /// Register `handle` under its `nativeResponseId`, returning any handle
    /// previously registered under the same id (normally `None`).
    pub fn register_response(&self, handle: ResponseHandle) -> Option<ResponseEntry> {
        let id = handle.id() as i64;
        self.responses.insert(id, ResponseEntry::new(handle))
    }

    /// Remove and return the response registered under `id`, if any.
    pub fn unregister_response(&self, id: i64) -> Option<ResponseEntry> {
        self.responses.remove(&id).map(|(_, entry)| entry)
    }

    /// Look the response `id` up, returning a clone of its entry.
    pub fn lookup_response(&self, id: i64) -> Option<ResponseEntry> {
        self.responses.get(&id).map(|e| e.clone())
    }

    /// Number of currently-registered requests. Mainly for diagnostics/tests.
    pub fn request_count(&self) -> usize {
        self.requests.len()
    }

    /// Number of currently-registered responses. Mainly for diagnostics/tests.
    pub fn response_count(&self) -> usize {
        self.responses.len()
    }
}

/// Backing storage for [`registry`].
static HANDLE_REGISTRY: OnceLock<HandleRegistry> = OnceLock::new();

/// The process-global [`HandleRegistry`], created on first access.
pub fn registry() -> &'static HandleRegistry {
    HANDLE_REGISTRY.get_or_init(HandleRegistry::default)
}

/// Register a request handle in the process-global registry. Callable without
/// the `jvm` feature so `dispatch` / `invoker` code can populate it.
pub fn register_request(handle: RequestHandle) {
    registry().register_request(handle);
}

/// Remove the request `id` from the process-global registry.
pub fn unregister_request(id: i64) -> Option<RequestEntry> {
    registry().unregister_request(id)
}

/// Register a response handle in the process-global registry. Callable without
/// the `jvm` feature so `dispatch` / `invoker` code can populate it.
pub fn register_response(handle: ResponseHandle) {
    registry().register_response(handle);
}

/// Remove the response `id` from the process-global registry.
pub fn unregister_response(id: i64) -> Option<ResponseEntry> {
    registry().unregister_response(id)
}

// ---------------------------------------------------------------------------
// Real JNI entry points — only compiled with `--features jvm`.
// ---------------------------------------------------------------------------
#[cfg(feature = "jvm")]
pub use imp::register_native_methods;

#[cfg(feature = "jvm")]
mod imp {
    //! Real JNI entry points. Registered via [`register_native_methods`] at
    //! start-up. These back `static native` Java methods, hence the
    //! [`jni::objects::JClass`] receiver.
    //!
    //! Every body runs inside [`std::panic::catch_unwind`]: a panic must never
    //! be allowed to unwind across the `extern "system"` FFI boundary (that is
    //! undefined behaviour). On a caught panic — or any other recoverable
    //! failure — the shim throws a `java.io.IOException` into the JVM and
    //! returns a benign default value.

    use std::panic::{catch_unwind, AssertUnwindSafe};

    use jni::objects::{JByteArray, JClass, JObjectArray, JString, JValue};
    use jni::sys::{jboolean, jint, jlong, JNI_FALSE};
    use jni::JNIEnv;

    use super::{registry, RequestEntry, ResponseEntry};

    /// Throw a `java.io.IOException` carrying `msg`. Best-effort: if the JVM
    /// rejects the throw (e.g. an exception is already pending) the error is
    /// logged and dropped — there is nothing else the shim can do.
    fn throw_io(env: &mut JNIEnv, msg: &str) {
        if let Err(e) = env.throw_new("java/io/IOException", msg) {
            tracing::error!(error = %e, original = msg, "failed to throw IOException across JNI");
        }
    }

    /// Render a caught panic payload as a human-readable string.
    fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
        if let Some(s) = payload.downcast_ref::<&str>() {
            (*s).to_owned()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic in native bridge method".to_owned()
        }
    }

    /// Run `body` under [`catch_unwind`]; on a panic, throw an `IOException`
    /// and substitute `default`. JNI handles are not unwind-safe in the
    /// `RefUnwindSafe` sense, so the closure is wrapped in [`AssertUnwindSafe`]
    /// — sound here because a panic aborts the call rather than resuming it.
    fn guard<'local, R>(
        env: &mut JNIEnv<'local>,
        what: &str,
        default: R,
        body: impl FnOnce(&mut JNIEnv<'local>) -> R,
    ) -> R {
        match catch_unwind(AssertUnwindSafe(|| body(env))) {
            Ok(value) => value,
            Err(payload) => {
                let msg = format!("panic in {what}: {}", panic_message(&*payload));
                tracing::error!("{msg}");
                throw_io(env, &msg);
                default
            }
        }
    }

    /// Resolve a `nativeRequestId` to its registered [`RequestEntry`].
    fn lookup_request(id: jlong) -> Option<RequestEntry> {
        registry().lookup_request(id)
    }

    /// Resolve a `nativeResponseId` to its registered [`ResponseEntry`].
    fn lookup_response(id: jlong) -> Option<ResponseEntry> {
        registry().lookup_response(id)
    }

    /// Build a Java `String`, falling back to an empty one (then, if even that
    /// fails, to a null reference) so a shim can always return *something*.
    fn java_string<'local>(env: &mut JNIEnv<'local>, value: &str) -> JString<'local> {
        match env.new_string(value) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "JNIEnv::new_string failed; substituting empty string");
                env.new_string("")
                    .unwrap_or_else(|_| JString::from(jni::objects::JObject::null()))
            }
        }
    }

    /// Read a Java `String` argument into a Rust `String`. A failure (e.g. a
    /// `null` reference) yields an empty string rather than erroring — the
    /// callers treat "" as "absent".
    fn rust_string(env: &mut JNIEnv, value: &JString) -> String {
        env.get_string(value).map(Into::into).unwrap_or_default()
    }

    // -- NativeRequest ------------------------------------------------------

    /// `NativeRequest.nativeGetMethod(long) -> String`
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeRequest_nativeGetMethod<'local>(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
    ) -> JString<'local> {
        let default = JString::from(jni::objects::JObject::null());
        guard(&mut env, "nativeGetMethod", default, |env| {
            let value = lookup_request(request_id)
                .map(|r| r.method().to_owned())
                .unwrap_or_default();
            java_string(env, &value)
        })
    }

    /// `NativeRequest.nativeGetRequestUri(long) -> String`
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeRequest_nativeGetRequestUri<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
    ) -> JString<'local> {
        let default = JString::from(jni::objects::JObject::null());
        guard(&mut env, "nativeGetRequestUri", default, |env| {
            let value = lookup_request(request_id)
                .map(|r| r.request_uri().to_owned())
                .unwrap_or_default();
            java_string(env, &value)
        })
    }

    /// `NativeRequest.nativeGetQueryString(long) -> String`
    ///
    /// Returns `null` when the request had no query string (the Servlet API
    /// contract for `getQueryString()`).
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeRequest_nativeGetQueryString<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
    ) -> JString<'local> {
        let default = JString::from(jni::objects::JObject::null());
        guard(
            &mut env,
            "nativeGetQueryString",
            default,
            |env| match lookup_request(request_id).and_then(|r| r.query_string().map(str::to_owned))
            {
                Some(q) => java_string(env, &q),
                None => JString::from(jni::objects::JObject::null()),
            },
        )
    }

    /// `NativeRequest.nativeGetProtocol(long) -> String`
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeRequest_nativeGetProtocol<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
    ) -> JString<'local> {
        let default = JString::from(jni::objects::JObject::null());
        guard(&mut env, "nativeGetProtocol", default, |env| {
            let value = lookup_request(request_id)
                .map(|r| r.protocol().to_owned())
                .unwrap_or_default();
            java_string(env, &value)
        })
    }

    /// `NativeRequest.nativeGetScheme(long) -> String`
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeRequest_nativeGetScheme<'local>(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
    ) -> JString<'local> {
        let default = JString::from(jni::objects::JObject::null());
        guard(&mut env, "nativeGetScheme", default, |env| {
            let value = lookup_request(request_id)
                .map(|r| r.scheme().to_owned())
                .unwrap_or_default();
            java_string(env, &value)
        })
    }

    /// `NativeRequest.nativeGetRemoteAddr(long) -> String`
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeRequest_nativeGetRemoteAddr<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
    ) -> JString<'local> {
        let default = JString::from(jni::objects::JObject::null());
        guard(&mut env, "nativeGetRemoteAddr", default, |env| {
            let value = lookup_request(request_id)
                .map(|r| r.remote_addr().to_owned())
                .unwrap_or_default();
            java_string(env, &value)
        })
    }

    /// `NativeRequest.nativeGetHeader(long, String) -> String`
    ///
    /// The canonical lazy-materialization path: one header, one JNI call.
    /// Returns `null` when the header is absent.
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeRequest_nativeGetHeader<'local>(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
        name: JString<'local>,
    ) -> JString<'local> {
        let default = JString::from(jni::objects::JObject::null());
        guard(&mut env, "nativeGetHeader", default, |env| {
            let name = rust_string(env, &name);
            match lookup_request(request_id).and_then(|r| r.header(&name).map(str::to_owned)) {
                Some(v) => java_string(env, &v),
                None => JString::from(jni::objects::JObject::null()),
            }
        })
    }

    /// `NativeRequest.nativeGetHeaderNames(long) -> String[]`
    ///
    /// Returns every header name in arrival order (duplicates preserved). An
    /// empty array is returned for an unknown request id.
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeRequest_nativeGetHeaderNames<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
    ) -> JObjectArray<'local> {
        let default = JObjectArray::from(jni::objects::JObject::null());
        guard(&mut env, "nativeGetHeaderNames", default, |env| {
            let names = lookup_request(request_id)
                .map(|r| r.header_names())
                .unwrap_or_default();
            let string_class = match env.find_class("java/lang/String") {
                Ok(c) => c,
                Err(e) => {
                    throw_io(env, &format!("cannot resolve java/lang/String: {e}"));
                    return JObjectArray::from(jni::objects::JObject::null());
                }
            };
            let empty = match env.new_string("") {
                Ok(s) => s,
                Err(e) => {
                    throw_io(env, &format!("cannot allocate placeholder string: {e}"));
                    return JObjectArray::from(jni::objects::JObject::null());
                }
            };
            let array = match env.new_object_array(names.len() as jint, &string_class, &empty) {
                Ok(a) => a,
                Err(e) => {
                    throw_io(env, &format!("cannot allocate String[]: {e}"));
                    return JObjectArray::from(jni::objects::JObject::null());
                }
            };
            for (i, name) in names.iter().enumerate() {
                let jname = java_string(env, name);
                if let Err(e) = env.set_object_array_element(&array, i as jint, &jname) {
                    throw_io(env, &format!("cannot populate header-names array: {e}"));
                    return JObjectArray::from(jni::objects::JObject::null());
                }
            }
            array
        })
    }

    /// `NativeRequest.nativeGetContentLength(long) -> long`
    ///
    /// Returns `-1` when there is no (valid) `Content-Length` header, matching
    /// `HttpServletRequest.getContentLengthLong()`.
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeRequest_nativeGetContentLength<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
    ) -> jlong {
        guard(&mut env, "nativeGetContentLength", -1, |_env| {
            lookup_request(request_id)
                .map(|r| r.content_length())
                .unwrap_or(-1)
        })
    }

    /// `NativeRequest.nativeGetAttribute(long, String) -> String`
    ///
    /// Returns `null` when the attribute is unset.
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeRequest_nativeGetAttribute<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
        name: JString<'local>,
    ) -> JString<'local> {
        let default = JString::from(jni::objects::JObject::null());
        guard(&mut env, "nativeGetAttribute", default, |env| {
            let name = rust_string(env, &name);
            match lookup_request(request_id).and_then(|r| r.handle().attribute(&name)) {
                Some(v) => java_string(env, &v),
                None => JString::from(jni::objects::JObject::null()),
            }
        })
    }

    /// `NativeRequest.nativeSetAttribute(long, String, String)`
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeRequest_nativeSetAttribute<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
        name: JString<'local>,
        value: JString<'local>,
    ) {
        guard(&mut env, "nativeSetAttribute", (), |env| {
            let name = rust_string(env, &name);
            let value = rust_string(env, &value);
            if let Some(entry) = lookup_request(request_id) {
                entry.handle().set_attribute(name, value);
            }
        })
    }

    /// `NativeRequest.nativeReadBody(long, byte[], int off, int len) -> int`
    ///
    /// Streams the request body: copies up to `len` bytes into the Java buffer
    /// starting at `off`, and returns the count — or `-1` at end-of-stream.
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeRequest_nativeReadBody<'local>(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
        buffer: JByteArray<'local>,
        off: jint,
        len: jint,
    ) -> jint {
        guard(&mut env, "nativeReadBody", -1, |env| {
            let Some(entry) = lookup_request(request_id) else {
                throw_io(env, &format!("unknown nativeRequestId {request_id}"));
                return -1;
            };
            if off < 0 || len < 0 {
                throw_io(env, &format!("negative off/len: off={off} len={len}"));
                return -1;
            }
            let want = len as usize;
            if want == 0 {
                return 0;
            }
            let chunk = entry.handle().read_body(want);
            if chunk.is_empty() {
                // End-of-stream — the Servlet `InputStream.read` contract.
                return -1;
            }
            // JNI byte arrays are `jbyte` (i8); reinterpret the bytes.
            let signed: &[i8] =
                unsafe { std::slice::from_raw_parts(chunk.as_ptr().cast::<i8>(), chunk.len()) };
            match env.set_byte_array_region(&buffer, off, signed) {
                Ok(()) => signed.len() as jint,
                Err(e) => {
                    throw_io(env, &format!("cannot copy body into Java buffer: {e}"));
                    -1
                }
            }
        })
    }

    /// `NativeRequest.nativeBodyRemaining(long) -> int`
    ///
    /// Bytes of request body not yet consumed; `0` for an unknown id.
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeRequest_nativeBodyRemaining<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
    ) -> jint {
        guard(&mut env, "nativeBodyRemaining", 0, |_env| {
            lookup_request(request_id)
                .map(|r| r.handle().body_remaining().min(jint::MAX as usize) as jint)
                .unwrap_or(0)
        })
    }

    // -- NativeResponse -----------------------------------------------------

    /// `NativeResponse.nativeSetStatus(long, int)`
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeResponse_nativeSetStatus<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        response_id: jlong,
        status: jint,
    ) {
        guard(&mut env, "nativeSetStatus", (), |_env| {
            if let Some(entry) = lookup_response(response_id) {
                entry
                    .handle()
                    .set_status(status.max(0).min(u16::MAX as jint) as u16);
            }
        })
    }

    /// `NativeResponse.nativeSetHeader(long, String, String)`
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeResponse_nativeSetHeader<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        response_id: jlong,
        name: JString<'local>,
        value: JString<'local>,
    ) {
        guard(&mut env, "nativeSetHeader", (), |env| {
            let name = rust_string(env, &name);
            let value = rust_string(env, &value);
            if let Some(entry) = lookup_response(response_id) {
                entry.handle().set_header(name, value);
            }
        })
    }

    /// `NativeResponse.nativeAddHeader(long, String, String)`
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeResponse_nativeAddHeader<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        response_id: jlong,
        name: JString<'local>,
        value: JString<'local>,
    ) {
        guard(&mut env, "nativeAddHeader", (), |env| {
            let name = rust_string(env, &name);
            let value = rust_string(env, &value);
            if let Some(entry) = lookup_response(response_id) {
                entry.handle().add_header(name, value);
            }
        })
    }

    /// `NativeResponse.nativeWriteBody(long, byte[], int off, int len)`
    ///
    /// Drains a chunk of the Java `ServletOutputStream` into the Rust sink.
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeResponse_nativeWriteBody<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        response_id: jlong,
        buffer: JByteArray<'local>,
        off: jint,
        len: jint,
    ) {
        guard(&mut env, "nativeWriteBody", (), |env| {
            let Some(entry) = lookup_response(response_id) else {
                throw_io(env, &format!("unknown nativeResponseId {response_id}"));
                return;
            };
            if off < 0 || len < 0 {
                throw_io(env, &format!("negative off/len: off={off} len={len}"));
                return;
            }
            if len == 0 {
                return;
            }
            let mut signed = vec![0i8; len as usize];
            if let Err(e) = env.get_byte_array_region(&buffer, off, &mut signed) {
                throw_io(env, &format!("cannot read Java buffer: {e}"));
                return;
            }
            // Reinterpret `jbyte` (i8) back to `u8` without a copy.
            let bytes: &[u8] =
                unsafe { std::slice::from_raw_parts(signed.as_ptr().cast::<u8>(), signed.len()) };
            entry.handle().write_body(bytes);
        })
    }

    /// `NativeResponse.nativeFlush(long)`
    ///
    /// Flushes the response buffer. In this buffered model body bytes are
    /// already in the sink, so a flush simply *commits* the response (freezing
    /// the status line and headers), mirroring `ServletResponse.flushBuffer()`.
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeResponse_nativeFlush<'local>(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        response_id: jlong,
    ) {
        guard(&mut env, "nativeFlush", (), |_env| {
            if let Some(entry) = lookup_response(response_id) {
                entry.handle().commit();
            }
        })
    }

    /// `NativeResponse.nativeCommit(long) -> boolean`
    ///
    /// Commits the response and reports the committed flag (always `true`).
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeResponse_nativeCommit<'local>(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        response_id: jlong,
    ) -> jboolean {
        guard(
            &mut env,
            "nativeCommit",
            JNI_FALSE,
            |_env| match lookup_response(response_id) {
                Some(entry) => jboolean::from(entry.handle().commit()),
                None => JNI_FALSE,
            },
        )
    }

    /// `NativeResponse.nativeIsCommitted(long) -> boolean`
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeResponse_nativeIsCommitted<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        response_id: jlong,
    ) -> jboolean {
        guard(
            &mut env,
            "nativeIsCommitted",
            JNI_FALSE,
            |_env| match lookup_response(response_id) {
                Some(entry) => jboolean::from(entry.handle().is_committed()),
                None => JNI_FALSE,
            },
        )
    }

    // -- registration ------------------------------------------------------

    /// Wire every native above into the JVM with `RegisterNatives`, so the Java
    /// `NativeRequest` / `NativeResponse` classes resolve their `native`
    /// methods to these Rust functions instead of a shared library.
    ///
    /// This is intentionally a thin stub for now: it resolves the two facade
    /// classes (proving they are on the classpath) and logs intent. The actual
    /// [`JNIEnv::register_native_methods`] calls — which need
    /// [`jni::NativeMethod`] tables built from raw function pointers — are
    /// filled in when the `JvmRuntime` start-up sequence is integrated, since
    /// that is what owns the `JNIEnv` at the right point in the lifecycle.
    pub fn register_native_methods(env: &mut JNIEnv) -> tomcatrs_core::Result<()> {
        use tomcatrs_core::Error;

        for class in [
            "org/apache/tomcatrs/bridge/NativeRequest",
            "org/apache/tomcatrs/bridge/NativeResponse",
        ] {
            env.find_class(class).map_err(|e| {
                Error::bridge(format!("bridge class {class} not on JVM classpath: {e}"))
            })?;
        }

        // `_ = JValue::Void` keeps the `JValue` import meaningful for callers
        // that pattern-match registration results once this is fleshed out.
        let _ = JValue::Void;

        tracing::debug!(
            request_natives = super::NATIVE_REQUEST_METHODS.len(),
            response_natives = super::NATIVE_RESPONSE_METHODS.len(),
            "bridge facade classes resolved; native-method registration pending JvmRuntime wiring"
        );
        Ok(())
    }
}

/// Stub [`register_native_methods`] for the default (no-`jvm`) build.
///
/// With the `jvm` feature off there is no `JNIEnv` type, so this variant takes
/// no arguments and is a no-op — it exists only so non-JNI code can refer to
/// the symbol unconditionally.
#[cfg(not(feature = "jvm"))]
pub fn register_native_methods() {
    tracing::debug!("register_native_methods: no-op (built without the `jvm` feature)");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request_facade::{RequestBody, RequestParts};
    use bytes::Bytes;

    #[test]
    fn registration_tables_are_non_empty_and_well_formed() {
        for (name, sig) in NATIVE_REQUEST_METHODS.iter().chain(NATIVE_RESPONSE_METHODS) {
            assert!(name.starts_with("native"), "bad native name: {name}");
            assert!(sig.starts_with('('), "bad JNI signature: {sig}");
        }
        // Spot-check that the lazy-materialization header accessor is present.
        assert!(NATIVE_REQUEST_METHODS
            .iter()
            .any(|(n, _)| *n == "nativeGetHeader"));
        assert!(NATIVE_RESPONSE_METHODS
            .iter()
            .any(|(n, _)| *n == "nativeWriteBody"));
    }

    fn sample_request() -> RequestHandle {
        RequestHandle::with_body(
            RequestParts {
                method: "POST".into(),
                uri: "/app/submit".into(),
                query: Some("a=1&b=2".into()),
                protocol: "HTTP/1.1".into(),
                scheme: "https".into(),
                remote_addr: "203.0.113.7:54321".into(),
                headers: vec![
                    ("Content-Type".into(), "application/json".into()),
                    ("Content-Length".into(), "11".into()),
                    ("X-Trace".into(), "abc".into()),
                ],
            },
            RequestBody::Buffered {
                data: Bytes::from_static(b"hello world"),
                position: 0,
            },
        )
    }

    #[test]
    fn request_register_lookup_unregister_round_trip() {
        let reg = HandleRegistry::default();
        let handle = sample_request();
        let id = handle.id() as i64;

        assert!(reg.lookup_request(id).is_none());
        assert!(reg.register_request(handle).is_none());
        assert_eq!(reg.request_count(), 1);

        let entry = reg.lookup_request(id).expect("registered request");
        assert_eq!(entry.handle().id() as i64, id);

        let removed = reg.unregister_request(id).expect("entry was present");
        assert_eq!(removed.handle().id() as i64, id);
        assert!(reg.lookup_request(id).is_none());
        assert_eq!(reg.request_count(), 0);
    }

    #[test]
    fn response_register_lookup_unregister_round_trip() {
        let reg = HandleRegistry::default();
        let handle = ResponseHandle::new();
        let id = handle.id() as i64;

        assert!(reg.lookup_response(id).is_none());
        assert!(reg.register_response(handle).is_none());
        assert_eq!(reg.response_count(), 1);

        let entry = reg.lookup_response(id).expect("registered response");
        entry.handle().set_status(204);
        assert_eq!(entry.handle().id() as i64, id);

        let removed = reg.unregister_response(id).expect("entry was present");
        // The handle is shared, so the status set above is visible on the
        // removed entry too.
        assert_eq!(removed.handle().sink_handle().snapshot().status, Some(204));
        assert!(reg.lookup_response(id).is_none());
        assert_eq!(reg.response_count(), 0);
    }

    #[test]
    fn request_entry_header_lookup_is_case_insensitive() {
        let entry = RequestEntry::new(sample_request());
        assert_eq!(entry.header("content-type"), Some("application/json"));
        assert_eq!(entry.header("CONTENT-TYPE"), Some("application/json"));
        assert_eq!(entry.header("X-Trace"), Some("abc"));
        assert_eq!(entry.header("missing"), None);
    }

    #[test]
    fn request_entry_exposes_parts_and_content_length() {
        let entry = RequestEntry::new(sample_request());
        assert_eq!(entry.method(), "POST");
        assert_eq!(entry.request_uri(), "/app/submit");
        assert_eq!(entry.query_string(), Some("a=1&b=2"));
        assert_eq!(entry.protocol(), "HTTP/1.1");
        assert_eq!(entry.scheme(), "https");
        assert_eq!(entry.remote_addr(), "203.0.113.7:54321");
        assert_eq!(entry.content_length(), 11);
        assert_eq!(
            entry.header_names(),
            vec![
                "Content-Type".to_string(),
                "Content-Length".to_string(),
                "X-Trace".to_string()
            ]
        );
    }

    #[test]
    fn content_length_is_minus_one_when_absent_or_invalid() {
        let no_len = RequestEntry::new(RequestHandle::new(RequestParts::default()));
        assert_eq!(no_len.content_length(), -1);

        let bad_len = RequestEntry::new(RequestHandle::new(RequestParts {
            headers: vec![("Content-Length".into(), "not-a-number".into())],
            ..RequestParts::default()
        }));
        assert_eq!(bad_len.content_length(), -1);
    }

    #[test]
    fn process_global_registry_helpers_round_trip() {
        let request = RequestHandle::new(RequestParts::default());
        let response = ResponseHandle::new();
        let req_id = request.id() as i64;
        let resp_id = response.id() as i64;

        register_request(request);
        register_response(response);

        assert!(registry().lookup_request(req_id).is_some());
        assert!(registry().lookup_response(resp_id).is_some());

        assert!(unregister_request(req_id).is_some());
        assert!(unregister_response(resp_id).is_some());
        assert!(registry().lookup_request(req_id).is_none());
        assert!(registry().lookup_response(resp_id).is_none());
    }
}
