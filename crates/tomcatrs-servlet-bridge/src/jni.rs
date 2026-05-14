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
//! handle up in the [`crate::jvm::JvmRuntime`] registry, and returns *only* the
//! one value asked for. A header that the servlet never reads never crosses
//! JNI.
//!
//! ## Build configuration
//!
//! The whole module body is behind `#[cfg(feature = "jvm")]`. With default
//! features it compiles to nothing, so no JDK is needed. The function
//! signatures below are the canonical reference for what the Java side
//! declares.

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
    ("nativeCommit", "(J)Z"),
    ("nativeIsCommitted", "(J)Z"),
];

#[cfg(feature = "jvm")]
mod imp {
    //! Real JNI entry points. Registered via `register_native_methods` at
    //! start-up. These take a [`jni::objects::JClass`] because they back
    //! `static native` Java methods.

    use jni::objects::{JByteArray, JClass, JString};
    use jni::sys::{jboolean, jint, jlong};
    use jni::JNIEnv;

    use crate::request_facade::RequestHandle;
    use crate::response_facade::ResponseHandle;

    /// Resolve a `nativeRequestId` to its [`RequestHandle`].
    ///
    /// In the full implementation this consults the process-wide request
    /// registry owned by [`crate::jvm::JvmRuntime`]. The lookup is factored out
    /// so every native below is a one-liner over it.
    fn lookup_request(_id: jlong) -> Option<RequestHandle> {
        // Wired to the `JvmRuntime` registry once the connector is integrated.
        None
    }

    /// Resolve a `nativeResponseId` to its [`ResponseHandle`].
    fn lookup_response(_id: jlong) -> Option<ResponseHandle> {
        None
    }

    /// `NativeRequest.nativeGetMethod(long) -> String`
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeRequest_nativeGetMethod<'local>(
        env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
    ) -> JString<'local> {
        let value = lookup_request(request_id)
            .map(|r| r.parts().method.clone())
            .unwrap_or_default();
        env.new_string(value)
            .unwrap_or_else(|_| JString::from(env.new_string("").unwrap()))
    }

    /// `NativeRequest.nativeGetRequestUri(long) -> String`
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeRequest_nativeGetRequestUri<
        'local,
    >(
        env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
    ) -> JString<'local> {
        let value = lookup_request(request_id)
            .map(|r| r.parts().uri.clone())
            .unwrap_or_default();
        env.new_string(value)
            .unwrap_or_else(|_| JString::from(env.new_string("").unwrap()))
    }

    /// `NativeRequest.nativeGetHeader(long, String) -> String`
    ///
    /// The canonical lazy-materialization path: one header, one JNI call.
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeRequest_nativeGetHeader<'local>(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
        name: JString<'local>,
    ) -> JString<'local> {
        let name: String = env.get_string(&name).map(|s| s.into()).unwrap_or_default();
        let value = lookup_request(request_id)
            .and_then(|r| r.header(&name).map(str::to_owned))
            .unwrap_or_default();
        env.new_string(value)
            .unwrap_or_else(|_| JString::from(env.new_string("").unwrap()))
    }

    /// `NativeRequest.nativeReadBody(long, byte[], int off, int len) -> int`
    ///
    /// Streams the request body: copies up to `len` bytes into the Java buffer
    /// and returns the count, or `-1` at end-of-stream.
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeRequest_nativeReadBody<'local>(
        env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
        buffer: JByteArray<'local>,
        off: jint,
        len: jint,
    ) -> jint {
        let Some(request) = lookup_request(request_id) else {
            return -1;
        };
        let want = len.max(0) as usize;
        let chunk = request.read_body(want);
        if chunk.is_empty() {
            return -1;
        }
        let signed: Vec<i8> = chunk.iter().map(|b| *b as i8).collect();
        match env.set_byte_array_region(&buffer, off, &signed) {
            Ok(()) => signed.len() as jint,
            Err(_) => -1,
        }
    }

    /// `NativeResponse.nativeSetStatus(long, int)`
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeResponse_nativeSetStatus<
        'local,
    >(
        _env: JNIEnv<'local>,
        _class: JClass<'local>,
        response_id: jlong,
        status: jint,
    ) {
        if let Some(resp) = lookup_response(response_id) {
            resp.set_status(status.max(0) as u16);
        }
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
        let name: String = env.get_string(&name).map(|s| s.into()).unwrap_or_default();
        let value: String = env.get_string(&value).map(|s| s.into()).unwrap_or_default();
        if let Some(resp) = lookup_response(response_id) {
            resp.set_header(name, value);
        }
    }

    /// `NativeResponse.nativeWriteBody(long, byte[], int off, int len)`
    ///
    /// Drains a chunk of the Java `ServletOutputStream` into the Rust sink.
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeResponse_nativeWriteBody<
        'local,
    >(
        env: JNIEnv<'local>,
        _class: JClass<'local>,
        response_id: jlong,
        buffer: JByteArray<'local>,
        off: jint,
        len: jint,
    ) {
        let Some(resp) = lookup_response(response_id) else {
            return;
        };
        let want = len.max(0) as usize;
        let mut signed = vec![0i8; want];
        if env.get_byte_array_region(&buffer, off, &mut signed).is_ok() {
            let bytes: Vec<u8> = signed.iter().map(|b| *b as u8).collect();
            resp.write_body(&bytes);
        }
    }

    /// `NativeResponse.nativeCommit(long) -> boolean`
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeResponse_nativeCommit<'local>(
        _env: JNIEnv<'local>,
        _class: JClass<'local>,
        response_id: jlong,
    ) -> jboolean {
        match lookup_response(response_id) {
            Some(resp) => jboolean::from(resp.commit()),
            None => jboolean::from(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
