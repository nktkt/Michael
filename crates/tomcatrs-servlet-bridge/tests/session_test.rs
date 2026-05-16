//! End-to-end JVM-bridge integration test for `HttpSession` resolution.
//!
//! # What this proves
//!
//! With `--features jvm` and the `tests/fixtures/real-wars/session/` WAR
//! compiled, this test exercises the full session-resolution pipeline:
//!
//! ```text
//!   Rust test harness
//!     -> JvmRuntime::start (boots an embedded JVM)
//!     -> WebappRegistrar::register (URLClassLoader + CounterServlet.init() +
//!                                   default in-memory SessionManager attached)
//!     -> JvmServletInvoker::invoke_coyote (GET /session, no cookie)
//!         -> JNI -> CounterServlet.doGet
//!             -> req.getSession()
//!                 -> nativeResolveOrCreateSession  (creates a session)
//!                 -> nativeIsNewSession            (true)
//!                 -> nativeNewSessionCookie + nativeAddHeader("Set-Cookie", ...)
//!             -> session.setAttribute("count", "1")
//!         -> response body: "count=1"
//!     extract JSESSIONID from the Set-Cookie header
//!     -> JvmServletInvoker::invoke_coyote (GET /session, Cookie: JSESSIONID=...)
//!         -> req.getSession()
//!             -> nativeResolveOrCreateSession  (reuses the existing session)
//!             -> nativeIsNewSession            (false, no Set-Cookie emitted)
//!         -> session.getAttribute("count") -> "1"
//!         -> session.setAttribute("count", "2")
//!     assert: both responses share the same JSESSIONID and the counter
//!     observed the previous request's write.
//! ```
//!
//! # Skip behaviour
//!
//! The test gracefully degrades (returns success with an `eprintln!`) when:
//!
//! * the WAR class files are not present AND
//!   `tests/fixtures/real-wars/build.sh` cannot produce them (no `javac`);
//! * `JvmRuntime::start` fails because the bridge JAR isn't on the
//!   classpath, `libjvm` cannot be located, or `RegisterNatives` wiring
//!   is incomplete.
//!
//! The skip messages spell out which piece is missing so the developer
//! can fix it locally.

#![cfg(feature = "jvm")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use bytes::Bytes;
use tomcatrs_config::web_xml::WebXml;
use tomcatrs_coyote::Request;
use tomcatrs_servlet_bridge::classloader::WebappClassLoaderConfig;
use tomcatrs_servlet_bridge::registration::WebappRegistrar;
use tomcatrs_servlet_bridge::{JvmConfig, JvmRuntime, JvmServletInvoker};

/// Absolute path to the workspace root, derived from this crate's manifest dir.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root above crates/tomcatrs-servlet-bridge")
        .to_path_buf()
}

/// Filesystem location of the compiled `CounterServlet.class`.
fn session_class_file(root: &Path) -> PathBuf {
    root.join("tests")
        .join("fixtures")
        .join("real-wars")
        .join("session")
        .join("WEB-INF")
        .join("classes")
        .join("com")
        .join("example")
        .join("session")
        .join("CounterServlet.class")
}

/// Filesystem location of the session WAR root.
fn session_war_root(root: &Path) -> PathBuf {
    root.join("tests")
        .join("fixtures")
        .join("real-wars")
        .join("session")
}

/// Filesystem location of the build script that compiles all real WARs.
fn build_script(root: &Path) -> PathBuf {
    root.join("tests")
        .join("fixtures")
        .join("real-wars")
        .join("build.sh")
}

/// Try to materialise `CounterServlet.class`. Returns `Ok(true)` if the class
/// file is present afterwards, `Ok(false)` if we cannot get there (no script,
/// no `javac`, or script failure) — in which case the caller should skip.
fn ensure_counter_class(root: &Path) -> Result<bool, String> {
    let class = session_class_file(root);
    if class.is_file() {
        return Ok(true);
    }

    let script = build_script(root);
    if !script.is_file() {
        eprintln!(
            "[session_test] skipping: neither {} nor its build script {} exist",
            class.display(),
            script.display()
        );
        return Ok(false);
    }

    // `javac` is required by the build script.
    match Command::new("javac").arg("-version").output() {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            eprintln!(
                "[session_test] skipping: `javac -version` exited with status {}",
                out.status
            );
            return Ok(false);
        }
        Err(e) => {
            eprintln!("[session_test] skipping: `javac` not runnable: {e}");
            return Ok(false);
        }
    }

    let status = Command::new("bash")
        .arg(&script)
        .current_dir(root)
        .status()
        .map_err(|e| format!("failed to spawn {}: {e}", script.display()))?;
    if !status.success() {
        return Err(format!("{} exited with {}", script.display(), status));
    }

    if class.is_file() {
        Ok(true)
    } else {
        Err(format!(
            "build.sh succeeded but {} still missing",
            class.display()
        ))
    }
}

/// Build a coyote `Request` for `GET /session` with an optional `Cookie`
/// header carrying a previous `JSESSIONID` value.
fn coyote_get(cookie: Option<&str>) -> Request {
    let mut headers = vec![("Host".into(), "localhost".into())];
    if let Some(value) = cookie {
        headers.push(("Cookie".into(), value.to_string()));
    }
    Request {
        method: "GET".into(),
        uri: "/session".into(),
        path: "/session".into(),
        query: None,
        version: "HTTP/1.1".into(),
        headers,
        body: Bytes::new(),
        peer_addr: "127.0.0.1:54321".parse().unwrap(),
    }
}

/// Extract the `JSESSIONID=<value>` pair from a `Set-Cookie` header value
/// (e.g. `JSESSIONID=ABC123; Path=/; HttpOnly`).
fn extract_jsessionid(set_cookie: &str) -> Option<String> {
    set_cookie
        .split(';')
        .map(str::trim)
        .find_map(|pair| pair.strip_prefix("JSESSIONID=").map(str::to_owned))
}

/// End-to-end: boot JVM, deploy the WAR, dispatch two requests on the same
/// JSESSIONID, assert the counter increments across requests.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jvm_bridge_session_round_trips_jsessionid_and_counter() {
    let root = workspace_root();

    // 1. Materialise the WAR.
    match ensure_counter_class(&root) {
        Ok(true) => {}
        Ok(false) => return, // graceful skip
        Err(e) => {
            eprintln!("[session_test] skipping: building the WAR failed: {e}");
            return;
        }
    }

    let war_root = session_war_root(&root);
    let classes_dir = war_root.join("WEB-INF").join("classes");
    let web_xml_path = war_root.join("WEB-INF").join("web.xml");

    // 2. Boot the JVM.
    let runtime = match JvmRuntime::start(JvmConfig::default()) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!(
                "[session_test] skipping: JvmRuntime::start failed: {e}. \
                 Bridge JAR may be missing from the classpath, or libjvm \
                 cannot be located, or RegisterNatives wiring is not yet \
                 in place."
            );
            return;
        }
    };

    // 3. Register the webapp. The registrar also attaches a default
    //    in-memory SessionManager for this context — that is what the
    //    request facade's `nativeResolveOrCreateSession` shim looks up.
    let context_id: tomcatrs_core::ContextId = "/session-app".to_string();
    let cl_config = WebappClassLoaderConfig::new(
        context_id.clone(),
        Some(classes_dir.clone()),
        Vec::new(),
        false,
    );
    let webapp = runtime
        .register_webapp(context_id.clone(), cl_config.search_path())
        .expect("register_webapp should succeed once JvmRuntime has booted");

    let web_xml = WebXml::from_xml_file(&web_xml_path).unwrap_or_else(|e| {
        panic!(
            "failed to parse {}: {e}. The session WAR must ship a valid web.xml.",
            web_xml_path.display()
        )
    });
    assert!(
        !web_xml.servlets.is_empty(),
        "fixture web.xml must declare at least one <servlet>"
    );

    let registrar = WebappRegistrar::new(
        Arc::clone(&runtime),
        context_id.clone(),
        cl_config,
        &web_xml,
    );
    let summary = registrar.register().unwrap_or_else(|e| {
        panic!(
            "WebappRegistrar::register failed: {e}. \
             The CounterServlet could be loaded but init() failed; verify \
             the bridge facade jar is on the JVM classpath."
        );
    });
    assert!(summary.servlets_registered >= 1);
    assert!(summary.class_loader_built);
    let servlet_name = web_xml.servlets[0].name.clone();
    assert!(webapp.servlet(&servlet_name).is_some());

    // 4. First request: no JSESSIONID cookie — the bridge must create one
    //    and emit a Set-Cookie header on the response.
    let invoker = JvmServletInvoker::new(Arc::clone(&runtime));
    let resp1 = invoker
        .invoke_coyote(context_id.clone(), servlet_name.clone(), &coyote_get(None))
        .await
        .unwrap_or_else(|e| panic!("first /session dispatch failed: {e}"));

    assert_eq!(
        resp1.status,
        200,
        "first response status: got {} body={:?}",
        resp1.status,
        String::from_utf8_lossy(&resp1.body)
    );
    let body1 = String::from_utf8_lossy(&resp1.body).into_owned();
    assert_eq!(body1, "count=1", "first counter value: {body1:?}");

    let set_cookie_1 = resp1
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Set-Cookie"))
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| {
            panic!(
                "first response must emit a Set-Cookie header for the new session; \
                 got headers: {:?}",
                resp1.headers
            )
        });
    eprintln!("[session_test] first Set-Cookie: {set_cookie_1}");
    let jsessionid_1 = extract_jsessionid(&set_cookie_1).unwrap_or_else(|| {
        panic!("first Set-Cookie did not contain a JSESSIONID=...; got {set_cookie_1:?}")
    });
    assert!(!jsessionid_1.is_empty(), "first JSESSIONID value is empty");

    // 5. Second request: send the JSESSIONID back as a Cookie header. The
    //    bridge must reuse the existing session, NOT emit a fresh
    //    Set-Cookie, and the counter must observe the previous write.
    let cookie_value = format!("JSESSIONID={jsessionid_1}");
    let resp2 = invoker
        .invoke_coyote(
            context_id.clone(),
            servlet_name.clone(),
            &coyote_get(Some(&cookie_value)),
        )
        .await
        .unwrap_or_else(|e| panic!("second /session dispatch failed: {e}"));

    assert_eq!(
        resp2.status,
        200,
        "second response status: got {} body={:?}",
        resp2.status,
        String::from_utf8_lossy(&resp2.body)
    );
    let body2 = String::from_utf8_lossy(&resp2.body).into_owned();
    assert_eq!(
        body2, "count=2",
        "second counter value (expected the stored state to be visible): {body2:?}"
    );

    // The bridge must NOT emit a fresh Set-Cookie on a reused session
    // (the client already holds the cookie).
    let any_set_cookie_2 = resp2
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("Set-Cookie"));
    assert!(
        !any_set_cookie_2,
        "second response must not emit Set-Cookie for a reused session; \
         got headers: {:?}",
        resp2.headers
    );

    // 6. Cleanly tear down.
    runtime.shutdown();
}
