//! Background processing — Tomcat's periodic container-tree tick.
//!
//! Apache Tomcat runs a single `ContainerBackgroundProcessor` thread that, once
//! per `backgroundProcessorDelay` seconds, walks the `Server → Service → Engine
//! → Host → Context → Wrapper` tree and calls `backgroundProcess()` on every
//! container. Components use that callback for periodic housekeeping: session
//! expiry, reloadable-context change detection, war auto-deployment scans, and
//! so on.
//!
//! This module ports that mechanism:
//!
//! * [`BackgroundProcessor`] is the opt-in trait — a component implements it to
//!   receive the periodic callback.
//! * [`BackgroundEngine`] owns an [`Arc<Server>`] and drives the tick from an
//!   async task: every `interval` it walks the live component tree, invokes
//!   [`BackgroundProcessor::background_process`] on each component that opts in,
//!   and emits [`LifecycleEvent::Periodic`].
//! * [`spawn_background`] is the convenience entry point the CLI uses: it spawns
//!   the engine on the current runtime and hands back a [`JoinHandle`] plus a
//!   shutdown [`watch::Sender`].
//!
//! The tick loop is cancel-safe: it waits on either the interval timer or the
//! shutdown channel inside a [`tokio::select!`], so a shutdown signal stops it
//! promptly without leaving a half-finished traversal.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use tomcatrs_core::{LifecycleEvent, LifecycleListener};

use crate::context::Context;
use crate::server::Server;

/// Opt-in contract for components that want the periodic background tick.
///
/// Mirrors the `backgroundProcess()` method Tomcat puts on every `Container`.
/// Implementations must be cheap and non-blocking — they run inline on the
/// background task, so any heavy or blocking work should be dispatched onto the
/// runtime (e.g. with [`tokio::task::spawn_blocking`]) rather than executed
/// here.
pub trait BackgroundProcessor: Send + Sync {
    /// Perform one unit of periodic housekeeping.
    ///
    /// Called roughly once per [`BackgroundEngine`] interval. It must not panic;
    /// a panic would tear down the shared background task for the whole server.
    fn background_process(&self);
}

/// Tracks the last-seen `WEB-INF` modification time for each reloadable
/// [`Context`], keyed by context path.
///
/// Kept here, in `background.rs`, rather than as a field on [`Context`] so that
/// the periodic-reload bookkeeping stays entirely within the background
/// subsystem and `context.rs` needs no changes. The map is shared (via
/// [`Arc`]) with the [`ContextBackgroundProcessor`] wrappers the engine builds
/// for each context.
#[derive(Debug, Default)]
struct ReloadTracker {
    /// `context path → last observed WEB-INF mtime`.
    seen: Mutex<HashMap<String, SystemTime>>,
}

impl ReloadTracker {
    /// Record `mtime` for `path`, returning the previously stored value (if the
    /// path had been observed before).
    fn update(&self, path: &str, mtime: SystemTime) -> Option<SystemTime> {
        self.seen.lock().insert(path.to_string(), mtime)
    }
}

/// Read the modification time of the context's `WEB-INF` directory.
///
/// Returns `None` when the context has no document base on disk yet, or when
/// the directory cannot be stat-ed — both are normal during early startup and
/// are simply skipped until the next tick.
fn web_inf_mtime(doc_base: &Path) -> Option<SystemTime> {
    let web_inf = doc_base.join("WEB-INF");
    std::fs::metadata(&web_inf).ok()?.modified().ok()
}

/// A [`BackgroundProcessor`] wrapper that gives a [`Context`] a real, if
/// minimal, periodic job: reloadable-context change detection.
///
/// On each tick, if the context is `reloadable`, it stats the `WEB-INF`
/// directory under the context's document base and compares its mtime against
/// the value stored in the shared `ReloadTracker`. When the mtime changes it
/// logs an `info` line; performing the actual reload is a later milestone.
///
/// Non-reloadable contexts are inspected but do no work, exactly as in Tomcat.
pub struct ContextBackgroundProcessor {
    /// The context this processor watches.
    context: Arc<Context>,
    /// Shared store of last-seen `WEB-INF` mtimes, keyed by context path.
    tracker: Arc<ReloadTracker>,
}

impl ContextBackgroundProcessor {
    /// Wrap `context` with a processor that records change state into `tracker`.
    fn new(context: Arc<Context>, tracker: Arc<ReloadTracker>) -> Self {
        Self { context, tracker }
    }
}

impl BackgroundProcessor for ContextBackgroundProcessor {
    fn background_process(&self) {
        // Non-reloadable contexts opt out of the change check, matching Tomcat,
        // where `backgroundProcess()` only triggers a reload when reloadable.
        if !self.context.reloadable() {
            return;
        }

        let path = self.context.path();
        let label = if path.is_empty() { "/" } else { path };

        let Some(mtime) = web_inf_mtime(self.context.doc_base()) else {
            // No WEB-INF on disk yet (or unreadable) — nothing to compare
            // against this tick.
            return;
        };

        match self.tracker.update(path, mtime) {
            // First observation: just remember it, no change to report.
            None => {
                tracing::debug!(
                    context = label,
                    "background: recorded initial WEB-INF mtime"
                );
            }
            // Seen before and unchanged: quiet.
            Some(previous) if previous == mtime => {}
            // Seen before and changed: a reload is warranted. Actual reloading
            // arrives in a later milestone — for now we detect and log.
            Some(_) => {
                tracing::info!(context = label, "context {label} would reload");
            }
        }
    }
}

/// Implementing [`BackgroundProcessor`] directly on [`Context`] is convenient
/// for callers that hold a bare context and want the same behaviour without the
/// shared tracker. It uses a process-local tracker so repeated calls on the
/// *same* context still detect changes across ticks.
impl BackgroundProcessor for Context {
    fn background_process(&self) {
        // A per-Context-type lazily-initialised tracker. Keyed by context path,
        // so distinct contexts do not collide. This keeps the `&self`-only
        // signature without adding a field to `Context`.
        use std::sync::OnceLock;
        static TRACKER: OnceLock<ReloadTracker> = OnceLock::new();
        let tracker = TRACKER.get_or_init(ReloadTracker::default);

        if !self.reloadable() {
            return;
        }
        let path = self.path();
        let label = if path.is_empty() { "/" } else { path };
        let Some(mtime) = web_inf_mtime(self.doc_base()) else {
            return;
        };
        match tracker.update(path, mtime) {
            None => {
                tracing::debug!(
                    context = label,
                    "background: recorded initial WEB-INF mtime"
                );
            }
            Some(previous) if previous == mtime => {}
            Some(_) => {
                tracing::info!(context = label, "context {label} would reload");
            }
        }
    }
}

/// Drives the periodic background tick over a [`Server`]'s component tree.
///
/// Construct one with [`BackgroundEngine::new`], then move it into a task with
/// [`BackgroundEngine::run`] (or use [`spawn_background`], which does both). The
/// engine holds an [`Arc<Server>`] so it can re-walk the live tree on every
/// tick — contexts added or removed between ticks are picked up automatically.
pub struct BackgroundEngine {
    /// The server whose component tree is walked each tick.
    server: Arc<Server>,
    /// Shared last-seen-mtime store for reloadable contexts.
    tracker: Arc<ReloadTracker>,
    /// Listener notified with [`LifecycleEvent::Periodic`] on every tick.
    listener: Arc<dyn LifecycleListener>,
}

impl BackgroundEngine {
    /// Create a background engine for `server`.
    ///
    /// The [`LifecycleEvent::Periodic`] event is delivered to a
    /// [`tomcatrs_core::event::NoopListener`]; use
    /// [`BackgroundEngine::with_listener`] to observe the tick.
    pub fn new(server: Arc<Server>) -> Self {
        Self {
            server,
            tracker: Arc::new(ReloadTracker::default()),
            listener: Arc::new(tomcatrs_core::event::NoopListener),
        }
    }

    /// Replace the [`LifecycleListener`] that receives the periodic event,
    /// returning `self` for builder-style chaining.
    pub fn with_listener(mut self, listener: Arc<dyn LifecycleListener>) -> Self {
        self.listener = listener;
        self
    }

    /// Collect every [`BackgroundProcessor`] in the current component tree.
    ///
    /// Walks `Server → Service → Engine → Host → Context`, wrapping each
    /// [`Context`] in a [`ContextBackgroundProcessor`] bound to the shared
    /// [`ReloadTracker`]. Re-run on each tick so the traversal always reflects
    /// the live tree.
    fn collect_processors(&self) -> Vec<Box<dyn BackgroundProcessor>> {
        let mut processors: Vec<Box<dyn BackgroundProcessor>> = Vec::new();
        for service in self.server.services() {
            let engine = service.engine();
            for host in engine.hosts().iter() {
                for context in host.value().contexts().iter() {
                    processors.push(Box::new(ContextBackgroundProcessor::new(
                        Arc::clone(context.value()),
                        Arc::clone(&self.tracker),
                    )));
                }
            }
        }
        processors
    }

    /// Run a single background pass: invoke every processor and emit the
    /// periodic event. Factored out so it can be unit-tested without a timer.
    fn tick(&self) {
        for processor in self.collect_processors() {
            processor.background_process();
        }
        self.listener.on_event("Catalina", LifecycleEvent::Periodic);
    }

    /// Run the background tick loop until shutdown.
    ///
    /// Every `interval` the loop walks the component tree, invokes
    /// [`BackgroundProcessor::background_process`] on each opted-in component,
    /// and emits [`LifecycleEvent::Periodic`]. The loop is cancel-safe: it
    /// `select!`s between the interval timer and the `shutdown` channel, so a
    /// `true` (or a dropped sender) on `shutdown` ends it promptly — no tick is
    /// left half-finished, since `background_process` calls are synchronous.
    ///
    /// The first tick fires one `interval` after `run` is awaited, not
    /// immediately, matching Tomcat's `backgroundProcessorDelay` semantics.
    pub async fn run(self, interval: Duration, mut shutdown: watch::Receiver<bool>) {
        let mut timer = tokio::time::interval(interval);
        // The first `tick()` of a fresh interval completes immediately; burn it
        // so the loop's first *real* tick lands one `interval` in.
        timer.tick().await;
        tracing::info!(?interval, "background processor started");

        loop {
            tokio::select! {
                _ = timer.tick() => {
                    self.tick();
                }
                changed = shutdown.changed() => {
                    // `changed()` errors only once the sender is dropped; either
                    // a `true` value or a dropped sender means "stop".
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }

        tracing::info!("background processor stopped");
    }
}

/// Spawn a [`BackgroundEngine`] for `server` on the current Tokio runtime.
///
/// Returns the task's [`JoinHandle`] and a [`watch::Sender<bool>`]; send `true`
/// (or drop the sender) to ask the loop to stop, then `await` the handle to
/// join it. This is the entry point the CLI uses to wire background processing
/// into the server lifecycle.
///
/// # Panics
///
/// Panics if called outside a Tokio runtime, like any other `tokio::spawn`.
pub fn spawn_background(
    server: Arc<Server>,
    interval: Duration,
) -> (JoinHandle<()>, watch::Sender<bool>) {
    let (tx, rx) = watch::channel(false);
    let engine = BackgroundEngine::new(server);
    let handle = tokio::spawn(engine.run(interval, rx));
    (handle, tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tomcatrs_config::{
        ConnectorConfig, ContextConfig, EngineConfig, HostConfig, Protocol, RequestLimits,
        ServerConfig, ServiceConfig,
    };

    /// A counting [`BackgroundProcessor`] used to assert the tick fires.
    struct Counter {
        hits: Arc<AtomicUsize>,
    }

    impl BackgroundProcessor for Counter {
        fn background_process(&self) {
            self.hits.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A [`LifecycleListener`] that counts [`LifecycleEvent::Periodic`] events.
    #[derive(Default)]
    struct PeriodicCounter {
        ticks: AtomicUsize,
    }

    impl LifecycleListener for PeriodicCounter {
        fn on_event(&self, _component: &str, event: LifecycleEvent) {
            if event == LifecycleEvent::Periodic {
                self.ticks.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    /// A minimal one-context server config for exercising the engine.
    fn sample_config() -> ServerConfig {
        ServerConfig {
            port: 8005,
            shutdown: "SHUTDOWN".to_string(),
            services: vec![ServiceConfig {
                name: "Catalina".to_string(),
                connectors: vec![ConnectorConfig {
                    protocol: Protocol::Http11,
                    address: None,
                    port: 8080,
                    tls: None,
                    limits: RequestLimits::default(),
                }],
                engine: EngineConfig {
                    name: "Catalina".to_string(),
                    default_host: "localhost".to_string(),
                    hosts: vec![HostConfig {
                        name: "localhost".to_string(),
                        app_base: PathBuf::from("webapps"),
                        aliases: vec![],
                        auto_deploy: true,
                        contexts: vec![ContextConfig {
                            path: "/app".to_string(),
                            doc_base: PathBuf::from("webapps/app"),
                            reloadable: true,
                        }],
                    }],
                },
            }],
        }
    }

    #[test]
    fn counter_processor_is_invoked() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Counter {
            hits: Arc::clone(&hits),
        };
        counter.background_process();
        counter.background_process();
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn engine_collects_one_processor_per_context() {
        let server = Arc::new(Server::from_config(&sample_config()).unwrap());
        let engine = BackgroundEngine::new(server);
        assert_eq!(engine.collect_processors().len(), 1);
    }

    #[test]
    fn non_reloadable_context_does_nothing() {
        // A non-reloadable context with a bogus doc_base must not panic and must
        // simply return — exercising the early-out branch.
        let ctx = Context::new("/x", PathBuf::from("/nonexistent/xyz"), false, vec![]);
        ctx.background_process();
    }

    #[tokio::test]
    async fn background_process_runs_within_a_few_ticks() {
        let hits = Arc::new(AtomicUsize::new(0));

        // Drive a bare `BackgroundProcessor` directly through a tiny tick loop
        // so the test is fast and deterministic.
        let counter = Counter {
            hits: Arc::clone(&hits),
        };
        let (tx, mut rx) = watch::channel(false);
        let handle = tokio::spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_millis(10));
            timer.tick().await;
            loop {
                tokio::select! {
                    _ = timer.tick() => counter.background_process(),
                    changed = rx.changed() => {
                        if changed.is_err() || *rx.borrow() {
                            break;
                        }
                    }
                }
            }
        });

        // Within a handful of 10ms ticks the processor must have run.
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            hits.load(Ordering::SeqCst) >= 1,
            "expected at least one background_process call"
        );

        tx.send(true).unwrap();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn engine_emits_periodic_and_ticks_context() {
        let server = Arc::new(Server::from_config(&sample_config()).unwrap());
        let listener = Arc::new(PeriodicCounter::default());
        let engine =
            BackgroundEngine::new(Arc::clone(&server)).with_listener(Arc::clone(&listener) as _);

        let (tx, rx) = watch::channel(false);
        let handle = tokio::spawn(engine.run(Duration::from_millis(10), rx));

        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            listener.ticks.load(Ordering::SeqCst) >= 1,
            "expected at least one Periodic event"
        );

        tx.send(true).unwrap();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn loop_stops_promptly_on_shutdown() {
        let server = Arc::new(Server::from_config(&sample_config()).unwrap());
        let engine = BackgroundEngine::new(server);

        // A long interval: if shutdown were not cancel-safe, the join below
        // would block well past the test timeout.
        let (tx, rx) = watch::channel(false);
        let handle = tokio::spawn(engine.run(Duration::from_secs(3600), rx));

        tx.send(true).unwrap();

        // Joining must complete near-instantly, not after the 1h interval.
        let joined = tokio::time::timeout(Duration::from_millis(200), handle).await;
        assert!(joined.is_ok(), "background loop did not stop promptly");
        joined.unwrap().unwrap();
    }

    #[tokio::test]
    async fn spawn_background_stops_when_sender_signals() {
        let server = Arc::new(Server::from_config(&sample_config()).unwrap());
        let (handle, tx) = spawn_background(Arc::clone(&server), Duration::from_secs(3600));

        tx.send(true).unwrap();
        let joined = tokio::time::timeout(Duration::from_millis(200), handle).await;
        assert!(
            joined.is_ok(),
            "spawn_background loop did not stop promptly"
        );
        joined.unwrap().unwrap();
    }

    #[tokio::test]
    async fn loop_stops_when_sender_dropped() {
        let server = Arc::new(Server::from_config(&sample_config()).unwrap());
        let (handle, tx) = spawn_background(server, Duration::from_secs(3600));

        // Dropping the sender must also end the loop (the `changed()` error arm).
        drop(tx);
        let joined = tokio::time::timeout(Duration::from_millis(200), handle).await;
        assert!(joined.is_ok(), "loop did not stop when sender was dropped");
        joined.unwrap().unwrap();
    }
}
