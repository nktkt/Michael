//! Integration tests against the workspace-wide WAR fixture corpus.
//!
//! The corpus lives at `tests/fixtures/wars/` in the repository root and is a
//! set of *exploded* webapp directories. Each fixture has a real,
//! well-formed Jakarta 6.0 `WEB-INF/web.xml` that exercises one slice of the
//! deployment-descriptor surface:
//!
//! | Fixture     | What it covers                                                |
//! |-------------|---------------------------------------------------------------|
//! | `hello/`    | minimal: one servlet + one mapping                            |
//! | `static/`   | no servlets, static assets at the document root               |
//! | `filtered/` | a filter mapped to `/*` in front of a servlet                 |
//! | `listener/` | a single `ServletContextListener`                             |
//! | `welcome/`  | a `<welcome-file-list>` plus an `index.html`                  |
//! | `secure/`   | a `<security-constraint>` on `/admin/*` (tolerantly skipped)  |
//!
//! These tests open each fixture with [`Webapp::open`], inspect its parsed
//! [`WebDescriptor`], and assert the structure each one is supposed to expose.
//! A final test runs [`DeploymentScanner::scan`] over the corpus directory to
//! confirm every fixture is discovered as an exploded deployment.

use std::path::PathBuf;

use tomcatrs_webapp::{DeploymentKind, DeploymentScanner, Webapp};

/// Resolve the workspace-root `tests/fixtures/wars` directory from this
/// crate's `CARGO_MANIFEST_DIR`.
fn corpus_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // `crates/tomcatrs-webapp` → workspace root is two levels up.
    manifest
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("wars")
}

/// Open a fixture by its directory name, asserting that the descriptor was
/// parsed successfully.
fn open_fixture(name: &str) -> Webapp {
    let root = corpus_root();
    let path = root.join(name);
    assert!(
        path.is_dir(),
        "fixture directory {} should exist",
        path.display()
    );
    Webapp::open(format!("/{name}"), &path)
        .unwrap_or_else(|e| panic!("opening fixture {name} at {}: {e}", path.display()))
}

#[test]
fn corpus_root_exists_and_lists_every_fixture() {
    let root = corpus_root();
    assert!(
        root.is_dir(),
        "corpus root {} should be a directory",
        root.display()
    );
    for name in [
        "hello", "static", "filtered", "listener", "welcome", "secure",
    ] {
        let p = root.join(name);
        assert!(p.is_dir(), "fixture {} missing at {}", name, p.display());
        assert!(
            p.join("WEB-INF").join("web.xml").is_file(),
            "fixture {} missing WEB-INF/web.xml",
            name
        );
    }
}

#[test]
fn hello_fixture_declares_single_servlet_mapping() {
    let app = open_fixture("hello");
    let desc = app.descriptor().expect("hello has a web.xml");

    assert_eq!(desc.servlets.len(), 1);
    let s = &desc.servlets[0];
    assert_eq!(s.name, "HelloServlet");
    assert_eq!(s.class.as_deref(), Some("com.example.hello.HelloServlet"));
    assert!(s.load_on_startup);

    assert_eq!(desc.servlet_mappings.len(), 1);
    assert_eq!(desc.servlet_mappings[0].servlet_name, "HelloServlet");
    assert_eq!(desc.servlet_mappings[0].url_pattern, "/hello");

    assert!(desc.filters.is_empty());
    assert!(desc.filter_mappings.is_empty());
    assert!(desc.listeners.is_empty());
    assert!(desc.welcome_files.is_empty());

    // The placeholder classes/ directory is present and discoverable.
    assert!(app.classes_dir().is_some());
}

#[test]
fn static_fixture_has_empty_descriptor_and_static_assets() {
    let app = open_fixture("static");
    let desc = app.descriptor().expect("static has a web.xml");

    assert!(desc.servlets.is_empty());
    assert!(desc.servlet_mappings.is_empty());
    assert!(desc.filters.is_empty());
    assert!(desc.filter_mappings.is_empty());
    assert!(desc.listeners.is_empty());
    assert!(desc.welcome_files.is_empty());

    // Static files at the document root must be reachable via the resource root.
    let resources = app.resource_root();
    assert!(
        resources.exists("/index.html").unwrap_or(false),
        "static fixture should expose index.html at the document root"
    );
    assert!(
        resources.exists("/style.css").unwrap_or(false),
        "static fixture should expose style.css"
    );
    assert!(
        resources.exists("/js/app.js").unwrap_or(false),
        "static fixture should expose js/app.js"
    );
}

#[test]
fn filtered_fixture_declares_filter_and_servlet() {
    let app = open_fixture("filtered");
    let desc = app.descriptor().expect("filtered has a web.xml");

    assert_eq!(desc.filters.len(), 1);
    let f = &desc.filters[0];
    assert_eq!(f.name, "EncodingFilter");
    assert_eq!(
        f.class.as_deref(),
        Some("com.example.filtered.EncodingFilter")
    );

    assert_eq!(desc.filter_mappings.len(), 1);
    assert_eq!(desc.filter_mappings[0].filter_name, "EncodingFilter");
    assert_eq!(desc.filter_mappings[0].url_pattern, "/*");

    assert_eq!(desc.servlets.len(), 1);
    assert_eq!(desc.servlets[0].name, "EchoServlet");
    assert_eq!(
        desc.servlets[0].class.as_deref(),
        Some("com.example.filtered.EchoServlet")
    );

    assert_eq!(desc.servlet_mappings.len(), 1);
    assert_eq!(desc.servlet_mappings[0].servlet_name, "EchoServlet");
    assert_eq!(desc.servlet_mappings[0].url_pattern, "/echo");
}

#[test]
fn listener_fixture_declares_servlet_context_listener() {
    let app = open_fixture("listener");
    let desc = app.descriptor().expect("listener has a web.xml");

    assert_eq!(
        desc.listeners,
        vec!["com.example.listener.BootListener".to_string()]
    );
    assert!(desc.servlets.is_empty());
    assert!(desc.filters.is_empty());
    assert!(desc.welcome_files.is_empty());
}

#[test]
fn welcome_fixture_lists_welcome_files_in_order() {
    let app = open_fixture("welcome");
    let desc = app.descriptor().expect("welcome has a web.xml");

    assert_eq!(
        desc.welcome_files,
        vec![
            "index.html".to_string(),
            "index.htm".to_string(),
            "index.jsp".to_string(),
        ]
    );

    // The first welcome file is a real, fetchable resource.
    let resources = app.resource_root();
    assert!(
        resources.exists("/index.html").unwrap_or(false),
        "welcome fixture should ship index.html at the document root"
    );

    assert!(desc.servlets.is_empty());
    assert!(desc.filters.is_empty());
    assert!(desc.listeners.is_empty());
}

#[test]
fn secure_fixture_parses_cleanly_with_security_constraint_present() {
    // The `WebDescriptor` model intentionally does not surface
    // `<security-constraint>` — it is one of the elements the parser
    // tolerantly skips. The contract we *do* assert here is that:
    //   1. opening the webapp succeeds (the XML is well-formed),
    //   2. the descriptor's modelled fields are all empty for this fixture,
    //   3. the on-disk `web.xml` text still contains the security constraint
    //      so future versions of the descriptor can pick it up without us
    //      having to touch the fixture.
    let app = open_fixture("secure");
    let desc = app.descriptor().expect("secure has a web.xml");

    assert!(desc.servlets.is_empty());
    assert!(desc.servlet_mappings.is_empty());
    assert!(desc.filters.is_empty());
    assert!(desc.filter_mappings.is_empty());
    assert!(desc.listeners.is_empty());
    assert!(desc.welcome_files.is_empty());

    let web_xml = app
        .web_xml_path()
        .expect("secure fixture has a web.xml path");
    let raw = std::fs::read_to_string(web_xml).expect("read web.xml");
    assert!(
        raw.contains("<security-constraint>"),
        "secure fixture web.xml should declare a <security-constraint>"
    );
    assert!(
        raw.contains("/admin/*"),
        "secure fixture should constrain /admin/*"
    );
    assert!(
        raw.contains("<role-name>admin</role-name>"),
        "secure fixture should mention the 'admin' role"
    );
}

#[test]
fn deployment_scanner_discovers_every_fixture() {
    let root = corpus_root();
    let units = DeploymentScanner::new()
        .scan(&root)
        .expect("scanning the fixtures corpus");

    // Every unit must be an exploded directory.
    for u in &units {
        assert_eq!(
            u.kind,
            DeploymentKind::ExplodedDirectory,
            "{} should be exploded",
            u.context_path
        );
    }

    let paths: Vec<&str> = units.iter().map(|u| u.context_path.as_str()).collect();
    // Sorted lexicographically by context path; "/static" sorts before
    // "/welcome" etc. The full set must match exactly.
    assert_eq!(
        paths,
        vec![
            "/filtered",
            "/hello",
            "/listener",
            "/secure",
            "/static",
            "/welcome",
        ]
    );

    // Each unit's doc_base must point back inside the corpus root.
    for u in &units {
        assert!(
            u.doc_base.starts_with(&root),
            "doc_base {} should live under corpus root {}",
            u.doc_base.display(),
            root.display()
        );
    }
}
