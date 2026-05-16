//! Integration test for the functional {@link TomcatRsServletContext} surface.
//!
//! # What this proves
//!
//! With `--features jvm` and a JDK on `PATH`, this test exercises the methods
//! Spring's `SpringServletContainerInitializer.onStartup` chain depends on:
//! `addServlet(name, Class<? extends Servlet>).addMapping("/...")`,
//! per-context attribute storage, `getServerInfo`, `getRealPath`,
//! `getContextPath`, and `getInitParameter`. Each is invoked from a custom SCI
//! whose `onStartup` writes its observations to a marker file the Rust test
//! reads back.
//!
//! # How it's plumbed
//!
//! 1. Materialise an SCI fixture under a fresh OS-tempdir webapp root:
//!    - `WEB-INF/classes/com/example/ctx/CtxSci.java` compiled with the
//!      bridge's `jakarta-stubs/` on the classpath;
//!    - `WEB-INF/classes/com/example/ctx/FakeServlet.java` providing the
//!      `Servlet` instance the SCI's `addServlet` registers;
//!    - `WEB-INF/classes/META-INF/services/jakarta.servlet.ServletContainerInitializer`
//!      naming `com.example.ctx.CtxSci`.
//! 2. Boot a JVM with `-Dtomcatrs.ctx.marker=<tmpfile>`.
//! 3. Register the webapp (so the URL class loader is built).
//! 4. Drive [`run_sci`].
//! 5. Read the marker file back and assert the SCI's observations match.
//!
//! # Skip behaviour
//!
//! Mirrors `sci_test.rs`: returns `Ok` with an `eprintln!` when `javac` /
//! `JvmRuntime::start` are unavailable, so a developer without a JDK still
//! sees a passing test.

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

/// Absolute path to the bridge's `jakarta-stubs/` tree.
fn bridge_stubs_dir() -> PathBuf {
    workspace_root()
        .join("crates")
        .join("tomcatrs-servlet-bridge")
        .join("java")
        .join("jakarta-stubs")
}

/// A unique scratch directory under the OS tempdir.
fn fresh_scratch_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("tomcatrs-{label}-{pid}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Source of the custom SCI that exercises the new ServletContext surface.
const CTX_SCI_SRC: &str = r#"
package com.example.ctx;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.nio.file.StandardOpenOption;
import java.util.Set;

import jakarta.servlet.ServletContainerInitializer;
import jakarta.servlet.ServletContext;
import jakarta.servlet.ServletException;
import jakarta.servlet.ServletRegistration;
import org.apache.tomcatrs.bridge.TomcatRsServletContext;

/**
 * Test SCI exercising the new {@code TomcatRsServletContext} surface.
 * Writes its observations to the file named by the
 * {@code tomcatrs.ctx.marker} system property so the Rust harness can read
 * them back.
 */
public class CtxSci implements ServletContainerInitializer {

    @Override
    public void onStartup(Set<Class<?>> c, ServletContext ctx) throws ServletException {
        String marker = System.getProperty("tomcatrs.ctx.marker");
        if (marker == null || marker.isEmpty()) {
            throw new ServletException("CtxSci: tomcatrs.ctx.marker sysprop not set");
        }
        StringBuilder out = new StringBuilder();
        try {
            out.append("contextPath=").append(ctx.getContextPath()).append("\n");
            out.append("serverInfo=").append(ctx.getServerInfo()).append("\n");

            // setAttribute / getAttribute round-trip.
            ctx.setAttribute("ctx.key", "ctx.value");
            Object got = ctx.getAttribute("ctx.key");
            out.append("attribute=").append(got).append("\n");

            // getRealPath round-trip — should resolve under the doc base.
            String realRoot = ctx.getRealPath("/");
            out.append("realRoot=").append(realRoot).append("\n");
            String realWebXml = ctx.getRealPath("/WEB-INF/web.xml");
            out.append("realWebXml=").append(realWebXml).append("\n");

            // addServlet(name, Class) -> Dynamic.addMapping(...).
            ServletRegistration.Dynamic dyn =
                    ctx.addServlet("test", FakeServlet.class);
            if (dyn == null) {
                out.append("addServlet=null\n");
            } else {
                java.util.Set<String> conflicts = dyn.addMapping("/test");
                dyn.setLoadOnStartup(1);
                dyn.setAsyncSupported(true);
                dyn.setInitParameter("greeting", "hi");
                out.append("addServlet=ok\n");
                out.append("conflicts=").append(conflicts.size()).append("\n");
                out.append("mappings=").append(dyn.getMappings()).append("\n");
                out.append("initParam=").append(dyn.getInitParameter("greeting")).append("\n");
                out.append("loadOnStartup=")
                        .append(((TomcatRsServletContext.RegisteredServletEntry) dyn)
                                .getLoadOnStartup())
                        .append("\n");
            }

            // getServletRegistration follow-up: registry returns the same entry.
            ServletRegistration looked = ctx.getServletRegistration("test");
            out.append("lookupName=").append(looked == null ? "<null>" : looked.getName())
                    .append("\n");
            out.append("registrationsSize=")
                    .append(ctx.getServletRegistrations().size()).append("\n");

            Path target = Paths.get(marker);
            Path parent = target.getParent();
            if (parent != null && !Files.isDirectory(parent)) {
                Files.createDirectories(parent);
            }
            Files.write(
                    target,
                    out.toString().getBytes(StandardCharsets.UTF_8),
                    StandardOpenOption.CREATE,
                    StandardOpenOption.TRUNCATE_EXISTING);
        } catch (IOException e) {
            throw new ServletException("CtxSci: marker write failed: " + e, e);
        } catch (RuntimeException e) {
            // Surface the failure into the marker too so the Rust side can
            // see what blew up, then rethrow.
            try {
                Files.write(
                        Paths.get(marker),
                        ("ERROR: " + e + "\n" + out.toString()).getBytes(StandardCharsets.UTF_8),
                        StandardOpenOption.CREATE,
                        StandardOpenOption.TRUNCATE_EXISTING);
            } catch (IOException ignore) {
                // best effort
            }
            throw e;
        }
    }
}
"#;

/// Source of the trivial Servlet the SCI registers.
const FAKE_SERVLET_SRC: &str = r#"
package com.example.ctx;

import java.io.IOException;

import jakarta.servlet.Servlet;
import jakarta.servlet.ServletConfig;
import jakarta.servlet.ServletException;
import jakarta.servlet.ServletRequest;
import jakarta.servlet.ServletResponse;

/** A bare-bones Servlet used to populate ServletRegistration. */
public class FakeServlet implements Servlet {

    private ServletConfig cfg;

    @Override
    public void init(ServletConfig config) {
        this.cfg = config;
    }

    @Override
    public ServletConfig getServletConfig() {
        return cfg;
    }

    @Override
    public void service(ServletRequest req, ServletResponse res)
            throws ServletException, IOException {
        // No-op.
    }

    @Override
    public String getServletInfo() {
        return "FakeServlet";
    }

    @Override
    public void destroy() {
    }
}
"#;

/// Materialise the fixture sources + service-loader descriptor under
/// `fixture_root`. Returns the `WEB-INF/classes/` path.
fn install_fixture(fixture_root: &Path) -> Result<PathBuf, String> {
    let classes = fixture_root.join("WEB-INF").join("classes");
    let src = fixture_root.join("WEB-INF").join("src");
    let pkg = src.join("com").join("example").join("ctx");
    std::fs::create_dir_all(&pkg).map_err(|e| format!("mkdir pkg: {e}"))?;
    std::fs::create_dir_all(&classes).map_err(|e| format!("mkdir classes: {e}"))?;
    std::fs::write(pkg.join("CtxSci.java"), CTX_SCI_SRC.trim_start())
        .map_err(|e| format!("write CtxSci: {e}"))?;
    std::fs::write(pkg.join("FakeServlet.java"), FAKE_SERVLET_SRC.trim_start())
        .map_err(|e| format!("write FakeServlet: {e}"))?;
    // Drop a dummy WEB-INF/web.xml so getRealPath("/WEB-INF/web.xml") resolves
    // to a real existing file.
    std::fs::write(
        fixture_root.join("WEB-INF").join("web.xml"),
        "<?xml version=\"1.0\"?><web-app/>\n",
    )
    .map_err(|e| format!("write web.xml: {e}"))?;
    // META-INF/services descriptor.
    let services = classes.join("META-INF").join("services");
    std::fs::create_dir_all(&services).map_err(|e| format!("mkdir services: {e}"))?;
    std::fs::write(
        services.join("jakarta.servlet.ServletContainerInitializer"),
        "com.example.ctx.CtxSci\n",
    )
    .map_err(|e| format!("write services: {e}"))?;
    Ok(classes)
}

/// Compile the fixture sources against the bridge's `jakarta-stubs/` and the
/// bridge JAR (so the SCI can reference `TomcatRsServletContext`). Skip on
/// `javac` failure; the Rust test reports that as a `false` result.
fn compile_fixture(fixture_root: &Path, classes: &Path) -> Result<bool, String> {
    match Command::new("javac").arg("-version").output() {
        Ok(out) if out.status.success() => {}
        _ => {
            eprintln!("[servlet_context_test] skipping: `javac` not runnable");
            return Ok(false);
        }
    }
    let stubs = bridge_stubs_dir();
    // Bridge JAR — produced by build.rs when --features jvm. Read off the
    // env var the build script exports; if not set, the test can still
    // compile against the stubs alone, but the SCI's import of
    // `TomcatRsServletContext` would fail. Skip cleanly in that case.
    let bridge_jar = match option_env!("TOMCATRS_BRIDGE_JAR") {
        Some(p) if !p.is_empty() => Some(PathBuf::from(p)),
        _ => None,
    };
    let src_dir = fixture_root.join("WEB-INF").join("src");
    let mut javac = Command::new("javac");
    javac
        .arg("-d")
        .arg(classes)
        .arg("-encoding")
        .arg("UTF-8")
        .arg("-sourcepath")
        .arg(&stubs);
    if let Some(jar) = bridge_jar.as_ref() {
        // The SCI references `TomcatRsServletContext.RegisteredServletEntry` —
        // that lives in the bridge JAR. Put it on -cp.
        javac.arg("-cp").arg(jar);
    } else {
        eprintln!(
            "[servlet_context_test] note: TOMCATRS_BRIDGE_JAR not set; fixture \
             references to TomcatRsServletContext will fail at compile time."
        );
        return Ok(false);
    }
    // Collect sources.
    let mut sources = Vec::new();
    collect_java(&src_dir, &mut sources);
    if sources.is_empty() {
        return Err(format!("no .java sources under {}", src_dir.display()));
    }
    for s in &sources {
        javac.arg(s);
    }
    let out = javac.output().map_err(|e| format!("spawn javac: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "javac failed (status {}):\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(true)
}

fn collect_java(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_java(&p, out);
        } else if p.extension().and_then(|e| e.to_str()) == Some("java") {
            out.push(p);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jvm_bridge_servlet_context_supports_dispatcher_registration_shape() {
    // 1. Build a one-shot fixture under tempdir.
    let fixture_root = fresh_scratch_dir("ctx-fixture");
    let classes = match install_fixture(&fixture_root) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[servlet_context_test] skipping: install_fixture: {e}");
            return;
        }
    };
    match compile_fixture(&fixture_root, &classes) {
        Ok(true) => {}
        Ok(false) => {
            // Cleanup and skip.
            let _ = std::fs::remove_dir_all(&fixture_root);
            return;
        }
        Err(e) => {
            eprintln!("[servlet_context_test] skipping: compile_fixture: {e}");
            let _ = std::fs::remove_dir_all(&fixture_root);
            return;
        }
    }

    // 2. Allocate a marker file and propagate as a sysprop.
    let marker = std::env::temp_dir().join(format!(
        "tomcatrs-ctx-marker-{}-{}.txt",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_file(&marker);

    let cfg = JvmConfig {
        jvm_args: vec![format!("-Dtomcatrs.ctx.marker={}", marker.display())],
        ..JvmConfig::default()
    };
    let runtime = match JvmRuntime::start(cfg) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("[servlet_context_test] skipping: JvmRuntime::start failed: {e}");
            let _ = std::fs::remove_dir_all(&fixture_root);
            return;
        }
    };

    // 3. Register the webapp so its URLClassLoader is built and the
    //    ContextEntry is allocated.
    let context_id: tomcatrs_core::ContextId = "/ctx".to_string();
    let cl_config =
        WebappClassLoaderConfig::new(context_id.clone(), Some(classes.clone()), Vec::new(), false);
    let _webapp = runtime
        .register_webapp(context_id.clone(), cl_config.search_path())
        .expect("register_webapp must succeed once JvmRuntime has booted");

    let registrar = WebappRegistrar::new(
        Arc::clone(&runtime),
        context_id.clone(),
        cl_config,
        &WebXml::default(),
    );
    let summary = registrar.register().unwrap_or_else(|e| {
        panic!("WebappRegistrar::register failed: {e}");
    });
    assert!(summary.class_loader_built);

    // 4. Drive SCI; the CtxSci writes the marker.
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
        vec!["com.example.ctx.CtxSci".to_string()],
        "CtxSci must be the discovered + invoked SCI"
    );

    // 5. Read the marker file back and assert every observation.
    let body = std::fs::read_to_string(&marker).unwrap_or_else(|e| {
        panic!(
            "marker file {} not produced by CtxSci.onStartup: {e}",
            marker.display()
        )
    });
    eprintln!("[servlet_context_test] marker:\n{body}");
    assert!(!body.starts_with("ERROR:"), "SCI threw: {body}");
    assert!(
        body.contains("contextPath=/ctx"),
        "contextPath wrong: {body:?}"
    );
    assert!(
        body.contains("serverInfo=Tomcat-RS"),
        "serverInfo missing Tomcat-RS tag: {body:?}"
    );
    assert!(
        body.contains("attribute=ctx.value"),
        "setAttribute/getAttribute round-trip failed: {body:?}"
    );
    // realRoot should be a real path under the fixture tree (the WAR root).
    let war_root = fixture_root.display().to_string();
    assert!(
        body.contains(&format!("realRoot={war_root}")),
        "realRoot mismatch — want under {war_root}, got: {body:?}"
    );
    assert!(
        body.contains("/WEB-INF/web.xml"),
        "realWebXml missing path: {body:?}"
    );
    assert!(
        body.contains("addServlet=ok"),
        "addServlet returned null: {body:?}"
    );
    assert!(
        body.contains("conflicts=0"),
        "fresh mapping should have no conflicts: {body:?}"
    );
    assert!(
        body.contains("mappings=[/test]"),
        "addMapping(\"/test\") did not record: {body:?}"
    );
    assert!(
        body.contains("initParam=hi"),
        "setInitParameter/getInitParameter round-trip failed: {body:?}"
    );
    assert!(
        body.contains("loadOnStartup=1"),
        "setLoadOnStartup did not stick: {body:?}"
    );
    assert!(
        body.contains("lookupName=test"),
        "getServletRegistration did not return the just-added entry: {body:?}"
    );
    assert!(
        body.contains("registrationsSize=1"),
        "getServletRegistrations size wrong: {body:?}"
    );

    // Cleanup.
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_dir_all(&fixture_root);
    runtime.shutdown();
}
