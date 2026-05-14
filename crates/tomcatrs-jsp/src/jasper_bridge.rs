//! [`JasperBridge`] — the Rust-side handle to JVM-hosted Jasper.
//!
//! Apache Tomcat compiles and executes JavaServer Pages with *Jasper*, exposed
//! to applications as `org.apache.jasper.servlet.JspServlet`. Tomcat-RS keeps
//! Jasper on the JVM side of the servlet bridge: a request for a `*.jsp` URL is
//! routed, through the bridge, to that `JspServlet` instance, which performs
//! compilation and execution using the JVM's class loader and `javac`.
//!
//! This module provides the configuration ([`JspConfig`]) and the bridge
//! handle ([`JasperBridge`]) the Rust runtime uses to describe and locate that
//! JVM-side Jasper. It deliberately does **not** compile JSPs in Rust: any
//! request for *runtime* compilation from the Rust side is rejected with
//! [`Error::Other`], because that work belongs to the JVM Jasper bridge.

use std::path::{Path, PathBuf};

use tomcatrs_core::{Error, Result};

/// Configuration for the JVM-hosted Jasper engine, mirroring the
/// `JspServlet` init parameters Tomcat exposes in `conf/web.xml`.
#[derive(Debug, Clone)]
pub struct JspConfig {
    /// Development mode: recompile JSPs when their source changes and surface
    /// detailed compilation diagnostics. Production deployments set this to
    /// `false` and rely on precompilation.
    pub development: bool,
    /// The scratch directory Jasper uses for generated `.java` and `.class`
    /// files (the `scratchdir` init parameter).
    pub scratch_dir: PathBuf,
    /// Whether generated servlet source should be kept on disk for debugging
    /// (the `keepgenerated` init parameter).
    pub keep_generated: bool,
    /// The fully-qualified class name of the JVM-side JSP servlet requests are
    /// delegated to. Overridable for alternative Jasper-compatible engines.
    pub jsp_servlet_class: String,
}

impl JspConfig {
    /// Build a default configuration rooted at `scratch_dir`.
    ///
    /// Defaults match a production-leaning Tomcat: `development = false`,
    /// `keep_generated = false`, delegating to the standard
    /// `org.apache.jasper.servlet.JspServlet`.
    pub fn new(scratch_dir: impl AsRef<Path>) -> JspConfig {
        JspConfig {
            development: false,
            scratch_dir: scratch_dir.as_ref().to_path_buf(),
            keep_generated: false,
            jsp_servlet_class: "org.apache.jasper.servlet.JspServlet".to_string(),
        }
    }

    /// Enable development mode (runtime recompilation, verbose diagnostics).
    pub fn with_development(mut self, development: bool) -> JspConfig {
        self.development = development;
        self
    }

    /// Keep generated servlet sources on disk for debugging.
    pub fn with_keep_generated(mut self, keep: bool) -> JspConfig {
        self.keep_generated = keep;
        self
    }
}

/// The Rust-side handle to a JVM-hosted Jasper engine for one context.
///
/// In `v0.1.0` this records configuration and documents the delegation
/// contract; the actual JNI wiring to `JspServlet` lands with the servlet
/// bridge in a later version.
#[derive(Debug, Clone)]
pub struct JasperBridge {
    config: JspConfig,
}

impl JasperBridge {
    /// Create a bridge handle for the given [`JspConfig`].
    pub fn new(config: JspConfig) -> JasperBridge {
        tracing::info!(
            jsp_servlet_class = %config.jsp_servlet_class,
            development = config.development,
            scratch_dir = %config.scratch_dir.display(),
            "initialised Jasper bridge handle; JSP execution is delegated to \
             the JVM-side JspServlet through the servlet bridge"
        );
        JasperBridge { config }
    }

    /// The configuration this bridge was built with.
    pub fn config(&self) -> &JspConfig {
        &self.config
    }

    /// The fully-qualified JVM class name JSP requests are delegated to.
    pub fn jsp_servlet_class(&self) -> &str {
        &self.config.jsp_servlet_class
    }

    /// Request *runtime* compilation of a single JSP resource.
    ///
    /// # Errors
    ///
    /// Always returns [`Error::Other`]: runtime JSP compilation is delegated to
    /// the JVM Jasper bridge and is never performed on the Rust side. Callers
    /// that need this should either precompile (see
    /// [`PrecompileTask`](crate::precompile::PrecompileTask)) or route the
    /// request through the JVM servlet bridge.
    pub fn compile_at_runtime(&self, jsp_path: &Path) -> Result<()> {
        tracing::warn!(
            jsp = %jsp_path.display(),
            "rejecting Rust-side runtime JSP compilation request"
        );
        Err(Error::Other(
            "runtime JSP compilation is delegated to the JVM Jasper bridge".to_string(),
        ))
    }

    /// Resolve a `*.jsp` request to the JVM-side servlet that will handle it.
    ///
    /// # Errors
    ///
    /// Always returns [`Error::Other`]: dispatching a JSP request requires the
    /// JVM servlet bridge, which is wired up in a later version.
    pub fn service(&self, jsp_path: &str) -> Result<()> {
        tracing::warn!(
            jsp = %jsp_path,
            "JSP request dispatch requires the JVM servlet bridge"
        );
        Err(Error::Other(
            "runtime JSP compilation is delegated to the JVM Jasper bridge".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_builder_sets_fields() {
        let cfg = JspConfig::new("/tmp/scratch")
            .with_development(true)
            .with_keep_generated(true);
        assert!(cfg.development);
        assert!(cfg.keep_generated);
        assert_eq!(cfg.scratch_dir, PathBuf::from("/tmp/scratch"));
        assert_eq!(
            cfg.jsp_servlet_class,
            "org.apache.jasper.servlet.JspServlet"
        );
    }

    #[test]
    fn runtime_compilation_is_rejected() {
        let bridge = JasperBridge::new(JspConfig::new("/tmp/scratch"));
        let err = bridge
            .compile_at_runtime(Path::new("/app/index.jsp"))
            .unwrap_err();
        match err {
            Error::Other(msg) => {
                assert_eq!(
                    msg,
                    "runtime JSP compilation is delegated to the JVM Jasper bridge"
                );
            }
            other => panic!("expected Error::Other, got {other:?}"),
        }
    }

    #[test]
    fn service_is_rejected() {
        let bridge = JasperBridge::new(JspConfig::new("/tmp/scratch"));
        assert!(matches!(bridge.service("/index.jsp"), Err(Error::Other(_))));
    }
}
