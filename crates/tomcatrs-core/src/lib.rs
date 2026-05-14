//! `tomcatrs-core` — shared primitives for the **Tomcat-RS Compatibility Runtime**.
//!
//! This crate is the foundation every other `tomcatrs-*` crate depends on. It
//! intentionally carries no heavy dependencies: only error types, the
//! [`Lifecycle`] state machine, lifecycle events, and a small shared
//! [`Runtime`] handle.
//!
//! The design mirrors Apache Tomcat's component model (`Server` → `Service` →
//! `Engine` → `Host` → `Context` → `Wrapper`), where every component implements
//! a common lifecycle contract.

pub mod error;
pub mod event;
pub mod lifecycle;
pub mod runtime;

pub use error::{Error, Result};
pub use event::{LifecycleEvent, LifecycleListener};
pub use lifecycle::{Lifecycle, LifecycleContext, LifecycleState};
pub use runtime::{ContextId, Runtime, SessionId, WrapperId};

/// Crate version, sourced from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Human-readable server identity, reported in the `Server:` response header.
pub const SERVER_INFO: &str = "Tomcat-RS Compatibility Runtime";
