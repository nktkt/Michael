//! [`ClassScanner`] — the entry point for classpath annotation scanning.
//!
//! A full implementation walks a web application's effective classpath —
//! `WEB-INF/classes` plus every `WEB-INF/lib/*.jar` — reads each Java class
//! file, and records the Servlet-spec annotations it carries into an
//! [`AnnotationIndex`].
//!
//! Tomcat-RS deliberately does **not** parse Java bytecode in Rust: the JVM
//! side of the servlet bridge already has a class loader and reflection, so
//! annotation discovery is performed there and handed back across the bridge.
//! Wiring that bridge call up is scheduled for a later version.
//!
//! For `v0.1.0`, [`ClassScanner::scan`] is therefore a documented no-op: it
//! validates its inputs, emits a `tracing::info!` note explaining that real
//! scanning happens through the JVM bridge, and returns an empty
//! [`AnnotationIndex`]. It never panics and never fails.

use crate::annotations::AnnotationIndex;
use crate::war::Webapp;

/// Scans a web application's classpath for Servlet-spec annotations.
///
/// The scanner is stateless and cheap to create. See the [module
/// docs](self) for why `v0.1.0` defers the actual bytecode scan to the JVM
/// bridge.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClassScanner;

impl ClassScanner {
    /// Create a new class scanner.
    pub fn new() -> ClassScanner {
        ClassScanner
    }

    /// Scan a deployed [`Webapp`] for annotated components.
    ///
    /// In `v0.1.0` this returns an empty [`AnnotationIndex`] after logging an
    /// informational note: real annotation discovery is performed by the JVM
    /// side of the servlet bridge in a later version. The classpath that
    /// *would* be scanned (`WEB-INF/classes` + `WEB-INF/lib/*.jar`) is logged
    /// for diagnostics.
    ///
    /// This method is infallible and never panics.
    pub fn scan(&self, webapp: &Webapp) -> AnnotationIndex {
        let classpath_entries =
            usize::from(webapp.classes_dir().is_some()) + webapp.lib_jars().len();

        tracing::info!(
            context_path = %webapp.context_path(),
            classpath_entries,
            "annotation scanning is delegated to the JVM bridge in a later \
             version; returning an empty AnnotationIndex for v0.1.0"
        );

        AnnotationIndex::new()
    }

    /// Scan an explicit set of classpath roots (class directories and `.jar`
    /// files) for annotated components.
    ///
    /// Behaves like [`Self::scan`]: a documented `v0.1.0` no-op that logs the
    /// number of roots it received and returns an empty [`AnnotationIndex`].
    /// Infallible and panic-free even if a root does not exist.
    pub fn scan_classpath<I, P>(&self, roots: I) -> AnnotationIndex
    where
        I: IntoIterator<Item = P>,
        P: AsRef<std::path::Path>,
    {
        let count = roots.into_iter().count();
        tracing::info!(
            roots = count,
            "classpath annotation scanning is delegated to the JVM bridge in \
             a later version; returning an empty AnnotationIndex for v0.1.0"
        );
        AnnotationIndex::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_classpath_yields_empty_index() {
        let scanner = ClassScanner::new();
        let idx = scanner.scan_classpath(["WEB-INF/classes", "WEB-INF/lib/a.jar"]);
        assert!(idx.is_empty());
    }

    #[test]
    fn scan_classpath_handles_empty_input() {
        let scanner = ClassScanner::new();
        let idx = scanner.scan_classpath(Vec::<std::path::PathBuf>::new());
        assert!(idx.is_empty());
    }
}
