//! Health-smoke integration test for the JVM bridge under `--features jvm`.
//!
//! Proves end-to-end that:
//!
//! 1. [`JvmRuntime::start`] boots the embedded JVM with the build-time bridge
//!    JAR auto-loaded onto the classpath, so [`JNIEnv::find_class`] can resolve
//!    the bridge entry point `org/apache/tomcatrs/bridge/TomcatRsBridge`.
//! 2. [`register_native_methods`] succeeds for all four facade classes — i.e.
//!    `RegisterNatives` actually binds the `extern "system"` Rust fns to the
//!    Java `native` declarations. (It is called once already from
//!    [`JvmRuntime::start`], so calling it again here just re-registers and
//!    must remain `Ok`.)
//! 3. A trivial native round-trip works: a fake [`RequestHandle`] is placed in
//!    the process-global handle registry, then a Java
//!    `TomcatRsRequestFacade(id).getMethod()` is invoked across JNI; the
//!    returned `String` must match the value the Rust side registered.
//!
//! Together this proves the registration table, the JNI shim, and the handle
//! registry are wired end-to-end — the minimum needed before a real servlet
//! can call back into Rust.

#![cfg(feature = "jvm")]

use jni::objects::{JObject, JValue};
use tomcatrs_servlet_bridge::jni::{register_native_methods, register_request};
use tomcatrs_servlet_bridge::request_facade::{RequestHandle, RequestParts};
use tomcatrs_servlet_bridge::{JvmConfig, JvmRuntime};

#[test]
fn jvm_bridge_smoke() {
    // 1. Boot the JVM. Default config means: empty caller classpath; the
    //    bridge JAR is auto-appended by JvmRuntime::start via the build-time
    //    `TOMCATRS_BRIDGE_JAR` env var (set by build.rs). If start() fails,
    //    that is itself the bug under test — bail loudly rather than skip.
    let runtime = JvmRuntime::start(JvmConfig::default())
        .expect("JvmRuntime::start must succeed (bridge JAR should be auto-loaded)");

    // 2. The bridge JAR really is on the classpath: TomcatRsBridge resolves.
    let tomcatrs_bridge_found = runtime
        .with_env(|env| {
            Ok(env
                .find_class("org/apache/tomcatrs/bridge/TomcatRsBridge")
                .is_ok())
        })
        .expect("with_env must dispatch onto the worker pool");
    assert!(
        tomcatrs_bridge_found,
        "TomcatRsBridge must be findable on the JVM classpath \
         (the bridge JAR auto-load is not wired correctly)"
    );

    // 3. RegisterNatives wiring succeeds (it has already happened inside
    //    JvmRuntime::start; calling it a second time must remain idempotent).
    runtime
        .with_env(|env| register_native_methods(env))
        .expect("register_native_methods must succeed against the bridge JAR");

    // 4. Native round-trip: build a Rust RequestHandle, register it, ask Java
    //    `new TomcatRsRequestFacade(id).getMethod()` what method it sees. The
    //    Java side will call across JNI into our Rust `nativeGetMethod`, which
    //    looks the id up in HANDLE_REGISTRY and returns the value below.
    let request = RequestHandle::new(RequestParts {
        method: "PATCH".to_string(),
        uri: "/smoke".to_string(),
        ..RequestParts::default()
    });
    let native_id = request.native_id();
    register_request(request);

    let observed_method = runtime
        .with_env(|env| {
            // new TomcatRsRequestFacade(long nativeRequestId)
            let facade_class = env
                .find_class("org/apache/tomcatrs/bridge/TomcatRsRequestFacade")
                .map_err(|e| {
                    tomcatrs_core::Error::bridge(format!(
                        "find_class(TomcatRsRequestFacade) failed: {e}"
                    ))
                })?;
            let facade = env
                .new_object(&facade_class, "(J)V", &[JValue::Long(native_id)])
                .map_err(|e| {
                    let _ = env.exception_clear();
                    tomcatrs_core::Error::bridge(format!(
                        "new TomcatRsRequestFacade({native_id}) failed: {e}"
                    ))
                })?;

            // facade.getMethod() -> String
            let method_value = env
                .call_method(&facade, "getMethod", "()Ljava/lang/String;", &[])
                .map_err(|e| {
                    let _ = env.exception_clear();
                    tomcatrs_core::Error::bridge(format!(
                        "TomcatRsRequestFacade.getMethod() failed: {e}"
                    ))
                })?;
            let method_obj: JObject = method_value.l().map_err(|e| {
                tomcatrs_core::Error::bridge(format!("getMethod() did not return an object: {e}"))
            })?;
            let method_jstr = jni::objects::JString::from(method_obj);
            let s: String = env
                .get_string(&method_jstr)
                .map_err(|e| tomcatrs_core::Error::bridge(format!("get_string failed: {e}")))?
                .into();
            Ok(s)
        })
        .expect("native round-trip must succeed once RegisterNatives is wired");

    assert_eq!(
        observed_method, "PATCH",
        "TomcatRsRequestFacade.getMethod() must return what the Rust handle holds; \
         this is the canonical evidence the JNI shim + RegisterNatives + handle \
         registry are wired end-to-end"
    );

    // Tear down before drop so the workers detach cleanly.
    runtime.shutdown();
}
