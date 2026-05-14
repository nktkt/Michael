//! Shared interior-mutable lifecycle-state cell.
//!
//! The [`tomcatrs_core::Lifecycle`] trait takes `&self`, so every Catalina
//! container needs interior mutability to record which
//! [`LifecycleState`] it currently occupies. [`StateCell`] is the small,
//! cheap wrapper used uniformly across `Server`, `Service`, `Engine`, `Host`,
//! `Context` and `Wrapper`.

use parking_lot::Mutex;
use tomcatrs_core::LifecycleState;

/// A thread-safe cell holding a component's current [`LifecycleState`].
///
/// Backed by a [`parking_lot::Mutex`]; reads and writes are short and
/// non-blocking, so lock contention is a non-issue in practice.
#[derive(Debug)]
pub struct StateCell {
    inner: Mutex<LifecycleState>,
}

impl StateCell {
    /// Create a cell in the [`LifecycleState::New`] state.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(LifecycleState::New),
        }
    }

    /// Return the current state.
    pub fn get(&self) -> LifecycleState {
        *self.inner.lock()
    }

    /// Overwrite the current state, returning the previous one.
    pub fn set(&self, state: LifecycleState) -> LifecycleState {
        let mut guard = self.inner.lock();
        std::mem::replace(&mut *guard, state)
    }

    /// `true` when the component is in [`LifecycleState::Started`].
    pub fn is_available(&self) -> bool {
        self.get().is_available()
    }
}

impl Default for StateCell {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_in_new_and_transitions() {
        let cell = StateCell::new();
        assert_eq!(cell.get(), LifecycleState::New);
        let prev = cell.set(LifecycleState::Initialized);
        assert_eq!(prev, LifecycleState::New);
        assert_eq!(cell.get(), LifecycleState::Initialized);
        assert!(!cell.is_available());
        cell.set(LifecycleState::Started);
        assert!(cell.is_available());
    }
}
