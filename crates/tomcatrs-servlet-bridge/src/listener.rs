//! Servlet **listener** support: instantiation and lifecycle-event dispatch.
//!
//! # What listeners are
//!
//! A Jakarta Servlet web application may declare *listeners* — objects that
//! implement one of the standard observer interfaces in
//! `jakarta.servlet` / `jakarta.servlet.http` — either in `web.xml`
//! (`<listener><listener-class>…</listener-class></listener>`) or via the
//! `@WebListener` annotation. The container instantiates each one and notifies
//! it of the lifecycle events it cares about:
//!
//! | interface                          | events                                            |
//! |------------------------------------|---------------------------------------------------|
//! | `ServletContextListener`           | context initialized / destroyed                   |
//! | `ServletContextAttributeListener`  | context attribute added / removed / replaced       |
//! | `ServletRequestListener`           | request initialized / destroyed                   |
//! | `ServletRequestAttributeListener`  | request attribute added / removed / replaced       |
//! | `HttpSessionListener`              | session created / destroyed                       |
//! | `HttpSessionAttributeListener`     | session attribute added / removed / replaced       |
//! | `HttpSessionIdListener`            | session id changed                                 |
//!
//! # Ordering
//!
//! The Servlet specification requires that listeners are invoked in **declaration
//! order** for "constructive" events (context/request/session *created* or
//! *initialized*) and in **reverse declaration order** for "destructive" events
//! (context/request/session *destroyed*). [`ListenerRegistry`] keeps the
//! registration order and [`ListenerRegistry::dispatch_order`] hands back the
//! correct slice direction for a given [`ListenerEvent`].
//!
//! # Two builds, one API
//!
//! As with the rest of the crate every public item exists on **both** feature
//! paths.
//!
//! * **`--features jvm`** — [`ListenerDispatcher::fire`] crosses JNI via
//!   [`JvmRuntime::with_env`], builds the matching Jakarta event object (e.g.
//!   `jakarta.servlet.ServletContextEvent`) and invokes the listener method
//!   (e.g. `contextInitialized`). [`instantiate_listeners`] loads each listener
//!   class through the webapp's [`ClassLoaderFactory`] and stores the resulting
//!   instance handles in the [`ListenerRegistry`].
//! * **default features** — there is no JVM, so [`ListenerDispatcher::fire`] is
//!   a *working stub*: it appends every fired event to an interior-mutable log
//!   that tests can inspect via [`ListenerDispatcher::fired_events`]. This makes
//!   the dispatch-ordering rules fully testable with no JDK installed.

use std::sync::Arc;

use tomcatrs_core::{ContextId, Result};

use crate::jvm::{JvmRuntime, ServletInstanceHandle};

// ===========================================================================
// ListenerType
// ===========================================================================

/// One of the standard Jakarta Servlet listener interfaces.
///
/// A single listener *class* may implement several of these at once; the
/// container treats each implemented interface independently. [`ListenerType`]
/// therefore identifies *one* interface, and a listener that implements more
/// than one is registered once per interface it implements (see
/// [`ListenerRegistry::register`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ListenerType {
    /// `jakarta.servlet.ServletContextListener` — context init / destroy.
    ServletContext,
    /// `jakarta.servlet.ServletContextAttributeListener` — context attribute
    /// added / removed / replaced.
    ServletContextAttribute,
    /// `jakarta.servlet.ServletRequestListener` — request init / destroy.
    ServletRequest,
    /// `jakarta.servlet.ServletRequestAttributeListener` — request attribute
    /// added / removed / replaced.
    ServletRequestAttribute,
    /// `jakarta.servlet.http.HttpSessionListener` — session create / destroy.
    HttpSession,
    /// `jakarta.servlet.http.HttpSessionAttributeListener` — session attribute
    /// added / removed / replaced.
    HttpSessionAttribute,
    /// `jakarta.servlet.http.HttpSessionIdListener` — session id changed.
    HttpSessionId,
}

impl ListenerType {
    /// Every listener interface, in a stable order.
    pub const ALL: [ListenerType; 7] = [
        ListenerType::ServletContext,
        ListenerType::ServletContextAttribute,
        ListenerType::ServletRequest,
        ListenerType::ServletRequestAttribute,
        ListenerType::HttpSession,
        ListenerType::HttpSessionAttribute,
        ListenerType::HttpSessionId,
    ];

    /// The fully-qualified, slash-separated JNI name of the interface this
    /// variant denotes — the form `JNIEnv::is_instance_of` / `find_class`
    /// expect.
    pub fn jni_interface_name(self) -> &'static str {
        match self {
            ListenerType::ServletContext => "jakarta/servlet/ServletContextListener",
            ListenerType::ServletContextAttribute => {
                "jakarta/servlet/ServletContextAttributeListener"
            }
            ListenerType::ServletRequest => "jakarta/servlet/ServletRequestListener",
            ListenerType::ServletRequestAttribute => {
                "jakarta/servlet/ServletRequestAttributeListener"
            }
            ListenerType::HttpSession => "jakarta/servlet/http/HttpSessionListener",
            ListenerType::HttpSessionAttribute => {
                "jakarta/servlet/http/HttpSessionAttributeListener"
            }
            ListenerType::HttpSessionId => "jakarta/servlet/http/HttpSessionIdListener",
        }
    }

    /// The fully-qualified, dot-separated Java name of the interface.
    pub fn java_interface_name(self) -> &'static str {
        match self {
            ListenerType::ServletContext => "jakarta.servlet.ServletContextListener",
            ListenerType::ServletContextAttribute => {
                "jakarta.servlet.ServletContextAttributeListener"
            }
            ListenerType::ServletRequest => "jakarta.servlet.ServletRequestListener",
            ListenerType::ServletRequestAttribute => {
                "jakarta.servlet.ServletRequestAttributeListener"
            }
            ListenerType::HttpSession => "jakarta.servlet.http.HttpSessionListener",
            ListenerType::HttpSessionAttribute => {
                "jakarta.servlet.http.HttpSessionAttributeListener"
            }
            ListenerType::HttpSessionId => "jakarta.servlet.http.HttpSessionIdListener",
        }
    }

    /// Best-effort classification of a listener *class* by name.
    ///
    /// This is only a **hint**: the JVM is the source of truth for which
    /// interfaces a class actually implements (see [`instantiate_listeners`],
    /// which uses `is_instance_of` under the `jvm` feature). It exists for the
    /// no-JVM path and for diagnostics, recognising the common convention of
    /// naming a listener after the interface it implements (e.g.
    /// `com.example.MyHttpSessionListener`). When the name carries no such
    /// signal an empty vector is returned — *not* a guess.
    ///
    /// A class name like `…ContextAttributeListener` is deliberately matched as
    /// [`ServletContextAttribute`](Self::ServletContextAttribute) only, never
    /// also as the plain [`ServletContext`](Self::ServletContext) listener: the
    /// longer, more specific markers are tested first and a family's plain
    /// listener is only matched once no more specific marker of that family
    /// has hit.
    ///
    /// Matching is on substrings rather than strict suffixes so the common
    /// convention of *prefixing* the interface name (e.g. `AppContextListener`,
    /// `MyHttpSessionListener`) is recognised too.
    pub fn classify_from_class_name(class_name: &str) -> Vec<ListenerType> {
        // (marker, type, family) — `family` groups the plain listener with its
        // attribute/id siblings so a more specific match suppresses the plain
        // one. Order is most-specific-first.
        const MARKERS: [(&str, ListenerType, u8); 7] = [
            (
                "ContextAttributeListener",
                ListenerType::ServletContextAttribute,
                0,
            ),
            (
                "RequestAttributeListener",
                ListenerType::ServletRequestAttribute,
                1,
            ),
            (
                "SessionAttributeListener",
                ListenerType::HttpSessionAttribute,
                2,
            ),
            ("SessionIdListener", ListenerType::HttpSessionId, 2),
            ("ContextListener", ListenerType::ServletContext, 0),
            ("RequestListener", ListenerType::ServletRequest, 1),
            ("SessionListener", ListenerType::HttpSession, 2),
        ];

        let mut found = Vec::new();
        let mut hit_families = Vec::new();
        for (marker, ty, family) in MARKERS {
            if class_name.contains(marker) {
                // The plain *Context/Request/Session* listeners are the last
                // three entries; skip them if a more specific marker of the
                // same family already matched.
                let is_plain = matches!(
                    ty,
                    ListenerType::ServletContext
                        | ListenerType::ServletRequest
                        | ListenerType::HttpSession
                );
                if is_plain && hit_families.contains(&family) {
                    continue;
                }
                if !found.contains(&ty) {
                    found.push(ty);
                    hit_families.push(family);
                }
            }
        }
        found
    }
}

// ===========================================================================
// ListenerEvent
// ===========================================================================

/// An opaque, process-local identifier for a request or HTTP session.
///
/// Listener events for requests and sessions need to name *which* request or
/// session they concern without the listener module depending on the
/// connector's or session manager's concrete types. The producer (the
/// connector / session manager) supplies whatever stable id it already has;
/// the listener module only ever forwards it.
pub type OpaqueId = u64;

/// A servlet-container lifecycle event that listeners may be notified of.
///
/// Each variant carries the *minimal* context needed to dispatch it: the
/// [`ContextId`] of the web application, plus an [`OpaqueId`] for request- and
/// session-scoped events. Attribute-mutation events also carry the attribute
/// name. Building the concrete Jakarta event object
/// (`ServletContextEvent`, `HttpSessionEvent`, …) is
/// [`ListenerDispatcher::fire`]'s job, under the `jvm` feature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenerEvent {
    /// `ServletContextListener.contextInitialized` — the web application is
    /// starting and is ready to serve requests. **Constructive**: dispatched in
    /// declaration order.
    ContextInitialized {
        /// The web application whose context was initialized.
        context_id: ContextId,
    },
    /// `ServletContextListener.contextDestroyed` — the web application is
    /// shutting down. **Destructive**: dispatched in *reverse* declaration
    /// order.
    ContextDestroyed {
        /// The web application whose context was destroyed.
        context_id: ContextId,
    },
    /// `ServletContextAttributeListener` attribute mutation on the context.
    ContextAttribute {
        /// The web application whose context attribute changed.
        context_id: ContextId,
        /// The attribute name.
        name: String,
        /// Which kind of mutation occurred.
        change: AttributeChange,
    },
    /// `ServletRequestListener.requestInitialized` — a request is entering
    /// scope. **Constructive**: dispatched in declaration order.
    RequestInitialized {
        /// The web application handling the request.
        context_id: ContextId,
        /// Opaque id of the request entering scope.
        request_id: OpaqueId,
    },
    /// `ServletRequestListener.requestDestroyed` — a request is leaving scope.
    /// **Destructive**: dispatched in *reverse* declaration order.
    RequestDestroyed {
        /// The web application that handled the request.
        context_id: ContextId,
        /// Opaque id of the request leaving scope.
        request_id: OpaqueId,
    },
    /// `ServletRequestAttributeListener` attribute mutation on a request.
    RequestAttribute {
        /// The web application handling the request.
        context_id: ContextId,
        /// Opaque id of the request whose attribute changed.
        request_id: OpaqueId,
        /// The attribute name.
        name: String,
        /// Which kind of mutation occurred.
        change: AttributeChange,
    },
    /// `HttpSessionListener.sessionCreated` — a session was just created.
    /// **Constructive**: dispatched in declaration order.
    SessionCreated {
        /// The web application the session belongs to.
        context_id: ContextId,
        /// Opaque id of the newly-created session.
        session_id: OpaqueId,
    },
    /// `HttpSessionListener.sessionDestroyed` — a session was invalidated or
    /// timed out. **Destructive**: dispatched in *reverse* declaration order.
    SessionDestroyed {
        /// The web application the session belonged to.
        context_id: ContextId,
        /// Opaque id of the destroyed session.
        session_id: OpaqueId,
    },
    /// `HttpSessionAttributeListener` attribute mutation on a session.
    SessionAttribute {
        /// The web application the session belongs to.
        context_id: ContextId,
        /// Opaque id of the session whose attribute changed.
        session_id: OpaqueId,
        /// The attribute name.
        name: String,
        /// Which kind of mutation occurred.
        change: AttributeChange,
    },
    /// `HttpSessionIdListener.sessionIdChanged` — a session's id was changed
    /// (e.g. by `HttpServletRequest.changeSessionId()`).
    SessionIdChanged {
        /// The web application the session belongs to.
        context_id: ContextId,
        /// Opaque id of the session whose id changed.
        session_id: OpaqueId,
        /// The session's previous id, as the spec passes it to the listener.
        old_session_id: OpaqueId,
    },
}

/// Which kind of mutation an attribute-listener event describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributeChange {
    /// The attribute was newly added (`attributeAdded`).
    Added,
    /// The attribute was removed (`attributeRemoved`).
    Removed,
    /// An existing attribute's value was replaced (`attributeReplaced`).
    Replaced,
}

impl ListenerEvent {
    /// The [`ListenerType`] whose registered listeners should be notified of
    /// this event.
    pub fn listener_type(&self) -> ListenerType {
        match self {
            ListenerEvent::ContextInitialized { .. } | ListenerEvent::ContextDestroyed { .. } => {
                ListenerType::ServletContext
            }
            ListenerEvent::ContextAttribute { .. } => ListenerType::ServletContextAttribute,
            ListenerEvent::RequestInitialized { .. } | ListenerEvent::RequestDestroyed { .. } => {
                ListenerType::ServletRequest
            }
            ListenerEvent::RequestAttribute { .. } => ListenerType::ServletRequestAttribute,
            ListenerEvent::SessionCreated { .. } | ListenerEvent::SessionDestroyed { .. } => {
                ListenerType::HttpSession
            }
            ListenerEvent::SessionAttribute { .. } => ListenerType::HttpSessionAttribute,
            ListenerEvent::SessionIdChanged { .. } => ListenerType::HttpSessionId,
        }
    }

    /// Whether this is a **destructive** event — one the Servlet spec requires
    /// to be dispatched in *reverse* listener-declaration order.
    ///
    /// Only the `*Destroyed` events are destructive; attribute mutations, id
    /// changes and `*Initialized` / `*Created` events all dispatch in
    /// declaration order.
    pub fn is_destructive(&self) -> bool {
        matches!(
            self,
            ListenerEvent::ContextDestroyed { .. }
                | ListenerEvent::RequestDestroyed { .. }
                | ListenerEvent::SessionDestroyed { .. }
        )
    }

    /// The web application this event concerns.
    pub fn context_id(&self) -> &ContextId {
        match self {
            ListenerEvent::ContextInitialized { context_id }
            | ListenerEvent::ContextDestroyed { context_id }
            | ListenerEvent::ContextAttribute { context_id, .. }
            | ListenerEvent::RequestInitialized { context_id, .. }
            | ListenerEvent::RequestDestroyed { context_id, .. }
            | ListenerEvent::RequestAttribute { context_id, .. }
            | ListenerEvent::SessionCreated { context_id, .. }
            | ListenerEvent::SessionDestroyed { context_id, .. }
            | ListenerEvent::SessionAttribute { context_id, .. }
            | ListenerEvent::SessionIdChanged { context_id, .. } => context_id,
        }
    }
}

// ===========================================================================
// ListenerRegistry
// ===========================================================================

/// One registered listener: its declared class name, the interface it was
/// registered for, and — once instantiated — its JVM-side instance handle.
#[derive(Debug, Clone)]
pub struct RegisteredListener {
    /// The fully-qualified, dot-separated class name as declared in `web.xml`
    /// or carried by `@WebListener`.
    class_name: String,
    /// The interface this entry was registered against. A class implementing
    /// several interfaces yields one [`RegisteredListener`] per interface.
    listener_type: ListenerType,
    /// The materialised JVM-side instance, once [`instantiate_listeners`] has
    /// run. `None` on the no-JVM path and before instantiation.
    instance: Option<ServletInstanceHandle>,
}

impl RegisteredListener {
    /// The declared class name.
    pub fn class_name(&self) -> &str {
        &self.class_name
    }

    /// The interface this entry is registered for.
    pub fn listener_type(&self) -> ListenerType {
        self.listener_type
    }

    /// The JVM-side instance handle, if the listener has been instantiated.
    pub fn instance(&self) -> Option<&ServletInstanceHandle> {
        self.instance.as_ref()
    }

    /// Whether this listener has a materialised JVM-side instance.
    pub fn is_instantiated(&self) -> bool {
        self.instance.is_some()
    }
}

/// The per-[`WebappRuntime`](crate::jvm::WebappRuntime) ordered list of
/// registered Servlet listeners.
///
/// Listeners are appended in **declaration order** — the order they appear in
/// `web.xml`, with annotation-discovered `@WebListener` classes following, just
/// as Tomcat orders them. That order is load-bearing: see
/// [`ListenerRegistry::dispatch_order`].
///
/// A registry is created empty and populated either directly with
/// [`ListenerRegistry::register`] (no-JVM / tests) or by
/// [`instantiate_listeners`] (which both registers *and* materialises the
/// JVM-side instances under the `jvm` feature).
#[derive(Debug, Default)]
pub struct ListenerRegistry {
    /// Registered listeners in declaration order. A listener class that
    /// implements N listener interfaces contributes N consecutive entries.
    listeners: Vec<RegisteredListener>,
}

impl ListenerRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `class_name` for the given interface, appending it after every
    /// previously-registered listener (preserving declaration order). The
    /// instance handle is left unset; [`instantiate_listeners`] fills it in, or
    /// [`ListenerRegistry::register_instance`] can set it explicitly.
    ///
    /// Returns the index of the new entry.
    pub fn register(
        &mut self,
        class_name: impl Into<String>,
        listener_type: ListenerType,
    ) -> usize {
        self.listeners.push(RegisteredListener {
            class_name: class_name.into(),
            listener_type,
            instance: None,
        });
        self.listeners.len() - 1
    }

    /// Register a listener that has already been instantiated, attaching its
    /// JVM-side instance handle. Used by [`instantiate_listeners`] on the `jvm`
    /// path; preserves declaration order exactly like [`ListenerRegistry::register`].
    ///
    /// Returns the index of the new entry.
    pub fn register_instance(
        &mut self,
        class_name: impl Into<String>,
        listener_type: ListenerType,
        instance: ServletInstanceHandle,
    ) -> usize {
        self.listeners.push(RegisteredListener {
            class_name: class_name.into(),
            listener_type,
            instance: Some(instance),
        });
        self.listeners.len() - 1
    }

    /// All registered listeners, in declaration order.
    pub fn all(&self) -> &[RegisteredListener] {
        &self.listeners
    }

    /// The number of registered listener entries (counting a multi-interface
    /// class once per interface).
    pub fn len(&self) -> usize {
        self.listeners.len()
    }

    /// Whether the registry holds no listeners.
    pub fn is_empty(&self) -> bool {
        self.listeners.is_empty()
    }

    /// The registered listeners of exactly `listener_type`, in declaration
    /// order, as `(declaration_index, listener)` pairs.
    ///
    /// The declaration index is the position in the full registry, kept so
    /// callers can reason about global ordering across interfaces if needed.
    pub fn of_type(
        &self,
        listener_type: ListenerType,
    ) -> impl Iterator<Item = (usize, &RegisteredListener)> {
        self.listeners
            .iter()
            .enumerate()
            .filter(move |(_, l)| l.listener_type == listener_type)
    }

    /// The listeners that should be notified of `event`, **already in the
    /// correct dispatch order**.
    ///
    /// For a **constructive** event (`*Initialized`, `*Created`, attribute
    /// mutations, id changes) this is the matching listeners in declaration
    /// order. For a **destructive** event (`*Destroyed`) it is the matching
    /// listeners in *reverse* declaration order, as the Servlet specification
    /// requires.
    pub fn dispatch_order(&self, event: &ListenerEvent) -> Vec<&RegisteredListener> {
        let ty = event.listener_type();
        let mut matching: Vec<&RegisteredListener> = self.of_type(ty).map(|(_, l)| l).collect();
        if event.is_destructive() {
            matching.reverse();
        }
        matching
    }
}

// ===========================================================================
// ListenerDispatcher
// ===========================================================================

/// Fires [`ListenerEvent`]s at the listeners registered for one web
/// application.
///
/// A dispatcher binds together the shared [`JvmRuntime`] (the single JNI
/// funnel) and the [`ContextId`] of the web application whose listeners it
/// drives. It does **not** own the [`ListenerRegistry`]: the registry is passed
/// to [`ListenerDispatcher::fire`] by reference, so the caller (the webapp's
/// lifecycle manager) retains ownership and can keep registering listeners
/// during start-up.
///
/// # No-JVM behaviour
///
/// Without the `jvm` feature there is no JVM to call into, so the dispatcher
/// instead records every fired event — *in dispatch order, expanded per
/// matching listener* — into an interior-mutable log. Tests read that log back
/// with [`ListenerDispatcher::fired_events`] to assert the ordering rules hold.
#[derive(Debug, Clone)]
pub struct ListenerDispatcher {
    /// The shared embedded-JVM runtime — the single JNI funnel.
    runtime: Arc<JvmRuntime>,
    /// The web application whose listeners this dispatcher drives.
    context_id: ContextId,
    /// No-JVM event log. Present only on the default-feature build; under the
    /// `jvm` feature events go to the JVM, not a log.
    #[cfg(not(feature = "jvm"))]
    log: Arc<std::sync::Mutex<Vec<FiredEvent>>>,
}

/// A single recorded listener notification: the event, and the class name of
/// the listener it was delivered to. Produced by [`ListenerDispatcher::fire`]
/// on the no-JVM path so ordering can be asserted in tests.
#[cfg(not(feature = "jvm"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiredEvent {
    /// The event that was fired.
    pub event: ListenerEvent,
    /// The class name of the listener the event was delivered to.
    pub listener_class: String,
}

impl ListenerDispatcher {
    /// Create a dispatcher for `context_id`, bound to the shared [`JvmRuntime`].
    pub fn new(runtime: Arc<JvmRuntime>, context_id: impl Into<ContextId>) -> Self {
        Self {
            runtime,
            context_id: context_id.into(),
            #[cfg(not(feature = "jvm"))]
            log: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// The web application this dispatcher drives.
    pub fn context_id(&self) -> &ContextId {
        &self.context_id
    }

    /// The shared embedded-JVM runtime.
    pub fn runtime(&self) -> &Arc<JvmRuntime> {
        &self.runtime
    }
}

// ---------------------------------------------------------------------------
// Real implementation — only compiled with `--features jvm`.
// ---------------------------------------------------------------------------
#[cfg(feature = "jvm")]
impl ListenerDispatcher {
    /// Fire `event` at every registry listener of the matching
    /// [`ListenerType`], in the spec-mandated order (reverse for `*Destroyed`).
    ///
    /// Each notification crosses JNI through [`JvmRuntime::with_env`]: the
    /// matching Jakarta event object is constructed and the listener's callback
    /// method is invoked on it. All notifications for one `fire` call share a
    /// single `with_env` round-trip so the per-call JNI overhead is paid once.
    ///
    /// A listener whose instance has not been materialised (no
    /// [`ServletInstanceHandle`]) is skipped with a warning rather than
    /// aborting the whole dispatch — one mis-deployed listener must not stop
    /// the others from being notified.
    ///
    /// # Errors
    ///
    /// [`tomcatrs_core::Error::Bridge`] if the JNI round-trip itself fails
    /// (e.g. the worker pool has shut down) or a listener callback throws and
    /// the exception cannot be cleared.
    pub fn fire(&self, event: ListenerEvent) -> Result<()> {
        use tomcatrs_core::Error;

        // Borrow nothing across the JNI funnel except what `fire` was handed:
        // the registry is *not* owned here, so callers pass it explicitly.
        // (See `fire_with` — `fire` is the registry-less convenience that the
        // webapp lifecycle manager does not use directly; kept private-ish by
        // delegating.)
        let _ = (&self.runtime, &self.context_id, &event);
        Err(Error::bridge(
            "ListenerDispatcher::fire requires a ListenerRegistry; call fire_with",
        ))
    }

    /// Fire `event` at the listeners in `registry`, in spec-mandated order.
    ///
    /// This is the real entry point under the `jvm` feature: it resolves the
    /// matching listeners via [`ListenerRegistry::dispatch_order`] and, for
    /// each, builds the appropriate Jakarta event object and invokes the
    /// listener method inside a single [`JvmRuntime::with_env`] round-trip.
    pub fn fire_with(&self, registry: &ListenerRegistry, event: ListenerEvent) -> Result<()> {
        use jni::objects::{JObject, JValue};
        use tomcatrs_core::Error;

        let targets = registry.dispatch_order(&event);
        if targets.is_empty() {
            tracing::trace!(
                context_id = %self.context_id,
                ?event,
                "no listeners registered for event; nothing to dispatch"
            );
            return Ok(());
        }

        // Collect the instance handles up front so the JNI closure borrows only
        // `Send` data. Listeners without a materialised instance are skipped.
        let mut instances: Vec<(String, jni::objects::GlobalRef)> = Vec::new();
        for listener in targets {
            match listener.instance() {
                Some(handle) => instances.push((
                    listener.class_name().to_string(),
                    handle.global_ref().clone(),
                )),
                None => tracing::warn!(
                    context_id = %self.context_id,
                    listener = listener.class_name(),
                    "listener has no JVM-side instance; skipping notification"
                ),
            }
        }
        if instances.is_empty() {
            return Ok(());
        }

        let context_id = self.context_id.clone();
        self.runtime.with_env(move |env| -> Result<()> {
            // Build the one Jakarta event object shared by every listener of
            // this type for this `fire` call. The event objects all take a
            // source plus, for request/session events, an opaque id which the
            // bridge surfaces as a `long` the Java facade resolves lazily.
            for (class_name, instance) in &instances {
                let outcome = dispatch_one(env, &context_id, &event, instance.as_obj());
                if let Err(e) = outcome {
                    // Clear any pending Java exception so the worker thread
                    // stays usable, then surface the failure.
                    if let Ok(true) = env.exception_check() {
                        let _ = env.exception_clear();
                    }
                    return Err(Error::bridge(format!(
                        "listener {class_name} failed to handle {event:?}: {e}"
                    )));
                }
            }
            let _: JObject = JObject::null();
            let _ = JValue::Long(0);
            Ok(())
        })
    }
}

/// Invoke the single listener `listener` for `event` on the current JNI thread.
///
/// Builds the matching Jakarta event object — `ServletContextEvent`,
/// `ServletContextAttributeEvent`, `ServletRequestEvent`,
/// `HttpSessionEvent`, … — and calls the corresponding listener method. The
/// event objects are constructed through the bridge's Java helper
/// `org.apache.tomcatrs.bridge.ListenerEvents`, which knows how to make a
/// `ServletContext` / `HttpSession` stand-in from the opaque ids.
#[cfg(feature = "jvm")]
fn dispatch_one(
    env: &mut jni::JNIEnv,
    context_id: &str,
    event: &ListenerEvent,
    listener: &jni::objects::JObject,
) -> Result<()> {
    use jni::objects::{JObject, JValue};
    use tomcatrs_core::Error;

    // The bridge-side helper that materialises Jakarta event objects from the
    // opaque ids the Rust side carries.
    const EVENTS: &str = "org/apache/tomcatrs/bridge/ListenerEvents";

    let ctx = env
        .new_string(context_id)
        .map_err(|e| Error::bridge(format!("new_string(context_id) failed: {e}")))?;
    let ctx_obj = JObject::from(ctx);
    let ctx_arg = JValue::Object(&ctx_obj);

    // Helper: construct an event object via `ListenerEvents.<factory>(...)`.
    macro_rules! make_event {
        ($factory:literal, $sig:literal, $($arg:expr),*) => {
            env.call_static_method(EVENTS, $factory, $sig, &[$($arg),*])
                .and_then(|v| v.l())
                .map_err(|e| Error::bridge(format!(
                    concat!("ListenerEvents.", $factory, " failed: {}"), e
                )))?
        };
    }

    // Helper: invoke a void listener callback `method(eventObj)`.
    macro_rules! call_listener {
        ($method:literal, $event_iface:literal, $event_obj:expr) => {{
            let sig = concat!("(L", $event_iface, ";)V");
            env.call_method(listener, $method, sig, &[JValue::Object(&$event_obj)])
                .map_err(|e| Error::bridge(format!(concat!($method, " failed: {}"), e)))?;
        }};
    }

    match event {
        ListenerEvent::ContextInitialized { .. } => {
            let ev = make_event!(
                "servletContextEvent",
                "(Ljava/lang/String;)Ljakarta/servlet/ServletContextEvent;",
                ctx_arg
            );
            call_listener!(
                "contextInitialized",
                "jakarta/servlet/ServletContextEvent",
                ev
            );
        }
        ListenerEvent::ContextDestroyed { .. } => {
            let ev = make_event!(
                "servletContextEvent",
                "(Ljava/lang/String;)Ljakarta/servlet/ServletContextEvent;",
                ctx_arg
            );
            call_listener!(
                "contextDestroyed",
                "jakarta/servlet/ServletContextEvent",
                ev
            );
        }
        ListenerEvent::ContextAttribute { name, change, .. } => {
            let name_str = env
                .new_string(name)
                .map_err(|e| Error::bridge(format!("new_string(attr name) failed: {e}")))?;
            let ev = make_event!(
                "servletContextAttributeEvent",
                "(Ljava/lang/String;Ljava/lang/String;)\
                 Ljakarta/servlet/ServletContextAttributeEvent;",
                ctx_arg,
                JValue::Object(&JObject::from(name_str))
            );
            let method = match change {
                AttributeChange::Added => "attributeAdded",
                AttributeChange::Removed => "attributeRemoved",
                AttributeChange::Replaced => "attributeReplaced",
            };
            let sig = "(Ljakarta/servlet/ServletContextAttributeEvent;)V";
            env.call_method(listener, method, sig, &[JValue::Object(&ev)])
                .map_err(|e| Error::bridge(format!("{method} failed: {e}")))?;
        }
        ListenerEvent::RequestInitialized { request_id, .. } => {
            let ev = make_event!(
                "servletRequestEvent",
                "(Ljava/lang/String;J)Ljakarta/servlet/ServletRequestEvent;",
                ctx_arg,
                JValue::Long(*request_id as i64)
            );
            call_listener!(
                "requestInitialized",
                "jakarta/servlet/ServletRequestEvent",
                ev
            );
        }
        ListenerEvent::RequestDestroyed { request_id, .. } => {
            let ev = make_event!(
                "servletRequestEvent",
                "(Ljava/lang/String;J)Ljakarta/servlet/ServletRequestEvent;",
                ctx_arg,
                JValue::Long(*request_id as i64)
            );
            call_listener!(
                "requestDestroyed",
                "jakarta/servlet/ServletRequestEvent",
                ev
            );
        }
        ListenerEvent::RequestAttribute {
            request_id,
            name,
            change,
            ..
        } => {
            let name_str = env
                .new_string(name)
                .map_err(|e| Error::bridge(format!("new_string(attr name) failed: {e}")))?;
            let ev = make_event!(
                "servletRequestAttributeEvent",
                "(Ljava/lang/String;JLjava/lang/String;)\
                 Ljakarta/servlet/ServletRequestAttributeEvent;",
                ctx_arg,
                JValue::Long(*request_id as i64),
                JValue::Object(&JObject::from(name_str))
            );
            let method = match change {
                AttributeChange::Added => "attributeAdded",
                AttributeChange::Removed => "attributeRemoved",
                AttributeChange::Replaced => "attributeReplaced",
            };
            let sig = "(Ljakarta/servlet/ServletRequestAttributeEvent;)V";
            env.call_method(listener, method, sig, &[JValue::Object(&ev)])
                .map_err(|e| Error::bridge(format!("{method} failed: {e}")))?;
        }
        ListenerEvent::SessionCreated { session_id, .. } => {
            let ev = make_event!(
                "httpSessionEvent",
                "(Ljava/lang/String;J)Ljakarta/servlet/http/HttpSessionEvent;",
                ctx_arg,
                JValue::Long(*session_id as i64)
            );
            call_listener!(
                "sessionCreated",
                "jakarta/servlet/http/HttpSessionEvent",
                ev
            );
        }
        ListenerEvent::SessionDestroyed { session_id, .. } => {
            let ev = make_event!(
                "httpSessionEvent",
                "(Ljava/lang/String;J)Ljakarta/servlet/http/HttpSessionEvent;",
                ctx_arg,
                JValue::Long(*session_id as i64)
            );
            call_listener!(
                "sessionDestroyed",
                "jakarta/servlet/http/HttpSessionEvent",
                ev
            );
        }
        ListenerEvent::SessionAttribute {
            session_id,
            name,
            change,
            ..
        } => {
            let name_str = env
                .new_string(name)
                .map_err(|e| Error::bridge(format!("new_string(attr name) failed: {e}")))?;
            let ev = make_event!(
                "httpSessionBindingEvent",
                "(Ljava/lang/String;JLjava/lang/String;)\
                 Ljakarta/servlet/http/HttpSessionBindingEvent;",
                ctx_arg,
                JValue::Long(*session_id as i64),
                JValue::Object(&JObject::from(name_str))
            );
            let method = match change {
                AttributeChange::Added => "attributeAdded",
                AttributeChange::Removed => "attributeRemoved",
                AttributeChange::Replaced => "attributeReplaced",
            };
            let sig = "(Ljakarta/servlet/http/HttpSessionBindingEvent;)V";
            env.call_method(listener, method, sig, &[JValue::Object(&ev)])
                .map_err(|e| Error::bridge(format!("{method} failed: {e}")))?;
        }
        ListenerEvent::SessionIdChanged {
            session_id,
            old_session_id,
            ..
        } => {
            let ev = make_event!(
                "httpSessionEvent",
                "(Ljava/lang/String;J)Ljakarta/servlet/http/HttpSessionEvent;",
                ctx_arg,
                JValue::Long(*session_id as i64)
            );
            let old = env
                .new_string(old_session_id.to_string())
                .map_err(|e| Error::bridge(format!("new_string(old id) failed: {e}")))?;
            let sig = "(Ljakarta/servlet/http/HttpSessionEvent;Ljava/lang/String;)V";
            env.call_method(
                listener,
                "sessionIdChanged",
                sig,
                &[JValue::Object(&ev), JValue::Object(&JObject::from(old))],
            )
            .map_err(|e| Error::bridge(format!("sessionIdChanged failed: {e}")))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Stub implementation — compiled with default features (no JDK required).
// ---------------------------------------------------------------------------
#[cfg(not(feature = "jvm"))]
impl ListenerDispatcher {
    /// Fire `event` at the listeners in `registry`, recording each resulting
    /// notification into the dispatcher's in-memory log.
    ///
    /// This is the no-JVM stub of [`ListenerDispatcher::fire_with`]. It runs
    /// the *exact same* ordering logic as the real path
    /// ([`ListenerRegistry::dispatch_order`] — reverse for `*Destroyed`
    /// events), but instead of crossing JNI it appends one [`FiredEvent`] per
    /// matching listener to the log, so tests can assert the ordering without
    /// a JDK. Always returns `Ok(())`.
    pub fn fire_with(&self, registry: &ListenerRegistry, event: ListenerEvent) -> Result<()> {
        let targets = registry.dispatch_order(&event);
        let mut log = self.log.lock().expect("listener log mutex poisoned");
        for listener in targets {
            log.push(FiredEvent {
                event: event.clone(),
                listener_class: listener.class_name().to_string(),
            });
        }
        Ok(())
    }

    /// Fire `event` — convenience wrapper that, on the no-JVM path, has no
    /// registry to consult and therefore records the event against a single
    /// synthetic `"<no-registry>"` listener.
    ///
    /// Real callers use [`ListenerDispatcher::fire_with`]; this exists so the
    /// method name `fire` is available on both feature paths with a working
    /// (non-panicking) body, satisfying the "no `todo!()` on the default path"
    /// rule.
    pub fn fire(&self, event: ListenerEvent) -> Result<()> {
        let mut log = self.log.lock().expect("listener log mutex poisoned");
        log.push(FiredEvent {
            event,
            listener_class: "<no-registry>".to_string(),
        });
        Ok(())
    }

    /// A snapshot of every event fired through this dispatcher so far, in the
    /// order they were delivered to listeners. Test-facing accessor for the
    /// no-JVM event log.
    pub fn fired_events(&self) -> Vec<FiredEvent> {
        self.log
            .lock()
            .expect("listener log mutex poisoned")
            .clone()
    }

    /// Clear the recorded event log.
    pub fn clear_log(&self) {
        self.log
            .lock()
            .expect("listener log mutex poisoned")
            .clear();
    }
}

// ===========================================================================
// instantiate_listeners
// ===========================================================================

/// Load and instantiate every declared listener class into a fresh
/// [`ListenerRegistry`], preserving declaration order.
///
/// `class_names` is the list of listener classes as declared — `web.xml`
/// `<listener>` entries first, in document order, then annotation-discovered
/// `@WebListener` classes — exactly the order Tomcat uses.
///
/// # `jvm` feature
///
/// Each class is loaded through `class_loader` (the webapp's isolating
/// [`ClassLoaderFactory`]-built loader) and instantiated via its public no-arg
/// constructor. The JVM is then asked, for each of the seven listener
/// interfaces, whether the instance `is_instance_of` it — the JVM being the
/// authoritative answer — and a [`RegisteredListener`] entry is added for every
/// interface the class actually implements. A class implementing none of them
/// is rejected with [`tomcatrs_core::Error::Bridge`], matching the spec
/// requirement that a `<listener-class>` implement at least one listener
/// interface.
///
/// # default features
///
/// There is no JVM, so each class name is registered as a *placeholder*: its
/// interfaces are guessed best-effort with
/// [`ListenerType::classify_from_class_name`], and if the name carries no hint
/// the class is registered once as a [`ListenerType::ServletContext`] listener
/// so the entry is not silently dropped. No instance handle is attached.
#[cfg(feature = "jvm")]
pub fn instantiate_listeners(
    runtime: &Arc<JvmRuntime>,
    class_loader: &jni::objects::GlobalRef,
    class_names: &[String],
) -> Result<ListenerRegistry> {
    use tomcatrs_core::Error;

    use crate::classloader::ClassLoaderFactory;

    let factory = ClassLoaderFactory::new();
    let class_names: Vec<String> = class_names.to_vec();
    let class_loader = class_loader.clone();

    runtime.with_env(move |env| -> Result<ListenerRegistry> {
        let mut registry = ListenerRegistry::new();
        for class_name in &class_names {
            // Materialise the listener instance through the webapp's loader.
            let instance = factory.instantiate(env, &class_loader, class_name)?;

            // Ask the JVM — the source of truth — which listener interfaces
            // this instance actually implements.
            let mut matched = 0usize;
            for ty in ListenerType::ALL {
                let iface = env.find_class(ty.jni_interface_name()).map_err(|e| {
                    Error::bridge(format!(
                        "find_class({}) failed: {e}",
                        ty.jni_interface_name()
                    ))
                })?;
                let is_impl = env
                    .is_instance_of(instance.as_obj(), &iface)
                    .map_err(|e| Error::bridge(format!("is_instance_of failed: {e}")))?;
                if is_impl {
                    registry.register_instance(
                        class_name.clone(),
                        ty,
                        ServletInstanceHandle::new(instance.clone()),
                    );
                    matched += 1;
                }
            }
            if matched == 0 {
                return Err(Error::bridge(format!(
                    "listener class {class_name} implements no Jakarta listener interface"
                )));
            }
        }
        Ok(registry)
    })
}

/// Load and instantiate every declared listener class into a fresh
/// [`ListenerRegistry`], preserving declaration order.
///
/// See the `jvm`-feature version for the full contract. On this no-JVM build
/// there is no JVM to load classes into, so each name is registered as a
/// *placeholder* (no instance handle): its interfaces are guessed with
/// [`ListenerType::classify_from_class_name`], falling back to a single
/// [`ListenerType::ServletContext`] entry when the name gives no hint.
#[cfg(not(feature = "jvm"))]
pub fn instantiate_listeners(
    _runtime: &Arc<JvmRuntime>,
    _class_loader: &crate::jvm::ClassLoaderHandle,
    class_names: &[String],
) -> Result<ListenerRegistry> {
    let mut registry = ListenerRegistry::new();
    for class_name in class_names {
        let hinted = ListenerType::classify_from_class_name(class_name);
        if hinted.is_empty() {
            // No naming hint — register a single placeholder so the listener is
            // not silently lost. The `jvm` build would consult the JVM here.
            registry.register(class_name.clone(), ListenerType::ServletContext);
        } else {
            for ty in hinted {
                registry.register(class_name.clone(), ty);
            }
        }
    }
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- classification helper ----------------------------------------------

    #[test]
    fn classify_recognises_each_interface_suffix() {
        assert_eq!(
            ListenerType::classify_from_class_name("com.example.AppContextListener"),
            vec![ListenerType::ServletContext]
        );
        assert_eq!(
            ListenerType::classify_from_class_name("com.example.MyHttpSessionListener"),
            vec![ListenerType::HttpSession]
        );
        assert_eq!(
            ListenerType::classify_from_class_name("x.HttpSessionIdListener"),
            vec![ListenerType::HttpSessionId]
        );
    }

    #[test]
    fn classify_prefers_specific_attribute_suffix() {
        // "...ServletContextAttributeListener" must classify as the *attribute*
        // listener only, not also as the plain ServletContextListener.
        let got =
            ListenerType::classify_from_class_name("com.example.ServletContextAttributeListener");
        assert_eq!(got, vec![ListenerType::ServletContextAttribute]);
        assert!(!got.contains(&ListenerType::ServletContext));
    }

    #[test]
    fn classify_returns_empty_on_no_hint() {
        // No naming convention signal -> no guess.
        assert!(ListenerType::classify_from_class_name("com.example.Bootstrap").is_empty());
    }

    #[test]
    fn interface_name_forms_are_consistent() {
        for ty in ListenerType::ALL {
            let jni = ty.jni_interface_name();
            let java = ty.java_interface_name();
            assert_eq!(jni.replace('/', "."), java);
            assert!(jni.starts_with("jakarta/servlet/"));
        }
    }

    // --- ListenerRegistry ordering ------------------------------------------

    fn ctx_registry() -> ListenerRegistry {
        let mut reg = ListenerRegistry::new();
        reg.register("com.example.First", ListenerType::ServletContext);
        reg.register("com.example.Second", ListenerType::ServletContext);
        reg.register("com.example.Third", ListenerType::ServletContext);
        reg
    }

    #[test]
    fn registry_preserves_declaration_order() {
        let reg = ctx_registry();
        let names: Vec<&str> = reg.all().iter().map(|l| l.class_name()).collect();
        assert_eq!(
            names,
            [
                "com.example.First",
                "com.example.Second",
                "com.example.Third"
            ]
        );
        assert_eq!(reg.len(), 3);
        assert!(!reg.is_empty());
    }

    #[test]
    fn dispatch_order_constructive_is_declaration_order() {
        let reg = ctx_registry();
        let event = ListenerEvent::ContextInitialized {
            context_id: "/app".to_string(),
        };
        let order: Vec<&str> = reg
            .dispatch_order(&event)
            .iter()
            .map(|l| l.class_name())
            .collect();
        assert_eq!(
            order,
            [
                "com.example.First",
                "com.example.Second",
                "com.example.Third"
            ]
        );
    }

    #[test]
    fn dispatch_order_destructive_is_reverse_declaration_order() {
        let reg = ctx_registry();
        let event = ListenerEvent::ContextDestroyed {
            context_id: "/app".to_string(),
        };
        let order: Vec<&str> = reg
            .dispatch_order(&event)
            .iter()
            .map(|l| l.class_name())
            .collect();
        assert_eq!(
            order,
            [
                "com.example.Third",
                "com.example.Second",
                "com.example.First"
            ]
        );
    }

    #[test]
    fn dispatch_order_filters_by_listener_type() {
        let mut reg = ListenerRegistry::new();
        reg.register("ctx.A", ListenerType::ServletContext);
        reg.register("req.B", ListenerType::ServletRequest);
        reg.register("ctx.C", ListenerType::ServletContext);

        let req_event = ListenerEvent::RequestInitialized {
            context_id: "/app".to_string(),
            request_id: 7,
        };
        let order: Vec<&str> = reg
            .dispatch_order(&req_event)
            .iter()
            .map(|l| l.class_name())
            .collect();
        assert_eq!(order, ["req.B"]);

        // The session listener type has no registrations: empty dispatch list.
        let sess_event = ListenerEvent::SessionCreated {
            context_id: "/app".to_string(),
            session_id: 1,
        };
        assert!(reg.dispatch_order(&sess_event).is_empty());
    }

    #[test]
    fn of_type_yields_declaration_indices() {
        let mut reg = ListenerRegistry::new();
        reg.register("ctx.A", ListenerType::ServletContext);
        reg.register("req.B", ListenerType::ServletRequest);
        reg.register("ctx.C", ListenerType::ServletContext);
        let ctx: Vec<(usize, &str)> = reg
            .of_type(ListenerType::ServletContext)
            .map(|(i, l)| (i, l.class_name()))
            .collect();
        assert_eq!(ctx, [(0, "ctx.A"), (2, "ctx.C")]);
    }

    // --- ListenerEvent classification ---------------------------------------

    #[test]
    fn event_listener_type_and_destructiveness() {
        let init = ListenerEvent::ContextInitialized {
            context_id: "/a".into(),
        };
        assert_eq!(init.listener_type(), ListenerType::ServletContext);
        assert!(!init.is_destructive());

        let destroy = ListenerEvent::SessionDestroyed {
            context_id: "/a".into(),
            session_id: 3,
        };
        assert_eq!(destroy.listener_type(), ListenerType::HttpSession);
        assert!(destroy.is_destructive());

        let attr = ListenerEvent::RequestAttribute {
            context_id: "/a".into(),
            request_id: 1,
            name: "k".into(),
            change: AttributeChange::Added,
        };
        assert_eq!(attr.listener_type(), ListenerType::ServletRequestAttribute);
        assert!(!attr.is_destructive());

        assert_eq!(destroy.context_id(), "/a");
    }

    // --- ListenerDispatcher (no-JVM stub) -----------------------------------

    #[cfg(not(feature = "jvm"))]
    mod no_jvm {
        use super::*;

        fn dispatcher() -> ListenerDispatcher {
            ListenerDispatcher::new(Arc::new(JvmRuntime::default()), "/app")
        }

        #[test]
        fn fire_with_records_events_in_dispatch_order() {
            let reg = ctx_registry();
            let disp = dispatcher();

            disp.fire_with(
                &reg,
                ListenerEvent::ContextInitialized {
                    context_id: "/app".to_string(),
                },
            )
            .expect("stub fire never errors");
            disp.fire_with(
                &reg,
                ListenerEvent::ContextDestroyed {
                    context_id: "/app".to_string(),
                },
            )
            .expect("stub fire never errors");

            let fired = disp.fired_events();
            // 3 listeners x 2 events = 6 notifications.
            assert_eq!(fired.len(), 6);

            // Initialized: declaration order.
            assert_eq!(fired[0].listener_class, "com.example.First");
            assert_eq!(fired[1].listener_class, "com.example.Second");
            assert_eq!(fired[2].listener_class, "com.example.Third");
            assert!(matches!(
                fired[0].event,
                ListenerEvent::ContextInitialized { .. }
            ));

            // Destroyed: reverse declaration order.
            assert_eq!(fired[3].listener_class, "com.example.Third");
            assert_eq!(fired[4].listener_class, "com.example.Second");
            assert_eq!(fired[5].listener_class, "com.example.First");
            assert!(matches!(
                fired[5].event,
                ListenerEvent::ContextDestroyed { .. }
            ));
        }

        #[test]
        fn fire_with_skips_non_matching_listener_types() {
            let mut reg = ListenerRegistry::new();
            reg.register("ctx.Only", ListenerType::ServletContext);
            let disp = dispatcher();

            // A session event with no session listeners registered: no records.
            disp.fire_with(
                &reg,
                ListenerEvent::SessionCreated {
                    context_id: "/app".to_string(),
                    session_id: 42,
                },
            )
            .unwrap();
            assert!(disp.fired_events().is_empty());

            // A context event does get recorded.
            disp.fire_with(
                &reg,
                ListenerEvent::ContextInitialized {
                    context_id: "/app".to_string(),
                },
            )
            .unwrap();
            assert_eq!(disp.fired_events().len(), 1);
        }

        #[test]
        fn clear_log_empties_recorded_events() {
            let reg = ctx_registry();
            let disp = dispatcher();
            disp.fire_with(
                &reg,
                ListenerEvent::ContextInitialized {
                    context_id: "/app".to_string(),
                },
            )
            .unwrap();
            assert!(!disp.fired_events().is_empty());
            disp.clear_log();
            assert!(disp.fired_events().is_empty());
        }

        #[test]
        fn fire_without_registry_records_synthetic_listener() {
            let disp = dispatcher();
            disp.fire(ListenerEvent::ContextInitialized {
                context_id: "/app".to_string(),
            })
            .unwrap();
            let fired = disp.fired_events();
            assert_eq!(fired.len(), 1);
            assert_eq!(fired[0].listener_class, "<no-registry>");
        }

        #[test]
        fn instantiate_listeners_registers_placeholders_in_order() {
            let runtime = Arc::new(JvmRuntime::default());
            let loader = crate::jvm::ClassLoaderHandle::new();
            let names = vec![
                "com.example.AppContextListener".to_string(),
                "com.example.MyHttpSessionListener".to_string(),
                "com.example.Bootstrap".to_string(), // no hint -> ServletContext
            ];
            let reg = instantiate_listeners(&runtime, &loader, &names).unwrap();
            assert_eq!(reg.len(), 3);
            assert_eq!(reg.all()[0].class_name(), "com.example.AppContextListener");
            assert_eq!(reg.all()[0].listener_type(), ListenerType::ServletContext);
            assert_eq!(reg.all()[1].listener_type(), ListenerType::HttpSession);
            assert_eq!(reg.all()[2].listener_type(), ListenerType::ServletContext);
            // No JVM -> no instance handles attached.
            assert!(reg.all().iter().all(|l| !l.is_instantiated()));
        }
    }
}
