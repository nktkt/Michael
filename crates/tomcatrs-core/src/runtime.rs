//! Shared identifier types and the global [`Runtime`] handle.

use std::sync::Arc;
use std::time::{Duration, Instant};

/// Identifies a deployed web application (`Context`), e.g. `"/myapp"`.
pub type ContextId = String;

/// Identifies a servlet registration (`Wrapper`) within a context.
pub type WrapperId = String;

/// Identifies an HTTP session.
pub type SessionId = String;

/// A cheaply-cloneable handle to process-wide runtime state.
///
/// Every long-lived component holds a `Runtime` so it can report uptime and a
/// consistent server identity without threading globals everywhere.
#[derive(Clone)]
pub struct Runtime {
    inner: Arc<RuntimeInner>,
}

struct RuntimeInner {
    server_info: String,
    started_at: Instant,
}

impl Runtime {
    /// Create a fresh runtime handle, stamping the start time as "now".
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RuntimeInner {
                server_info: format!("Tomcat-RS/{}", crate::VERSION),
                started_at: Instant::now(),
            }),
        }
    }

    /// The value reported in the `Server:` response header.
    pub fn server_info(&self) -> &str {
        &self.inner.server_info
    }

    /// Wall-clock time since this runtime handle was created.
    pub fn uptime(&self) -> Duration {
        self.inner.started_at.elapsed()
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_info_contains_version() {
        let rt = Runtime::new();
        assert!(rt.server_info().starts_with("Tomcat-RS/"));
        assert!(rt.uptime() < Duration::from_secs(1));
    }
}
