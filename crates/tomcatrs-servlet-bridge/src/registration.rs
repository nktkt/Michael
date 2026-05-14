//! `web.xml` → JVM-side servlet/filter registration.
//!
//! # Where this sits
//!
//! Once a WAR has been deployed and its `WEB-INF/web.xml` parsed (into a
//! [`tomcatrs_config::web_xml::WebXml`]), *something* has to turn that static
//! description into live JVM-side state: the webapp's isolating class loader,
//! one materialised `jakarta.servlet.Servlet` instance per `<servlet>`
//! (`init()`-ed with its `<init-param>`s), one `jakarta.servlet.Filter` per
//! `<filter>`, and a record of every `<servlet-mapping>` so the connector's
//! mapper can later resolve a URL pattern back to a servlet name.
//!
//! That is what this module does. [`WebappRegistrar`] drives the process
//! against a shared [`JvmRuntime`]; [`RegistrationPlan`] is the pure-data
//! description of *what* to register and *in what order* — extracted from the
//! `WebXml` with no JVM involved, so it is fully unit-testable without a JDK.
//!
//! ```text
//!   WebXml ──RegistrationPlan::from_web_xml──▶ RegistrationPlan
//!                                                  │
//!                          WebappRegistrar::register│
//!                                                  ▼
//!        ┌──────────── #[cfg(feature = "jvm")] ───────────────┐
//!        │  JvmRuntime::with_env(|env| {                      │
//!        │     ClassLoaderFactory: common + webapp loader     │
//!        │     for each servlet: instantiate + init()         │
//!        │     for each filter:  instantiate + init()         │
//!        │  })                                                │
//!        │  store handles in WebappRuntime registries         │
//!        └────────────────────────────────────────────────────┘
//!        ┌──────────── #[cfg(not(feature = "jvm")) ───────────┐
//!        │  record the intended registrations with           │
//!        │  placeholder ServletInstanceHandle values, log     │
//!        └────────────────────────────────────────────────────┘
//!                                                  │
//!                                                  ▼
//!                                          RegistrationSummary
//! ```
//!
//! # Load-on-startup ordering
//!
//! The Servlet specification says servlets with a non-negative
//! `<load-on-startup>` are initialised at deployment time in ascending order of
//! that value; servlets with a negative or absent value may be initialised
//! lazily, in no particular order. [`RegistrationPlan`] orders the servlets
//! exactly the way Tomcat's `StandardContext` does: non-negative
//! `load-on-startup` first (ascending, ties broken by declaration order), then
//! every remaining servlet in declaration order. Filters and listeners keep
//! their declaration order.
//!
//! # Two builds, one API
//!
//! As with the rest of the crate every public item exists on both feature
//! paths. [`RegistrationPlan`] and [`RegistrationSummary`] are
//! feature-independent. [`WebappRegistrar::register`] does the real JNI work
//! under `--features jvm`; on the default path it records the *intended*
//! registrations into the [`WebappRuntime`] registries using placeholder
//! [`ServletInstanceHandle`]s, so the data model, ordering, and registry
//! plumbing are all exercised by `cargo test` with no JDK installed.

use std::collections::HashMap;
use std::sync::Arc;

use tomcatrs_config::web_xml::{FilterDef, FilterMapping, ServletDef, ServletMapping, WebXml};
use tomcatrs_core::{ContextId, Result};

use crate::classloader::WebappClassLoaderConfig;
use crate::jvm::{JvmRuntime, WebappRuntime};

// ===========================================================================
// RegistrationPlan — the pure-data, fully-testable core.
// ===========================================================================

/// One servlet to register, lifted out of a [`ServletDef`] and tagged with the
/// declaration index it had in `web.xml`.
///
/// The plan owns its copy of the declaration so [`WebappRegistrar`] can consult
/// it without re-borrowing the source [`WebXml`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedServlet {
    /// Logical servlet name (`<servlet-name>`), the registry key.
    pub name: String,
    /// Fully-qualified servlet class (`<servlet-class>`).
    pub class: String,
    /// `<load-on-startup>` value, if the descriptor gave one.
    pub load_on_startup: Option<i32>,
    /// `<init-param>` name/value pairs passed to `ServletConfig`.
    pub init_params: HashMap<String, String>,
    /// Zero-based index of this `<servlet>` element in the descriptor, used to
    /// break `load-on-startup` ties and to order the lazy tail.
    pub declaration_index: usize,
}

impl PlannedServlet {
    /// Whether this servlet must be initialised eagerly at deployment time —
    /// i.e. it has a non-negative `<load-on-startup>`.
    pub fn is_eager(&self) -> bool {
        matches!(self.load_on_startup, Some(v) if v >= 0)
    }
}

/// One filter to register, lifted out of a [`FilterDef`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFilter {
    /// Logical filter name (`<filter-name>`), the registry key.
    pub name: String,
    /// Fully-qualified filter class (`<filter-class>`).
    pub class: String,
    /// `<init-param>` name/value pairs passed to `FilterConfig`.
    pub init_params: HashMap<String, String>,
    /// Zero-based index of this `<filter>` element in the descriptor.
    pub declaration_index: usize,
}

/// An ordered, JVM-agnostic description of everything one web application's
/// `web.xml` asks to be registered.
///
/// Built with [`RegistrationPlan::from_web_xml`]. The lists are already in the
/// order [`WebappRegistrar`] should walk them:
///
/// * [`servlets`](Self::servlets) — sorted by `<load-on-startup>` (non-negative
///   ascending, ties and the negative/absent tail broken by declaration order);
/// * [`filters`](Self::filters) — declaration order;
/// * [`listeners`](Self::listeners) — declaration order;
/// * [`servlet_mappings`](Self::servlet_mappings) /
///   [`filter_mappings`](Self::filter_mappings) — declaration order, copied
///   verbatim so the mapper can resolve `url-pattern` → name later.
#[derive(Debug, Clone, Default)]
pub struct RegistrationPlan {
    /// Servlets in initialisation order (see the type docs).
    pub servlets: Vec<PlannedServlet>,
    /// Filters in declaration order.
    pub filters: Vec<PlannedFilter>,
    /// `<listener-class>` names in declaration order.
    pub listeners: Vec<String>,
    /// Every `<servlet-mapping>`, in declaration order.
    pub servlet_mappings: Vec<ServletMapping>,
    /// Every `<filter-mapping>`, in declaration order.
    pub filter_mappings: Vec<FilterMapping>,
}

impl RegistrationPlan {
    /// Build a plan from a parsed `web.xml`.
    ///
    /// Servlets are reordered into Servlet-spec initialisation order; every
    /// other list keeps the descriptor's declaration order. This performs no
    /// I/O and touches no JVM — it is a pure transformation of the input.
    pub fn from_web_xml(web: &WebXml) -> Self {
        let mut servlets: Vec<PlannedServlet> = web
            .servlets
            .iter()
            .enumerate()
            .map(|(i, s)| planned_servlet(s, i))
            .collect();
        servlets.sort_by(|a, b| servlet_order_key(a).cmp(&servlet_order_key(b)));

        let filters: Vec<PlannedFilter> = web
            .filters
            .iter()
            .enumerate()
            .map(|(i, f)| planned_filter(f, i))
            .collect();

        Self {
            servlets,
            filters,
            listeners: web.listeners.clone(),
            servlet_mappings: web.servlet_mappings.clone(),
            filter_mappings: web.filter_mappings.clone(),
        }
    }

    /// Number of servlets in the plan.
    pub fn servlet_count(&self) -> usize {
        self.servlets.len()
    }

    /// Number of filters in the plan.
    pub fn filter_count(&self) -> usize {
        self.filters.len()
    }

    /// Number of lifecycle listeners in the plan.
    pub fn listener_count(&self) -> usize {
        self.listeners.len()
    }

    /// The servlets that have a non-negative `<load-on-startup>` and must be
    /// initialised eagerly at deployment time, in initialisation order.
    pub fn eager_servlets(&self) -> impl Iterator<Item = &PlannedServlet> {
        self.servlets.iter().filter(|s| s.is_eager())
    }
}

/// Convert a [`ServletDef`] (+ its declaration index) into a [`PlannedServlet`].
fn planned_servlet(def: &ServletDef, declaration_index: usize) -> PlannedServlet {
    PlannedServlet {
        name: def.name.clone(),
        class: def.class.clone(),
        load_on_startup: def.load_on_startup,
        init_params: def.init_params.clone(),
        declaration_index,
    }
}

/// Convert a [`FilterDef`] (+ its declaration index) into a [`PlannedFilter`].
fn planned_filter(def: &FilterDef, declaration_index: usize) -> PlannedFilter {
    PlannedFilter {
        name: def.name.clone(),
        class: def.class.clone(),
        init_params: def.init_params.clone(),
        declaration_index,
    }
}

/// The total-order sort key implementing Servlet-spec initialisation order.
///
/// The tuple sorts so that:
///
/// 1. every servlet with a non-negative `<load-on-startup>` (`bucket = 0`)
///    comes before every servlet with a negative or absent one (`bucket = 1`);
/// 2. within the eager bucket, lower `<load-on-startup>` values come first;
/// 3. ties at every level fall back to declaration order.
fn servlet_order_key(s: &PlannedServlet) -> (u8, i32, usize) {
    match s.load_on_startup {
        Some(v) if v >= 0 => (0, v, s.declaration_index),
        // Negative or absent: lazy tail, ordered purely by declaration index.
        // The middle field is irrelevant here but kept `0` for a stable tuple.
        _ => (1, 0, s.declaration_index),
    }
}

// ===========================================================================
// RegistrationSummary — what `register` reports back.
// ===========================================================================

/// The outcome of a [`WebappRegistrar::register`] run: how much of the plan was
/// actually installed into the [`WebappRuntime`].
///
/// On the `jvm` build the counts reflect servlets/filters that were
/// instantiated and `init()`-ed successfully. On the default-feature build they
/// reflect the placeholder handles recorded into the registries — the same
/// bookkeeping, exercised without a JDK.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistrationSummary {
    /// Servlets registered into [`WebappRuntime`]'s servlet registry.
    pub servlets_registered: usize,
    /// Filters registered into [`WebappRuntime`]'s filter registry.
    pub filters_registered: usize,
    /// `<servlet-mapping>`s recorded for the mapper.
    pub servlet_mappings_recorded: usize,
    /// `<filter-mapping>`s recorded for the mapper.
    pub filter_mappings_recorded: usize,
    /// Lifecycle listeners noted in the plan.
    pub listeners: usize,
    /// Whether a real JVM-side class loader was built for the webapp. Always
    /// `false` on the default-feature build (no JVM to build it on).
    pub class_loader_built: bool,
}

impl RegistrationSummary {
    /// Total number of components (servlets + filters) registered.
    pub fn components_registered(&self) -> usize {
        self.servlets_registered + self.filters_registered
    }
}

// ===========================================================================
// ServletMappingTable — url-pattern → servlet-name resolution.
// ===========================================================================

/// The `<servlet-mapping>` records extracted from a `web.xml`, kept so the
/// connector's mapper can later resolve a request's `url-pattern` back to the
/// servlet name whose instance lives in the [`WebappRuntime`] registry.
///
/// [`WebappRuntime`] itself keys servlets by *name*; this table is the missing
/// half — *pattern* → *name* — that the registrar produces alongside the
/// instance registration. It is plain data with no JVM dependency.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServletMappingTable {
    /// `(url-pattern, servlet-name)` pairs, in `web.xml` declaration order.
    entries: Vec<(String, String)>,
}

impl ServletMappingTable {
    /// Build a table from a slice of parsed [`ServletMapping`]s.
    pub fn from_mappings(mappings: &[ServletMapping]) -> Self {
        Self {
            entries: mappings
                .iter()
                .map(|m| (m.url_pattern.clone(), m.servlet_name.clone()))
                .collect(),
        }
    }

    /// Resolve a `url-pattern` to the servlet name it was mapped to.
    ///
    /// This is an exact match on the declared pattern string — the connector's
    /// mapper is responsible for the spec's prefix/extension/default matching
    /// rules; this is the raw lookup table feeding it.
    pub fn servlet_for_pattern(&self, url_pattern: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(p, _)| p == url_pattern)
            .map(|(_, name)| name.as_str())
    }

    /// All `(url-pattern, servlet-name)` pairs, in declaration order.
    pub fn entries(&self) -> &[(String, String)] {
        &self.entries
    }

    /// Number of mappings in the table.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table has no mappings.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ===========================================================================
// WebappRegistrar — drives the registration against a JvmRuntime.
// ===========================================================================

/// Registers one web application's servlets, filters and mappings against a
/// shared [`JvmRuntime`].
///
/// Construct one with [`WebappRegistrar::new`], passing the shared runtime, the
/// webapp's [`ContextId`], its [`WebappClassLoaderConfig`], and the parsed
/// [`WebXml`]. Then call [`WebappRegistrar::register`].
///
/// The registrar holds a [`RegistrationPlan`] built up front from the `WebXml`,
/// so the ordering decisions are made once and are independently inspectable
/// via [`WebappRegistrar::plan`].
#[derive(Debug, Clone)]
pub struct WebappRegistrar {
    /// The shared embedded-JVM runtime owning the [`WebappRuntime`] registry.
    runtime: Arc<JvmRuntime>,
    /// The context id (context path) of the web application being registered.
    context_id: ContextId,
    /// How to build this webapp's isolating class loader.
    class_loader_config: WebappClassLoaderConfig,
    /// The ordered, JVM-agnostic plan derived from the `web.xml`.
    plan: RegistrationPlan,
}

impl WebappRegistrar {
    /// Create a registrar for one web application.
    ///
    /// The [`RegistrationPlan`] is computed eagerly from `web` here; no JVM work
    /// happens until [`register`](Self::register) is called.
    pub fn new(
        runtime: Arc<JvmRuntime>,
        context_id: impl Into<ContextId>,
        class_loader_config: WebappClassLoaderConfig,
        web: &WebXml,
    ) -> Self {
        Self {
            runtime,
            context_id: context_id.into(),
            class_loader_config,
            plan: RegistrationPlan::from_web_xml(web),
        }
    }

    /// The context id of the web application this registrar serves.
    pub fn context_id(&self) -> &ContextId {
        &self.context_id
    }

    /// The ordered registration plan derived from the `web.xml`.
    pub fn plan(&self) -> &RegistrationPlan {
        &self.plan
    }

    /// This webapp's class-loader configuration.
    pub fn class_loader_config(&self) -> &WebappClassLoaderConfig {
        &self.class_loader_config
    }

    /// The `url-pattern` → `servlet-name` table for the connector's mapper.
    pub fn servlet_mapping_table(&self) -> ServletMappingTable {
        ServletMappingTable::from_mappings(&self.plan.servlet_mappings)
    }

    /// Ensure the [`WebappRuntime`] for this context exists in the runtime's
    /// registry, creating it (with this webapp's search path as its class path)
    /// if necessary. Shared by both feature paths.
    fn ensure_webapp(&self) -> Result<Arc<WebappRuntime>> {
        if let Some(existing) = self.runtime.webapp(&self.context_id) {
            return Ok(existing);
        }
        self.runtime.register_webapp(
            self.context_id.clone(),
            self.class_loader_config.search_path(),
        )
    }

    /// Register every servlet, filter and mapping the `web.xml` declares into
    /// the [`WebappRuntime`].
    ///
    /// Behaviour by feature:
    ///
    /// * **`--features jvm`** — builds the webapp's class loader via
    ///   [`ClassLoaderFactory`](crate::classloader::ClassLoaderFactory), then,
    ///   inside a single [`JvmRuntime::with_env`] funnel, loads + instantiates
    ///   every servlet/filter class, calls `init()` on each with its
    ///   `<init-param>`s, and stores the resulting
    ///   [`ServletInstanceHandle`](crate::jvm::ServletInstanceHandle)s in the
    ///   `WebappRuntime` registries keyed by name.
    /// * **default features** — records the *intended* registrations with
    ///   placeholder handles and logs via `tracing`, so the data model and
    ///   ordering are fully exercised without a JDK.
    ///
    /// Either way the returned [`RegistrationSummary`] reports the counts
    /// actually installed.
    ///
    /// # Errors
    ///
    /// Returns [`tomcatrs_core::Error::Bridge`] if the [`WebappRuntime`] cannot
    /// be created, or — on the `jvm` build — if any JNI step fails.
    pub fn register(&self) -> Result<RegistrationSummary> {
        let webapp = self.ensure_webapp()?;
        register_impl(self, &webapp)
    }
}

// ---------------------------------------------------------------------------
// Real implementation — only compiled with `--features jvm`.
// ---------------------------------------------------------------------------
#[cfg(feature = "jvm")]
fn register_impl(
    registrar: &WebappRegistrar,
    webapp: &Arc<WebappRuntime>,
) -> Result<RegistrationSummary> {
    use crate::classloader::ClassLoaderFactory;
    use crate::jvm::{ClassLoaderHandle, ServletInstanceHandle};
    use jni::objects::JValue;
    use tomcatrs_core::Error;

    let context_id = registrar.context_id.clone();
    let cl_config = registrar.class_loader_config.clone();
    let plan = registrar.plan.clone();

    tracing::info!(
        context_id = %context_id,
        servlets = plan.servlet_count(),
        filters = plan.filter_count(),
        listeners = plan.listener_count(),
        "registering web application against the embedded JVM"
    );

    // Everything that touches JNI runs on a single worker thread through the
    // `with_env` funnel. The closure returns the freshly-built handles so they
    // can be stored in the (thread-safe) `WebappRuntime` registries afterwards.
    let (loader_handle, servlet_handles, filter_handles) = registrar.runtime.with_env(
        |env| -> Result<(
            ClassLoaderHandle,
            Vec<(String, ServletInstanceHandle)>,
            Vec<(String, ServletInstanceHandle)>,
        )> {
            let factory = ClassLoaderFactory::new();

            // 1. Build the Common loader, then this webapp's child loader.
            let common = factory.common_loader(env)?;
            let webapp_loader = factory.webapp_loader(env, &cl_config, &common)?;

            // 2. Instantiate + init() each servlet, in the plan's order.
            let mut servlet_handles = Vec::with_capacity(plan.servlets.len());
            for servlet in &plan.servlets {
                let instance = factory.instantiate(env, &webapp_loader, &servlet.class)?;
                let config = build_servlet_config(env, &servlet.name, &servlet.init_params)?;
                // Servlet.init(ServletConfig)
                env.call_method(
                    instance.as_obj(),
                    "init",
                    "(Ljakarta/servlet/ServletConfig;)V",
                    &[JValue::Object(&config)],
                )
                .map_err(|e| {
                    Error::bridge(format!(
                        "servlet '{}' ({}) init() failed: {e}",
                        servlet.name, servlet.class
                    ))
                })?;
                servlet_handles.push((servlet.name.clone(), ServletInstanceHandle::new(instance)));
            }

            // 3. Instantiate + init() each filter, in declaration order.
            let mut filter_handles = Vec::with_capacity(plan.filters.len());
            for filter in &plan.filters {
                let instance = factory.instantiate(env, &webapp_loader, &filter.class)?;
                let config = build_filter_config(env, &filter.name, &filter.init_params)?;
                // Filter.init(FilterConfig)
                env.call_method(
                    instance.as_obj(),
                    "init",
                    "(Ljakarta/servlet/FilterConfig;)V",
                    &[JValue::Object(&config)],
                )
                .map_err(|e| {
                    Error::bridge(format!(
                        "filter '{}' ({}) init() failed: {e}",
                        filter.name, filter.class
                    ))
                })?;
                filter_handles.push((filter.name.clone(), ServletInstanceHandle::new(instance)));
            }

            Ok((
                ClassLoaderHandle::new(webapp_loader),
                servlet_handles,
                filter_handles,
            ))
        },
    )?;

    // 4. Install everything into the (thread-safe) WebappRuntime registries.
    webapp.set_class_loader(loader_handle);
    let mut servlets_registered = 0;
    for (name, handle) in servlet_handles {
        webapp.register_servlet(name, handle);
        servlets_registered += 1;
    }
    let mut filters_registered = 0;
    for (name, handle) in filter_handles {
        webapp.register_filter(name, handle);
        filters_registered += 1;
    }

    tracing::info!(
        context_id = %context_id,
        servlets_registered,
        filters_registered,
        "web application registered against the embedded JVM"
    );

    Ok(RegistrationSummary {
        servlets_registered,
        filters_registered,
        servlet_mappings_recorded: plan.servlet_mappings.len(),
        filter_mappings_recorded: plan.filter_mappings.len(),
        listeners: plan.listeners.len(),
        class_loader_built: true,
    })
}

/// Build a `org.apache.tomcatrs.bridge.TomcatRsServletConfig` carrying the
/// servlet's name and `<init-param>` map. The bridge JAR's facade class wraps a
/// plain `Map<String,String>`; we construct that map here.
#[cfg(feature = "jvm")]
fn build_servlet_config<'l>(
    env: &mut jni::JNIEnv<'l>,
    servlet_name: &str,
    init_params: &HashMap<String, String>,
) -> Result<jni::objects::JObject<'l>> {
    build_named_config(
        env,
        "org/apache/tomcatrs/bridge/TomcatRsServletConfig",
        servlet_name,
        init_params,
    )
}

/// Build a `org.apache.tomcatrs.bridge.TomcatRsFilterConfig` carrying the
/// filter's name and `<init-param>` map.
#[cfg(feature = "jvm")]
fn build_filter_config<'l>(
    env: &mut jni::JNIEnv<'l>,
    filter_name: &str,
    init_params: &HashMap<String, String>,
) -> Result<jni::objects::JObject<'l>> {
    build_named_config(
        env,
        "org/apache/tomcatrs/bridge/TomcatRsFilterConfig",
        filter_name,
        init_params,
    )
}

/// Shared helper: build a bridge config object of `class_name` from a `(name,
/// Map<String,String>)` pair. Both `TomcatRsServletConfig` and
/// `TomcatRsFilterConfig` expose the same `(String, java.util.Map)` constructor.
#[cfg(feature = "jvm")]
fn build_named_config<'l>(
    env: &mut jni::JNIEnv<'l>,
    class_name: &str,
    name: &str,
    init_params: &HashMap<String, String>,
) -> Result<jni::objects::JObject<'l>> {
    use jni::objects::{JObject, JValue};
    use tomcatrs_core::Error;

    // java.util.HashMap of the init-params.
    let map = env
        .new_object("java/util/HashMap", "()V", &[])
        .map_err(|e| Error::bridge(format!("new HashMap() failed: {e}")))?;
    for (k, v) in init_params {
        let jk = env
            .new_string(k)
            .map_err(|e| Error::bridge(format!("new_string(init-param name) failed: {e}")))?;
        let jv = env
            .new_string(v)
            .map_err(|e| Error::bridge(format!("new_string(init-param value) failed: {e}")))?;
        env.call_method(
            &map,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[
                JValue::Object(&JObject::from(jk)),
                JValue::Object(&JObject::from(jv)),
            ],
        )
        .map_err(|e| Error::bridge(format!("HashMap.put(init-param) failed: {e}")))?;
    }

    let jname = env
        .new_string(name)
        .map_err(|e| Error::bridge(format!("new_string(config name) failed: {e}")))?;
    env.new_object(
        class_name,
        "(Ljava/lang/String;Ljava/util/Map;)V",
        &[JValue::Object(&JObject::from(jname)), JValue::Object(&map)],
    )
    .map_err(|e| Error::bridge(format!("new {class_name}(String, Map) failed: {e}")))
}

// ---------------------------------------------------------------------------
// Stub implementation — compiled with default features (no JDK required).
// ---------------------------------------------------------------------------
#[cfg(not(feature = "jvm"))]
fn register_impl(
    registrar: &WebappRegistrar,
    webapp: &Arc<WebappRuntime>,
) -> Result<RegistrationSummary> {
    use crate::jvm::ServletInstanceHandle;

    let plan = &registrar.plan;

    tracing::info!(
        context_id = %registrar.context_id,
        servlets = plan.servlet_count(),
        filters = plan.filter_count(),
        listeners = plan.listener_count(),
        "registering web application without an embedded JVM; recording \
         intended registrations with placeholder handles"
    );

    // Servlets, in the plan's load-on-startup order. Each gets a fresh
    // process-unique placeholder handle — the same registry plumbing the real
    // path uses, so ordering and bookkeeping are fully exercised.
    let mut servlets_registered = 0;
    for servlet in &plan.servlets {
        tracing::debug!(
            context_id = %registrar.context_id,
            servlet = %servlet.name,
            class = %servlet.class,
            load_on_startup = ?servlet.load_on_startup,
            init_params = servlet.init_params.len(),
            "would instantiate + init() servlet"
        );
        webapp.register_servlet(servlet.name.clone(), ServletInstanceHandle::new());
        servlets_registered += 1;
    }

    // Filters, in declaration order.
    let mut filters_registered = 0;
    for filter in &plan.filters {
        tracing::debug!(
            context_id = %registrar.context_id,
            filter = %filter.name,
            class = %filter.class,
            init_params = filter.init_params.len(),
            "would instantiate + init() filter"
        );
        webapp.register_filter(filter.name.clone(), ServletInstanceHandle::new());
        filters_registered += 1;
    }

    for listener in &plan.listeners {
        tracing::debug!(
            context_id = %registrar.context_id,
            listener = %listener,
            "would instantiate lifecycle listener"
        );
    }

    tracing::info!(
        context_id = %registrar.context_id,
        servlets_registered,
        filters_registered,
        "web application registrations recorded (no-JVM stub)"
    );

    Ok(RegistrationSummary {
        servlets_registered,
        filters_registered,
        servlet_mappings_recorded: plan.servlet_mappings.len(),
        filter_mappings_recorded: plan.filter_mappings.len(),
        listeners: plan.listeners.len(),
        // No JVM on this path, so no real class loader is ever built.
        class_loader_built: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tomcatrs_config::web_xml::{FilterDef, ServletDef, ServletMapping, WebXml};

    /// A `ServletDef` with a name, class and optional load-on-startup.
    fn servlet(name: &str, class: &str, los: Option<i32>) -> ServletDef {
        ServletDef {
            name: name.into(),
            class: class.into(),
            load_on_startup: los,
            init_params: HashMap::new(),
        }
    }

    /// A `FilterDef` with a name and class.
    fn filter(name: &str, class: &str) -> FilterDef {
        FilterDef {
            name: name.into(),
            class: class.into(),
            init_params: HashMap::new(),
        }
    }

    // --- RegistrationPlan: load-on-startup ordering --------------------------

    #[test]
    fn plan_orders_servlets_by_load_on_startup_then_declaration() {
        // Declaration order: lazy, order-5, order-1, lazy-2, order-1-dup.
        let web = WebXml {
            servlets: vec![
                servlet("lazy", "com.example.Lazy", None),
                servlet("five", "com.example.Five", Some(5)),
                servlet("one", "com.example.One", Some(1)),
                servlet("lazy2", "com.example.Lazy2", None),
                servlet("one_dup", "com.example.OneDup", Some(1)),
            ],
            ..WebXml::default()
        };
        let plan = RegistrationPlan::from_web_xml(&web);
        let order: Vec<&str> = plan.servlets.iter().map(|s| s.name.as_str()).collect();
        // Eager bucket first: order 1 before order 5; ties (`one`, `one_dup`)
        // broken by declaration order. Then the lazy tail in declaration order.
        assert_eq!(order, vec!["one", "one_dup", "five", "lazy", "lazy2"]);
    }

    #[test]
    fn plan_treats_negative_load_on_startup_as_lazy() {
        let web = WebXml {
            servlets: vec![
                servlet("neg", "com.example.Neg", Some(-1)),
                servlet("eager", "com.example.Eager", Some(0)),
                servlet("absent", "com.example.Absent", None),
            ],
            ..WebXml::default()
        };
        let plan = RegistrationPlan::from_web_xml(&web);
        let order: Vec<&str> = plan.servlets.iter().map(|s| s.name.as_str()).collect();
        // `eager` (load-on-startup 0) is the only eager servlet; `neg` and
        // `absent` are the lazy tail, in declaration order.
        assert_eq!(order, vec!["eager", "neg", "absent"]);
        assert!(plan.servlets[0].is_eager());
        assert!(!plan.servlets[1].is_eager());
        assert!(!plan.servlets[2].is_eager());

        let eager: Vec<&str> = plan.eager_servlets().map(|s| s.name.as_str()).collect();
        assert_eq!(eager, vec!["eager"]);
    }

    #[test]
    fn plan_preserves_filter_declaration_order() {
        let web = WebXml {
            filters: vec![
                filter("first", "com.example.First"),
                filter("second", "com.example.Second"),
                filter("third", "com.example.Third"),
            ],
            ..WebXml::default()
        };
        let plan = RegistrationPlan::from_web_xml(&web);
        let order: Vec<&str> = plan.filters.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(order, vec!["first", "second", "third"]);
        // Declaration indices are preserved verbatim.
        assert_eq!(plan.filters[0].declaration_index, 0);
        assert_eq!(plan.filters[2].declaration_index, 2);
    }

    #[test]
    fn plan_copies_listeners_and_mappings_in_order() {
        let web = WebXml {
            listeners: vec![
                "com.example.AListener".into(),
                "com.example.BListener".into(),
            ],
            servlet_mappings: vec![
                ServletMapping {
                    servlet_name: "one".into(),
                    url_pattern: "/one".into(),
                },
                ServletMapping {
                    servlet_name: "five".into(),
                    url_pattern: "/five/*".into(),
                },
            ],
            ..WebXml::default()
        };
        let plan = RegistrationPlan::from_web_xml(&web);
        assert_eq!(plan.listener_count(), 2);
        assert_eq!(plan.listeners[0], "com.example.AListener");
        assert_eq!(plan.servlet_mappings.len(), 2);
        assert_eq!(plan.servlet_mappings[1].url_pattern, "/five/*");
    }

    #[test]
    fn plan_carries_init_params_onto_planned_servlet() {
        let mut def = servlet("p", "com.example.P", Some(1));
        def.init_params.insert("greeting".into(), "hi".into());
        let web = WebXml {
            servlets: vec![def],
            ..WebXml::default()
        };
        let plan = RegistrationPlan::from_web_xml(&web);
        assert_eq!(
            plan.servlets[0]
                .init_params
                .get("greeting")
                .map(String::as_str),
            Some("hi")
        );
    }

    #[test]
    fn empty_web_xml_yields_empty_plan() {
        let plan = RegistrationPlan::from_web_xml(&WebXml::default());
        assert_eq!(plan.servlet_count(), 0);
        assert_eq!(plan.filter_count(), 0);
        assert_eq!(plan.listener_count(), 0);
        assert!(plan.servlet_mappings.is_empty());
        assert!(plan.filter_mappings.is_empty());
    }

    // --- ServletMappingTable -------------------------------------------------

    #[test]
    fn servlet_mapping_table_resolves_pattern_to_name() {
        let mappings = vec![
            ServletMapping {
                servlet_name: "hello".into(),
                url_pattern: "/hello".into(),
            },
            ServletMapping {
                servlet_name: "api".into(),
                url_pattern: "/api/*".into(),
            },
        ];
        let table = ServletMappingTable::from_mappings(&mappings);
        assert_eq!(table.len(), 2);
        assert!(!table.is_empty());
        assert_eq!(table.servlet_for_pattern("/hello"), Some("hello"));
        assert_eq!(table.servlet_for_pattern("/api/*"), Some("api"));
        assert_eq!(table.servlet_for_pattern("/missing"), None);
    }

    // --- WebappRegistrar::register on the no-JVM stub ------------------------

    #[cfg(not(feature = "jvm"))]
    mod no_jvm {
        use super::*;

        /// A representative `web.xml`: two servlets (one eager, one lazy), one
        /// filter, one listener, two servlet mappings, one filter mapping.
        fn sample_web_xml() -> WebXml {
            let mut hello = servlet("hello", "com.example.HelloServlet", Some(1));
            hello.init_params.insert("greeting".into(), "hi".into());
            WebXml {
                servlets: vec![hello, servlet("api", "com.example.ApiServlet", None)],
                servlet_mappings: vec![
                    ServletMapping {
                        servlet_name: "hello".into(),
                        url_pattern: "/hello".into(),
                    },
                    ServletMapping {
                        servlet_name: "api".into(),
                        url_pattern: "/api/*".into(),
                    },
                ],
                filters: vec![filter("encoding", "com.example.EncodingFilter")],
                filter_mappings: vec![tomcatrs_config::web_xml::FilterMapping {
                    filter_name: "encoding".into(),
                    url_pattern: "/*".into(),
                }],
                listeners: vec!["com.example.AppListener".into()],
                ..WebXml::default()
            }
        }

        #[test]
        fn register_populates_webapp_registries_and_returns_counts() {
            let runtime = Arc::new(JvmRuntime::default());
            let ctx: ContextId = "/app".to_string();
            let cl_config = WebappClassLoaderConfig::new(
                ctx.clone(),
                Some("/srv/app/WEB-INF/classes".into()),
                Vec::new(),
                false,
            );
            let web = sample_web_xml();

            let registrar =
                WebappRegistrar::new(Arc::clone(&runtime), ctx.clone(), cl_config, &web);
            assert_eq!(registrar.context_id(), &ctx);
            // The plan ordering is visible before registering.
            assert_eq!(registrar.plan().servlet_count(), 2);

            let summary = registrar.register().expect("stub register never fails");

            assert_eq!(summary.servlets_registered, 2);
            assert_eq!(summary.filters_registered, 1);
            assert_eq!(summary.servlet_mappings_recorded, 2);
            assert_eq!(summary.filter_mappings_recorded, 1);
            assert_eq!(summary.listeners, 1);
            assert!(!summary.class_loader_built);
            assert_eq!(summary.components_registered(), 3);

            // The WebappRuntime was created and its registries populated.
            let webapp = runtime.webapp(&ctx).expect("webapp registered");
            assert_eq!(webapp.servlet_count(), 2);
            assert_eq!(webapp.filter_count(), 1);
            assert!(webapp.servlet("hello").is_some());
            assert!(webapp.servlet("api").is_some());
            assert!(webapp.filter("encoding").is_some());
            assert!(webapp.servlet("missing").is_none());
            // No JVM, so no class loader was installed.
            assert!(!webapp.has_class_loader());
        }

        #[test]
        fn register_reuses_an_already_registered_webapp() {
            let runtime = Arc::new(JvmRuntime::default());
            let ctx: ContextId = "/shop".to_string();
            // Pre-register the webapp so `ensure_webapp` must reuse it.
            let pre = runtime
                .register_webapp(ctx.clone(), Vec::new())
                .expect("pre-register");

            let cl_config = WebappClassLoaderConfig::new(ctx.clone(), None, Vec::new(), false);
            let registrar = WebappRegistrar::new(
                Arc::clone(&runtime),
                ctx.clone(),
                cl_config,
                &sample_web_xml(),
            );
            registrar.register().expect("register");

            let webapp = runtime.webapp(&ctx).expect("still registered");
            // Same Arc — the registrar reused the existing entry.
            assert!(Arc::ptr_eq(&pre, &webapp));
            assert_eq!(webapp.servlet_count(), 2);
        }

        #[test]
        fn register_servlet_mapping_table_matches_plan() {
            let runtime = Arc::new(JvmRuntime::default());
            let ctx: ContextId = "/x".to_string();
            let cl_config = WebappClassLoaderConfig::new(ctx.clone(), None, Vec::new(), false);
            let registrar =
                WebappRegistrar::new(Arc::clone(&runtime), ctx, cl_config, &sample_web_xml());
            let table = registrar.servlet_mapping_table();
            assert_eq!(table.servlet_for_pattern("/hello"), Some("hello"));
            assert_eq!(table.servlet_for_pattern("/api/*"), Some("api"));
        }
    }
}
