//! [`PrecompileTask`] — ahead-of-time "JSP → servlet" compilation.
//!
//! The preferred Tomcat-RS deployment model is *precompilation*: every JSP in a
//! web application is compiled to a servlet before the application starts
//! serving traffic. This removes first-request compilation latency, removes the
//! need for a Java compiler in the production runtime, and turns JSP errors
//! into deploy-time failures instead of runtime 500s.
//!
//! In `v0.1.0` this module performs the real discovery half of that task —
//! finding every `*.jsp` (and `*.jspx`) file under a web application — and
//! records the set as a [`PrecompileTask`]. Driving the actual compilation is
//! done through the JVM Jasper bridge (Jasper's `JspC` ahead-of-time compiler)
//! and is wired up in a later version.

use std::path::{Path, PathBuf};

use tomcatrs_core::{Error, Result};

/// JSP file extensions recognised by the precompiler, lower-cased.
const JSP_EXTENSIONS: [&str; 2] = ["jsp", "jspx"];

/// An ahead-of-time JSP precompilation task for one web application.
///
/// Construct one with [`PrecompileTask::discover`], which walks the webapp and
/// records every JSP file found. The compilation step itself is delegated to
/// the JVM Jasper bridge (`JspC`) in a later version.
#[derive(Debug, Clone)]
pub struct PrecompileTask {
    webapp_root: PathBuf,
    jsp_files: Vec<PathBuf>,
}

impl PrecompileTask {
    /// Discover every JSP file under `webapp_root` and build a precompile task.
    ///
    /// # Errors
    ///
    /// * [`Error::NotFound`] if `webapp_root` does not exist or is not a
    ///   directory.
    /// * [`Error::Io`] if the directory tree cannot be traversed.
    pub fn discover(webapp_root: impl AsRef<Path>) -> Result<PrecompileTask> {
        let webapp_root = webapp_root.as_ref().to_path_buf();
        let jsp_files = find_jsp_files(&webapp_root)?;
        tracing::info!(
            webapp_root = %webapp_root.display(),
            jsp_files = jsp_files.len(),
            "discovered JSP files; precompile-first strategy will compile them \
             ahead of time via the JVM Jasper bridge in a later version"
        );
        Ok(PrecompileTask {
            webapp_root,
            jsp_files,
        })
    }

    /// The web application root this task was discovered from.
    pub fn webapp_root(&self) -> &Path {
        &self.webapp_root
    }

    /// Every JSP file discovered under the webapp, sorted for determinism.
    pub fn jsp_files(&self) -> &[PathBuf] {
        &self.jsp_files
    }

    /// Whether the web application contains no JSP files at all.
    pub fn is_empty(&self) -> bool {
        self.jsp_files.is_empty()
    }

    /// Run the precompilation.
    ///
    /// # Errors
    ///
    /// Always returns [`Error::Other`] in `v0.1.0`: the ahead-of-time
    /// compilation step is delegated to the JVM Jasper bridge (`JspC`), which
    /// is wired up in a later version. Discovery ([`Self::discover`]) is the
    /// real, usable half today.
    pub fn run(&self) -> Result<()> {
        Err(Error::Other(format!(
            "JSP precompilation of {} file(s) is delegated to the JVM Jasper \
             bridge (JspC) in a later version",
            self.jsp_files.len()
        )))
    }
}

/// Recursively find every `*.jsp` / `*.jspx` file under `webapp_root`.
///
/// Results are sorted by path for deterministic ordering. Symlink loops are not
/// followed beyond the standard library's directory iteration semantics.
///
/// # Errors
///
/// * [`Error::NotFound`] if `webapp_root` does not exist or is not a directory.
/// * [`Error::Io`] if a directory in the tree cannot be read.
pub fn find_jsp_files(webapp_root: &Path) -> Result<Vec<PathBuf>> {
    if !webapp_root.exists() {
        return Err(Error::NotFound(format!(
            "webapp root {} does not exist",
            webapp_root.display()
        )));
    }
    if !webapp_root.is_dir() {
        return Err(Error::NotFound(format!(
            "webapp root {} is not a directory",
            webapp_root.display()
        )));
    }

    let mut found = Vec::new();
    let mut stack = vec![webapp_root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() && has_jsp_extension(&path) {
                found.push(path);
            }
        }
    }

    found.sort();
    Ok(found)
}

/// Whether `path` has a recognised JSP extension (case-insensitive).
fn has_jsp_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let lower = e.to_ascii_lowercase();
            JSP_EXTENSIONS.contains(&lower.as_str())
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn unique_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tomcatrs-jsp-precompile-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn find_jsp_files_walks_recursively() {
        let root = unique_root("walk");
        fs::create_dir_all(root.join("WEB-INF/jsp")).unwrap();
        fs::create_dir_all(root.join("pages")).unwrap();
        fs::write(root.join("index.jsp"), b"<%-- --%>").unwrap();
        fs::write(root.join("pages/about.JSPX"), b"<jsp:root/>").unwrap();
        fs::write(root.join("WEB-INF/jsp/admin.jsp"), b"<%-- --%>").unwrap();
        fs::write(root.join("readme.txt"), b"not a jsp").unwrap();
        fs::write(root.join("style.css"), b"body{}").unwrap();

        let files = find_jsp_files(&root).unwrap();
        assert_eq!(files.len(), 3);
        assert!(files.iter().any(|p| p.ends_with("index.jsp")));
        assert!(files.iter().any(|p| p.ends_with("about.JSPX")));
        assert!(files.iter().any(|p| p.ends_with("admin.jsp")));

        // Sorted ordering.
        let mut sorted = files.clone();
        sorted.sort();
        assert_eq!(files, sorted);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn discover_builds_task() {
        let root = unique_root("discover");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("home.jsp"), b"x").unwrap();

        let task = PrecompileTask::discover(&root).unwrap();
        assert!(!task.is_empty());
        assert_eq!(task.jsp_files().len(), 1);
        assert_eq!(task.webapp_root(), root.as_path());
        assert!(matches!(task.run(), Err(Error::Other(_))));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_root_is_not_found() {
        let missing = unique_root("missing");
        let err = find_jsp_files(&missing).unwrap_err();
        assert!(matches!(err, Error::NotFound(_)));
    }

    #[test]
    fn empty_webapp_yields_empty_task() {
        let root = unique_root("empty");
        fs::create_dir_all(&root).unwrap();
        let task = PrecompileTask::discover(&root).unwrap();
        assert!(task.is_empty());
        fs::remove_dir_all(&root).ok();
    }
}
