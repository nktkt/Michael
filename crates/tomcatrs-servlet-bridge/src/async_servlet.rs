//! Servlet 3.0+ asynchronous request support — a real, working `AsyncContext`.
//!
//! When a servlet calls `request.startAsync()` the request is *not* finished
//! when `service()` returns: the container must keep the request and response
//! alive — and the worker thread free — until the application calls
//! `AsyncContext.complete()` (or `AsyncContext.dispatch()`), or the async
//! timeout fires. The bridge models this with an [`AsyncContextState`].
//!
//! # Lifecycle
//!
//! ```text
//!   Dispatched ──startAsync()──▶ Starting ──▶ Started
//!                                               │
//!            ┌──────────────────────────────────┼────────────────────────┐
//!            │                                  │                        │
//!         complete()                       dispatch(path)            timeout
//!            │                                  │                        │
//!            ▼                                  ▼                        ▼
//!        Completing ──▶ Completed         Dispatching ──▶ Dispatched     Timing ──▶ Error
//! ```
//!
//! * `Dispatched` is the synchronous default: the request is being serviced on
//!   a worker thread and is *not* in async mode.
//! * `Starting` is the brief window inside `startAsync()` before the handles
//!   have been pinned; `start()` then moves it to `Started`.
//! * From `Started` the request may be `complete()`d, `dispatch()`ed back onto
//!   a worker thread, or time out.
//! * `Completing`/`Completed` and `Dispatching`/`Dispatched` are the two-step
//!   terminal-ish transitions; `Timing`→`Error` is the timeout path.
//!
//! Illegal transitions are rejected with [`tomcatrs_core::Error::bridge`] by
//! [`AsyncState::transition`].
//!
//! # Pinning the handles
//!
//! [`AsyncContextState`] holds the [`RequestHandle`] / [`ResponseHandle`] (so
//! the `nativeRequestId` / `nativeResponseId` the Java facades carry stay
//! valid for as long as the async request is in flight), a timeout
//! [`Duration`], the start [`Instant`], and a completion signal. It is shared
//! (`Arc`-backed, `Clone`) so the Java `AsyncContext`, the connector, and any
//! application-spawned thread that holds the `AsyncContext` all refer to the
//! same underlying state.
//!
//! # The registry
//!
//! [`AsyncContextRegistry`] is a process-global `DashMap<i64, _>` (behind a
//! `OnceLock`) keyed by the request's `native_id`. The JNI layer
//! (`Java_org_apache_tomcatrs_bridge_NativeAsyncContext_*`) looks an async
//! context up by id to service `complete()` / `dispatch()` / `setTimeout()`
//! calls coming back from Java. The registry API is plain Rust and is
//! available **with or without** the `jvm` feature, so the state machine and
//! its registration logic stay unit-testable on a host with no JDK.
//!
//! # Integration with `invoker.rs` / `dispatch.rs`
//!
//! The async request lifecycle plugs into the synchronous invoker path as
//! follows (these modules are *not* edited here — this is the contract they
//! should implement):
//!
//! 1. Before dispatching, the caller may pre-allocate an [`AsyncContextState`]
//!    or let the Java `startAsync()` natives create and [`register`] one keyed
//!    by the request's `native_id`.
//! 2. After [`crate::invoker::JvmServletInvoker::invoke`] (i.e. after
//!    `ServletDispatcher.dispatch` returns), the caller checks
//!    [`AsyncContextRegistry::lookup`] for the request id. If an entry exists
//!    and [`AsyncContextState::is_async_started`] is `true`, the servlet went
//!    async: the worker thread must **not** serialize the response yet.
//! 3. Instead it `await`s [`AsyncContextState::await_completion`], which
//!    resolves when Java calls `complete()` / `dispatch()` or the timeout
//!    fires, yielding an [`AsyncOutcome`].
//! 4. On [`AsyncOutcome::Completed`] / [`AsyncOutcome::TimedOut`] /
//!    [`AsyncOutcome::Errored`] the caller snapshots the [`ResponseHandle`] and
//!    serializes it, then [`AsyncContextRegistry::unregister`]s the entry. On
//!    [`AsyncOutcome::Dispatched`] it re-routes the request to the recorded
//!    path and dispatches again.
//!
//! In short: `invoke()` stays handle-based and synchronous; the *caller*
//! decides, by consulting the registry, whether to also `await_completion()`
//! before touching the wire.

use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::Notify;
use tomcatrs_core::{Error, Result};

use crate::request_facade::RequestHandle;
use crate::response_facade::ResponseHandle;

/// Lifecycle state of an asynchronous servlet request.
///
/// The default, synchronous state is [`AsyncState::Dispatched`]. `startAsync()`
/// moves the request through [`AsyncState::Starting`] to
/// [`AsyncState::Started`]; from there it either completes
/// ([`AsyncState::Completing`] → [`AsyncState::Completed`]), re-dispatches
/// ([`AsyncState::Dispatching`] → [`AsyncState::Dispatched`]), or times out
/// ([`AsyncState::Timing`] → [`AsyncState::Error`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsyncState {
    /// Synchronous mode — the request is being serviced on a worker thread and
    /// is not (or no longer) in async mode. This is the default state and also
    /// the state a request returns to after `dispatch()` re-routes it.
    Dispatched,
    /// `startAsync()` has been called but the request/response handles have
    /// not yet been pinned into the [`AsyncContextState`].
    Starting,
    /// The request is in async mode: the initiating worker thread has been
    /// released and the container is waiting for `complete()`, `dispatch()`,
    /// or the timeout.
    Started,
    /// `AsyncContext.complete()` was called; the completion signal is being
    /// fired and the response is about to be finalised.
    Completing,
    /// The response is final; the connector may flush it and recycle handles.
    Completed,
    /// `AsyncContext.dispatch(path)` was called; the container is about to
    /// re-dispatch the request to the recorded path on a worker thread.
    Dispatching,
    /// The async timeout elapsed before `complete()` or `dispatch()`; the
    /// container is running the timeout path.
    Timing,
    /// A terminal error state, reached from [`AsyncState::Timing`] (an expired
    /// timeout with no `onTimeout` recovery) or an explicit error transition.
    Error,
}

impl AsyncState {
    /// Whether the request has reached a terminal state and the pinned handles
    /// may be recycled.
    ///
    /// [`AsyncState::Dispatched`] is *not* terminal: it is both the initial
    /// synchronous state and the state a request returns to after a
    /// re-dispatch, so it may still go async again.
    pub fn is_terminal(self) -> bool {
        matches!(self, AsyncState::Completed | AsyncState::Error)
    }

    /// Whether the request is currently in async mode (`startAsync()` has been
    /// called and the request has not yet reached a terminal state or been
    /// re-dispatched).
    pub fn is_async(self) -> bool {
        matches!(
            self,
            AsyncState::Starting
                | AsyncState::Started
                | AsyncState::Completing
                | AsyncState::Dispatching
                | AsyncState::Timing
        )
    }

    /// Attempt the transition `self -> next`, returning the new state on
    /// success or an [`Error::bridge`] describing the illegal transition.
    ///
    /// The legal edges are exactly those drawn in the module diagram:
    ///
    /// * `Dispatched   -> Starting`
    /// * `Starting     -> Started`
    /// * `Started      -> Completing | Dispatching | Timing`
    /// * `Completing   -> Completed`
    /// * `Dispatching  -> Dispatched`
    /// * `Timing       -> Error | Started` (an `onTimeout` listener may revive
    ///   the request by completing or dispatching, modelled as a return to
    ///   `Started`)
    ///
    /// Every other pair is rejected.
    pub fn transition(self, next: AsyncState) -> Result<AsyncState> {
        use AsyncState::*;
        let legal = matches!(
            (self, next),
            (Dispatched, Starting)
                | (Starting, Started)
                | (Started, Completing)
                | (Started, Dispatching)
                | (Started, Timing)
                | (Completing, Completed)
                | (Dispatching, Dispatched)
                | (Timing, Error)
                | (Timing, Started)
        );
        if legal {
            Ok(next)
        } else {
            // Fully-qualified: the `use AsyncState::*` above shadows the
            // `Error` name with the `AsyncState::Error` variant.
            Err(tomcatrs_core::Error::bridge(format!(
                "illegal AsyncContext state transition: {self:?} -> {next:?}"
            )))
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            AsyncState::Dispatched => 0,
            AsyncState::Starting => 1,
            AsyncState::Started => 2,
            AsyncState::Completing => 3,
            AsyncState::Completed => 4,
            AsyncState::Dispatching => 5,
            AsyncState::Timing => 6,
            AsyncState::Error => 7,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            0 => AsyncState::Dispatched,
            1 => AsyncState::Starting,
            2 => AsyncState::Started,
            3 => AsyncState::Completing,
            4 => AsyncState::Completed,
            5 => AsyncState::Dispatching,
            6 => AsyncState::Timing,
            _ => AsyncState::Error,
        }
    }
}

/// The default async timeout, matching Tomcat's `30_000` ms default.
pub const DEFAULT_ASYNC_TIMEOUT: Duration = Duration::from_millis(30_000);

/// The result of awaiting an asynchronous request to leave async mode.
///
/// Returned by [`AsyncContextState::await_completion`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AsyncOutcome {
    /// `AsyncContext.complete()` was called: the response is final and may be
    /// serialized to the wire.
    Completed,
    /// `AsyncContext.dispatch(path)` was called: the request must be
    /// re-dispatched to the carried path on a worker thread.
    Dispatched(String),
    /// The async timeout elapsed before `complete()` or `dispatch()`.
    TimedOut,
    /// The request left async mode in an error state, carrying a description.
    Errored(String),
}

#[derive(Debug)]
struct AsyncInner {
    /// `native_id` of the pinned request handle — the registry key.
    request_id: i64,
    /// `native_id` of the pinned response handle.
    response_id: i64,
    /// The pinned request handle; held so its `nativeRequestId` stays valid.
    request: RequestHandle,
    /// The pinned response handle; held so its `nativeResponseId` stays valid.
    response: ResponseHandle,
    /// Current [`AsyncState`], encoded as a `u8` for lock-free reads from the
    /// JNI callbacks.
    state: AtomicU8,
    /// Configured timeout in milliseconds; `0` disables the timeout.
    timeout_ms: AtomicI64,
    /// When the request entered async mode (set by [`AsyncContextState::start`]).
    started_at: Mutex<Option<Instant>>,
    /// Dispatch target recorded by `AsyncContext.dispatch(path)`, if any.
    dispatch_path: Mutex<Option<String>>,
    /// If the request left async mode in [`AsyncState::Error`], the reason.
    error_message: Mutex<Option<String>>,
    /// Whether the [`AsyncState::Error`] state was reached via the timeout
    /// path (`Started -> Timing -> Error`) rather than a generic error. Lets
    /// [`AsyncContextState::await_completion`] report [`AsyncOutcome::TimedOut`]
    /// distinctly from [`AsyncOutcome::Errored`].
    timed_out: std::sync::atomic::AtomicBool,
    /// Completion signal: fired by `complete()`, `dispatch()`, and the timeout
    /// path. [`AsyncContextState::await_completion`] waits on it.
    signal: Notify,
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
    /// Create a fresh async context for the given request/response pair, in
    /// the synchronous [`AsyncState::Dispatched`] state.
    ///
    /// This does *not* itself enter async mode — call
    /// [`AsyncContextState::start`] for that. It is the Rust analogue of
    /// constructing (but not yet activating) the `AsyncContext`.
    pub fn new(request: RequestHandle, response: ResponseHandle) -> Self {
        let request_id = request.native_id();
        let response_id = response.native_id();
        Self {
            inner: Arc::new(AsyncInner {
                request_id,
                response_id,
                request,
                response,
                state: AtomicU8::new(AsyncState::Dispatched.as_u8()),
                timeout_ms: AtomicI64::new(DEFAULT_ASYNC_TIMEOUT.as_millis() as i64),
                started_at: Mutex::new(None),
                dispatch_path: Mutex::new(None),
                error_message: Mutex::new(None),
                timed_out: std::sync::atomic::AtomicBool::new(false),
                signal: Notify::new(),
            }),
        }
    }

    /// Enter async mode: `Dispatched -> Starting -> Started`, pinning the
    /// request/response ids and arming the timeout clock.
    ///
    /// This is the Rust side of `ServletRequest.startAsync()`. The
    /// `request_id` / `response_id` arguments are validated against the pinned
    /// handles so a mismatched pair is rejected rather than silently accepted.
    ///
    /// # Errors
    ///
    /// Returns [`Error::bridge`] if the context is not in
    /// [`AsyncState::Dispatched`] (e.g. `startAsync()` called twice) or if the
    /// supplied ids do not match the pinned handles.
    pub fn start(&self, request_id: i64, response_id: i64) -> Result<()> {
        if request_id != self.inner.request_id || response_id != self.inner.response_id {
            return Err(Error::bridge(format!(
                "startAsync id mismatch: got ({request_id}, {response_id}), \
                 context pins ({}, {})",
                self.inner.request_id, self.inner.response_id
            )));
        }
        self.advance(AsyncState::Starting)?;
        self.advance(AsyncState::Started)?;
        *self
            .inner
            .started_at
            .lock()
            .expect("started_at mutex poisoned") = Some(Instant::now());
        Ok(())
    }

    /// Complete the async request: `Started -> Completing -> Completed`, firing
    /// the completion signal so [`AsyncContextState::await_completion`] resolves
    /// with [`AsyncOutcome::Completed`].
    ///
    /// This is the Rust side of `AsyncContext.complete()`. Idempotent-ish:
    /// calling it again once the request is already [`AsyncState::Completed`]
    /// is a no-op that returns `Ok(())`, so the connector flushes exactly once.
    ///
    /// # Errors
    ///
    /// Returns [`Error::bridge`] if the request is in a state from which
    /// completion is not legal (e.g. it was never started, or already errored).
    pub fn complete(&self) -> Result<()> {
        if self.state() == AsyncState::Completed {
            return Ok(());
        }
        self.advance(AsyncState::Completing)?;
        self.advance(AsyncState::Completed)?;
        self.inner.signal.notify_waiters();
        Ok(())
    }

    /// Record a re-dispatch target and move the request toward
    /// [`AsyncState::Dispatching`], firing the completion signal so
    /// [`AsyncContextState::await_completion`] resolves with
    /// [`AsyncOutcome::Dispatched`].
    ///
    /// This is the Rust side of `AsyncContext.dispatch(path)`. The caller that
    /// awaited the outcome is then responsible for actually re-routing the
    /// request and, once it has, calling [`AsyncContextState::redispatched`] to
    /// move `Dispatching -> Dispatched`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::bridge`] if the request is not in [`AsyncState::Started`]
    /// (the only state from which a dispatch is legal).
    pub fn dispatch(&self, path: impl Into<String>) -> Result<()> {
        let path = path.into();
        self.advance(AsyncState::Dispatching)?;
        *self
            .inner
            .dispatch_path
            .lock()
            .expect("dispatch_path mutex poisoned") = Some(path);
        self.inner.signal.notify_waiters();
        Ok(())
    }

    /// Finish a re-dispatch: `Dispatching -> Dispatched`. Called by the caller
    /// of [`AsyncContextState::await_completion`] once it has actually
    /// re-routed the request, so the context is ready to (potentially) go
    /// async again.
    ///
    /// # Errors
    ///
    /// Returns [`Error::bridge`] if the request is not in
    /// [`AsyncState::Dispatching`].
    pub fn redispatched(&self) -> Result<()> {
        self.advance(AsyncState::Dispatched)
    }

    /// Drive the timeout path: `Started -> Timing -> Error`, recording the
    /// reason and firing the completion signal so
    /// [`AsyncContextState::await_completion`] resolves with
    /// [`AsyncOutcome::TimedOut`].
    ///
    /// Normally [`AsyncContextState::await_completion`] fires this itself when
    /// its internal timer elapses; it is also exposed so a connector-side
    /// reaper can force a timeout out-of-band.
    ///
    /// # Errors
    ///
    /// Returns [`Error::bridge`] if the request is not in [`AsyncState::Started`].
    pub fn time_out(&self) -> Result<()> {
        self.advance(AsyncState::Timing)?;
        self.advance(AsyncState::Error)?;
        self.inner.timed_out.store(true, Ordering::SeqCst);
        *self
            .inner
            .error_message
            .lock()
            .expect("error_message mutex poisoned") = Some("async request timed out".to_string());
        self.inner.signal.notify_waiters();
        Ok(())
    }

    /// Perform a single checked state transition, storing the new state.
    fn advance(&self, next: AsyncState) -> Result<()> {
        // A short critical section over the atomic: load, validate, store. The
        // `Mutex`-free CAS loop keeps JNI-side reads (`state()`) lock-free.
        loop {
            let current_u8 = self.inner.state.load(Ordering::SeqCst);
            let current = AsyncState::from_u8(current_u8);
            let validated = current.transition(next)?;
            if self
                .inner
                .state
                .compare_exchange(
                    current_u8,
                    validated.as_u8(),
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
            {
                return Ok(());
            }
            // Lost the race; re-read and re-validate.
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

    /// The `nativeRequestId` of the pinned request handle — the registry key.
    pub fn request_id(&self) -> i64 {
        self.inner.request_id
    }

    /// The `nativeResponseId` of the pinned response handle.
    pub fn response_id(&self) -> i64 {
        self.inner.response_id
    }

    /// Current lifecycle state.
    pub fn state(&self) -> AsyncState {
        AsyncState::from_u8(self.inner.state.load(Ordering::SeqCst))
    }

    /// Whether `startAsync()` has been called and the request has not yet
    /// reached a terminal state or been re-dispatched. The Rust side of
    /// `ServletRequest.isAsyncStarted()`.
    pub fn is_async_started(&self) -> bool {
        self.state().is_async()
    }

    /// Whether the request has reached [`AsyncState::Completed`].
    pub fn is_complete(&self) -> bool {
        self.state() == AsyncState::Completed
    }

    /// How long the request has been in async mode, or [`Duration::ZERO`] if it
    /// has not been [`started`](AsyncContextState::start) yet.
    pub fn elapsed(&self) -> Duration {
        self.inner
            .started_at
            .lock()
            .expect("started_at mutex poisoned")
            .map(|t| t.elapsed())
            .unwrap_or(Duration::ZERO)
    }

    /// Configure the async timeout (the Rust side of
    /// `AsyncContext.setTimeout`). A zero duration disables the timeout.
    pub fn set_timeout(&self, timeout: Duration) {
        self.inner
            .timeout_ms
            .store(timeout.as_millis() as i64, Ordering::SeqCst);
    }

    /// The configured async timeout; `None` if timeouts are disabled (`0`).
    pub fn timeout(&self) -> Option<Duration> {
        match self.inner.timeout_ms.load(Ordering::SeqCst) {
            ms if ms <= 0 => None,
            ms => Some(Duration::from_millis(ms as u64)),
        }
    }

    /// The path recorded by the most recent [`AsyncContextState::dispatch`].
    pub fn dispatch_path(&self) -> Option<String> {
        self.inner
            .dispatch_path
            .lock()
            .expect("dispatch_path mutex poisoned")
            .clone()
    }

    /// The error reason if the request left async mode in [`AsyncState::Error`].
    pub fn error_message(&self) -> Option<String> {
        self.inner
            .error_message
            .lock()
            .expect("error_message mutex poisoned")
            .clone()
    }

    /// Whether the request left async mode via the timeout path.
    pub fn is_timed_out(&self) -> bool {
        self.inner.timed_out.load(Ordering::SeqCst)
    }

    /// Map the current (assumed settled) state to an [`AsyncOutcome`].
    fn settled_outcome(&self) -> Option<AsyncOutcome> {
        match self.state() {
            AsyncState::Completed => Some(AsyncOutcome::Completed),
            AsyncState::Dispatching => Some(AsyncOutcome::Dispatched(
                self.dispatch_path().unwrap_or_default(),
            )),
            // The timeout path also ends in `Error`; distinguish it so the
            // caller gets `TimedOut` rather than a generic `Errored`.
            AsyncState::Error if self.is_timed_out() => Some(AsyncOutcome::TimedOut),
            AsyncState::Error => Some(AsyncOutcome::Errored(
                self.error_message()
                    .unwrap_or_else(|| "async request errored".to_string()),
            )),
            _ => None,
        }
    }

    /// Await the request leaving async mode.
    ///
    /// Resolves when [`AsyncContextState::complete`] or
    /// [`AsyncContextState::dispatch`] fires the completion signal, or when the
    /// configured [`timeout`](AsyncContextState::timeout) elapses — whichever
    /// happens first — yielding the corresponding [`AsyncOutcome`].
    ///
    /// This is the call the connector / invoker integration point `await`s
    /// after a servlet has gone async (see the module docs). If the request is
    /// already settled when this is called, it returns immediately. If the
    /// request is not in async mode at all, it returns
    /// [`AsyncOutcome::Completed`] (there is nothing to wait for).
    pub async fn await_completion(&self) -> AsyncOutcome {
        // Already settled, or never went async: nothing to wait for.
        if let Some(outcome) = self.settled_outcome() {
            return outcome;
        }
        if !self.is_async_started() {
            return AsyncOutcome::Completed;
        }

        // Arm the wait on the completion signal *before* checking state again,
        // so a `notify_waiters()` racing in cannot be missed.
        let notified = self.inner.signal.notified();

        // Re-check: `complete()`/`dispatch()` may have fired between the first
        // check and arming the notification.
        if let Some(outcome) = self.settled_outcome() {
            return outcome;
        }

        match self.timeout() {
            Some(limit) => {
                // Account for time already spent in async mode so a long
                // `setTimeout` set after `startAsync` is still honoured
                // relative to the original start.
                let remaining = limit.saturating_sub(self.elapsed());
                tokio::select! {
                    _ = notified => {}
                    _ = tokio::time::sleep(remaining) => {
                        // Timer won the race. Drive the timeout path; if a
                        // concurrent `complete()`/`dispatch()` already moved
                        // the state, `time_out()` fails harmlessly and the
                        // settled outcome below reflects the real winner.
                        let _ = self.time_out();
                    }
                }
            }
            None => {
                // No timeout configured: wait indefinitely for the signal.
                notified.await;
            }
        }

        self.settled_outcome().unwrap_or(AsyncOutcome::Completed)
    }
}

// ---------------------------------------------------------------------------
// Process-global async-context registry — always compiled.
// ---------------------------------------------------------------------------

/// A process-global registry mapping a request's `native_id` to its live
/// [`AsyncContextState`].
///
/// The JNI layer uses this to resolve the opaque `nativeRequestId` a Java
/// `AsyncContext` carries back to the Rust-side state machine. There is exactly
/// one per process; it is populated when a servlet calls `startAsync()` and
/// drained once the async request settles.
///
/// The API is plain Rust and available with or without the `jvm` feature.
#[derive(Debug, Default)]
pub struct AsyncContextRegistry {
    contexts: DashMap<i64, Arc<AsyncContextState>>,
}

impl AsyncContextRegistry {
    /// Register `state` under its [`request_id`](AsyncContextState::request_id),
    /// returning any context previously registered under the same id.
    pub fn register(&self, state: Arc<AsyncContextState>) -> Option<Arc<AsyncContextState>> {
        self.contexts.insert(state.request_id(), state)
    }

    /// Remove and return the async context registered under `request_id`.
    pub fn unregister(&self, request_id: i64) -> Option<Arc<AsyncContextState>> {
        self.contexts.remove(&request_id).map(|(_, v)| v)
    }

    /// Look up the async context registered under `request_id`.
    pub fn lookup(&self, request_id: i64) -> Option<Arc<AsyncContextState>> {
        self.contexts
            .get(&request_id)
            .map(|e| Arc::clone(e.value()))
    }

    /// Number of currently-registered async contexts. Mainly for diagnostics.
    pub fn len(&self) -> usize {
        self.contexts.len()
    }

    /// Whether the registry holds no async contexts.
    pub fn is_empty(&self) -> bool {
        self.contexts.is_empty()
    }
}

/// Backing storage for [`registry`].
static ASYNC_REGISTRY: OnceLock<AsyncContextRegistry> = OnceLock::new();

/// The process-global [`AsyncContextRegistry`], created on first access.
pub fn registry() -> &'static AsyncContextRegistry {
    ASYNC_REGISTRY.get_or_init(AsyncContextRegistry::default)
}

/// Register an async context in the process-global registry, keyed by its
/// request id. Callable without the `jvm` feature.
pub fn register(state: Arc<AsyncContextState>) -> Option<Arc<AsyncContextState>> {
    registry().register(state)
}

/// Remove the async context for `request_id` from the process-global registry.
pub fn unregister(request_id: i64) -> Option<Arc<AsyncContextState>> {
    registry().unregister(request_id)
}

/// Look the async context for `request_id` up in the process-global registry.
pub fn lookup(request_id: i64) -> Option<Arc<AsyncContextState>> {
    registry().lookup(request_id)
}

// ---------------------------------------------------------------------------
// Real JNI entry points — only compiled with `--features jvm`.
// ---------------------------------------------------------------------------
#[cfg(feature = "jvm")]
pub use imp::*;

#[cfg(feature = "jvm")]
mod imp {
    //! `extern "system"` JNI entry points backing the `static native` methods
    //! of `org.apache.tomcatrs.bridge.NativeAsyncContext`.
    //!
    //! Each native takes the `jlong nativeRequestId` a Java
    //! `TomcatRsAsyncContext` carries, resolves the [`AsyncContextState`] via
    //! the process-global [`super::registry`], and drives the state machine.
    //!
    //! Every body runs inside [`std::panic::catch_unwind`]: a panic must never
    //! unwind across the `extern "system"` FFI boundary. On a caught panic — or
    //! a recoverable failure — the shim throws a `java.lang.IllegalStateException`
    //! into the JVM and returns a benign default, mirroring `jni.rs`.

    use std::panic::{catch_unwind, AssertUnwindSafe};

    use jni::objects::{JClass, JString};
    use jni::sys::{jboolean, jlong, JNI_FALSE, JNI_TRUE};
    use jni::JNIEnv;

    use super::{lookup, AsyncContextState, DEFAULT_ASYNC_TIMEOUT};
    use std::time::Duration;

    /// Throw a `java.lang.IllegalStateException` carrying `msg`. Best-effort.
    fn throw_ise(env: &mut JNIEnv, msg: &str) {
        if let Err(e) = env.throw_new("java/lang/IllegalStateException", msg) {
            tracing::error!(error = %e, original = msg, "failed to throw IllegalStateException across JNI");
        }
    }

    /// Render a caught panic payload as a human-readable string.
    fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
        if let Some(s) = payload.downcast_ref::<&str>() {
            (*s).to_owned()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic in NativeAsyncContext bridge method".to_owned()
        }
    }

    /// Run `body` under [`catch_unwind`]; on a panic, throw an
    /// `IllegalStateException` and substitute `default`.
    fn guard<'local, R>(
        env: &mut JNIEnv<'local>,
        what: &str,
        default: R,
        body: impl FnOnce(&mut JNIEnv<'local>) -> R,
    ) -> R {
        match catch_unwind(AssertUnwindSafe(|| body(env))) {
            Ok(value) => value,
            Err(payload) => {
                let msg = format!("panic in {what}: {}", panic_message(&*payload));
                tracing::error!("{msg}");
                throw_ise(env, &msg);
                default
            }
        }
    }

    /// Resolve a `nativeRequestId` to its registered [`AsyncContextState`].
    fn ctx(id: jlong) -> Option<std::sync::Arc<AsyncContextState>> {
        lookup(id)
    }

    /// Read a Java `String` argument into a Rust `String`; a `null` reference
    /// (or any read failure) yields an empty string.
    fn rust_string(env: &mut JNIEnv, value: &JString) -> String {
        env.get_string(value).map(Into::into).unwrap_or_default()
    }

    /// `NativeAsyncContext.nativeStartAsync(long requestId, long responseId) -> boolean`
    ///
    /// Drives `Dispatched -> Starting -> Started` on the context registered
    /// under `requestId`. Returns `true` on success; throws and returns `false`
    /// if no context is registered or the transition is illegal.
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeAsyncContext_nativeStartAsync<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
        response_id: jlong,
    ) -> jboolean {
        guard(&mut env, "nativeStartAsync", JNI_FALSE, |env| {
            let Some(state) = ctx(request_id) else {
                throw_ise(
                    env,
                    &format!("no AsyncContext registered for request {request_id}"),
                );
                return JNI_FALSE;
            };
            match state.start(request_id, response_id) {
                Ok(()) => JNI_TRUE,
                Err(e) => {
                    throw_ise(env, &e.to_string());
                    JNI_FALSE
                }
            }
        })
    }

    /// `NativeAsyncContext.nativeComplete(long requestId)`
    ///
    /// Drives `Started -> Completing -> Completed` and fires the completion
    /// signal so the Rust connector serializes the response.
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeAsyncContext_nativeComplete<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
    ) {
        guard(&mut env, "nativeComplete", (), |env| {
            let Some(state) = ctx(request_id) else {
                throw_ise(
                    env,
                    &format!("no AsyncContext registered for request {request_id}"),
                );
                return;
            };
            if let Err(e) = state.complete() {
                throw_ise(env, &e.to_string());
            }
        })
    }

    /// `NativeAsyncContext.nativeDispatch(long requestId, String path)`
    ///
    /// Records the re-dispatch target and moves the context toward
    /// `Dispatching`, firing the completion signal.
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeAsyncContext_nativeDispatch<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
        path: JString<'local>,
    ) {
        guard(&mut env, "nativeDispatch", (), |env| {
            let path = rust_string(env, &path);
            let Some(state) = ctx(request_id) else {
                throw_ise(
                    env,
                    &format!("no AsyncContext registered for request {request_id}"),
                );
                return;
            };
            if let Err(e) = state.dispatch(path) {
                throw_ise(env, &e.to_string());
            }
        })
    }

    /// `NativeAsyncContext.nativeSetTimeout(long requestId, long timeoutMillis)`
    ///
    /// Sets the async timeout; `0` (or negative) disables it.
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeAsyncContext_nativeSetTimeout<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
        timeout_millis: jlong,
    ) {
        guard(&mut env, "nativeSetTimeout", (), |env| {
            let Some(state) = ctx(request_id) else {
                throw_ise(
                    env,
                    &format!("no AsyncContext registered for request {request_id}"),
                );
                return;
            };
            let millis = timeout_millis.max(0) as u64;
            state.set_timeout(Duration::from_millis(millis));
        })
    }

    /// `NativeAsyncContext.nativeGetTimeout(long requestId) -> long`
    ///
    /// Returns the configured timeout in milliseconds (`0` if disabled), or the
    /// [`DEFAULT_ASYNC_TIMEOUT`] if no context is registered.
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeAsyncContext_nativeGetTimeout<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
    ) -> jlong {
        let default = DEFAULT_ASYNC_TIMEOUT.as_millis() as jlong;
        guard(&mut env, "nativeGetTimeout", default, |_env| {
            match ctx(request_id) {
                Some(state) => state.timeout().map(|d| d.as_millis() as jlong).unwrap_or(0),
                None => default,
            }
        })
    }

    /// `NativeAsyncContext.nativeIsAsyncStarted(long requestId) -> boolean`
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeAsyncContext_nativeIsAsyncStarted<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
    ) -> jboolean {
        guard(
            &mut env,
            "nativeIsAsyncStarted",
            JNI_FALSE,
            |_env| match ctx(request_id) {
                Some(state) if state.is_async_started() => JNI_TRUE,
                _ => JNI_FALSE,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request_facade::RequestParts;

    fn ctx() -> AsyncContextState {
        AsyncContextState::new(
            RequestHandle::new(RequestParts::default()),
            ResponseHandle::new(),
        )
    }

    #[test]
    fn starts_in_dispatched_state() {
        let c = ctx();
        assert_eq!(c.state(), AsyncState::Dispatched);
        assert!(!c.is_async_started());
        assert!(!c.is_complete());
    }

    #[test]
    fn full_transition_path_dispatched_started_completed() {
        let c = ctx();
        assert_eq!(c.state(), AsyncState::Dispatched);
        c.start(c.request_id(), c.response_id())
            .expect("start from Dispatched is legal");
        assert_eq!(c.state(), AsyncState::Started);
        assert!(c.is_async_started());
        c.complete().expect("complete from Started is legal");
        assert_eq!(c.state(), AsyncState::Completed);
        assert!(c.is_complete());
        assert!(c.state().is_terminal());
        // Idempotent.
        c.complete().expect("re-complete is a harmless no-op");
    }

    #[test]
    fn illegal_transition_is_rejected() {
        // Direct state-machine check.
        let err = AsyncState::Dispatched
            .transition(AsyncState::Completed)
            .expect_err("Dispatched -> Completed is illegal");
        match err {
            Error::Bridge(msg) => assert!(msg.contains("illegal AsyncContext state transition")),
            other => panic!("expected Error::Bridge, got {other:?}"),
        }

        // And through the context: complete() before start() is illegal.
        let c = ctx();
        c.complete()
            .expect_err("complete() without start() must be rejected");
        assert_eq!(c.state(), AsyncState::Dispatched);

        // start() with mismatched ids is rejected.
        c.start(c.request_id() + 999, c.response_id())
            .expect_err("mismatched request id must be rejected");
    }

    #[test]
    fn start_twice_is_rejected() {
        let c = ctx();
        c.start(c.request_id(), c.response_id()).unwrap();
        c.start(c.request_id(), c.response_id())
            .expect_err("startAsync twice must be rejected");
    }

    #[tokio::test]
    async fn await_completion_returns_completed_after_complete() {
        let c = ctx();
        c.start(c.request_id(), c.response_id()).unwrap();

        let waiter = c.clone();
        let handle = tokio::spawn(async move { waiter.await_completion().await });

        // Give the waiter a moment to arm, then complete from "another thread".
        tokio::time::sleep(Duration::from_millis(10)).await;
        c.complete().expect("complete is legal");

        let outcome = handle.await.expect("waiter task did not panic");
        assert_eq!(outcome, AsyncOutcome::Completed);
    }

    #[tokio::test]
    async fn await_completion_times_out_when_nothing_completes() {
        let c = ctx();
        c.start(c.request_id(), c.response_id()).unwrap();
        c.set_timeout(Duration::from_millis(20));

        let outcome = c.await_completion().await;
        assert_eq!(outcome, AsyncOutcome::TimedOut);
        assert_eq!(c.state(), AsyncState::Error);
        assert!(c.state().is_terminal());
        assert!(c.error_message().is_some());
    }

    #[tokio::test]
    async fn await_completion_returns_dispatched_with_path() {
        let c = ctx();
        c.start(c.request_id(), c.response_id()).unwrap();

        let waiter = c.clone();
        let handle = tokio::spawn(async move { waiter.await_completion().await });

        tokio::time::sleep(Duration::from_millis(10)).await;
        c.dispatch("/async/result").expect("dispatch is legal");

        let outcome = handle.await.expect("waiter task did not panic");
        assert_eq!(
            outcome,
            AsyncOutcome::Dispatched("/async/result".to_string())
        );
        assert_eq!(c.state(), AsyncState::Dispatching);
        assert_eq!(c.dispatch_path().as_deref(), Some("/async/result"));

        // The caller finishes the re-dispatch.
        c.redispatched()
            .expect("Dispatching -> Dispatched is legal");
        assert_eq!(c.state(), AsyncState::Dispatched);
    }

    #[tokio::test]
    async fn await_completion_is_immediate_when_already_settled() {
        let c = ctx();
        c.start(c.request_id(), c.response_id()).unwrap();
        c.complete().unwrap();
        // Already Completed: must not block.
        assert_eq!(c.await_completion().await, AsyncOutcome::Completed);
    }

    #[tokio::test]
    async fn await_completion_no_timeout_waits_for_signal() {
        let c = ctx();
        c.start(c.request_id(), c.response_id()).unwrap();
        c.set_timeout(Duration::ZERO); // disable the timeout
        assert_eq!(c.timeout(), None);

        let waiter = c.clone();
        let handle = tokio::spawn(async move { waiter.await_completion().await });
        tokio::time::sleep(Duration::from_millis(15)).await;
        c.complete().unwrap();
        assert_eq!(handle.await.unwrap(), AsyncOutcome::Completed);
    }

    #[test]
    fn registry_round_trip() {
        let c = Arc::new(ctx());
        let id = c.request_id();
        let reg = AsyncContextRegistry::default();

        assert!(reg.lookup(id).is_none());
        assert!(reg.is_empty());
        assert!(reg.register(Arc::clone(&c)).is_none());
        assert_eq!(reg.len(), 1);

        let looked_up = reg.lookup(id).expect("just registered");
        assert_eq!(looked_up.request_id(), id);
        assert!(Arc::ptr_eq(&looked_up, &c));

        let removed = reg.unregister(id).expect("entry was present");
        assert_eq!(removed.request_id(), id);
        assert!(reg.lookup(id).is_none());
        assert!(reg.is_empty());
    }

    #[test]
    fn process_global_registry_helpers_round_trip() {
        let c = Arc::new(ctx());
        let id = c.request_id();
        assert!(register(Arc::clone(&c)).is_none());
        assert!(lookup(id).is_some());
        assert!(unregister(id).is_some());
        assert!(lookup(id).is_none());
    }

    #[test]
    fn timeout_configuration() {
        let c = ctx();
        assert_eq!(c.timeout(), Some(DEFAULT_ASYNC_TIMEOUT));
        c.set_timeout(Duration::from_secs(5));
        assert_eq!(c.timeout(), Some(Duration::from_secs(5)));
        c.set_timeout(Duration::ZERO);
        assert_eq!(c.timeout(), None);
    }

    #[test]
    fn handles_are_shared_across_clones() {
        let c = ctx();
        let id = c.request().id();
        let clone = c.clone();
        assert_eq!(clone.request().id(), id);
        clone.start(c.request_id(), c.response_id()).unwrap();
        // The clone shares state: the original observes the transition.
        assert_eq!(c.state(), AsyncState::Started);
    }
}
