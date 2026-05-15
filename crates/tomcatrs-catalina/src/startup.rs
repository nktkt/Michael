//! Servlet startup orchestration — the **load-on-startup** ordering and the
//! eager-initialization pass that Catalina runs when a [`Context`] starts.
//!
//! # What Tomcat does
//!
//! Each `<servlet>` in `web.xml` may carry a `<load-on-startup>` element:
//!
//! * a **non-negative** integer means the servlet is instantiated and `init()`ed
//!   *eagerly* when the web application starts, in ascending order of that
//!   integer (ties broken by declaration order in `web.xml`);
//! * a **negative** value, or the element being **absent**, means the servlet is
//!   *lazy* — it is not touched at boot and is instantiated on first request.
//!
//! This module ports that contract. It does not itself run a Java `init()` —
//! that happens across the JNI bridge inside `tomcatrs-servlet-bridge` — so the
//! eager pass here *validates* that each eager servlet is resolvable and records
//! the outcome in a [`StartupReport`]. A failing eager servlet is logged and
//! collected into [`StartupReport::failed`]; it does **not** abort the rest of
//! the pass unless the wrapper is marked critical (see [`ServletInitConfig`]).
//!
//! # Expected `Wrapper` / `Context` API
//!
//! This module is written against accessors being added concurrently to
//! [`Wrapper`] and [`Context`]:
//!
//! * `Wrapper::load_on_startup() -> Option<i32>`
//! * `Wrapper::init_params() -> &HashMap<String, String>`
//! * `Wrapper::servlet_name() -> &str`
//! * `Wrapper::servlet_class() -> &str`
//! * `Context::wrappers() -> &[Arc<Wrapper>]` (or `Vec<Arc<Wrapper>>`)
//!
//! If a name drifts slightly at integration time the fix is local to this file.

use std::collections::HashMap;
use std::sync::Arc;

use tomcatrs_core::{Error, Result};
use tomcatrs_servlet_bridge::ServletInvoker;

use crate::context::Context;
use crate::wrapper::Wrapper;

/// How a servlet participates in web-application startup.
///
/// Derived from the wrapper's `load_on_startup()` value:
///
/// * `Some(n)` with `n >= 0` ⇒ [`ServletLoadMode::Eager`] — initialized at boot.
/// * `Some(n)` with `n < 0`, or `None` ⇒ [`ServletLoadMode::Lazy`] — initialized
///   on first request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServletLoadMode {
    /// The servlet is loaded eagerly at startup, ordered by the carried
    /// non-negative `load-on-startup` value (ascending; ties broken by
    /// declaration order).
    Eager(i32),
    /// The servlet is loaded lazily on first request.
    Lazy,
}

impl ServletLoadMode {
    /// Classify a raw `load_on_startup()` value into a load mode.
    ///
    /// A non-negative value is [`Eager`](ServletLoadMode::Eager); a negative
    /// value or `None` is [`Lazy`](ServletLoadMode::Lazy).
    pub fn from_load_on_startup(value: Option<i32>) -> Self {
        match value {
            Some(n) if n >= 0 => ServletLoadMode::Eager(n),
            _ => ServletLoadMode::Lazy,
        }
    }

    /// Whether this mode means the servlet is initialized at boot.
    pub fn is_eager(self) -> bool {
        matches!(self, ServletLoadMode::Eager(_))
    }
}

/// Per-servlet configuration handed across the JVM bridge when a servlet is
/// initialized.
///
/// The bridge needs the servlet's name, implementing class, and the
/// `<init-param>` map to build the Java `ServletConfig`. This struct is a small
/// owned snapshot of exactly that, decoupled from the live [`Wrapper`] so it can
/// be moved into the bridge worker without borrowing the container tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServletInitConfig {
    /// The servlet's registered name (`<servlet-name>`).
    pub servlet_name: String,
    /// The fully-qualified implementing class (`<servlet-class>`).
    pub servlet_class: String,
    /// The `<init-param>` name/value pairs for this servlet.
    pub init_params: HashMap<String, String>,
    /// How the servlet participates in startup.
    pub load_mode: ServletLoadMode,
    /// Whether a failure to initialize this servlet must abort the whole
    /// startup pass. Mirrors the (rare) Tomcat behaviour where a servlet is
    /// considered essential to the application. Defaults to `false`.
    pub critical: bool,
}

impl ServletInitConfig {
    /// Snapshot the init configuration out of a live [`Wrapper`].
    pub fn from_wrapper(wrapper: &Wrapper) -> Self {
        Self {
            servlet_name: wrapper.servlet_name().to_string(),
            servlet_class: wrapper.servlet_class().to_string(),
            init_params: wrapper.init_params().clone(),
            load_mode: ServletLoadMode::from_load_on_startup(wrapper.load_on_startup()),
            critical: false,
        }
    }

    /// Mark this servlet as critical: a failed eager `init()` aborts startup.
    pub fn critical(mut self, critical: bool) -> Self {
        self.critical = critical;
        self
    }
}

/// The outcome of an [`initialize_servlets`] pass over a [`Context`].
///
/// Every eager wrapper lands in exactly one of [`initialized`](Self::initialized)
/// or [`failed`](Self::failed); every lazy wrapper lands in
/// [`lazy`](Self::lazy). The three lists therefore account for every wrapper in
/// the context.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartupReport {
    /// Servlet names of eager servlets that were initialized successfully, in
    /// load-on-startup order.
    pub initialized: Vec<String>,
    /// Servlet names of lazy servlets, deferred to first request.
    pub lazy: Vec<String>,
    /// `(servlet_name, reason)` for each eager servlet whose initialization
    /// failed. A non-critical failure is recorded here and the pass continues.
    pub failed: Vec<(String, String)>,
}

impl StartupReport {
    /// `true` if no eager servlet failed to initialize.
    pub fn is_success(&self) -> bool {
        self.failed.is_empty()
    }

    /// Total number of wrappers accounted for by this report.
    pub fn total(&self) -> usize {
        self.initialized.len() + self.lazy.len() + self.failed.len()
    }
}

/// Computes the load-on-startup ordering for a [`Context`] and drives the eager
/// servlet-initialization pass.
///
/// `StartupOrchestrator` is stateless — every method takes the [`Context`] it
/// operates on — so a single value can be reused across contexts, or the
/// associated functions can be called directly.
#[derive(Debug, Clone, Copy, Default)]
pub struct StartupOrchestrator;

impl StartupOrchestrator {
    /// Create a new orchestrator.
    pub fn new() -> Self {
        Self
    }

    /// The eager startup order: every wrapper with a non-negative
    /// `load_on_startup()` value, sorted ascending by that value, with ties
    /// broken by declaration order in the context.
    ///
    /// Lazy wrappers (negative or absent `load-on-startup`) are excluded.
    pub fn startup_order(ctx: &Context) -> Vec<Arc<Wrapper>> {
        let mut eager: Vec<(usize, i32, Arc<Wrapper>)> = ctx
            .wrappers()
            .iter()
            .enumerate()
            .filter_map(|(decl_idx, wrapper)| {
                match ServletLoadMode::from_load_on_startup(wrapper.load_on_startup()) {
                    ServletLoadMode::Eager(order) => Some((decl_idx, order, Arc::clone(wrapper))),
                    ServletLoadMode::Lazy => None,
                }
            })
            .collect();

        // Stable-sort by the load-on-startup value; `sort_by_key` is stable, so
        // equal values keep their relative (declaration) order. The explicit
        // `decl_idx` tie-breaker makes the intent unmistakable and is robust
        // even if the sort strategy ever changes.
        eager.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));

        eager.into_iter().map(|(_, _, w)| w).collect()
    }

    /// Every servlet in the context paired with its [`ServletLoadMode`]: first
    /// the eager wrappers in load-on-startup order, then the lazy wrappers in
    /// declaration order.
    pub fn all_servlets_load_order(ctx: &Context) -> Vec<(Arc<Wrapper>, ServletLoadMode)> {
        let eager_first = Self::startup_order(ctx).into_iter().map(|w| {
            let mode = ServletLoadMode::from_load_on_startup(w.load_on_startup());
            (w, mode)
        });

        let lazy =
            ctx.wrappers().iter().filter_map(
                |wrapper| match ServletLoadMode::from_load_on_startup(wrapper.load_on_startup()) {
                    ServletLoadMode::Lazy => Some((Arc::clone(wrapper), ServletLoadMode::Lazy)),
                    ServletLoadMode::Eager(_) => None,
                },
            );

        eager_first.chain(lazy).collect()
    }

    /// The [`ServletInitConfig`] snapshots for the eager wrappers, in startup
    /// order — ready to hand to the JVM bridge.
    pub fn eager_init_configs(ctx: &Context) -> Vec<ServletInitConfig> {
        Self::startup_order(ctx)
            .iter()
            .map(|w| ServletInitConfig::from_wrapper(w))
            .collect()
    }
}

/// Validate that a wrapper has a resolvable servlet definition.
///
/// The real servlet `init()` runs inside the JVM (the bridge has already
/// registered the servlet by the time this runs), so the orchestrator's job is
/// to confirm the wrapper carries the minimum a servlet needs — a non-empty
/// servlet name and implementing class. An unresolvable wrapper yields an
/// [`Error::NotFound`] describing what is missing.
fn validate_servlet(wrapper: &Wrapper) -> Result<()> {
    if wrapper.servlet_name().trim().is_empty() {
        return Err(Error::NotFound(
            "servlet wrapper has an empty servlet-name".to_string(),
        ));
    }
    if wrapper.servlet_class().trim().is_empty() {
        return Err(Error::NotFound(format!(
            "servlet '{}' has no servlet-class to resolve",
            wrapper.servlet_name()
        )));
    }
    Ok(())
}

/// Run the eager servlet-initialization pass for `ctx`.
///
/// For each eager wrapper, in load-on-startup order:
///
/// 1. log the init step;
/// 2. validate the wrapper has a resolvable servlet (see `validate_servlet`);
/// 3. on success, record the servlet name in [`StartupReport::initialized`];
/// 4. on failure, log it and record `(name, reason)` in
///    [`StartupReport::failed`] — **continuing** with the remaining servlets,
///    *unless* the wrapper is critical, in which case the whole pass aborts with
///    an [`Error::Lifecycle`].
///
/// Lazy wrappers are not initialized; their names are collected into
/// [`StartupReport::lazy`].
///
/// The `invoker` is the bridge that ultimately owns servlet instances. Today the
/// registration happens inside the bridge itself (see
/// [`tomcatrs_servlet_bridge::JvmServletInvoker`]); it is threaded through here
/// so this signature is stable once the orchestrator drives a real per-servlet
/// `init()` call. The default [`NoopServletInvoker`][noop] makes the whole pass
/// runnable with no JVM present.
///
/// [noop]: tomcatrs_servlet_bridge::NoopServletInvoker
pub async fn initialize_servlets(
    ctx: &Context,
    invoker: &Arc<dyn ServletInvoker>,
) -> Result<StartupReport> {
    // `invoker` is the handoff point to the bridge; touch it so the dependency
    // is real and the signature is honest even before per-servlet `init()` is
    // wired across JNI.
    let _ = Arc::as_ptr(invoker);

    let mut report = StartupReport::default();

    let load_order = StartupOrchestrator::all_servlets_load_order(ctx);
    let eager_total = load_order.iter().filter(|(_, m)| m.is_eager()).count();
    tracing::info!(
        context = %ctx.path(),
        servlets = load_order.len(),
        eager = eager_total,
        "servlet startup: beginning load-on-startup pass"
    );

    for (wrapper, mode) in load_order {
        match mode {
            ServletLoadMode::Lazy => {
                tracing::debug!(
                    context = %ctx.path(),
                    servlet = %wrapper.servlet_name(),
                    "servlet startup: lazy servlet deferred to first request"
                );
                report.lazy.push(wrapper.servlet_name().to_string());
            }
            ServletLoadMode::Eager(order) => {
                let config = ServletInitConfig::from_wrapper(&wrapper);
                tracing::info!(
                    context = %ctx.path(),
                    servlet = %wrapper.servlet_name(),
                    class = %wrapper.servlet_class(),
                    load_on_startup = order,
                    init_params = config.init_params.len(),
                    "servlet startup: initializing eager servlet"
                );

                match validate_servlet(&wrapper) {
                    Ok(()) => {
                        report.initialized.push(wrapper.servlet_name().to_string());
                    }
                    Err(err) => {
                        let reason = err.to_string();
                        if config.critical {
                            tracing::error!(
                                context = %ctx.path(),
                                servlet = %wrapper.servlet_name(),
                                error = %reason,
                                "servlet startup: critical servlet failed to \
                                 initialize; aborting startup"
                            );
                            return Err(Error::lifecycle(format!(
                                "critical servlet '{}' failed to initialize: {reason}",
                                wrapper.servlet_name()
                            )));
                        }
                        tracing::error!(
                            context = %ctx.path(),
                            servlet = %wrapper.servlet_name(),
                            error = %reason,
                            "servlet startup: eager servlet failed to initialize; \
                             continuing with the rest"
                        );
                        report
                            .failed
                            .push((wrapper.servlet_name().to_string(), reason));
                    }
                }
            }
        }
    }

    tracing::info!(
        context = %ctx.path(),
        initialized = report.initialized.len(),
        lazy = report.lazy.len(),
        failed = report.failed.len(),
        "servlet startup: load-on-startup pass complete"
    );

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use tomcatrs_servlet_bridge::NoopServletInvoker;

    use crate::mapper::UrlPattern;

    /// Build a wrapper with an explicit `load-on-startup` value.
    fn wrapper(name: &str, load_on_startup: Option<i32>) -> Arc<Wrapper> {
        let w = Wrapper::new(
            name,
            format!("com.example.{name}"),
            vec![UrlPattern::parse(&format!("/{name}"))],
        )
        .with_load_on_startup(load_on_startup);
        Arc::new(w)
    }

    /// A context whose wrappers have load-on-startup values `5, 1, -1, absent, 1`
    /// in declaration order.
    fn sample_context() -> Context {
        Context::new(
            "/app",
            PathBuf::from("/tmp/app"),
            false,
            vec![
                wrapper("s5", Some(5)),
                wrapper("s1a", Some(1)),
                wrapper("sNeg", Some(-1)),
                wrapper("sAbsent", None),
                wrapper("s1b", Some(1)),
            ],
        )
    }

    #[test]
    fn load_mode_classification() {
        assert_eq!(
            ServletLoadMode::from_load_on_startup(Some(0)),
            ServletLoadMode::Eager(0)
        );
        assert_eq!(
            ServletLoadMode::from_load_on_startup(Some(7)),
            ServletLoadMode::Eager(7)
        );
        assert_eq!(
            ServletLoadMode::from_load_on_startup(Some(-1)),
            ServletLoadMode::Lazy
        );
        assert_eq!(
            ServletLoadMode::from_load_on_startup(None),
            ServletLoadMode::Lazy
        );
    }

    #[test]
    fn startup_order_is_sorted_and_tie_broken_by_declaration() {
        let ctx = sample_context();
        let order = StartupOrchestrator::startup_order(&ctx);

        let names: Vec<&str> = order.iter().map(|w| w.servlet_name()).collect();
        // load values 1, 1, 5 — the two 1s keep declaration order (s1a before
        // s1b); the negative and absent ones are excluded entirely.
        assert_eq!(names, ["s1a", "s1b", "s5"]);
    }

    #[test]
    fn all_servlets_load_order_lists_eager_then_lazy() {
        let ctx = sample_context();
        let all = StartupOrchestrator::all_servlets_load_order(&ctx);

        let pairs: Vec<(&str, ServletLoadMode)> =
            all.iter().map(|(w, m)| (w.servlet_name(), *m)).collect();

        assert_eq!(
            pairs,
            [
                ("s1a", ServletLoadMode::Eager(1)),
                ("s1b", ServletLoadMode::Eager(1)),
                ("s5", ServletLoadMode::Eager(5)),
                ("sNeg", ServletLoadMode::Lazy),
                ("sAbsent", ServletLoadMode::Lazy),
            ]
        );
    }

    #[tokio::test]
    async fn initialize_servlets_reports_eager_and_lazy() {
        let ctx = sample_context();
        let invoker: Arc<dyn ServletInvoker> = Arc::new(NoopServletInvoker::new());

        let report = initialize_servlets(&ctx, &invoker).await.unwrap();

        assert!(report.is_success());
        assert_eq!(report.initialized, ["s1a", "s1b", "s5"]);
        assert_eq!(report.lazy, ["sNeg", "sAbsent"]);
        assert!(report.failed.is_empty());
        assert_eq!(report.total(), 5);
    }

    #[tokio::test]
    async fn eager_failure_is_collected_not_fatal() {
        // A wrapper with an empty servlet-class is unresolvable but not
        // critical: it must be recorded in `failed` while the rest still init.
        let bad = Wrapper::new("broken", "", vec![]).with_load_on_startup(Some(0));
        let ctx = Context::new(
            "/app",
            PathBuf::from("/tmp/app"),
            false,
            vec![Arc::new(bad), wrapper("ok", Some(1))],
        );
        let invoker: Arc<dyn ServletInvoker> = Arc::new(NoopServletInvoker::new());

        let report = initialize_servlets(&ctx, &invoker).await.unwrap();

        assert_eq!(report.initialized, ["ok"]);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, "broken");
        assert!(!report.is_success());
    }

    #[test]
    fn init_config_snapshots_wrapper() {
        let w = wrapper("snap", Some(3));
        let config = ServletInitConfig::from_wrapper(&w);
        assert_eq!(config.servlet_name, "snap");
        assert_eq!(config.servlet_class, "com.example.snap");
        assert_eq!(config.load_mode, ServletLoadMode::Eager(3));
        assert!(!config.critical);
        assert!(config.critical(true).critical);
    }
}
