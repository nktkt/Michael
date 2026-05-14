//! `tomcatrs-webapp` — the web application layer of the
//! **Tomcat-RS Compatibility Runtime**.
//!
//! This crate is responsible for everything that happens between "a directory
//! or `.war` file exists on disk" and "a deployed, introspectable web
//! application the servlet container can serve requests from". It mirrors the
//! roles Apache Tomcat splits across `HostConfig`, `ContextConfig`,
//! `WebResourceRoot`, and the annotation scanner:
//!
//! * [`war`] — [`Webapp`], a single deployed application. Exploded WAR
//!   directories are fully supported; packed `.war` archives are detected and
//!   politely rejected in `v0.1.0`.
//! * [`resources`] — [`WebResourceRoot`], safe, traversal-proof resource
//!   lookup rooted at the webapp directory, with `/WEB-INF` and `/META-INF`
//!   hidden from the outside world.
//! * [`deployment`] — [`DeploymentScanner`], which scans a host's `app_base`
//!   directory and maps directory / archive names to context paths.
//! * [`annotations`] / [`scanner`] — scaffolding for `@WebServlet`-style
//!   annotation discovery, which is completed through the JVM bridge in a
//!   later version.
//!
//! ## web.xml handling
//!
//! To stay decoupled from the parallel `tomcatrs-config` build, this crate
//! parses `WEB-INF/web.xml` itself with `quick-xml` into the lightweight
//! [`web_descriptor::WebDescriptor`] model. It only extracts what the webapp
//! layer needs (servlets, mappings, filters, listeners, welcome files) and is
//! deliberately tolerant of unknown elements.

#![deny(missing_docs)]

pub mod annotations;
pub mod deployment;
pub mod resources;
pub mod scanner;
pub mod war;
pub mod web_descriptor;

pub use annotations::{AnnotationIndex, WebServletInfo};
pub use deployment::{DeploymentKind, DeploymentScanner, DeploymentUnit};
pub use resources::WebResourceRoot;
pub use scanner::ClassScanner;
pub use war::Webapp;
pub use web_descriptor::WebDescriptor;

/// Crate version, sourced from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
