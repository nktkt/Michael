//! Servlet 3.0 asynchronous request support.
//!
//! When a servlet calls `request.startAsync()` the request is *not* finished
//! when `service()` returns: the container must keep the request and response
//! alive — and the worker thread free — until the application calls
//! `AsyncContext.complete()` (or a timeout fires).
//!
//! In the bridge, the Java `AsyncContext` implementation is backed by an
//! [`AsyncContextState`]. It pins the Rust [`RequestHandle`] /
//! [`ResponseHandle`] (so the `nativeRequestId` / `nativeResponseId` stay
//! valid) and tracks where the request is in the async lifecycle. The connector
//! polls [`AsyncContextState::state`] to know when it may finally write the
//! response to the wire and recycle the handles.

use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::request_facade::RequestHandle;
use crate::response_facade::ResponseHandle;

/// Lifecycle of an asynchronous servlet request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsyncState {
    /// `startAsync()` has been called; the request is in async mode and the
    /// initiating worker thread has been (or is about to be) released.
    Started,
    /// `AsyncContext.dispatch(...)` was called; the container must
    /// re-dispatch the request to the given path on a worker thread.
    Dispatching,
    /// `AsyncContext.complete()` was called; the response is final and the
    /// connector may flush it and recycle the handles.
    Completed,
    /// The async timeout elapsed before `complete()` or `dispatch()`.
    /// The container runs the timeout / error path.
    TimedOut,
}

impl AsyncState {
    /// Whether the request has reached a terminal state and the handles may be
    /// recycled.
    pub fn is_terminal(self) -> bool {
        matches!(self, AsyncState::Completed | AsyncState::TimedOut)
    }

    fn as_u8(self) -> u8 {
        match self {
            AsyncState::Started => 0,
            AsyncState::Dispatching => 1,
            AsyncState::Completed => 2,
            AsyncState::TimedOut => 3,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            0 => AsyncState::Started,
            1 => AsyncState::Dispatching,
            2 => AsyncState::Completed,
            _ => AsyncState::TimedOut,
        }
    }
}

/// The default async timeout, matching Tomcat's `30_000` ms default.
pub const DEFAULT_ASYNC_TIMEOUT: Duration = Duration::from_millis(30_000);

#[derive(Debug)]
struct AsyncInner {
    request: RequestHandle,
    response: ResponseHandle,
    /// Current [`AsyncState`], encoded as a `u8` for lock-free access from the
    /// JNI callbacks.
    state: AtomicU8,
    /// Configured timeout in milliseconds; `0` disables the timeout.
    timeout_ms: AtomicI64,
    /// When the request entered async mode.
    started_at: Instant,
    /// Dispatch target set by `AsyncContext.dispatch(path)`, if any.
    dispatch_path: std::sync::Mutex<Option<String>>,
}

/// Shared, cloneable state tracking one in-flight asynchronous servlet request.
///
/// Cloning shares the same underlying state. The Java `AsyncContext`, the
/// connector, and any application-spawned thread that holds the `AsyncContext`
/// all refer to the same [`AsyncInner`].
#[derive(Debug, Clone)]
pub struct AsyncContextState {
    inner: Arc<AsyncInner>,
}

impl AsyncContextState {
    /// Enter async mode for the given request/response pair. Equivalent to the
    /// Rust side of `ServletRequest.startAsync()`.
    pub fn start(request: RequestHandle, response: ResponseHandle) -> Self {
        Self {
            inner: Arc::new(AsyncInner {
                request,
                response,
                state: AtomicU8::new(AsyncState::Started.as_u8()),
                timeout_ms: AtomicI64::new(DEFAULT_ASYNC_TIMEOUT.as_millis() as i64),
                started_at: Instant::now(),
                dispatch_path: std::sync::Mutex::new(None),
            }),
        }
    }

    /// The pinned request handle.
    pub fn request(&self) -> &RequestHandle {
        &self.inner.request
    }

    /// The pinned response handle.
    pub fn response(&self) -> &ResponseHandle {
        &self.inner.response
    }

    /// Current lifecycle state.
    pub fn state(&self) -> AsyncState {
        AsyncState::from_u8(self.inner.state.load(Ordering::SeqCst))
    }

    /// How long the request has been in async mode.
    pub fn elapsed(&self) -> Duration {
        self.inner.started_at.elapsed()
    }

    /// Configure the async timeout (the Rust side of
    /// `AsyncContext.setTimeout`). A zero duration disables the timeout.
    pub fn set_timeout(&self, timeout: Duration) {
        self.inner
            .timeout_ms
            .store(timeout.as_millis() as i64, Ordering::SeqCst);
    }

    /// The configured async timeout; `None` if timeouts are disabled.
    pub fn timeout(&self) -> Option<Duration> {
        match self.inner.timeout_ms.load(Ordering::SeqCst) {
            0 => None,
            ms => Some(Duration::from_millis(ms as u64)),
        }
    }

    /// Whether the timeout has elapsed *and* the request is not already in a
    /// terminal state. The connector's reaper calls this and, when it returns
    /// `true`, invokes [`AsyncContextState::time_out`].
    pub fn is_timed_out(&self) -> bool {
        if self.state().is_terminal() {
            return false;
        }
        match self.timeout() {
            Some(limit) => self.elapsed() >= limit,
            None => false,
        }
    }

    /// Transition to [`AsyncState::Dispatching`] with the given target path
    /// (the Rust side of `AsyncContext.dispatch(path)`). Returns `false` if the
    /// request was already terminal.
    pub fn dispatch(&self, path: impl Into<String>) -> bool {
        if self.state().is_terminal() {
            return false;
        }
        *self
            .inner
            .dispatch_path
            .lock()
            .expect("dispatch_path mutex poisoned") = Some(path.into());
        self.inner
            .state
            .store(AsyncState::Dispatching.as_u8(), Ordering::SeqCst);
        true
    }

    /// The path set by the most recent [`AsyncContextState::dispatch`] call.
    pub fn dispatch_path(&self) -> Option<String> {
        self.inner
            .dispatch_path
            .lock()
            .expect("dispatch_path mutex poisoned")
            .clone()
    }

    /// Transition to [`AsyncState::Completed`] (the Rust side of
    /// `AsyncContext.complete()`). Idempotent-ish: returns `false` if the
    /// request was already terminal, so the connector flushes exactly once.
    pub fn complete(&self) -> bool {
        self.try_terminate(AsyncState::Completed)
    }

    /// Transition to [`AsyncState::TimedOut`]. Returns `false` if the request
    /// already reached a terminal state.
    pub fn time_out(&self) -> bool {
        self.try_terminate(AsyncState::TimedOut)
    }

    fn try_terminate(&self, target: AsyncState) -> bool {
        debug_assert!(target.is_terminal());
        loop {
            let current = self.inner.state.load(Ordering::SeqCst);
            if AsyncState::from_u8(current).is_terminal() {
                return false;
            }
            if self
                .inner
                .state
                .compare_exchange(current, target.as_u8(), Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request_facade::RequestParts;

    fn ctx() -> AsyncContextState {
        AsyncContextState::start(
            RequestHandle::new(RequestParts::default()),
            ResponseHandle::new(),
        )
    }

    #[test]
    fn starts_in_started_state() {
        let c = ctx();
        assert_eq!(c.state(), AsyncState::Started);
        assert!(!c.state().is_terminal());
    }

    #[test]
    fn complete_is_terminal_and_once() {
        let c = ctx();
        assert!(c.complete());
        assert_eq!(c.state(), AsyncState::Completed);
        assert!(c.state().is_terminal());
        assert!(!c.complete());
        // Cannot time out after completion.
        assert!(!c.time_out());
    }

    #[test]
    fn dispatch_sets_path_and_state() {
        let c = ctx();
        assert!(c.dispatch("/async/result"));
        assert_eq!(c.state(), AsyncState::Dispatching);
        assert_eq!(c.dispatch_path().as_deref(), Some("/async/result"));
        // Still completable after dispatch.
        assert!(c.complete());
        assert!(!c.dispatch("/too/late"));
    }

    #[test]
    fn timeout_logic() {
        let c = ctx();
        assert!(c.timeout().is_some());
        c.set_timeout(Duration::ZERO);
        assert_eq!(c.timeout(), None);
        assert!(!c.is_timed_out());

        c.set_timeout(Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(3));
        assert!(c.is_timed_out());
        assert!(c.time_out());
        assert_eq!(c.state(), AsyncState::TimedOut);
        // Terminal: no longer reports timed out.
        assert!(!c.is_timed_out());
    }

    #[test]
    fn handles_are_shared() {
        let c = ctx();
        let id = c.request().id();
        let clone = c.clone();
        assert_eq!(clone.request().id(), id);
        clone.complete();
        assert_eq!(c.state(), AsyncState::Completed);
    }
}
