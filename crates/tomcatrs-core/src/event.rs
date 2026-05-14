//! Lifecycle events and listeners.
//!
//! Mirrors Tomcat's `LifecycleListener` mechanism: components emit
//! [`LifecycleEvent`]s around each transition, and registered
//! [`LifecycleListener`]s react to them (logging, JMX registration, metrics).

/// An event emitted by a component around a lifecycle transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleEvent {
    /// Emitted immediately before `init()`.
    BeforeInit,
    /// Emitted immediately after a successful `init()`.
    AfterInit,
    /// Emitted immediately before `start()`.
    BeforeStart,
    /// Emitted immediately after a successful `start()`.
    AfterStart,
    /// Emitted immediately before `stop()`.
    BeforeStop,
    /// Emitted immediately after a successful `stop()`.
    AfterStop,
    /// Emitted immediately before `destroy()`.
    BeforeDestroy,
    /// Emitted immediately after a successful `destroy()`.
    AfterDestroy,
    /// Emitted on the background processing tick.
    Periodic,
    /// Emitted when any transition fails.
    Failed,
}

impl LifecycleEvent {
    /// A stable, lowercase string name for the event (used in logs/JMX).
    pub fn as_str(self) -> &'static str {
        match self {
            LifecycleEvent::BeforeInit => "before_init",
            LifecycleEvent::AfterInit => "after_init",
            LifecycleEvent::BeforeStart => "before_start",
            LifecycleEvent::AfterStart => "after_start",
            LifecycleEvent::BeforeStop => "before_stop",
            LifecycleEvent::AfterStop => "after_stop",
            LifecycleEvent::BeforeDestroy => "before_destroy",
            LifecycleEvent::AfterDestroy => "after_destroy",
            LifecycleEvent::Periodic => "periodic",
            LifecycleEvent::Failed => "failed",
        }
    }
}

/// A reactor for [`LifecycleEvent`]s.
///
/// Listeners must be cheap and non-blocking; long-running work should be
/// dispatched onto the async runtime instead of executed inline.
pub trait LifecycleListener: Send + Sync {
    /// Handle a single lifecycle event for the named component.
    fn on_event(&self, component: &str, event: LifecycleEvent);
}

/// A no-op listener, useful as a default and in tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopListener;

impl LifecycleListener for NoopListener {
    fn on_event(&self, _component: &str, _event: LifecycleEvent) {}
}
