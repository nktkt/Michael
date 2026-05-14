//! Unified error type shared across the workspace.

use std::io;

use thiserror::Error;

/// Convenient `Result` alias used throughout the Tomcat-RS crates.
pub type Result<T> = std::result::Result<T, Error>;

/// The single error type every `tomcatrs-*` crate returns.
///
/// Variants are coarse-grained on purpose: each subsystem maps its internal
/// failures onto the closest variant and carries a human-readable message.
#[derive(Debug, Error)]
pub enum Error {
    /// An underlying I/O failure.
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),

    /// `server.xml` / `context.xml` / `web.xml` parsing or validation failure.
    #[error("configuration error: {0}")]
    Config(String),

    /// A component could not complete a lifecycle transition.
    #[error("lifecycle error: {0}")]
    Lifecycle(String),

    /// A connector/protocol level failure (HTTP/1.1, HTTP/2, AJP, TLS).
    #[error("protocol error: {0}")]
    Protocol(String),

    /// A failure crossing the Rust ↔ JVM (JNI) boundary.
    #[error("jvm bridge error: {0}")]
    Bridge(String),

    /// A WAR could not be deployed or scanned.
    #[error("deployment error: {0}")]
    Deployment(String),

    /// The requested host, context, servlet, or resource does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// A request was rejected by a configured security limit.
    #[error("request rejected: {0}")]
    Rejected(String),

    /// Any error that does not fit a more specific variant.
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Build a [`Error::Config`] from anything string-like.
    pub fn config(msg: impl Into<String>) -> Self {
        Error::Config(msg.into())
    }

    /// Build a [`Error::Lifecycle`] from anything string-like.
    pub fn lifecycle(msg: impl Into<String>) -> Self {
        Error::Lifecycle(msg.into())
    }

    /// Build a [`Error::Protocol`] from anything string-like.
    pub fn protocol(msg: impl Into<String>) -> Self {
        Error::Protocol(msg.into())
    }

    /// Build a [`Error::Bridge`] from anything string-like.
    pub fn bridge(msg: impl Into<String>) -> Self {
        Error::Bridge(msg.into())
    }
}
