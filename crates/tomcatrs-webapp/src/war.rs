//! [`Webapp`] — a single deployed web application.
//!
//! A `Webapp` is the in-memory handle to one deployed application: its context
//! path, its document base on disk, the location of `WEB-INF/classes`, the set
//! of `WEB-INF/lib/*.jar` files that make up its classpath, and its parsed
//! `WEB-INF/web.xml` descriptor (when present).
//!
//! ## Exploded vs. packed
//!
//! `v0.1.0` fully supports **exploded WAR directories** — a directory that
//! contains a `WEB-INF/` subdirectory. Packed `.war` ZIP archives are detected
//! and rejected with a clear [`Error::Deployment`] message; expanding them is
//! deferred to a later version (see [`Webapp::open`]).

use std::path::{Path, PathBuf};

use tomcatrs_core::{Error, Result};

use crate::resources::WebResourceRoot;
use crate::web_descriptor::WebDescriptor;

/// How a [`Webapp`] is laid out on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebappLayout {
    /// An exploded WAR: a directory containing `WEB-INF/`.
    Exploded,
}

/// A deployed web application.
///
/// Construct one with [`Webapp::open`]. The handle is cheap to clone-by-value
/// only through its accessors; it owns its paths and parsed descriptor.
#[derive(Debug, Clone)]
pub struct Webapp {
    context_path: String,
    doc_base: PathBuf,
    layout: WebappLayout,
    web_inf: PathBuf,
    classes_dir: Option<PathBuf>,
    lib_jars: Vec<PathBuf>,
    web_xml_path: Option<PathBuf>,
    descriptor: Option<WebDescriptor>,
}

impl Webapp {
    /// Open and validate an exploded web application.
    ///
    /// `context_path` is the path the application is mounted at (e.g. `/myapp`
    /// or `""` for the root context). `doc_base` is the application's document
    /// base on disk.
    ///
    /// This validates that `doc_base` exists and is a directory containing a
    /// `WEB-INF/` subdirectory, then locates:
    ///
    /// * `WEB-INF/classes` — the unpacked-class directory, if present;
    /// * `WEB-INF/lib/*.jar` — every jar on the application classpath, sorted
    ///   for deterministic ordering;
    /// * `WEB-INF/web.xml` — the deployment descriptor, parsed into a
    ///   [`WebDescriptor`] if it exists.
    ///
    /// # Errors
    ///
    /// * [`Error::Deployment`] if `doc_base` points at a packed `.war` archive
    ///   (not supported in `v0.1.0`), does not exist, is not a directory, or
    ///   has no `WEB-INF/` directory.
    /// * [`Error::Deployment`] if `WEB-INF/web.xml` exists but is malformed.
    /// * [`Error::Io`] if a filesystem operation fails unexpectedly.
    pub fn open(context_path: impl Into<String>, doc_base: impl AsRef<Path>) -> Result<Webapp> {
        let context_path = normalize_context_path(context_path.into());
        let doc_base = doc_base.as_ref().to_path_buf();

        let metadata = std::fs::metadata(&doc_base).map_err(|e| {
            Error::Deployment(format!(
                "cannot open webapp document base {}: {e}",
                doc_base.display()
            ))
        })?;

        if metadata.is_file() {
            // A regular file at the doc base is, in practice, a packed .war.
            let is_war = doc_base
                .extension()
                .map(|e| e.eq_ignore_ascii_case("war"))
                .unwrap_or(false);
            if is_war {
                return Err(Error::Deployment(
                    "packed .war expansion not implemented in v0.1.0".to_string(),
                ));
            }
            return Err(Error::Deployment(format!(
                "webapp document base {} is a file, not an exploded directory",
                doc_base.display()
            )));
        }

        if !metadata.is_dir() {
            return Err(Error::Deployment(format!(
                "webapp document base {} is neither a directory nor a file",
                doc_base.display()
            )));
        }

        let web_inf = doc_base.join("WEB-INF");
        if !web_inf.is_dir() {
            return Err(Error::Deployment(format!(
                "{} is not a valid exploded webapp: missing WEB-INF/ directory",
                doc_base.display()
            )));
        }

        let classes = web_inf.join("classes");
        let classes_dir = if classes.is_dir() {
            Some(classes)
        } else {
            None
        };

        let lib_jars = collect_lib_jars(&web_inf.join("lib"))?;

        let web_xml = web_inf.join("web.xml");
        let (web_xml_path, descriptor) = if web_xml.is_file() {
            let parsed = WebDescriptor::from_xml_file(&web_xml)?;
            (Some(web_xml), Some(parsed))
        } else {
            tracing::info!(
                context_path = %context_path,
                "no WEB-INF/web.xml found; deploying with an empty descriptor"
            );
            (None, None)
        };

        tracing::info!(
            context_path = %context_path,
            doc_base = %doc_base.display(),
            jars = lib_jars.len(),
            has_classes = classes_dir.is_some(),
            has_web_xml = web_xml_path.is_some(),
            "opened exploded webapp"
        );

        Ok(Webapp {
            context_path,
            doc_base,
            layout: WebappLayout::Exploded,
            web_inf,
            classes_dir,
            lib_jars,
            web_xml_path,
            descriptor,
        })
    }

    /// The context path the application is mounted at (`""` for the root).
    pub fn context_path(&self) -> &str {
        &self.context_path
    }

    /// The application's document base directory on disk.
    pub fn doc_base(&self) -> &Path {
        &self.doc_base
    }

    /// How the application is laid out on disk.
    pub fn layout(&self) -> WebappLayout {
        self.layout
    }

    /// The `WEB-INF/` directory of the application.
    pub fn web_inf_dir(&self) -> &Path {
        &self.web_inf
    }

    /// The `WEB-INF/classes` directory, if it exists.
    pub fn classes_dir(&self) -> Option<&Path> {
        self.classes_dir.as_deref()
    }

    /// Every `WEB-INF/lib/*.jar` on the application classpath, sorted.
    pub fn lib_jars(&self) -> &[PathBuf] {
        &self.lib_jars
    }

    /// The path to `WEB-INF/web.xml`, if the application ships one.
    pub fn web_xml_path(&self) -> Option<&Path> {
        self.web_xml_path.as_deref()
    }

    /// The parsed `web.xml` descriptor, if the application ships one.
    pub fn descriptor(&self) -> Option<&WebDescriptor> {
        self.descriptor.as_ref()
    }

    /// Build a traversal-proof [`WebResourceRoot`] rooted at this webapp's
    /// document base.
    pub fn resource_root(&self) -> WebResourceRoot {
        WebResourceRoot::new(&self.doc_base)
    }
}

/// Normalise a context path to Tomcat's conventions: the empty string is the
/// root context, anything else is `/`-prefixed with no trailing slash.
fn normalize_context_path(raw: String) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "/" {
        return String::new();
    }
    let mut path = trimmed.to_string();
    if !path.starts_with('/') {
        path.insert(0, '/');
    }
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    path
}

/// Collect every `*.jar` under `lib_dir`, sorted by path for determinism.
///
/// A missing `lib/` directory is not an error — many applications have none.
fn collect_lib_jars(lib_dir: &Path) -> Result<Vec<PathBuf>> {
    if !lib_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut jars = Vec::new();
    for entry in std::fs::read_dir(lib_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .map(|e| e.eq_ignore_ascii_case("jar"))
                .unwrap_or(false)
        {
            jars.push(path);
        }
    }
    jars.sort();
    Ok(jars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Build a minimal exploded webapp under a unique temp directory and return
    /// its document base. The caller is responsible for cleanup.
    fn make_exploded_webapp(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "tomcatrs-webapp-war-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let web_inf = root.join("WEB-INF");
        fs::create_dir_all(web_inf.join("classes")).unwrap();
        fs::create_dir_all(web_inf.join("lib")).unwrap();
        fs::write(web_inf.join("lib").join("dep-b.jar"), b"jar").unwrap();
        fs::write(web_inf.join("lib").join("dep-a.jar"), b"jar").unwrap();
        fs::write(web_inf.join("lib").join("notes.txt"), b"ignored").unwrap();
        fs::write(
            web_inf.join("web.xml"),
            r#"<web-app><servlet><servlet-name>s</servlet-name>
               <servlet-class>C</servlet-class></servlet></web-app>"#,
        )
        .unwrap();
        fs::write(root.join("index.html"), b"<h1>hi</h1>").unwrap();
        root
    }

    #[test]
    fn open_detects_classes_lib_and_web_xml() {
        let root = make_exploded_webapp("detect");
        let app = Webapp::open("/myapp", &root).expect("open exploded webapp");

        assert_eq!(app.context_path(), "/myapp");
        assert_eq!(app.layout(), WebappLayout::Exploded);
        assert!(app.classes_dir().is_some());
        assert_eq!(app.classes_dir().unwrap(), root.join("WEB-INF/classes"));

        // Only the two jars, sorted, with the .txt excluded.
        assert_eq!(app.lib_jars().len(), 2);
        assert!(app.lib_jars()[0].ends_with("dep-a.jar"));
        assert!(app.lib_jars()[1].ends_with("dep-b.jar"));

        assert!(app.web_xml_path().is_some());
        let desc = app.descriptor().expect("descriptor parsed");
        assert_eq!(desc.servlets.len(), 1);
        assert_eq!(desc.servlets[0].name, "s");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn open_rejects_missing_web_inf() {
        let root =
            std::env::temp_dir().join(format!("tomcatrs-webapp-nowebinf-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let err = Webapp::open("/x", &root).unwrap_err();
        assert!(matches!(err, Error::Deployment(_)));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn open_rejects_packed_war() {
        let war =
            std::env::temp_dir().join(format!("tomcatrs-webapp-packed-{}.war", std::process::id()));
        fs::write(&war, b"PK\x03\x04 fake zip").unwrap();
        let err = Webapp::open("/packed", &war).unwrap_err();
        match err {
            Error::Deployment(msg) => {
                assert!(msg.contains("packed .war expansion not implemented in v0.1.0"))
            }
            other => panic!("expected Deployment error, got {other:?}"),
        }
        fs::remove_file(&war).ok();
    }

    #[test]
    fn root_context_path_is_normalized_to_empty() {
        let root = make_exploded_webapp("rootctx");
        let app = Webapp::open("ROOT-was-mapped-to-slash", "/nonexistent")
            .err()
            .map(|_| ());
        let _ = app;
        // Direct normalization checks.
        assert_eq!(normalize_context_path("/".into()), "");
        assert_eq!(normalize_context_path("".into()), "");
        assert_eq!(normalize_context_path("foo".into()), "/foo");
        assert_eq!(normalize_context_path("/foo/".into()), "/foo");
        fs::remove_dir_all(&root).ok();
    }
}
