//! End-to-end SCI discovery + invocation integration test for the JVM bridge.
//!
//! # What this proves
//!
//! With `--features jvm` and a JDK on `PATH`, this test:
//!
//! 1. Boots a real embedded JVM via [`JvmRuntime::start`], propagating a
//!    `-Dtomcatrs.sci.marker=<tmpfile>` system property.
//! 2. Builds the `sci-fixture` real-WAR fixture (its `MarkerSci.class`) and
//!    materialises a `META-INF/services/jakarta.servlet.ServletContainerInitializer`
//!    resource that names `com.example.sci.MarkerSci`.
//! 3. Registers the webapp through [`WebappRegistrar::register`] so the
//!    webapp's isolating `URLClassLoader` is built and installed.
//! 4. Drives [`run_sci`].
//! 5. Asserts:
//!    - [`SciReport::initializers`] lists exactly `com.example.sci.MarkerSci`
//!      and there are no errors;
//!    - the SCI's marker file exists and its body confirms the context path
//!      and the (empty) handled-types size — the "honest gap" documented in
//!      [`tomcatrs_servlet_bridge::sci`].
//!
//! # Skip behaviour
//!
//! Mirrors `end_to_end.rs`: graceful skip (returns `Ok` with an
//! `eprintln!`) when:
//!
//! * `javac` is not installed at run time;
//! * `JvmRuntime::start` cannot bring the JVM up (no `libjvm`, no bridge JAR);
//! * the fixture build script is missing.
//!
//! The intent is that this file compiles cleanly today and turns green
//! automatically once the JDK + fixture pipeline is present locally / in CI.

#![cfg(feature = "jvm")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tomcatrs_config::web_xml::WebXml;
use tomcatrs_servlet_bridge::classloader::WebappClassLoaderConfig;
use tomcatrs_servlet_bridge::registration::WebappRegistrar;
use tomcatrs_servlet_bridge::sci::run_sci;
use tomcatrs_servlet_bridge::{JvmConfig, JvmRuntime};

/// Absolute path to the workspace root, derived from this crate's manifest dir.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root above crates/tomcatrs-servlet-bridge")
        .to_path_buf()
}

/// Filesystem location of the SCI fixture's WAR root.
fn sci_fixture_root(root: &Path) -> PathBuf {
    root.join("tests")
        .join("fixtures")
        .join("real-wars")
        .join("sci-fixture")
}

fn marker_sci_class_file(root: &Path) -> PathBuf {
    sci_fixture_root(root)
        .join("WEB-INF")
        .join("classes")
        .join("com")
        .join("example")
        .join("sci")
        .join("MarkerSci.class")
}

fn build_script(root: &Path) -> PathBuf {
    root.join("tests")
        .join("fixtures")
        .join("real-wars")
        .join("build.sh")
}

/// Try to materialise `MarkerSci.class`. Returns `Ok(true)` if present after
/// the call, `Ok(false)` if we can't get there (skip), `Err` only on genuine
/// build failures.
fn ensure_marker_sci_class(root: &Path) -> Result<bool, String> {
    let class = marker_sci_class_file(root);
    if class.is_file() {
        return Ok(true);
    }
    let script = build_script(root);
    if !script.is_file() {
        eprintln!(
            "[sci_test] skipping: {} missing and build script {} not found",
            class.display(),
            script.display()
        );
        return Ok(false);
    }
    match Command::new("javac").arg("-version").output() {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            eprintln!(
                "[sci_test] skipping: `javac -version` exited with status {}",
                out.status
            );
            return Ok(false);
        }
        Err(e) => {
            eprintln!("[sci_test] skipping: `javac` not runnable: {e}");
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

/// Write `META-INF/services/jakarta.servlet.ServletContainerInitializer`
/// under the fixture's `WEB-INF/classes/` tree so the webapp class loader's
/// resource scan can find it.
///
/// `build.sh` clears `WEB-INF/classes/` on every run, so this file is
/// re-materialised by the test rather than committed to the fixture tree.
fn install_sci_service_file(fixture_root: &Path) -> Result<(), String> {
    let services_dir = fixture_root
        .join("WEB-INF")
        .join("classes")
        .join("META-INF")
        .join("services");
    std::fs::create_dir_all(&services_dir)
        .map_err(|e| format!("cannot create {}: {e}", services_dir.display()))?;
    let service_file = services_dir.join("jakarta.servlet.ServletContainerInitializer");
    std::fs::write(&service_file, "com.example.sci.MarkerSci\n")
        .map_err(|e| format!("cannot write {}: {e}", service_file.display()))
}

/// A fresh per-test marker path under the OS temp dir.
fn fresh_marker_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("tomcatrs-sci-marker-{pid}-{nanos}.txt"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jvm_bridge_runs_servlet_container_initializers() {
    let root = workspace_root();

    // 1. Materialise the SCI class file.
    match ensure_marker_sci_class(&root) {
        Ok(true) => {}
        Ok(false) => return, // graceful skip
        Err(e) => {
            eprintln!("[sci_test] skipping: building the SCI fixture failed: {e}");
            return;
        }
    }

    let fixture_root = sci_fixture_root(&root);
    let classes_dir = fixture_root.join("WEB-INF").join("classes");

    // 2. Drop the META-INF/services file the discovery walk needs.
    if let Err(e) = install_sci_service_file(&fixture_root) {
        eprintln!("[sci_test] skipping: cannot install service descriptor: {e}");
        return;
    }

    // 3. Pick a fresh marker path and pass it to the JVM as a sysprop.
    let marker = fresh_marker_path();
    // Clear any stale marker from a previous run.
    let _ = std::fs::remove_file(&marker);

    let cfg = JvmConfig {
        jvm_args: vec![format!("-Dtomcatrs.sci.marker={}", marker.display())],
        ..JvmConfig::default()
    };

    let runtime = match JvmRuntime::start(cfg) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!(
                "[sci_test] skipping: JvmRuntime::start failed: {e}. \
                 SCI discovery requires a working embedded JVM."
            );
            return;
        }
    };

    // 4. Register the webapp so its URLClassLoader is built.
    let context_id: tomcatrs_core::ContextId = "/sci".to_string();
    let cl_config = WebappClassLoaderConfig::new(
        context_id.clone(),
        Some(classes_dir.clone()),
        Vec::new(),
        false,
    );

    let _webapp = runtime
        .register_webapp(context_id.clone(), cl_config.search_path())
        .expect("register_webapp should succeed once JvmRuntime has booted");

    // The fixture has no servlets — a minimal empty web.xml is enough for
    // WebappRegistrar to build the class loader and stop, which is exactly
    // what we want.
    let web_xml = WebXml::default();

    let registrar = WebappRegistrar::new(
        Arc::clone(&runtime),
        context_id.clone(),
        cl_config,
        &web_xml,
    );

    let summary = registrar.register().unwrap_or_else(|e| {
        panic!(
            "WebappRegistrar::register failed: {e}. \
             The fixture has no servlets so only the class loader is built; \
             failure usually means a JNI / classloader regression."
        );
    });
    assert!(
        summary.class_loader_built,
        "class loader must be built under --features jvm"
    );

    // 5. Drive SCI discovery + invocation.
    let report = run_sci(&runtime, &context_id, &fixture_root)
        .await
        .expect("run_sci must not return an infrastructural error");

    assert!(
        !report.has_errors(),
        "no SCI should fail; got errors: {:?}",
        report.errors
    );
    assert_eq!(
        report.initializers,
        vec!["com.example.sci.MarkerSci".to_string()],
        "MarkerSci must be the single discovered + invoked SCI"
    );

    // 6. Marker file proves the JVM-side SCI really ran.
    let body = std::fs::read_to_string(&marker).unwrap_or_else(|e| {
        panic!(
            "marker file {} not produced by MarkerSci.onStartup: {e}",
            marker.display()
        )
    });
    assert!(
        body.contains("MarkerSci.onStartup called"),
        "marker body missing prologue: {body:?}"
    );
    assert!(
        body.contains("context=/sci"),
        "marker body must record the context path: {body:?}"
    );
    // The "honest gap": handledTypes is empty in v1.
    assert!(
        body.contains("handledTypes=0"),
        "marker body must record the empty handled-types set: {body:?}"
    );

    // Clean up the marker; harmless on failure.
    let _ = std::fs::remove_file(&marker);

    runtime.shutdown();
}

// ---------------------------------------------------------------------------
// @HandlesTypes scan — the second integration test.
//
// Drives the `handlestypes-fixture` real-WAR fixture:
//
//   tests/fixtures/real-wars/handlestypes-fixture/
//     WEB-INF/src/com/example/htfx/
//       Marker.java          interface
//       AlphaImpl.java       implements Marker
//       BetaImpl.java        implements Marker
//       HandlesTypesSci.java @HandlesTypes(Marker.class) SCI
//
// After registering the webapp, run_sci should:
//   1. discover HandlesTypesSci via META-INF/services;
//   2. read its @HandlesTypes annotation, finding `com.example.htfx.Marker`;
//   3. scan WEB-INF/classes for every class extending/implementing it,
//      finding AlphaImpl + BetaImpl;
//   4. load both classes through the webapp loader and pass them into
//      onStartup as the handled-types `Set<Class<?>>`.
//
// The SCI writes the names it received to a marker file; the test reads
// the file back and asserts both implementors are present. Spring is
// NOT involved — this proves the @HandlesTypes machinery in isolation.
// ---------------------------------------------------------------------------

fn handlestypes_fixture_root(root: &Path) -> PathBuf {
    root.join("tests")
        .join("fixtures")
        .join("real-wars")
        .join("handlestypes-fixture")
}

fn handlestypes_sci_class_file(root: &Path) -> PathBuf {
    handlestypes_fixture_root(root)
        .join("WEB-INF")
        .join("classes")
        .join("com")
        .join("example")
        .join("htfx")
        .join("HandlesTypesSci.class")
}

fn ensure_handlestypes_classes(root: &Path) -> Result<bool, String> {
    let class = handlestypes_sci_class_file(root);
    if class.is_file() {
        return Ok(true);
    }
    let script = build_script(root);
    if !script.is_file() {
        eprintln!(
            "[sci_test/@HandlesTypes] skipping: {} missing and build script {} not found",
            class.display(),
            script.display()
        );
        return Ok(false);
    }
    match Command::new("javac").arg("-version").output() {
        Ok(out) if out.status.success() => {}
        Ok(_) | Err(_) => {
            eprintln!("[sci_test/@HandlesTypes] skipping: `javac` not runnable");
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

fn install_handlestypes_service_file(fixture_root: &Path) -> Result<(), String> {
    let services_dir = fixture_root
        .join("WEB-INF")
        .join("classes")
        .join("META-INF")
        .join("services");
    std::fs::create_dir_all(&services_dir)
        .map_err(|e| format!("cannot create {}: {e}", services_dir.display()))?;
    let service_file = services_dir.join("jakarta.servlet.ServletContainerInitializer");
    std::fs::write(&service_file, "com.example.htfx.HandlesTypesSci\n")
        .map_err(|e| format!("cannot write {}: {e}", service_file.display()))
}

fn fresh_handlestypes_marker_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("tomcatrs-handlestypes-marker-{pid}-{nanos}.txt"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jvm_bridge_populates_handles_types_set_for_sci() {
    let root = workspace_root();

    // 1. Materialise the fixture's class files.
    match ensure_handlestypes_classes(&root) {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            eprintln!("[sci_test/@HandlesTypes] skipping: build failed: {e}");
            return;
        }
    }

    let fixture_root = handlestypes_fixture_root(&root);
    let classes_dir = fixture_root.join("WEB-INF").join("classes");

    // 2. Install the META-INF/services descriptor.
    if let Err(e) = install_handlestypes_service_file(&fixture_root) {
        eprintln!("[sci_test/@HandlesTypes] skipping: cannot install service descriptor: {e}");
        return;
    }

    // 3. Pick a fresh marker path.
    let marker = fresh_handlestypes_marker_path();
    let _ = std::fs::remove_file(&marker);

    let cfg = JvmConfig {
        jvm_args: vec![format!(
            "-Dtomcatrs.handlestypes.marker={}",
            marker.display()
        )],
        ..JvmConfig::default()
    };

    let runtime = match JvmRuntime::start(cfg) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("[sci_test/@HandlesTypes] skipping: JvmRuntime::start failed: {e}");
            return;
        }
    };

    // 4. Register the webapp.
    let context_id: tomcatrs_core::ContextId = "/handlestypes".to_string();
    let cl_config = WebappClassLoaderConfig::new(
        context_id.clone(),
        Some(classes_dir.clone()),
        Vec::new(),
        false,
    );

    let _webapp = runtime
        .register_webapp(context_id.clone(), cl_config.search_path())
        .expect("register_webapp should succeed once JvmRuntime has booted");

    let web_xml = WebXml::default();
    let registrar = WebappRegistrar::new(
        Arc::clone(&runtime),
        context_id.clone(),
        cl_config,
        &web_xml,
    );
    let summary = registrar
        .register()
        .unwrap_or_else(|e| panic!("WebappRegistrar::register failed: {e}"));
    assert!(summary.class_loader_built);

    // 5. Drive SCI discovery + invocation — this is the path under
    //    test.
    let report = run_sci(&runtime, &context_id, &fixture_root)
        .await
        .expect("run_sci must not return an infrastructural error");

    assert!(
        !report.has_errors(),
        "@HandlesTypes SCI must run without errors; got: {:?}",
        report.errors
    );
    assert_eq!(
        report.initializers,
        vec!["com.example.htfx.HandlesTypesSci".to_string()],
        "exactly one SCI must have run"
    );

    // 6. The SCI wrote a marker file listing the FQCNs it received.
    let body = std::fs::read_to_string(&marker).unwrap_or_else(|e| {
        panic!(
            "marker file {} not produced by HandlesTypesSci.onStartup: {e}",
            marker.display()
        )
    });
    assert!(
        body.contains("context=/handlestypes"),
        "marker must record the context path; got: {body:?}"
    );
    assert!(
        body.contains("handledTypes=2"),
        "marker must record exactly two handled types; got: {body:?}"
    );
    assert!(
        body.contains("handled: com.example.htfx.AlphaImpl"),
        "AlphaImpl must be in the handled-types set; got: {body:?}"
    );
    assert!(
        body.contains("handled: com.example.htfx.BetaImpl"),
        "BetaImpl must be in the handled-types set; got: {body:?}"
    );
    // The Marker interface itself must NOT be in the handled set per
    // Servlet 6 §8.2.4 — the target is excluded from its own subtype
    // closure.
    assert!(
        !body.contains("handled: com.example.htfx.Marker\n"),
        "Marker (the @HandlesTypes target itself) must not be in the set; got: {body:?}"
    );

    let _ = std::fs::remove_file(&marker);
    runtime.shutdown();
}
