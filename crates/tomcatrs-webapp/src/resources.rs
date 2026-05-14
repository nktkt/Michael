//! [`WebResourceRoot`] — safe, traversal-proof resource lookup for a webapp.
//!
//! Servlet containers must serve static resources out of a web application's
//! document base while guaranteeing two things:
//!
//! 1. requests can never escape the document base (no `../../etc/passwd`); and
//! 2. the protected `/WEB-INF` and `/META-INF` directories are invisible to
//!    clients — their contents are implementation detail, not web content.
//!
//! [`WebResourceRoot`] enforces both. It resolves request-style paths
//! (always `/`-separated, rooted at the webapp) into real filesystem paths,
//! rejecting anything that violates the rules above.

use std::path::{Component, Path, PathBuf};

use tomcatrs_core::{Error, Result};

/// The protected top-level directories that must never be served to clients.
const PROTECTED_DIRS: [&str; 2] = ["WEB-INF", "META-INF"];

/// A resource lookup root anchored at a web application's document base.
///
/// All lookups are expressed as request paths — `/`-separated, leading slash
/// optional — and are resolved relative to the root. See [`Self::resolve`] for
/// the exact safety contract.
#[derive(Debug, Clone)]
pub struct WebResourceRoot {
    root: PathBuf,
}

impl WebResourceRoot {
    /// Create a resource root anchored at `root` (a webapp document base).
    ///
    /// `root` is stored as given; it does not need to exist yet. Lookups
    /// canonicalise defensively at resolution time.
    pub fn new(root: impl AsRef<Path>) -> WebResourceRoot {
        WebResourceRoot {
            root: root.as_ref().to_path_buf(),
        }
    }

    /// The document-base directory this root is anchored at.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a request path to a real filesystem path within the root.
    ///
    /// Returns:
    ///
    /// * `Ok(Some(path))` — the request is safe and the resource *exists*;
    /// * `Ok(None)` — the request is safe but no such resource exists;
    /// * `Err(Error::NotFound)` — the request is *rejected*: it targets
    ///   `/WEB-INF` or `/META-INF` (or anything beneath them), or it attempts
    ///   to traverse outside the document base via `..`.
    ///
    /// The traversal check is performed purely on the logical path components,
    /// so it is correct even for resources that do not exist on disk.
    pub fn resolve(&self, path: &str) -> Result<Option<PathBuf>> {
        let rel = self.safe_relative(path)?;
        let full = self.root.join(&rel);
        if full.exists() {
            Ok(Some(full))
        } else {
            Ok(None)
        }
    }

    /// Whether a resource exists at `path`.
    ///
    /// A rejected path (protected directory or traversal) returns
    /// `Err(Error::NotFound)`, never `Ok(true)`.
    pub fn exists(&self, path: &str) -> Result<bool> {
        Ok(self.resolve(path)?.is_some())
    }

    /// Whether `path` resolves to an existing directory.
    ///
    /// A rejected path returns `Err(Error::NotFound)`.
    pub fn is_directory(&self, path: &str) -> Result<bool> {
        match self.resolve(path)? {
            Some(p) => Ok(p.is_dir()),
            None => Ok(false),
        }
    }

    /// List the immediate child resource names of the directory at `path`.
    ///
    /// Names are returned without any path prefix, sorted for determinism.
    ///
    /// # Errors
    ///
    /// * [`Error::NotFound`] if `path` is rejected, does not exist, or is not a
    ///   directory.
    /// * [`Error::Io`] if the directory cannot be read.
    pub fn list_dir(&self, path: &str) -> Result<Vec<String>> {
        let resolved = self
            .resolve(path)?
            .ok_or_else(|| Error::NotFound(format!("no such resource: {path}")))?;
        if !resolved.is_dir() {
            return Err(Error::NotFound(format!("not a directory: {path}")));
        }

        let mut names = Vec::new();
        for entry in std::fs::read_dir(&resolved)? {
            let entry = entry?;
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        Ok(names)
    }

    /// Validate `path` and reduce it to a safe relative `PathBuf` within the
    /// root, without touching the filesystem.
    ///
    /// Rejects (with [`Error::NotFound`]) any path that escapes the root or
    /// enters a protected directory.
    fn safe_relative(&self, path: &str) -> Result<PathBuf> {
        // Treat both separators defensively; request paths use '/'.
        let cleaned = path.replace('\\', "/");
        let trimmed = cleaned.trim_start_matches('/');

        let mut rel = PathBuf::new();
        for raw in trimmed.split('/') {
            match raw {
                "" | "." => continue,
                ".." => {
                    // Any '..' that would pop above the root is a traversal
                    // attempt. Since `rel` only ever holds normal components,
                    // a pop that empties it (or an initial '..') is a reject.
                    if !rel.pop() {
                        return Err(Error::NotFound(format!("path traversal rejected: {path}")));
                    }
                }
                segment => {
                    rel.push(segment);
                }
            }
        }

        // Reject the protected directories and anything nested under them,
        // matching their names case-insensitively (Windows / macOS realities).
        if let Some(Component::Normal(first)) = rel.components().next() {
            let first = first.to_string_lossy();
            if PROTECTED_DIRS.iter().any(|p| first.eq_ignore_ascii_case(p)) {
                return Err(Error::NotFound(format!(
                    "access to protected directory rejected: {path}"
                )));
            }
        }

        Ok(rel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "tomcatrs-webapp-res-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("WEB-INF/classes")).unwrap();
        fs::create_dir_all(root.join("META-INF")).unwrap();
        fs::create_dir_all(root.join("static/css")).unwrap();
        fs::write(root.join("index.html"), b"home").unwrap();
        fs::write(root.join("static/css/app.css"), b"body{}").unwrap();
        fs::write(root.join("WEB-INF/web.xml"), b"<web-app/>").unwrap();
        root
    }

    #[test]
    fn resolves_existing_public_resources() {
        let root = make_root("public");
        let r = WebResourceRoot::new(&root);

        assert!(r.exists("/index.html").unwrap());
        assert!(r.exists("static/css/app.css").unwrap());
        assert!(r.is_directory("/static").unwrap());
        assert_eq!(r.resolve("/missing.txt").unwrap(), None);

        let listed = r.list_dir("/static/css").unwrap();
        assert_eq!(listed, vec!["app.css".to_string()]);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rejects_protected_directories() {
        let root = make_root("protected");
        let r = WebResourceRoot::new(&root);

        for p in [
            "/WEB-INF",
            "/WEB-INF/",
            "/WEB-INF/web.xml",
            "WEB-INF/classes",
            "/web-inf/web.xml",
            "/META-INF",
            "/META-INF/MANIFEST.MF",
        ] {
            let err = r.resolve(p).unwrap_err();
            assert!(
                matches!(err, Error::NotFound(_)),
                "expected NotFound for {p}, got {err:?}"
            );
        }

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rejects_path_traversal() {
        let root = make_root("traversal");
        let r = WebResourceRoot::new(&root);

        for p in [
            "../secret",
            "/../secret",
            "/static/../../etc/passwd",
            "..",
            "/static/../../..",
        ] {
            let err = r.resolve(p).unwrap_err();
            assert!(
                matches!(err, Error::NotFound(_)),
                "expected NotFound for {p}, got {err:?}"
            );
        }

        // A '..' that stays inside the root is fine.
        assert!(r.exists("/static/../index.html").unwrap());

        fs::remove_dir_all(&root).ok();
    }
}
