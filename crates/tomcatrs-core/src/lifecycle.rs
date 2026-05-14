//! The lifecycle state machine shared by every container component.
//!
//! Apache Tomcat models `Server`, `Service`, `Engine`, `Host`, `Context` and
//! `Wrapper` as components that all move through the same set of states. The
//! Rust port keeps that contract: anything that can be started and stopped
//! implements [`Lifecycle`].

use async_trait::async_trait;

use crate::error::Result;

/// The discrete states a [`Lifecycle`] component can occupy.
///
/// Legal transitions follow Tomcat's model:
/// `New → Initialized → Starting → Started → Stopping → Stopped → Destroyed`,
/// with `Failed` reachable from any state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LifecycleState {
    /// Constructed but not yet initialized.
    New,
    /// `init()` completed successfully.
    Initialized,
    /// `start()` is in progress.
    Starting,
    /// `start()` completed; the component is serving.
    Started,
    /// `stop()` is in progress.
    Stopping,
    /// `stop()` completed; the component is idle but still initialized.
    Stopped,
    /// `destroy()` completed; the component must not be reused.
    Destroyed,
    /// A lifecycle transition failed.
    Failed,
}

impl LifecycleState {
    /// Returns `true` only in the [`Started`](LifecycleState::Started) state,
    /// i.e. when the component is able to serve traffic.
    pub fn is_available(self) -> bool {
        matches!(self, LifecycleState::Started)
    }

    /// Returns `true` if the component has reached a terminal state.
    pub fn is_terminal(self) -> bool {
        matches!(self, LifecycleState::Destroyed | LifecycleState::Failed)
    }
}

/// Context handed to every lifecycle callback.
///
/// It is deliberately small for v0.1.0 — it carries the component name used in
/// log lines. Future versions will thread shared runtime state through here.
#[derive(Debug, Clone)]
pub struct LifecycleContext {
    /// The name of the component being transitioned (e.g. `"Catalina/Engine"`).
    pub name: String,
}

impl LifecycleContext {
    /// Create a context for a named component.
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

/// The common contract for startable/stoppable container components.
///
/// Implementations should be idempotent where reasonable and must leave the
/// component in [`LifecycleState::Failed`] if a transition cannot complete.
#[async_trait]
pub trait Lifecycle: Send + Sync {
    /// Allocate resources and validate configuration. Moves `New → Initialized`.
    async fn init(&self, ctx: &LifecycleContext) -> Result<()>;

    /// Begin serving. Moves `Initialized | Stopped → Started`.
    async fn start(&self, ctx: &LifecycleContext) -> Result<()>;

    /// Stop serving but keep resources. Moves `Started → Stopped`.
    async fn stop(&self, ctx: &LifecycleContext) -> Result<()>;

    /// Release all resources. Moves `Stopped → Destroyed`.
    async fn destroy(&self, ctx: &LifecycleContext) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_is_started_only() {
        assert!(LifecycleState::Started.is_available());
        assert!(!LifecycleState::Stopped.is_available());
        assert!(!LifecycleState::New.is_available());
    }

    #[test]
    fn terminal_states() {
        assert!(LifecycleState::Destroyed.is_terminal());
        assert!(LifecycleState::Failed.is_terminal());
        assert!(!LifecycleState::Started.is_terminal());
    }
}
