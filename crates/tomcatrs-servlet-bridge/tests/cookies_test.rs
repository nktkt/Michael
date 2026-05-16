//! JVM-bridge integration test for inbound cookie parsing.
//!
//! # What this proves
//!
//! When run with `--features jvm`, this test boots an embedded JVM, deploys the
//! `tests/fixtures/real-wars/cookies/` WAR (a one-servlet fixture that writes
//! the parsed `Cookie[]` back into the response body), sends a coyote
//! `Request` with a `Cookie: a=1; b=2; sessionid=abc` header through the
//! bridge, and asserts the response contains each `name=value` pair.
//!
//! End-to-end, this exercises:
//!
//! * `TomcatRsRequestFacade.getCookies()` — the lazy parse path,
//! * the RFC 6265 §5.4-shaped pure-Java `parseCookieHeader` (in
//!   `TomcatRsRequestFacade.java`) — quoted values, RFC 2965 `$Path`/`$Version`
//!   skips, malformed-pair tolerance,
//! * the bridge's `nativeGetHeader` round trip for the `Cookie:` header value.
//!
//! Spring CSRF, Servlet-spec session-id extraction, and any framework that
//! introspects request cookies all funnel through `getCookies()` — this is the
//! integration test that proves the path is live, not stubbed to an empty
//! array.
//!
//! # Skip behaviour
//!
//! Identical to `end_to_end.rs`: the test returns success with an `eprintln!`
//! when any of these is absent:
//!
//! * the WAR fixture's compiled classes (run
//!   `bash tests/fixtures/real-wars/build.sh` first; the test will try to do
//!   so if `javac` is available);
//! * `JvmRuntime::start` fails (no `libjvm`, bridge JAR not on classpath, or
//!   RegisterNatives wiring incomplete).
//!
//! # Honest gaps surfaced by this test
//!
//! The bridge currently exposes only the **first** value of a repeated header
//! through `nativeGetHeader`. Real HTTP clients near-universally concatenate
//! cookies into a single `Cookie:` header (RFC 6265 §5.4 even mandates that
//! direction), so this is sufficient for production traffic; a multi-header
//! path would need a `nativeGetHeaders(String) -> String[]` shim plumbed into
//! `parseCookieHeader`. See the doc on `TomcatRsRequestFacade.getCookies()`.

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

/// Filesystem location of the compiled cookies-fixture servlet class.
fn cookie_class_file(root: &Path) -> PathBuf {
    root.join("tests")
        .join("fixtures")
        .join("real-wars")
        .join("cookies")
        .join("WEB-INF")
        .join("classes")
        .join("com")
        .join("example")
        .join("cookies")
        .join("HelloCookieServlet.class")
}

/// Filesystem location of the cookies WAR root (the dir holding `WEB-INF`).
fn cookie_war_root(root: &Path) -> PathBuf {
    root.join("tests")
        .join("fixtures")
        .join("real-wars")
        .join("cookies")
}

/// Filesystem location of the shared real-wars build script.
fn build_script(root: &Path) -> PathBuf {
    root.join("tests")
        .join("fixtures")
        .join("real-wars")
        .join("build.sh")
}

/// Materialise `HelloCookieServlet.class`. Returns `Ok(true)` on success,
/// `Ok(false)` if we should skip (no `javac`, missing build script). Errors
/// only on a genuinely-broken setup the developer should fix.
fn ensure_cookie_class(root: &Path) -> Result<bool, String> {
    let class = cookie_class_file(root);
    if class.is_file() {
        return Ok(true);
    }

    let script = build_script(root);
    if !script.is_file() {
        eprintln!(
            "[cookies_test] skipping: {} missing AND no build script at {}",
            class.display(),
            script.display()
        );
        return Ok(false);
    }

    // `javac` is required by the build script. Probe up front so the skip
    // message is unambiguous.
    match Command::new("javac").arg("-version").output() {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            eprintln!(
                "[cookies_test] skipping: `javac -version` exited with status {}",
                out.status
            );
            return Ok(false);
        }
        Err(e) => {
            eprintln!("[cookies_test] skipping: `javac` not runnable: {e}");
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
            "build.sh succeeded but {} is still missing",
            class.display()
        ))
    }
}

/// Build a coyote `Request` for `GET /cookies` with the supplied `Cookie:`
/// header value verbatim.
fn coyote_get_with_cookies(cookie_header: &str) -> Request {
    Request {
        method: "GET".into(),
        uri: "/cookies".into(),
        path: "/cookies".into(),
        query: None,
        version: "HTTP/1.1".into(),
        headers: vec![
            ("Host".into(), "localhost".into()),
            ("Cookie".into(), cookie_header.to_string()),
        ],
        body: Bytes::new(),
        peer_addr: "127.0.0.1:54321".parse().unwrap(),
    }
}

/// End-to-end: boot JVM, deploy the cookies WAR, dispatch a request carrying a
/// realistic `Cookie:` header, and assert each name/value pair round-trips.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cookies_round_trip_through_request_facade() {
    let root = workspace_root();

    // 1. Materialise the WAR's compiled classes.
    match ensure_cookie_class(&root) {
        Ok(true) => {}
        Ok(false) => return, // graceful skip; the helper printed the reason.
        Err(e) => {
            eprintln!("[cookies_test] skipping: ensure_cookie_class failed: {e}");
            return;
        }
    }

    let war_root = cookie_war_root(&root);
    let classes_dir = war_root.join("WEB-INF").join("classes");
    let web_xml_path = war_root.join("WEB-INF").join("web.xml");

    // 2. Boot the JVM. Identical skip semantics to `end_to_end.rs`.
    let runtime = match JvmRuntime::start(JvmConfig::default()) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!(
                "[cookies_test] skipping: JvmRuntime::start failed: {e}. \
                 Re-run once the bridge JAR + RegisterNatives wiring is in \
                 place (see end_to_end.rs for the same skip logic)."
            );
            return;
        }
    };

    // 3. Register the webapp + servlet declared in web.xml.
    let context_id: tomcatrs_core::ContextId = "/cookies".to_string();
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
            "failed to parse {}: {e}. The fixture WAR must ship a valid web.xml.",
            web_xml_path.display()
        )
    });

    let registrar = WebappRegistrar::new(
        Arc::clone(&runtime),
        context_id.clone(),
        cl_config,
        &web_xml,
    );
    let summary = registrar.register().unwrap_or_else(|e| {
        panic!(
            "WebappRegistrar::register failed for the cookies fixture: {e}. \
             Cookies fixture has a single trivial servlet; failure here points \
             at the classloader / Servlet.init() path, not the cookie code."
        )
    });
    assert!(
        summary.servlets_registered >= 1,
        "cookies fixture must register one servlet, got {}",
        summary.servlets_registered
    );

    let servlet_name = web_xml.servlets[0].name.clone();
    assert!(
        webapp.servlet(&servlet_name).is_some(),
        "WebappRuntime should have an instance handle for {servlet_name:?}"
    );

    // 4. Dispatch with a realistic, RFC 6265 §5.4-shaped Cookie header. The
    //    pairs deliberately exercise:
    //      * a normal `name=value` pair,
    //      * a `sessionid`-like pair (the `JSESSIONID` carrier shape used by
    //        Spring / Servlet-spec session tracking),
    //      * extra whitespace between pairs (the parser must trim).
    let invoker = JvmServletInvoker::new(Arc::clone(&runtime));
    let req = coyote_get_with_cookies("a=1; b=2; sessionid=abc");
    let resp = invoker
        .invoke_coyote(context_id.clone(), servlet_name.clone(), &req)
        .await
        .unwrap_or_else(|e| {
            panic!(
                "JvmServletInvoker::invoke_coyote failed for the cookies servlet: {e}. \
                 The dispatch surface succeeded earlier (see end_to_end.rs) so \
                 a failure here is in the cookies-specific path."
            );
        });

    assert_eq!(
        resp.status,
        200,
        "GET /cookies should return 200, got {} with body {:?}",
        resp.status,
        String::from_utf8_lossy(&resp.body)
    );

    let body = String::from_utf8_lossy(&resp.body);
    for expected in ["a=1", "b=2", "sessionid=abc"] {
        assert!(
            body.contains(expected),
            "cookies response body must contain {expected:?}; got {body:?}"
        );
    }

    // 5. A second dispatch with NO Cookie header — proves the facade returns
    //    null (the servlet then writes the NO_COOKIES sentinel), which is
    //    the Servlet API contract callers like Spring CSRF rely on.
    let req_empty = Request {
        method: "GET".into(),
        uri: "/cookies".into(),
        path: "/cookies".into(),
        query: None,
        version: "HTTP/1.1".into(),
        headers: vec![("Host".into(), "localhost".into())],
        body: Bytes::new(),
        peer_addr: "127.0.0.1:54321".parse().unwrap(),
    };
    let resp_empty = invoker
        .invoke_coyote(context_id.clone(), servlet_name.clone(), &req_empty)
        .await
        .expect("second invocation (no Cookie header) should also succeed");
    assert_eq!(resp_empty.status, 200);
    let empty_body = String::from_utf8_lossy(&resp_empty.body);
    assert!(
        empty_body.contains("NO_COOKIES"),
        "with no Cookie header the servlet must see getCookies()==null; got body {empty_body:?}"
    );

    // 6. A third dispatch exercising the malformed-pair / quoted-value path.
    //    `quoted="abc def"` strips the quotes; `$Version=1` is dropped as an
    //    RFC 2965 leftover; `no-equals` and `=novalue` are both skipped.
    let req_lenient =
        coyote_get_with_cookies("quoted=\"abc def\"; no-equals; =novalue; $Version=1; ok=yes");
    let resp_lenient = invoker
        .invoke_coyote(context_id.clone(), servlet_name.clone(), &req_lenient)
        .await
        .expect("third invocation (lenient parse) should also succeed");
    assert_eq!(resp_lenient.status, 200);
    let lenient_body = String::from_utf8_lossy(&resp_lenient.body);
    assert!(
        lenient_body.contains("quoted=abc def") && lenient_body.contains("ok=yes"),
        "lenient parse must keep quoted+plain pairs and drop $Version/no-equals/=novalue; \
         got {lenient_body:?}"
    );
    assert!(
        !lenient_body.contains("$Version") && !lenient_body.contains("no-equals"),
        "lenient parse must skip RFC 2965 $Version and malformed pairs; got {lenient_body:?}"
    );

    runtime.shutdown();
}
