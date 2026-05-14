//! `tomcatrs-jsp` — JSP / Jasper integration scaffold for the
//! **Tomcat-RS Compatibility Runtime**.
//!
//! JavaServer Pages are compiled to servlets by Apache Tomcat's *Jasper*
//! engine. Reimplementing the JSP compiler in Rust is out of scope for the
//! incremental rewrite; instead, Tomcat-RS keeps Jasper on the JVM side of the
//! servlet bridge and integrates with it from Rust.
//!
//! This crate provides the Rust-side scaffolding for that integration:
//!
//! * [`jasper_bridge`] — [`JasperBridge`], which documents and represents the
//!   delegation of JSP execution to the JVM's `JspServlet`. Runtime JSP
//!   compilation requests are rejected from the Rust side — they belong to the
//!   JVM Jasper bridge.
//! * [`precompile`] — [`PrecompileTask`], the ahead-of-time
//!   "JSP → servlet" precompile step. `v0.1.0` discovers the JSP files under a
//!   webapp and records the precompile-first strategy.
//! * [`scratchdir`] — [`ScratchDir`], real management of the per-context
//!   scratch directory Jasper uses for generated sources and class files.
//!
//! ## Strategy: precompile first
//!
//! The preferred deployment model is to *precompile* every JSP to a servlet
//! ahead of time (see [`PrecompileTask`]); this removes first-request latency
//! and the need for a compiler at runtime. Where runtime compilation is
//! genuinely required, it is handled JVM-side by Jasper through the bridge —
//! the Rust runtime never shells out to `javac`.

#![deny(missing_docs)]

pub mod jasper_bridge;
pub mod precompile;
pub mod scratchdir;

pub use jasper_bridge::{JasperBridge, JspConfig};
pub use precompile::PrecompileTask;
pub use scratchdir::ScratchDir;

/// Crate version, sourced from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
