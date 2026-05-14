//! [`ScratchDir`] — per-context scratch directory management for Jasper.
//!
//! Jasper needs a writable working directory for each web application context:
//! a place to emit generated servlet `.java` sources and their compiled
//! `.class` files. In classic Tomcat this is `work/Catalina/<host>/<context>`.
//!
//! Unlike the rest of this crate, [`ScratchDir`] is **not** a scaffold — it is
//! a real, working implementation of that directory's lifecycle: create it,
//! clean it out, and resolve paths within it. The JVM-side Jasper bridge is
//! handed the resulting path.

use std::path::{Path, PathBuf};

use tomcatrs_core::Result;

/// A managed per-context scratch directory.
///
/// Construct one with [`ScratchDir::new`] (records the path) or
/// [`ScratchDir::create`] (records the path *and* creates it on disk).
#[derive(Debug, Clone)]
pub struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    /// Record a scratch directory at `path` without touching the filesystem.
    ///
    /// Call [`ScratchDir::create`] (or [`ScratchDir::ensure`]) to materialise
    /// it.
    pub fn new(path: impl AsRef<Path>) -> ScratchDir {
        ScratchDir {
            path: path.as_ref().to_path_buf(),
        }
    }

    /// Build the conventional scratch path for a context under a `work` base
    /// directory: `<work_base>/<engine>/<host>/<context_dir>`.
    ///
    /// `context_path` is sanitised into a single directory component — `/` and
    /// `\` become `_`, and the root context (`""` or `"/"`) becomes `ROOT`,
    /// matching Tomcat's `work/` layout.
    pub fn for_context(
        work_base: impl AsRef<Path>,
        engine: &str,
        host: &str,
        context_path: &str,
    ) -> ScratchDir {
        let context_dir = sanitize_context(context_path);
        let path = work_base.as_ref().join(engine).join(host).join(context_dir);
        ScratchDir::new(path)
    }

    /// The scratch directory path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Create the scratch directory (and any missing parents).
    ///
    /// Idempotent: succeeds if the directory already exists.
    ///
    /// # Errors
    ///
    /// Returns [`tomcatrs_core::Error::Io`] if the directory cannot be created.
    pub fn create(&self) -> Result<()> {
        std::fs::create_dir_all(&self.path)?;
        tracing::debug!(scratch_dir = %self.path.display(), "created scratch directory");
        Ok(())
    }

    /// Ensure the scratch directory exists, creating it if necessary.
    ///
    /// An alias for [`Self::create`] with intention-revealing naming at call
    /// sites that only care that the directory is present.
    pub fn ensure(&self) -> Result<()> {
        self.create()
    }

    /// Whether the scratch directory currently exists on disk.
    pub fn exists(&self) -> bool {
        self.path.is_dir()
    }

    /// Remove every entry inside the scratch directory, leaving the directory
    /// itself in place and empty.
    ///
    /// If the directory does not exist yet it is created, so that a clean is
    /// always followed by a usable, empty directory.
    ///
    /// # Errors
    ///
    /// Returns [`tomcatrs_core::Error::Io`] if the directory cannot be read or
    /// an entry cannot be removed.
    pub fn clean(&self) -> Result<()> {
        if !self.path.exists() {
            return self.create();
        }

        for entry in std::fs::read_dir(&self.path)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                std::fs::remove_dir_all(&path)?;
            } else {
                std::fs::remove_file(&path)?;
            }
        }
        tracing::debug!(scratch_dir = %self.path.display(), "cleaned scratch directory");
        Ok(())
    }

    /// Remove the scratch directory entirely, including the directory itself.
    ///
    /// A no-op if it does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`tomcatrs_core::Error::Io`] if removal fails.
    pub fn remove(&self) -> Result<()> {
        if self.path.exists() {
            std::fs::remove_dir_all(&self.path)?;
            tracing::debug!(scratch_dir = %self.path.display(), "removed scratch directory");
        }
        Ok(())
    }

    /// Resolve a path *within* the scratch directory.
    ///
    /// `relative` is joined onto the scratch root. A leading `/` (or `\`) on
    /// `relative` is stripped so it cannot accidentally escape to an absolute
    /// path; `..` components are *not* interpreted here — callers pass
    /// generated, trusted file names.
    pub fn path_for(&self, relative: impl AsRef<Path>) -> PathBuf {
        let rel = relative.as_ref();
        let stripped = rel
            .to_str()
            .map(|s| s.trim_start_matches(['/', '\\']))
            .unwrap_or("");
        if stripped.is_empty() {
            self.path.clone()
        } else {
            self.path.join(stripped)
        }
    }
}

/// Reduce a context path to a single safe directory-component name.
fn sanitize_context(context_path: &str) -> String {
    let trimmed = context_path.trim();
    if trimmed.is_empty() || trimmed == "/" {
        return "ROOT".to_string();
    }
    trimmed.trim_matches('/').replace(['/', '\\'], "_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn unique_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tomcatrs-jsp-scratch-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn create_and_clean_roundtrip() {
        let dir = unique_dir("roundtrip");
        let scratch = ScratchDir::new(&dir);
        assert!(!scratch.exists());

        scratch.create().unwrap();
        assert!(scratch.exists());

        // Populate, then clean.
        fs::write(scratch.path_for("Generated.java"), b"class X {}").unwrap();
        fs::create_dir_all(scratch.path_for("nested/pkg")).unwrap();
        fs::write(scratch.path_for("nested/pkg/A.class"), b"\xCA\xFE").unwrap();
        assert!(scratch.path().join("Generated.java").exists());

        scratch.clean().unwrap();
        assert!(scratch.exists());
        assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);

        scratch.remove().unwrap();
        assert!(!scratch.exists());
    }

    #[test]
    fn clean_creates_missing_directory() {
        let dir = unique_dir("clean-missing");
        let scratch = ScratchDir::new(&dir);
        assert!(!scratch.exists());
        scratch.clean().unwrap();
        assert!(scratch.exists());
        scratch.remove().ok();
    }

    #[test]
    fn for_context_builds_tomcat_layout() {
        let base = unique_dir("layout");
        let root = ScratchDir::for_context(&base, "Catalina", "localhost", "/");
        assert!(root.path().ends_with("Catalina/localhost/ROOT"));

        let nested = ScratchDir::for_context(&base, "Catalina", "localhost", "/foo/bar");
        assert!(nested.path().ends_with("Catalina/localhost/foo_bar"));
    }

    #[test]
    fn path_for_stays_within_root() {
        let scratch = ScratchDir::new("/work/ctx");
        assert_eq!(
            scratch.path_for("a/b.java"),
            PathBuf::from("/work/ctx/a/b.java")
        );
        // Leading slash is stripped so it cannot become absolute.
        assert_eq!(
            scratch.path_for("/etc/passwd"),
            PathBuf::from("/work/ctx/etc/passwd")
        );
        assert_eq!(scratch.path_for(""), PathBuf::from("/work/ctx"));
    }
}
