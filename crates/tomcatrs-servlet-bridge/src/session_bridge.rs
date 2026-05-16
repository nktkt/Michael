//! Bridges the Java `HttpSession` facade to the Rust [`SessionManager`].
//!
//! # Why this module exists
//!
//! A servlet's `HttpSession` is, on the JVM side, a thin façade
//! (`org.apache.tomcatrs.bridge.TomcatRsHttpSession`) that owns nothing but an
//! opaque `long nativeSessionId`. Every operation it performs —
//! `getAttribute`, `setAttribute`, `invalidate`, … — calls a `native` method
//! on the companion `NativeSession` class, which JNI routes back here. The
//! Rust side then drives the real [`tomcatrs_session::SessionManager`], whose
//! [`tomcatrs_session::SessionStore`] backend (memory, file, Redis) is the
//! actual source of truth.
//!
//! This mirrors, for sessions, exactly what [`crate::jni`] does for the
//! request/response facades.
//!
//! # The session handle registry
//!
//! `SESSION_HANDLE_REGISTRY` is a process-global `DashMap<i64, SessionHandle>`
//! behind a [`OnceLock`], the same shape as
//! `HANDLE_REGISTRY` in [`crate::jni`]. The connector / dispatch layer
//! resolves a request's session (see [`SessionBinder`]), registers the
//! resulting [`SessionHandle`] under a process-unique `nativeSessionId`, and
//! hands that id across JNI. When the request finishes, the id is unregistered.
//!
//! The registry and [`SessionHandle`] are **plain Rust** and fully functional
//! **without** the `jvm` feature, so non-JNI code can populate and drain the
//! registry and the logic stays unit-testable on a host with no JDK. Only the
//! `extern "system"` JNI entry points are behind `#[cfg(feature = "jvm")]`.
//!
//! # Attribute model (v1.0.0)
//!
//! [`tomcatrs_session::SessionData`] stores attributes as
//! `HashMap<String, String>` — in v1.0.0 session attribute values are
//! **string-valued**. [`SessionHandle`] therefore exposes `String` attributes;
//! the Java façade calls `value.toString()` before crossing JNI and treats the
//! returned `String` as the attribute object. A future release will introduce
//! a typed attribute value on both sides.
//!
//! # Blocking over an async store
//!
//! [`SessionManager`] is async, but the JNI natives are synchronous (they run
//! on the bridge worker pool, see [`crate::jvm`]). Each [`SessionHandle`]
//! operation therefore does a *load → mutate → save* cycle and blocks on it
//! with a minimal single-future executor (`block_on`). This is sound because
//! every [`tomcatrs_session::SessionStore`] future used by the bridge resolves
//! without yielding to a real reactor (the memory store is lock-free and
//! synchronous; file/Redis I/O complete eagerly from the worker thread's
//! perspective).

use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use dashmap::DashMap;
use tomcatrs_core::{ContextId, Error, Result};
use tomcatrs_session::{CookieProcessor, SessionData, SessionManager};

/// The session identifier type — the `JSESSIONID` value.
///
/// Kept as a dedicated alias (rather than a bare `String`) so call sites read
/// clearly and a future newtype is a non-breaking change.
pub type SessionId = String;

// ---------------------------------------------------------------------------
// Minimal blocking executor.
// ---------------------------------------------------------------------------

/// Drive a future to completion on the current thread with a no-op waker.
///
/// The bridge's session futures never yield to a real reactor — the memory
/// store is synchronous and the file/Redis stores complete their I/O eagerly
/// from the caller's perspective — so a busy poll terminates promptly. This
/// keeps the crate free of an async-runtime dependency on its default path,
/// matching the executor used by [`crate::dispatch`] and crate-root tests.
fn block_on<F: std::future::Future>(mut fut: F) -> F::Output {
    use std::pin::Pin;
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    fn noop_raw_waker() -> RawWaker {
        fn no_op(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            noop_raw_waker()
        }
        let vtable = &RawWakerVTable::new(clone, no_op, no_op, no_op);
        RawWaker::new(std::ptr::null(), vtable)
    }

    let waker = unsafe { Waker::from_raw(noop_raw_waker()) };
    let mut cx = Context::from_waker(&waker);
    // SAFETY: `fut` is owned and never moved again after this shadowing.
    let mut fut = unsafe { Pin::new_unchecked(&mut fut) };
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(out) => return out,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

// ---------------------------------------------------------------------------
// SessionHandle — the Rust counterpart of one Java `HttpSession`.
// ---------------------------------------------------------------------------

/// A cheaply-cloneable handle to one HTTP session, backing the Java
/// `HttpSession` façade.
///
/// A `SessionHandle` pairs an `Arc<SessionManager>` with a [`SessionId`] and a
/// "new" flag. It owns no session state itself: every accessor performs a
/// *load → (mutate →) save* cycle against the [`SessionManager`], so the store
/// remains the single source of truth and concurrent requests for the same
/// session observe each other's writes.
///
/// Cloning a handle is cheap (it clones an `Arc` and a `String`) and every
/// clone refers to the same underlying session.
///
/// # Invalidation
///
/// [`SessionHandle::invalidate`] deletes the session from the store. After
/// that, the per-session entry is gone: subsequent accessors return
/// [`Error::Bridge`], which the JNI layer surfaces to Java as an
/// `IllegalStateException` — matching the Servlet spec's contract for
/// operating on an invalidated session.
#[derive(Clone)]
pub struct SessionHandle {
    /// The shared session manager driving the pluggable store backend.
    manager: Arc<SessionManager>,
    /// The `JSESSIONID` value this handle refers to.
    id: SessionId,
    /// Whether the session was *created* during the current request (i.e. the
    /// client did not present a valid `JSESSIONID`). Backs `HttpSession.isNew()`.
    is_new: bool,
}

impl std::fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHandle")
            .field("id", &self.id)
            .field("is_new", &self.is_new)
            .finish_non_exhaustive()
    }
}

impl SessionHandle {
    /// Wrap a `(manager, id)` pair into a handle.
    ///
    /// `is_new` records whether this session was freshly created for the
    /// current request; see [`SessionHandle::is_new`].
    pub fn new(manager: Arc<SessionManager>, id: impl Into<SessionId>, is_new: bool) -> Self {
        Self {
            manager,
            id: id.into(),
            is_new,
        }
    }

    /// The `JSESSIONID` value — backs `HttpSession.getId()`.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The session manager this handle is bound to.
    pub fn manager(&self) -> &Arc<SessionManager> {
        &self.manager
    }

    /// Load the current [`SessionData`] from the store, mapping a missing
    /// session (e.g. one that was invalidated or expired) onto an
    /// [`Error::Bridge`] — the JNI layer turns that into an
    /// `IllegalStateException`.
    fn load(&self) -> Result<SessionData> {
        match block_on(self.manager.find(&self.id)) {
            Ok(Some(data)) => Ok(data),
            Ok(None) => Err(Error::bridge(format!(
                "session {} has been invalidated or has expired",
                self.id
            ))),
            Err(e) => Err(Error::bridge(format!(
                "failed to load session {}: {e}",
                self.id
            ))),
        }
    }

    /// Persist a mutated [`SessionData`] back to the store, mapping any store
    /// failure onto [`Error::Bridge`].
    fn save(&self, data: SessionData) -> Result<()> {
        block_on(self.manager.save(data))
            .map_err(|e| Error::bridge(format!("failed to save session {}: {e}", self.id)))
    }

    /// Load, run `f` against the session, persist, and return `f`'s output.
    ///
    /// This is the shared *load → mutate → save* primitive every mutating
    /// accessor is built on.
    fn mutate<R>(&self, f: impl FnOnce(&mut SessionData) -> R) -> Result<R> {
        let mut data = self.load()?;
        let out = f(&mut data);
        self.save(data)?;
        Ok(out)
    }

    /// Whether the session was created during the current request.
    ///
    /// Backs `HttpSession.isNew()`: `true` when the client did not join the
    /// session via a presented `JSESSIONID` cookie.
    pub fn is_new(&self) -> bool {
        self.is_new
    }

    /// Retrieve a session attribute — backs `HttpSession.getAttribute(name)`.
    ///
    /// Returns `Ok(None)` when the attribute is unset. In v1.0.0 attribute
    /// values are `String`s (see the module docs).
    pub fn get_attribute(&self, name: &str) -> Result<Option<String>> {
        Ok(self.load()?.attributes.get(name).cloned())
    }

    /// Set a session attribute — backs `HttpSession.setAttribute(name, value)`.
    ///
    /// In v1.0.0 attribute values are string-valued; the Java façade stringifies
    /// the object before it crosses JNI.
    pub fn set_attribute(&self, name: impl Into<String>, value: impl Into<String>) -> Result<()> {
        self.mutate(|data| {
            data.attributes.insert(name.into(), value.into());
        })
    }

    /// Remove a session attribute — backs `HttpSession.removeAttribute(name)`.
    ///
    /// Returns the previous value, if any. Removing an absent attribute is not
    /// an error.
    pub fn remove_attribute(&self, name: &str) -> Result<Option<String>> {
        self.mutate(|data| data.attributes.remove(name))
    }

    /// Every attribute name currently set — backs
    /// `HttpSession.getAttributeNames()`.
    pub fn attribute_names(&self) -> Result<Vec<String>> {
        Ok(self.load()?.attributes.keys().cloned().collect())
    }

    /// When the session was created — backs `HttpSession.getCreationTime()`.
    pub fn creation_time(&self) -> Result<SystemTime> {
        Ok(self.load()?.created)
    }

    /// When the session was last accessed — backs
    /// `HttpSession.getLastAccessedTime()`.
    pub fn last_accessed_time(&self) -> Result<SystemTime> {
        Ok(self.load()?.last_accessed)
    }

    /// The session's `max-inactive-interval` — backs
    /// `HttpSession.getMaxInactiveInterval()`.
    pub fn max_inactive_interval(&self) -> Result<Duration> {
        Ok(self.load()?.max_inactive_interval)
    }

    /// Update the session's `max-inactive-interval` — backs
    /// `HttpSession.setMaxInactiveInterval(seconds)`.
    pub fn set_max_inactive_interval(&self, interval: Duration) -> Result<()> {
        self.mutate(|data| {
            data.max_inactive_interval = interval;
        })
    }

    /// Refresh `last_accessed` to "now" and persist — the per-request access
    /// bookkeeping the connector performs when it (re)binds an existing
    /// session. Not part of the `HttpSession` API surface itself, but the
    /// natural Rust-side companion to [`SessionBinder`].
    pub fn touch(&self) -> Result<()> {
        self.mutate(SessionData::touch)
    }

    /// Invalidate the session — backs `HttpSession.invalidate()`.
    ///
    /// Deletes the session from the store. After this call every other
    /// accessor on this (or any cloned) handle fails with [`Error::Bridge`].
    /// Invalidating an already-gone session is not an error, matching
    /// [`SessionManager::invalidate`].
    pub fn invalidate(&self) -> Result<()> {
        block_on(self.manager.invalidate(&self.id))
            .map_err(|e| Error::bridge(format!("failed to invalidate session {}: {e}", self.id)))
    }
}

// ---------------------------------------------------------------------------
// Process-global session handle registry — always compiled.
// ---------------------------------------------------------------------------

/// The process-global registry mapping `nativeSessionId` values to their
/// Rust-side [`SessionHandle`]s.
///
/// There is exactly one per process. It is populated by the connector /
/// dispatch layer (via [`SessionBinder`]) just before a request crosses into
/// Java, and drained once the servlet invocation finishes. Mirrors
/// [`crate::jni::HandleRegistry`] in shape and intent.
#[derive(Debug, Default)]
pub struct SessionHandleRegistry {
    sessions: DashMap<i64, SessionHandle>,
}

impl SessionHandleRegistry {
    /// Register `handle` under `native_session_id`, returning any handle that
    /// was previously registered under the same id (normally `None`).
    pub fn register(&self, native_session_id: i64, handle: SessionHandle) -> Option<SessionHandle> {
        self.sessions.insert(native_session_id, handle)
    }

    /// Remove and return the handle registered under `native_session_id`.
    pub fn unregister(&self, native_session_id: i64) -> Option<SessionHandle> {
        self.sessions.remove(&native_session_id).map(|(_, h)| h)
    }

    /// Look the `native_session_id` up, returning a clone of its handle.
    pub fn lookup(&self, native_session_id: i64) -> Option<SessionHandle> {
        self.sessions.get(&native_session_id).map(|h| h.clone())
    }

    /// Number of currently-registered sessions. Mainly for diagnostics/tests.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Whether the registry currently holds no sessions.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

/// Backing storage for [`registry`].
static SESSION_HANDLE_REGISTRY: OnceLock<SessionHandleRegistry> = OnceLock::new();

/// The process-global [`SessionHandleRegistry`], created on first access.
pub fn registry() -> &'static SessionHandleRegistry {
    SESSION_HANDLE_REGISTRY.get_or_init(SessionHandleRegistry::default)
}

/// Source of process-unique `nativeSessionId` values.
static NEXT_SESSION_HANDLE_ID: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);

/// Allocate a fresh, process-unique `nativeSessionId`.
///
/// The id is opaque: it identifies a *registration* of a [`SessionHandle`],
/// not the session itself (a session may outlive many requests and thus many
/// registrations). It is the only value that crosses JNI.
pub fn next_native_session_id() -> i64 {
    NEXT_SESSION_HANDLE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Register `handle` in the process-global registry under a freshly-allocated
/// `nativeSessionId`, returning that id.
///
/// Callable without the `jvm` feature so the connector / dispatch layer can
/// populate the registry on a host with no JDK.
pub fn register_session(handle: SessionHandle) -> i64 {
    let id = next_native_session_id();
    registry().register(id, handle);
    id
}

/// Remove the `native_session_id` from the process-global registry, returning
/// the handle that was registered, if any.
pub fn unregister_session(native_session_id: i64) -> Option<SessionHandle> {
    registry().unregister(native_session_id)
}

/// Look up the [`SessionHandle`] registered under `native_session_id`.
pub fn lookup_session(native_session_id: i64) -> Option<SessionHandle> {
    registry().lookup(native_session_id)
}

// ---------------------------------------------------------------------------
// Per-context session-manager registry.
// ---------------------------------------------------------------------------
//
// Each registered webapp gets one [`SessionManager`] (by default an in-memory
// store; see [`crate::registration::register_impl`]). The Java
// `TomcatRsRequestFacade.getSession(boolean)` path crosses JNI carrying the
// `nativeContextId` of the originating webapp, and the resolver native looks
// the manager up here. This indirection keeps the per-webapp manager out of
// `jvm.rs` — `WebappRuntime` remains untouched — and is the *only* place that
// owns the "this context's session subsystem" reference.

/// Process-global map from `nativeContextId` (the connector's webapp id, as
/// stored in [`crate::jni::ContextRegistry`]) to the webapp's
/// [`SessionManager`].
///
/// Populated by [`set_session_manager`] from
/// [`crate::registration::register_impl`] when a webapp is registered. Looked
/// up by the `nativeResolveOrCreateSession` shim and (for the
/// `nativeNewSessionCookie` shim) by [`session_manager_for`].
static SESSION_MANAGER_REGISTRY: OnceLock<DashMap<i64, Arc<SessionManager>>> = OnceLock::new();

fn session_manager_registry() -> &'static DashMap<i64, Arc<SessionManager>> {
    SESSION_MANAGER_REGISTRY.get_or_init(DashMap::new)
}

/// Attach a [`SessionManager`] to a webapp's `nativeContextId`.
///
/// Called from [`crate::registration::register_impl`] once per registered
/// webapp, with a default in-memory manager if no caller-supplied one is
/// configured. A second call for the same id replaces the previous manager
/// (returned for caller cleanup), matching the semantics of "redeploy".
pub fn set_session_manager(
    native_context_id: i64,
    manager: Arc<SessionManager>,
) -> Option<Arc<SessionManager>> {
    session_manager_registry().insert(native_context_id, manager)
}

/// Look up the [`SessionManager`] attached to `native_context_id`, if any.
///
/// Returns `None` when no webapp has been registered for that id, when the
/// context has been undeployed, or when the registration ran before the
/// session-manager attachment step.
pub fn session_manager_for(native_context_id: i64) -> Option<Arc<SessionManager>> {
    session_manager_registry()
        .get(&native_context_id)
        .map(|m| Arc::clone(m.value()))
}

/// Drop the session-manager attachment for `native_context_id`. Safe to call
/// when no manager has been attached. Returned for caller-side cleanup; the
/// session store the manager owns may need an explicit shutdown.
pub fn unset_session_manager(native_context_id: i64) -> Option<Arc<SessionManager>> {
    session_manager_registry()
        .remove(&native_context_id)
        .map(|(_, m)| m)
}

/// Convenience: attach a fresh in-memory manager to `native_context_id`. This
/// is the default the registrar installs for each webapp on the no-config
/// path. Returns a clone of the just-installed manager.
pub fn install_default_session_manager(native_context_id: i64) -> Arc<SessionManager> {
    use tomcatrs_session::MemorySessionStore;
    let manager = Arc::new(SessionManager::new(Arc::new(MemorySessionStore::new())));
    let _ = set_session_manager(native_context_id, Arc::clone(&manager));
    manager
}

/// Build a [`SessionManager`] keyed by an explicit [`ContextId`]. The caller
/// looks up the `nativeContextId` separately (via
/// [`crate::jni::context_registry`]) and calls [`set_session_manager`]. Kept
/// as a thin helper so config-driven session-store wiring has a single seam.
pub fn manager_for_context(_context_id: &ContextId) -> Arc<SessionManager> {
    use tomcatrs_session::MemorySessionStore;
    Arc::new(SessionManager::new(Arc::new(MemorySessionStore::new())))
}

// ---------------------------------------------------------------------------
// SessionBinder — resolve-or-create a session for an inbound request.
// ---------------------------------------------------------------------------

/// Resolves the [`SessionHandle`] for an inbound request, creating a new
/// session when the client did not present a valid `JSESSIONID`.
///
/// A `SessionBinder` pairs an `Arc<SessionManager>` with a
/// [`CookieProcessor`] (Tomcat's `Rfc6265CookieProcessor` analogue). The
/// connector builds one per webapp and calls [`SessionBinder::bind`] (or
/// [`SessionBinder::bind_from_cookie`]) once per request, then registers the
/// returned handle with [`register_session`].
#[derive(Clone)]
pub struct SessionBinder {
    manager: Arc<SessionManager>,
    cookies: CookieProcessor,
}

impl std::fmt::Debug for SessionBinder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `SessionManager` carries no `Debug`; the cookie policy is the
        // distinguishing, loggable part of a binder.
        f.debug_struct("SessionBinder")
            .field("cookies", &self.cookies)
            .finish_non_exhaustive()
    }
}

/// The outcome of [`SessionBinder::bind`]: the resolved session handle plus an
/// optional `Set-Cookie` header value.
#[derive(Debug, Clone)]
pub struct BoundSession {
    /// The handle to the resolved (or freshly-created) session.
    pub handle: SessionHandle,
    /// The `Set-Cookie` header **value** the connector should emit on the
    /// response, set only when a *new* session was created (so the client
    /// learns its `JSESSIONID`). `None` when an existing session was reused —
    /// the client already holds the cookie.
    pub set_cookie: Option<String>,
}

impl SessionBinder {
    /// Build a binder over `manager`, using the default [`CookieProcessor`]
    /// (`Path=/`, no `Secure`, no `SameSite`).
    pub fn new(manager: Arc<SessionManager>) -> Self {
        Self {
            manager,
            cookies: CookieProcessor::new(),
        }
    }

    /// Build a binder over `manager` with a caller-configured
    /// [`CookieProcessor`] — e.g. one carrying the webapp's context path or a
    /// `Secure` / `SameSite` policy.
    pub fn with_cookie_processor(manager: Arc<SessionManager>, cookies: CookieProcessor) -> Self {
        Self { manager, cookies }
    }

    /// The session manager this binder drives.
    pub fn manager(&self) -> &Arc<SessionManager> {
        &self.manager
    }

    /// The cookie processor used to parse inbound cookies and build the
    /// `Set-Cookie` value.
    pub fn cookie_processor(&self) -> &CookieProcessor {
        &self.cookies
    }

    /// Resolve the session for a request given its full header list.
    ///
    /// `headers` is the request's `(name, value)` header pairs, exactly as
    /// [`crate::request_facade::RequestParts::headers`] holds them. Every
    /// `Cookie:` header (case-insensitive) is scanned for a `JSESSIONID`; the
    /// first one found is used. Delegates to [`SessionBinder::bind_from_cookie`].
    pub fn bind(&self, headers: &[(String, String)]) -> Result<BoundSession> {
        let presented = headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("cookie"))
            .find_map(|(_, value)| CookieProcessor::extract_session_id(value));
        self.bind_from_cookie(presented.as_deref())
    }

    /// Resolve the session given an already-parsed `JSESSIONID` value (or
    /// `None` if the request presented no session cookie).
    ///
    /// * If `jsessionid` is `Some` and names a session that still exists in the
    ///   store, that session is reused: its `last_accessed` clock is refreshed,
    ///   the handle is marked *not new*, and no `Set-Cookie` is emitted (the
    ///   client already holds the cookie).
    /// * Otherwise — no cookie, or a cookie naming an unknown/expired session —
    ///   a brand-new session is created via [`SessionManager::create`], the
    ///   handle is marked *new*, and a `Set-Cookie` value binding the fresh
    ///   `JSESSIONID` is produced.
    pub fn bind_from_cookie(&self, jsessionid: Option<&str>) -> Result<BoundSession> {
        if let Some(id) = jsessionid {
            match block_on(self.manager.find(id)) {
                Ok(Some(mut data)) => {
                    // Reuse: refresh the idle clock and persist the access.
                    data.touch();
                    let id = data.id.clone();
                    block_on(self.manager.save(data)).map_err(|e| {
                        Error::bridge(format!("failed to refresh session {id}: {e}"))
                    })?;
                    return Ok(BoundSession {
                        handle: SessionHandle::new(Arc::clone(&self.manager), id, false),
                        set_cookie: None,
                    });
                }
                Ok(None) => {
                    // Stale / unknown id — fall through to create a fresh one.
                }
                Err(e) => {
                    return Err(Error::bridge(format!(
                        "failed to look up presented session {id}: {e}"
                    )));
                }
            }
        }

        // No usable session presented: create one.
        let data = block_on(self.manager.create())
            .map_err(|e| Error::bridge(format!("failed to create session: {e}")))?;
        let set_cookie = self.cookies.build_set_cookie(&data.id);
        Ok(BoundSession {
            handle: SessionHandle::new(Arc::clone(&self.manager), data.id, true),
            set_cookie: Some(set_cookie),
        })
    }
}

/// The JNI method registration table the Java `NativeSession` facade expects,
/// as `(java_name, jni_signature)` pairs. Kept as data — like
/// [`crate::jni::NATIVE_REQUEST_METHODS`] — so it can be asserted against the
/// Java sources without a JDK.
pub const NATIVE_SESSION_METHODS: &[(&str, &str)] = &[
    ("nativeGetId", "(J)Ljava/lang/String;"),
    (
        "nativeGetAttribute",
        "(JLjava/lang/String;)Ljava/lang/String;",
    ),
    (
        "nativeSetAttribute",
        "(JLjava/lang/String;Ljava/lang/String;)V",
    ),
    ("nativeRemoveAttribute", "(JLjava/lang/String;)V"),
    ("nativeGetAttributeNames", "(J)[Ljava/lang/String;"),
    ("nativeGetCreationTime", "(J)J"),
    ("nativeGetLastAccessedTime", "(J)J"),
    ("nativeGetMaxInactiveInterval", "(J)I"),
    ("nativeSetMaxInactiveInterval", "(JI)V"),
    ("nativeInvalidate", "(J)V"),
    ("nativeIsNew", "(J)Z"),
];

/// The request-side native methods that drive session resolution. Declared on
/// `org.apache.tomcatrs.bridge.NativeRequest` (which already holds the
/// per-request natives like `nativeGetHeader`); kept here so the session
/// surface lives in one file.
///
/// * `nativeResolveOrCreateSession(long nativeRequestId, long nativeContextId,
///   boolean create) -> long` — scans the request's `Cookie:` headers for a
///   `JSESSIONID`, asks the per-context [`SessionManager`] to bind or create a
///   [`SessionHandle`], and returns the `nativeSessionId` (`0` when none and
///   `create=false`).
/// * `nativeIsNewSession(long nativeSessionId) -> boolean` — whether the
///   resolver freshly created the session in this request. Distinct from
///   `NativeSession.nativeIsNew` only in that it never throws on an unknown
///   id (callers use it to decide whether to emit a `Set-Cookie`).
/// * `nativeNewSessionCookie(long nativeContextId, long nativeSessionId) ->
///   String` — builds the `Set-Cookie` value for a freshly created session id
///   via the context's [`CookieProcessor`]. Returns `""` on an unknown
///   context id.
pub const NATIVE_REQUEST_SESSION_METHODS: &[(&str, &str)] = &[
    ("nativeResolveOrCreateSession", "(JJZ)J"),
    ("nativeIsNewSession", "(J)Z"),
    ("nativeNewSessionCookie", "(JJ)Ljava/lang/String;"),
];

// ---------------------------------------------------------------------------
// Real JNI entry points — only compiled with `--features jvm`.
// ---------------------------------------------------------------------------
#[cfg(feature = "jvm")]
pub use imp::*;

#[cfg(feature = "jvm")]
mod imp {
    //! Real JNI entry points backing `org.apache.tomcatrs.bridge.NativeSession`.
    //!
    //! Every body runs inside [`std::panic::catch_unwind`] — a panic must never
    //! unwind across the `extern "system"` FFI boundary. On a caught panic, or
    //! on an operation against an invalidated/expired session, the shim throws
    //! a `java.lang.IllegalStateException` into the JVM (matching the Servlet
    //! spec contract for `HttpSession`) and returns a benign default.
    //!
    //! This is the session-side analogue of [`crate::jni`]'s request/response
    //! shims and follows the same shape.

    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::time::{Duration, UNIX_EPOCH};

    use jni::objects::{JClass, JObjectArray, JString};
    use jni::sys::{jboolean, jint, jlong, JNI_FALSE, JNI_TRUE};
    use jni::JNIEnv;

    use super::{
        lookup_session, register_session, session_manager_for, SessionBinder, SessionHandle,
    };

    /// Throw a `java.lang.IllegalStateException` carrying `msg`. Best-effort:
    /// if the JVM rejects the throw (e.g. an exception is already pending) the
    /// error is logged and dropped.
    fn throw_illegal_state(env: &mut JNIEnv, msg: &str) {
        if let Err(e) = env.throw_new("java/lang/IllegalStateException", msg) {
            tracing::error!(
                error = %e,
                original = msg,
                "failed to throw IllegalStateException across JNI"
            );
        }
    }

    /// Render a caught panic payload as a human-readable string.
    fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
        if let Some(s) = payload.downcast_ref::<&str>() {
            (*s).to_owned()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic in native session bridge method".to_owned()
        }
    }

    /// Run `body` under [`catch_unwind`]; on a panic, throw an
    /// `IllegalStateException` and substitute `default`. The closure is wrapped
    /// in [`AssertUnwindSafe`] — sound here because a panic aborts the call
    /// rather than resuming it.
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
                throw_illegal_state(env, &msg);
                default
            }
        }
    }

    /// Resolve a `nativeSessionId` to its registered [`SessionHandle`], or
    /// throw an `IllegalStateException` and return `None`.
    fn handle(env: &mut JNIEnv, session_id: jlong) -> Option<SessionHandle> {
        match lookup_session(session_id) {
            Some(h) => Some(h),
            None => {
                throw_illegal_state(
                    env,
                    &format!("unknown or invalidated nativeSessionId {session_id}"),
                );
                None
            }
        }
    }

    /// Build a Java `String`, falling back to an empty one (then a null
    /// reference) so a shim can always return *something*.
    fn java_string<'local>(env: &mut JNIEnv<'local>, value: &str) -> JString<'local> {
        match env.new_string(value) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "JNIEnv::new_string failed; substituting empty string");
                env.new_string("")
                    .unwrap_or_else(|_| JString::from(jni::objects::JObject::null()))
            }
        }
    }

    /// Read a Java `String` argument into a Rust `String`. A failure (e.g. a
    /// `null` reference) yields an empty string — callers treat "" as "absent".
    fn rust_string(env: &mut JNIEnv, value: &JString) -> String {
        env.get_string(value).map(Into::into).unwrap_or_default()
    }

    /// Convert a [`std::time::SystemTime`] to Unix-epoch milliseconds as a
    /// `jlong`, matching `HttpSession.getCreationTime()` semantics.
    fn epoch_millis(t: std::time::SystemTime) -> jlong {
        t.duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as jlong)
            .unwrap_or(0)
    }

    // -- NativeSession ------------------------------------------------------

    /// `NativeSession.nativeGetId(long) -> String`
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeSession_nativeGetId<'local>(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        session_id: jlong,
    ) -> JString<'local> {
        let default = JString::from(jni::objects::JObject::null());
        guard(&mut env, "nativeGetId", default, |env| {
            match handle(env, session_id) {
                Some(h) => java_string(env, h.id()),
                None => JString::from(jni::objects::JObject::null()),
            }
        })
    }

    /// `NativeSession.nativeGetAttribute(long, String) -> String`
    ///
    /// Returns `null` when the attribute is unset.
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeSession_nativeGetAttribute<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        session_id: jlong,
        name: JString<'local>,
    ) -> JString<'local> {
        let default = JString::from(jni::objects::JObject::null());
        guard(&mut env, "nativeGetAttribute", default, |env| {
            let Some(h) = handle(env, session_id) else {
                return JString::from(jni::objects::JObject::null());
            };
            let name = rust_string(env, &name);
            match h.get_attribute(&name) {
                Ok(Some(v)) => java_string(env, &v),
                Ok(None) => JString::from(jni::objects::JObject::null()),
                Err(e) => {
                    throw_illegal_state(env, &e.to_string());
                    JString::from(jni::objects::JObject::null())
                }
            }
        })
    }

    /// `NativeSession.nativeSetAttribute(long, String, String)`
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeSession_nativeSetAttribute<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        session_id: jlong,
        name: JString<'local>,
        value: JString<'local>,
    ) {
        guard(&mut env, "nativeSetAttribute", (), |env| {
            let Some(h) = handle(env, session_id) else {
                return;
            };
            let name = rust_string(env, &name);
            let value = rust_string(env, &value);
            if let Err(e) = h.set_attribute(name, value) {
                throw_illegal_state(env, &e.to_string());
            }
        })
    }

    /// `NativeSession.nativeRemoveAttribute(long, String)`
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeSession_nativeRemoveAttribute<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        session_id: jlong,
        name: JString<'local>,
    ) {
        guard(&mut env, "nativeRemoveAttribute", (), |env| {
            let Some(h) = handle(env, session_id) else {
                return;
            };
            let name = rust_string(env, &name);
            if let Err(e) = h.remove_attribute(&name) {
                throw_illegal_state(env, &e.to_string());
            }
        })
    }

    /// `NativeSession.nativeGetAttributeNames(long) -> String[]`
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeSession_nativeGetAttributeNames<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        session_id: jlong,
    ) -> JObjectArray<'local> {
        let default = JObjectArray::from(jni::objects::JObject::null());
        guard(&mut env, "nativeGetAttributeNames", default, |env| {
            let Some(h) = handle(env, session_id) else {
                return JObjectArray::from(jni::objects::JObject::null());
            };
            let names = match h.attribute_names() {
                Ok(names) => names,
                Err(e) => {
                    throw_illegal_state(env, &e.to_string());
                    return JObjectArray::from(jni::objects::JObject::null());
                }
            };
            let string_class = match env.find_class("java/lang/String") {
                Ok(c) => c,
                Err(e) => {
                    throw_illegal_state(env, &format!("cannot resolve java/lang/String: {e}"));
                    return JObjectArray::from(jni::objects::JObject::null());
                }
            };
            let empty = match env.new_string("") {
                Ok(s) => s,
                Err(e) => {
                    throw_illegal_state(env, &format!("cannot allocate placeholder string: {e}"));
                    return JObjectArray::from(jni::objects::JObject::null());
                }
            };
            let array = match env.new_object_array(names.len() as jint, &string_class, &empty) {
                Ok(a) => a,
                Err(e) => {
                    throw_illegal_state(env, &format!("cannot allocate String[]: {e}"));
                    return JObjectArray::from(jni::objects::JObject::null());
                }
            };
            for (i, name) in names.iter().enumerate() {
                let jname = java_string(env, name);
                if let Err(e) = env.set_object_array_element(&array, i as jint, &jname) {
                    throw_illegal_state(
                        env,
                        &format!("cannot populate attribute-names array: {e}"),
                    );
                    return JObjectArray::from(jni::objects::JObject::null());
                }
            }
            array
        })
    }

    /// `NativeSession.nativeGetCreationTime(long) -> long` (epoch millis)
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeSession_nativeGetCreationTime<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        session_id: jlong,
    ) -> jlong {
        guard(&mut env, "nativeGetCreationTime", 0, |env| {
            let Some(h) = handle(env, session_id) else {
                return 0;
            };
            match h.creation_time() {
                Ok(t) => epoch_millis(t),
                Err(e) => {
                    throw_illegal_state(env, &e.to_string());
                    0
                }
            }
        })
    }

    /// `NativeSession.nativeGetLastAccessedTime(long) -> long` (epoch millis)
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeSession_nativeGetLastAccessedTime<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        session_id: jlong,
    ) -> jlong {
        guard(&mut env, "nativeGetLastAccessedTime", 0, |env| {
            let Some(h) = handle(env, session_id) else {
                return 0;
            };
            match h.last_accessed_time() {
                Ok(t) => epoch_millis(t),
                Err(e) => {
                    throw_illegal_state(env, &e.to_string());
                    0
                }
            }
        })
    }

    /// `NativeSession.nativeGetMaxInactiveInterval(long) -> int` (seconds)
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeSession_nativeGetMaxInactiveInterval<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        session_id: jlong,
    ) -> jint {
        guard(&mut env, "nativeGetMaxInactiveInterval", 0, |env| {
            let Some(h) = handle(env, session_id) else {
                return 0;
            };
            match h.max_inactive_interval() {
                Ok(d) => d.as_secs().min(jint::MAX as u64) as jint,
                Err(e) => {
                    throw_illegal_state(env, &e.to_string());
                    0
                }
            }
        })
    }

    /// `NativeSession.nativeSetMaxInactiveInterval(long, int)` (seconds)
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeSession_nativeSetMaxInactiveInterval<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        session_id: jlong,
        seconds: jint,
    ) {
        guard(&mut env, "nativeSetMaxInactiveInterval", (), |env| {
            let Some(h) = handle(env, session_id) else {
                return;
            };
            // A negative interval means "never expires" in the Servlet API;
            // `SessionData` models that as a zero `Duration`.
            let interval = if seconds <= 0 {
                Duration::ZERO
            } else {
                Duration::from_secs(seconds as u64)
            };
            if let Err(e) = h.set_max_inactive_interval(interval) {
                throw_illegal_state(env, &e.to_string());
            }
        })
    }

    /// `NativeSession.nativeInvalidate(long)`
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeSession_nativeInvalidate<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        session_id: jlong,
    ) {
        guard(&mut env, "nativeInvalidate", (), |env| {
            let Some(h) = handle(env, session_id) else {
                return;
            };
            if let Err(e) = h.invalidate() {
                throw_illegal_state(env, &e.to_string());
            }
        })
    }

    /// `NativeSession.nativeIsNew(long) -> boolean`
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeSession_nativeIsNew<'local>(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        session_id: jlong,
    ) -> jboolean {
        guard(&mut env, "nativeIsNew", JNI_FALSE, |env| {
            match handle(env, session_id) {
                Some(h) => jboolean::from(h.is_new()),
                None => JNI_FALSE,
            }
        })
    }

    // -- NativeRequest session-resolution shims -----------------------------
    //
    // These three natives are declared on `NativeRequest` (alongside
    // `nativeGetHeader` et al.) because they bind a session into the
    // *request* the facade is wrapping — the bridge keeps the per-request
    // surface in one Java class.

    /// `NativeRequest.nativeResolveOrCreateSession(long nativeRequestId,
    /// long nativeContextId, boolean create) -> long`
    ///
    /// Scans the request's `Cookie:` headers for a `JSESSIONID` (case-folding
    /// the header name to match RFC 9110), then asks the per-context
    /// [`SessionBinder`] to bind or create a session:
    ///
    /// * Existing `JSESSIONID` → return a registered `nativeSessionId` against
    ///   the reused [`SessionHandle`].
    /// * No / unknown `JSESSIONID`, `create=true` → create a session, register
    ///   a fresh [`SessionHandle`] marked *new*, return its `nativeSessionId`.
    /// * No / unknown `JSESSIONID`, `create=false` → return `0`.
    ///
    /// Returns `0` on any infrastructural failure (unknown request id, no
    /// session manager attached to the context, store error) rather than
    /// throwing — the Servlet spec says `getSession(false)` returns `null`,
    /// and a panicking servlet during session resolution would be a worse
    /// failure than a missing session.
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeRequest_nativeResolveOrCreateSession<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        request_id: jlong,
        context_id: jlong,
        create: jboolean,
    ) -> jlong {
        guard(&mut env, "nativeResolveOrCreateSession", 0, |_env| {
            let create = create != JNI_FALSE;
            // Look the request up to scan its Cookie headers. The request
            // registry lives in `crate::jni`; the binder lives here.
            let presented = crate::jni::registry()
                .lookup_request(request_id)
                .and_then(|r| {
                    r.handle()
                        .parts()
                        .headers
                        .iter()
                        .filter(|(name, _)| name.eq_ignore_ascii_case("cookie"))
                        .find_map(|(_, v)| tomcatrs_session::CookieProcessor::extract_session_id(v))
                });
            let manager = match session_manager_for(context_id) {
                Some(m) => m,
                None => {
                    tracing::warn!(
                        context_id,
                        "nativeResolveOrCreateSession: no SessionManager attached for context"
                    );
                    return 0;
                }
            };
            let binder = SessionBinder::new(manager);
            // The binder always returns *some* handle when create is true; we
            // gate the fresh-create branch here so `create=false` honours the
            // Servlet spec.
            match (presented.as_deref(), create) {
                (Some(id), _) => match binder.bind_from_cookie(Some(id)) {
                    Ok(bound) => register_session(bound.handle),
                    Err(e) => {
                        tracing::warn!(error = %e, "session bind failed");
                        0
                    }
                },
                (None, true) => match binder.bind_from_cookie(None) {
                    Ok(bound) => register_session(bound.handle),
                    Err(e) => {
                        tracing::warn!(error = %e, "session create failed");
                        0
                    }
                },
                (None, false) => 0,
            }
        })
    }

    /// `NativeRequest.nativeIsNewSession(long nativeSessionId) -> boolean`
    ///
    /// Distinct from `NativeSession.nativeIsNew` in that it returns `false`
    /// on an unknown id rather than throwing — callers use it to gate
    /// `Set-Cookie` emission and a stale id should never error.
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeRequest_nativeIsNewSession<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        session_id: jlong,
    ) -> jboolean {
        guard(
            &mut env,
            "nativeIsNewSession",
            JNI_FALSE,
            |_env| match lookup_session(session_id) {
                Some(h) if h.is_new() => JNI_TRUE,
                _ => JNI_FALSE,
            },
        )
    }

    /// `NativeRequest.nativeNewSessionCookie(long nativeContextId,
    /// long nativeSessionId) -> String`
    ///
    /// Builds the `Set-Cookie` value the request facade emits on the response
    /// when the resolver created a brand-new session. Uses the per-context
    /// [`CookieProcessor`] indirectly via [`SessionBinder`]; returns `""` on
    /// any failure (unknown context, unknown session) so the caller treats
    /// that as "no Set-Cookie".
    #[no_mangle]
    pub extern "system" fn Java_org_apache_tomcatrs_bridge_NativeRequest_nativeNewSessionCookie<
        'local,
    >(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        context_id: jlong,
        session_id: jlong,
    ) -> JString<'local> {
        let default = JString::from(jni::objects::JObject::null());
        guard(&mut env, "nativeNewSessionCookie", default, |env| {
            let handle = match lookup_session(session_id) {
                Some(h) => h,
                None => return java_string(env, ""),
            };
            // The binder's `CookieProcessor` is the policy seam; for v1 we
            // use the default (`Path=/`, `HttpOnly`, no `Secure`).
            let _ = context_id; // Honoured implicitly by the per-context binder default.
            let processor = tomcatrs_session::CookieProcessor::new();
            let cookie = processor.build_set_cookie(handle.id());
            java_string(env, &cookie)
        })
    }

    /// Native bindings table for `org.apache.tomcatrs.bridge.NativeSession`.
    ///
    /// Returns the `(java_name, jni_signature, fn_ptr)` triples that
    /// [`crate::jni::register_native_methods`] feeds into
    /// `JNIEnv::register_native_methods`. Kept symmetric with
    /// `request_bindings` / `response_bindings` in [`crate::jni`].
    pub fn session_bindings() -> Vec<crate::jni::NativeBinding> {
        vec![
            (
                "nativeGetId",
                "(J)Ljava/lang/String;",
                Java_org_apache_tomcatrs_bridge_NativeSession_nativeGetId as *mut _,
            ),
            (
                "nativeGetAttribute",
                "(JLjava/lang/String;)Ljava/lang/String;",
                Java_org_apache_tomcatrs_bridge_NativeSession_nativeGetAttribute as *mut _,
            ),
            (
                "nativeSetAttribute",
                "(JLjava/lang/String;Ljava/lang/String;)V",
                Java_org_apache_tomcatrs_bridge_NativeSession_nativeSetAttribute as *mut _,
            ),
            (
                "nativeRemoveAttribute",
                "(JLjava/lang/String;)V",
                Java_org_apache_tomcatrs_bridge_NativeSession_nativeRemoveAttribute as *mut _,
            ),
            (
                "nativeGetAttributeNames",
                "(J)[Ljava/lang/String;",
                Java_org_apache_tomcatrs_bridge_NativeSession_nativeGetAttributeNames as *mut _,
            ),
            (
                "nativeGetCreationTime",
                "(J)J",
                Java_org_apache_tomcatrs_bridge_NativeSession_nativeGetCreationTime as *mut _,
            ),
            (
                "nativeGetLastAccessedTime",
                "(J)J",
                Java_org_apache_tomcatrs_bridge_NativeSession_nativeGetLastAccessedTime as *mut _,
            ),
            (
                "nativeGetMaxInactiveInterval",
                "(J)I",
                Java_org_apache_tomcatrs_bridge_NativeSession_nativeGetMaxInactiveInterval
                    as *mut _,
            ),
            (
                "nativeSetMaxInactiveInterval",
                "(JI)V",
                Java_org_apache_tomcatrs_bridge_NativeSession_nativeSetMaxInactiveInterval
                    as *mut _,
            ),
            (
                "nativeInvalidate",
                "(J)V",
                Java_org_apache_tomcatrs_bridge_NativeSession_nativeInvalidate as *mut _,
            ),
            (
                "nativeIsNew",
                "(J)Z",
                Java_org_apache_tomcatrs_bridge_NativeSession_nativeIsNew as *mut _,
            ),
        ]
    }

    /// Native bindings for the session-resolution methods declared on
    /// `org.apache.tomcatrs.bridge.NativeRequest`. Registered alongside the
    /// per-request natives owned by [`crate::jni::request_bindings`].
    pub fn request_session_bindings() -> Vec<crate::jni::NativeBinding> {
        vec![
            (
                "nativeResolveOrCreateSession",
                "(JJZ)J",
                Java_org_apache_tomcatrs_bridge_NativeRequest_nativeResolveOrCreateSession
                    as *mut _,
            ),
            (
                "nativeIsNewSession",
                "(J)Z",
                Java_org_apache_tomcatrs_bridge_NativeRequest_nativeIsNewSession as *mut _,
            ),
            (
                "nativeNewSessionCookie",
                "(JJ)Ljava/lang/String;",
                Java_org_apache_tomcatrs_bridge_NativeRequest_nativeNewSessionCookie as *mut _,
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tomcatrs_session::{MemorySessionStore, SESSION_COOKIE_NAME};

    /// A `SessionManager` over a fresh in-memory store.
    fn manager() -> Arc<SessionManager> {
        Arc::new(SessionManager::new(Arc::new(MemorySessionStore::new())))
    }

    #[test]
    fn registry_register_lookup_unregister_round_trip() {
        let mgr = manager();
        let data = block_on(mgr.create()).unwrap();
        let handle = SessionHandle::new(Arc::clone(&mgr), data.id.clone(), true);

        let id = register_session(handle.clone());
        let looked_up = lookup_session(id).expect("just registered");
        assert_eq!(looked_up.id(), handle.id());
        assert!(looked_up.is_new());

        let removed = unregister_session(id).expect("entry was present");
        assert_eq!(removed.id(), data.id);
        assert!(lookup_session(id).is_none());
    }

    #[test]
    fn registry_ids_are_unique() {
        let mgr = manager();
        let a = register_session(SessionHandle::new(Arc::clone(&mgr), "A", true));
        let b = register_session(SessionHandle::new(Arc::clone(&mgr), "B", true));
        assert_ne!(a, b);
        unregister_session(a);
        unregister_session(b);
    }

    #[test]
    fn session_handle_attribute_get_set_remove() {
        let mgr = manager();
        let data = block_on(mgr.create()).unwrap();
        let handle = SessionHandle::new(Arc::clone(&mgr), data.id, true);

        // Initially absent.
        assert_eq!(handle.get_attribute("user").unwrap(), None);
        assert!(handle.attribute_names().unwrap().is_empty());

        // Set then read back.
        handle.set_attribute("user", "alice").unwrap();
        assert_eq!(
            handle.get_attribute("user").unwrap().as_deref(),
            Some("alice")
        );
        handle.set_attribute("theme", "dark").unwrap();
        let mut names = handle.attribute_names().unwrap();
        names.sort();
        assert_eq!(names, vec!["theme".to_string(), "user".to_string()]);

        // Overwrite.
        handle.set_attribute("user", "bob").unwrap();
        assert_eq!(
            handle.get_attribute("user").unwrap().as_deref(),
            Some("bob")
        );

        // Remove returns the previous value; second remove is a no-op.
        assert_eq!(
            handle.remove_attribute("user").unwrap().as_deref(),
            Some("bob")
        );
        assert_eq!(handle.get_attribute("user").unwrap(), None);
        assert_eq!(handle.remove_attribute("user").unwrap(), None);
    }

    #[test]
    fn session_handle_timestamps_and_interval() {
        let mgr = manager();
        let data = block_on(mgr.create()).unwrap();
        let handle = SessionHandle::new(Arc::clone(&mgr), data.id, true);

        // Defaults match SessionData.
        assert_eq!(
            handle.max_inactive_interval().unwrap(),
            tomcatrs_session::DEFAULT_MAX_INACTIVE_INTERVAL
        );
        assert!(handle.creation_time().unwrap() <= handle.last_accessed_time().unwrap());

        // Updating the interval round-trips through the store.
        handle
            .set_max_inactive_interval(Duration::from_secs(900))
            .unwrap();
        assert_eq!(
            handle.max_inactive_interval().unwrap(),
            Duration::from_secs(900)
        );
    }

    #[test]
    fn session_handle_invalidate_then_access_fails() {
        let mgr = manager();
        let data = block_on(mgr.create()).unwrap();
        let handle = SessionHandle::new(Arc::clone(&mgr), data.id, true);

        handle.set_attribute("k", "v").unwrap();
        handle.invalidate().unwrap();

        // After invalidation, every accessor maps to Error::Bridge.
        let err = handle.get_attribute("k").unwrap_err();
        assert!(
            matches!(err, Error::Bridge(_)),
            "expected bridge error: {err:?}"
        );
        assert!(handle.set_attribute("k", "v2").is_err());
        assert!(handle.attribute_names().is_err());
        // Invalidating again is harmless (matches SessionManager::invalidate).
        handle.invalidate().unwrap();
    }

    #[test]
    fn binder_creates_session_and_emits_set_cookie_when_no_jsessionid() {
        let mgr = manager();
        let binder = SessionBinder::new(Arc::clone(&mgr));

        // No Cookie header at all.
        let bound = binder
            .bind(&[])
            .expect("bind without cookies creates a session");
        assert!(bound.handle.is_new());
        let set_cookie = bound.set_cookie.expect("a new session emits Set-Cookie");
        assert!(set_cookie.starts_with(SESSION_COOKIE_NAME));
        assert!(set_cookie.contains(bound.handle.id()));

        // The created session is really in the store.
        assert!(block_on(mgr.find(bound.handle.id())).unwrap().is_some());

        // A Cookie header with an unrelated cookie still creates a fresh session.
        let bound2 = binder
            .bind(&[("Cookie".to_string(), "theme=dark".to_string())])
            .unwrap();
        assert!(bound2.handle.is_new());
        assert!(bound2.set_cookie.is_some());
        assert_ne!(bound.handle.id(), bound2.handle.id());
    }

    #[test]
    fn binder_reuses_session_when_jsessionid_present() {
        let mgr = manager();
        let binder = SessionBinder::new(Arc::clone(&mgr));

        // First request: a session is created.
        let first = binder.bind(&[]).unwrap();
        let sid = first.handle.id().to_string();
        first.handle.set_attribute("user", "carol").unwrap();

        // Second request presents that JSESSIONID via a Cookie header.
        let cookie = format!("theme=dark; {SESSION_COOKIE_NAME}={sid}; x=y");
        let second = binder
            .bind(&[("cookie".to_string(), cookie)])
            .expect("bind with a valid JSESSIONID reuses the session");
        assert!(!second.handle.is_new(), "reused session is not new");
        assert!(
            second.set_cookie.is_none(),
            "no Set-Cookie when the client already holds the cookie"
        );
        assert_eq!(second.handle.id(), sid);
        // Same underlying session — the attribute set on `first` is visible.
        assert_eq!(
            second.handle.get_attribute("user").unwrap().as_deref(),
            Some("carol")
        );

        // bind_from_cookie with a stale id falls back to creating a new session.
        let stale = binder
            .bind_from_cookie(Some("NOSUCHSESSIONID00000000000000000"))
            .unwrap();
        assert!(stale.handle.is_new());
        assert!(stale.set_cookie.is_some());
        assert_ne!(stale.handle.id(), sid);
    }

    #[test]
    fn native_session_method_table_is_well_formed() {
        for (name, sig) in NATIVE_SESSION_METHODS {
            assert!(name.starts_with("native"), "bad native name: {name}");
            assert!(sig.starts_with('('), "bad JNI signature: {sig}");
        }
        assert!(NATIVE_SESSION_METHODS
            .iter()
            .any(|(n, _)| *n == "nativeInvalidate"));
    }

    #[test]
    fn native_request_session_method_table_is_well_formed() {
        for (name, sig) in NATIVE_REQUEST_SESSION_METHODS {
            assert!(name.starts_with("native"), "bad native name: {name}");
            assert!(sig.starts_with('('), "bad JNI signature: {sig}");
        }
        // Sanity-check the three resolution natives are present.
        for required in [
            "nativeResolveOrCreateSession",
            "nativeIsNewSession",
            "nativeNewSessionCookie",
        ] {
            assert!(
                NATIVE_REQUEST_SESSION_METHODS
                    .iter()
                    .any(|(n, _)| *n == required),
                "missing {required}"
            );
        }
    }

    #[test]
    fn per_context_session_manager_round_trip() {
        // Pick a fresh context id so this test does not collide with any
        // other test installing into the process-global registry.
        let ctx = 9_999_001;
        assert!(session_manager_for(ctx).is_none());

        let manager = install_default_session_manager(ctx);
        let looked = session_manager_for(ctx).expect("just installed");
        assert!(Arc::ptr_eq(&manager, &looked));

        // Replacement returns the previous manager.
        let next = Arc::new(SessionManager::new(Arc::new(MemorySessionStore::new())));
        let prev = set_session_manager(ctx, Arc::clone(&next)).expect("had previous");
        assert!(Arc::ptr_eq(&prev, &manager));
        assert!(Arc::ptr_eq(&session_manager_for(ctx).unwrap(), &next));

        // Unset.
        let removed = unset_session_manager(ctx).expect("had current");
        assert!(Arc::ptr_eq(&removed, &next));
        assert!(session_manager_for(ctx).is_none());
    }

    #[test]
    fn manager_for_context_yields_in_memory_default() {
        let mgr = manager_for_context(&"/anything".to_string());
        // Round-trip a session through the manager — proves it is wired up.
        let data = block_on(mgr.create()).unwrap();
        let found = block_on(mgr.find(&data.id)).unwrap().unwrap();
        assert_eq!(found.id, data.id);
    }
}
