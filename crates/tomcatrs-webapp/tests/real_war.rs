//! Integration tests against the real, *compiled* WAR fixture corpus at
//! `tests/fixtures/real-wars/`.
//!
//! Unlike the fixtures under `tests/fixtures/wars/` (which ship `web.xml`
//! descriptors plus documentation-only `.java` sources), the `real-wars/`
//! corpus ships **real, compilable Java servlet sources** that the JVM-bridge
//! integration tests need on a classpath. This test:
//!
//!   1. invokes `tests/fixtures/real-wars/build.sh` to compile the sources
//!      (or skips quietly with an `eprintln!` if `javac` isn't on PATH);
//!   2. opens each fixture with [`Webapp::open`] and asserts that
//!      `WEB-INF/classes/.../*.class` files are now on disk and that the
//!      parsed `web.xml` exposes the expected servlets, mappings, filters,
//!      listeners, and init-params.
//!
//! The test is JDK-aware but does **not** require a JDK to pass: a
//! "javac-missing" run logs a skip notice and returns `Ok` so it never blocks
//! a `cargo test` run on a JDK-less developer machine.

use std::path::{Path, PathBuf};
use std::process::Command;

use tomcatrs_webapp::Webapp;

/// Workspace-root-relative path to the real-WAR fixtures directory.
fn real_wars_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // `crates/tomcatrs-webapp` → workspace root is two levels up.
    manifest
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("real-wars")
}

/// Returns `true` if `javac` is reachable via `$PATH` or `$JAVA_HOME/bin`.
fn javac_available() -> bool {
    if let Ok(java_home) = std::env::var("JAVA_HOME") {
        let candidate = Path::new(&java_home).join("bin").join("javac");
        if candidate.is_file() {
            return true;
        }
    }
    Command::new("javac")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run `tests/fixtures/real-wars/build.sh` once, returning `true` on success.
///
/// Streams the script's stdout/stderr into the test's own output so a CI
/// failure shows exactly which fixture failed to compile.
fn run_build_script(real_wars: &Path) -> bool {
    let script = real_wars.join("build.sh");
    assert!(script.is_file(), "build.sh missing at {}", script.display());

    let status = Command::new("bash")
        .arg(&script)
        .status()
        .expect("spawn bash to run real-wars/build.sh");
    status.success()
}

#[test]
fn build_script_compiles_every_fixture() {
    let root = real_wars_root();
    assert!(
        root.is_dir(),
        "real-wars corpus missing at {}",
        root.display()
    );

    if !javac_available() {
        eprintln!(
            "real_war: javac not on PATH and JAVA_HOME unset; \
             skipping real-WAR compile test"
        );
        return;
    }

    assert!(
        run_build_script(&root),
        "tests/fixtures/real-wars/build.sh failed"
    );

    // After a successful build every fixture must have at least one class file
    // under WEB-INF/classes/. The `_stubs/` sibling is not a fixture.
    for war_name in ["hello-servlet", "spring-boot-style"] {
        let classes = root.join(war_name).join("WEB-INF").join("classes");
        assert!(
            classes.is_dir(),
            "{} classes dir not built",
            classes.display()
        );
        let mut found_any = false;
        walk_for_class(&classes, &mut found_any);
        assert!(
            found_any,
            "no .class files emitted under {}",
            classes.display()
        );
    }
}

#[test]
fn hello_servlet_fixture_opens_and_exposes_expected_descriptor() {
    let root = real_wars_root();
    if !javac_available() {
        eprintln!("real_war: javac not on PATH; skipping hello-servlet open test");
        return;
    }
    assert!(
        run_build_script(&root),
        "tests/fixtures/real-wars/build.sh failed"
    );

    let war = root.join("hello-servlet");
    let app = Webapp::open("/hello-servlet", &war).expect("Webapp::open hello-servlet");

    // The compiled class is exactly where the test plan asserts it should be.
    let class_path = war
        .join("WEB-INF")
        .join("classes")
        .join("com")
        .join("example")
        .join("hello")
        .join("HelloServlet.class");
    assert!(
        class_path.is_file(),
        "expected compiled servlet at {}",
        class_path.display()
    );

    // The parsed descriptor must declare the servlet, its class, its mapping,
    // and the `greeting` init-param — the integration test will rely on every
    // one of those being present.
    let desc = app.descriptor().expect("hello-servlet has a web.xml");
    assert_eq!(desc.servlets.len(), 1, "exactly one <servlet>");
    let s = &desc.servlets[0];
    assert_eq!(s.name, "HelloServlet");
    assert_eq!(s.class.as_deref(), Some("com.example.hello.HelloServlet"));
    assert!(s.load_on_startup, "HelloServlet should be load-on-startup");

    assert_eq!(desc.servlet_mappings.len(), 1);
    assert_eq!(desc.servlet_mappings[0].servlet_name, "HelloServlet");
    assert_eq!(desc.servlet_mappings[0].url_pattern, "/hello");

    // The init-param is not surfaced by `WebDescriptor` (it intentionally
    // models a small subset) but the raw web.xml on disk must still declare
    // it — re-parsing more aggressively in a later version must just work.
    let raw =
        std::fs::read_to_string(app.web_xml_path().expect("web.xml path")).expect("read web.xml");
    assert!(
        raw.contains("<param-name>greeting</param-name>"),
        "web.xml should declare the greeting init-param"
    );
    assert!(
        raw.contains("<param-value>Hello</param-value>"),
        "greeting init-param value should be 'Hello'"
    );

    // classes_dir must point at the directory holding the freshly-compiled
    // .class file.
    let classes_dir = app.classes_dir().expect("classes_dir present");
    assert_eq!(classes_dir, war.join("WEB-INF").join("classes"));
}

#[test]
fn spring_boot_style_fixture_exposes_filter_listener_and_servlets() {
    let root = real_wars_root();
    if !javac_available() {
        eprintln!("real_war: javac not on PATH; skipping spring-boot-style test");
        return;
    }
    assert!(
        run_build_script(&root),
        "tests/fixtures/real-wars/build.sh failed"
    );

    let war = root.join("spring-boot-style");
    let app = Webapp::open("/spring-boot-style", &war).expect("Webapp::open spring-boot-style");

    let desc = app.descriptor().expect("spring-boot-style has a web.xml");

    // Two servlets: RootServlet at "/" and ApiServlet at "/api/*".
    let servlet_names: Vec<&str> = desc.servlets.iter().map(|s| s.name.as_str()).collect();
    assert!(
        servlet_names.contains(&"RootServlet"),
        "RootServlet declared; got {:?}",
        servlet_names
    );
    assert!(
        servlet_names.contains(&"ApiServlet"),
        "ApiServlet declared; got {:?}",
        servlet_names
    );

    let mapping_patterns: Vec<&str> = desc
        .servlet_mappings
        .iter()
        .map(|m| m.url_pattern.as_str())
        .collect();
    assert!(mapping_patterns.contains(&"/"));
    assert!(mapping_patterns.contains(&"/api/*"));

    // One filter, mapped to /*.
    assert_eq!(desc.filters.len(), 1);
    assert_eq!(desc.filters[0].name, "RequestLoggingFilter");
    assert_eq!(
        desc.filters[0].class.as_deref(),
        Some("com.example.spring.RequestLoggingFilter")
    );
    assert_eq!(desc.filter_mappings.len(), 1);
    assert_eq!(desc.filter_mappings[0].url_pattern, "/*");

    // One ServletContextListener.
    assert_eq!(
        desc.listeners,
        vec!["com.example.spring.AppStartupListener".to_string()]
    );

    // Welcome file declared.
    assert_eq!(desc.welcome_files, vec!["index.html".to_string()]);

    // All four .class files emitted under WEB-INF/classes/com/example/spring/.
    let classes = war.join("WEB-INF").join("classes");
    for cls in [
        "RootServlet.class",
        "ApiServlet.class",
        "RequestLoggingFilter.class",
        "AppStartupListener.class",
    ] {
        let p = classes.join("com").join("example").join("spring").join(cls);
        assert!(p.is_file(), "expected compiled class at {}", p.display());
    }

    // WEB-INF/lib/ exists as a placeholder; `Webapp::open` must tolerate an
    // empty-but-present lib directory and report zero jars.
    let lib_dir = war.join("WEB-INF").join("lib");
    assert!(lib_dir.is_dir(), "WEB-INF/lib placeholder must exist");
    assert!(
        app.lib_jars().is_empty(),
        "lib dir is empty: got {:?}",
        app.lib_jars()
    );

    // welcome-file index.html is fetchable through the resource root.
    let resources = app.resource_root();
    assert!(
        resources.exists("/index.html").unwrap_or(false),
        "spring-boot-style ships an index.html welcome file"
    );
}

#[test]
fn build_script_is_idempotent() {
    let root = real_wars_root();
    if !javac_available() {
        eprintln!("real_war: javac not on PATH; skipping idempotency test");
        return;
    }

    // Two back-to-back runs both succeed, and the second leaves the same set
    // of class files on disk (no surprising deletions, no growth).
    assert!(run_build_script(&root));
    let before = collect_class_paths(&root);
    assert!(run_build_script(&root));
    let after = collect_class_paths(&root);
    assert_eq!(
        before, after,
        "second build run changed the .class file set"
    );
    assert!(!before.is_empty(), "build script produced no .class files");
}

/// Recursive walker — sets `*found` to true on the first `.class` file seen.
fn walk_for_class(dir: &Path, found: &mut bool) {
    if *found {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_for_class(&path, found);
        } else if path.extension().and_then(|e| e.to_str()) == Some("class") {
            *found = true;
            return;
        }
    }
}

/// Collect every `.class` file under each fixture's `WEB-INF/classes/`,
/// sorted, for stable equality checks.
fn collect_class_paths(real_wars: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(real_wars) else {
        return out;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('_') {
            continue;
        }
        let classes = p.join("WEB-INF").join("classes");
        if classes.is_dir() {
            collect_classes_into(&classes, &mut out);
        }
    }
    out.sort();
    out
}

fn collect_classes_into(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_classes_into(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("class") {
            out.push(path);
        }
    }
}
