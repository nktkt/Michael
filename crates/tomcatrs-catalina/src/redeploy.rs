//! [`HotRedeployer`] — class-reload watcher for [`Context`]s flagged
//! `reloadable`.
//!
//! Where [`crate::deployer::DeploymentWatcher`] watches the host's `app_base`
//! for *new* applications, this module watches *already deployed* contexts for
//! in-place changes that should trigger a hot reload — exactly Tomcat's
//! `Context.reload()` semantics, scaled down to the bits Tomcat-RS already
//! ports.
//!
//! For every context whose [`Context::reloadable`] flag is `true`, the
//! redeployer remembers the most recent modification time observed across:
//!
//! * `<doc_base>/WEB-INF/web.xml` — the deployment descriptor, and
//! * every entry reachable from `<doc_base>/WEB-INF/classes/**` — the compiled
//!   class tree.
//!
//! When the combined mtime advances, the redeployer drives the canonical
//! Tomcat reload sequence in-process:
//!
//! 1. [`Context::stop`] — bring the context (and its wrappers) to `Stopped`,
//! 2. re-open the [`Webapp`] from disk to re-validate the layout and re-parse
//!    `web.xml`,
//! 3. [`Context::deploy_descriptor`] — re-wire servlets, filters, welcome files
//!    and listeners into the context,
//! 4. [`Context::start`] — bring the context back to `Started`.
//!
//! Steps 2 and 3 only run when the context has never had its descriptor wired
//! in — `deploy_descriptor` is single-shot — so for an already-deployed context
//! the redeployer only restarts its lifecycle. That still picks up class
//! changes under `WEB-INF/classes`, which is the dominant developer use case.
//!
//! # Concurrency
//!
//! [`HotRedeployer`] is cheap to clone and holds no state of its own beyond a
//! shared mtime map keyed by context path. [`RedeployTask::run`] drives the
//! poll loop from a dedicated Tokio task and is cancel-safe via a
//! [`watch::Receiver<bool>`] shutdown channel; a `true` value (or a dropped
//! sender) ends the loop after the in-flight pass completes.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;
use tokio::sync::watch;
use tomcatrs_core::{Lifecycle, LifecycleContext};
use tomcatrs_webapp::Webapp;

use crate::context::Context;
use crate::host::Host;

/// The maximum number of `WEB-INF/classes` entries the recursive mtime walk
/// will visit per context. A guardrail against pathological class trees; well
/// in excess of any realistic webapp.
const MAX_CLASSES_ENTRIES: usize = 100_000;

/// Periodic hot-redeployer for reloadable [`Context`]s on a [`Host`].
///
/// Build one with [`HotRedeployer::new`] and drive it either manually via
/// [`HotRedeployer::tick`] (one pass) or as a background task with
/// [`RedeployTask::run`].
#[derive(Debug, Default, Clone)]
pub struct HotRedeployer {
    /// `context path → last observed combined WEB-INF mtime`, shared so the
    /// tracker survives across ticks. Wrapped in an `Arc<Mutex>` so the
    /// redeployer remains `Clone`able and cheap to share across tasks.
    seen: Arc<Mutex<HashMap<String, SystemTime>>>,
}

impl HotRedeployer {
    /// Create a fresh redeployer with no remembered mtimes.
    pub fn new() -> Self {
        Self::default()
    }

    /// Run a single redeployment pass over `host`.
    ///
    /// For every reloadable context currently registered on `host`, the
    /// combined mtime of `WEB-INF/web.xml` and the `WEB-INF/classes/**` tree
    /// is computed. The very first observation per context is recorded
    /// silently; on every subsequent observation that has advanced, the
    /// context is driven through stop → re-deploy descriptor → start.
    ///
    /// Returns the number of contexts that were redeployed in this pass.
    pub async fn tick(&self, host: &Host) -> usize {
        let mut redeployed = 0usize;
        let contexts: Vec<Arc<Context>> = host
            .contexts()
            .iter()
            .filter(|c| c.value().reloadable())
            .map(|c| Arc::clone(c.value()))
            .collect();

        for ctx in contexts {
            let Some(mtime) = combined_mtime(ctx.doc_base()) else {
                // The webapp's WEB-INF doesn't exist yet (or is unreadable).
                // That's a normal transient state during startup, so just
                // skip until the next tick.
                continue;
            };

            let previous = {
                let mut map = self.seen.lock();
                map.insert(ctx.path().to_string(), mtime)
            };

            match previous {
                None => {
                    // First observation: remember and move on.
                    tracing::debug!(
                        host = %host.name(),
                        context = %ctx.path(),
                        "redeploy: recorded initial mtime"
                    );
                }
                Some(prev) if prev >= mtime => {
                    // Unchanged (or somehow moved backwards — e.g. a clock
                    // skew or a restored file). Nothing to do.
                }
                Some(prev) => {
                    tracing::info!(
                        host = %host.name(),
                        context = %ctx.path(),
                        ?prev,
                        new = ?mtime,
                        "redeploy: change detected, hot-reloading context"
                    );
                    if let Err(err) = self.redeploy_one(host, &ctx).await {
                        tracing::warn!(
                            host = %host.name(),
                            context = %ctx.path(),
                            error = %err,
                            "redeploy: hot reload failed; context left in current state"
                        );
                    } else {
                        redeployed += 1;
                    }
                }
            }
        }

        redeployed
    }

    /// Drive a single context through the stop → re-validate → start cycle.
    ///
    /// `deploy_descriptor` is single-shot per [`Context`], so when the context
    /// already has wrappers wired in we only re-open the [`Webapp`] (to
    /// re-validate the layout and surface any descriptor parse errors early)
    /// and then bounce the lifecycle. Brand-new, never-deployed contexts get
    /// the full descriptor wire-in.
    async fn redeploy_one(&self, host: &Host, ctx: &Arc<Context>) -> tomcatrs_core::Result<()> {
        let label = format!("{}/{}", host.name(), ctx.path());
        let lc = LifecycleContext::new(&label);

        // 1. Stop the context. A context that was never started is allowed to
        //    "stop" — the lifecycle layer tolerates the transition.
        if let Err(err) = ctx.stop(&lc).await {
            tracing::warn!(
                context = %ctx.path(),
                error = %err,
                "redeploy: stop failed; continuing with re-deploy anyway"
            );
        }

        // 2. Re-validate the on-disk layout and re-parse web.xml. If this
        //    fails we abort the redeploy and leave the context stopped — the
        //    operator can fix the descriptor and we'll pick it up next tick.
        let webapp = Webapp::open(ctx.path().to_string(), ctx.doc_base())?;

        // 3. If the context has not yet been deployed (no wrappers wired in),
        //    take advantage of this redeploy to wire the descriptor in for the
        //    first time. Otherwise the single-shot guard in
        //    `deploy_descriptor` would (correctly) reject the call.
        if ctx.wrappers().is_empty() {
            if let Some(web_xml_path) = webapp.web_xml_path() {
                let web_xml = tomcatrs_config::web_xml::WebXml::from_xml_file(web_xml_path)?;
                ctx.deploy_descriptor(&web_xml)?;
            }
        }

        // 4. Bring the context back up.
        ctx.start(&lc).await?;

        tracing::info!(
            host = %host.name(),
            context = %ctx.path(),
            "redeploy: hot reload complete"
        );
        Ok(())
    }
}

/// Background task wrapper for [`HotRedeployer`].
///
/// Holds an `Arc<Host>` plus a shared [`HotRedeployer`] and exposes a single
/// async entry point — [`RedeployTask::run`] — that the CLI can `tokio::spawn`
/// for the lifetime of the server.
#[derive(Debug, Clone)]
pub struct RedeployTask {
    /// The host whose reloadable contexts are watched.
    host: Arc<Host>,
    /// The redeployer driving each tick.
    redeployer: HotRedeployer,
}

impl RedeployTask {
    /// Create a task that watches `host`.
    pub fn new(host: Arc<Host>) -> Self {
        Self {
            host,
            redeployer: HotRedeployer::new(),
        }
    }

    /// The host this task is bound to.
    pub fn host(&self) -> &Arc<Host> {
        &self.host
    }

    /// Drive the redeploy loop until `shutdown` fires.
    ///
    /// Ticks every `interval`. The loop is cancel-safe: it `select!`s the
    /// interval timer against the `shutdown` channel, so a `true` value (or a
    /// dropped sender) ends it promptly. An in-flight redeploy always finishes
    /// before the loop exits, leaving the host's contexts in a consistent
    /// lifecycle state.
    pub async fn run(host: Arc<Host>, interval: Duration, mut shutdown: watch::Receiver<bool>) {
        // Allow a pre-set shutdown signal to short-circuit before we even
        // start ticking.
        if *shutdown.borrow() {
            return;
        }

        let task = RedeployTask::new(host);
        let mut ticker = tokio::time::interval(interval);
        // The first tick of a fresh interval fires immediately. Burn it so
        // the first *real* tick is one `interval` in — same shape as
        // `BackgroundEngine::run`.
        ticker.tick().await;

        tracing::info!(
            host = %task.host.name(),
            interval_ms = interval.as_millis() as u64,
            "hot redeployer started"
        );

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    task.redeployer.tick(&task.host).await;
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        tracing::info!(
                            host = %task.host.name(),
                            "hot redeployer stopping"
                        );
                        return;
                    }
                }
            }
        }
    }
}

/// Compute the combined "last modified" time across `WEB-INF/web.xml` and
/// every entry under `WEB-INF/classes/**`.
///
/// Returns `None` if the `WEB-INF` directory itself does not exist — that's a
/// normal transient state during startup and not an error. Missing
/// `web.xml` or `classes/` are individually tolerated as long as at least one
/// of them yields an mtime.
fn combined_mtime(doc_base: &Path) -> Option<SystemTime> {
    let web_inf = doc_base.join("WEB-INF");
    if !web_inf.is_dir() {
        return None;
    }

    let mut newest: Option<SystemTime> = None;
    let mut visited = 0usize;

    // web.xml — checked first because it's the cheapest and most common.
    let web_xml = web_inf.join("web.xml");
    if let Ok(meta) = std::fs::metadata(&web_xml) {
        if let Ok(m) = meta.modified() {
            newest = Some(m);
        }
    }

    // classes/** — recursive walk, with a guardrail against pathological
    // trees.
    let classes_root = web_inf.join("classes");
    if classes_root.is_dir() {
        walk_mtime(&classes_root, &mut newest, &mut visited);
    }

    newest
}

/// Recursively visit every entry under `root`, threading the newest observed
/// mtime through `newest`. Skips entries we can't stat — the same policy
/// [`crate::deployer::HostDeployer`] uses for noisy filesystem states.
fn walk_mtime(root: &Path, newest: &mut Option<SystemTime>, visited: &mut usize) {
    if *visited >= MAX_CLASSES_ENTRIES {
        return;
    }
    // Stat the directory itself — directory mtimes change when entries are
    // added or removed, which is exactly the signal we want for class
    // additions and deletions.
    if let Ok(meta) = std::fs::metadata(root) {
        if let Ok(m) = meta.modified() {
            *newest = Some(match *newest {
                Some(prev) if prev >= m => prev,
                _ => m,
            });
        }
    }

    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if *visited >= MAX_CLASSES_ENTRIES {
            return;
        }
        *visited += 1;
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if let Ok(m) = meta.modified() {
            *newest = Some(match *newest {
                Some(prev) if prev >= m => prev,
                _ => m,
            });
        }
        if meta.is_dir() {
            walk_mtime(&path, newest, visited);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Build a unique temp directory tag — kept consistent with the deployer
    /// tests so debugging stray leftovers is easy.
    fn unique_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tomcatrs-catalina-redeploy-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    /// Build a minimal exploded webapp with the given `web.xml` body under a
    /// fresh temp directory. Returns the doc_base.
    fn make_webapp(tag: &str, web_xml_body: &str) -> PathBuf {
        let root = unique_dir(tag);
        let web_inf = root.join("WEB-INF");
        fs::create_dir_all(web_inf.join("classes")).unwrap();
        fs::write(web_inf.join("web.xml"), web_xml_body).unwrap();
        root
    }

    const TINY_WEB_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<web-app>
  <servlet>
    <servlet-name>s</servlet-name>
    <servlet-class>C</servlet-class>
  </servlet>
</web-app>
"#;

    #[tokio::test]
    async fn first_tick_records_initial_mtime_without_redeploying() {
        let doc_base = make_webapp("initial", TINY_WEB_XML);

        let context = Arc::new(Context::new("/app", doc_base.clone(), true, vec![]));
        let host = Host::new(
            "localhost",
            doc_base.clone(),
            vec![],
            vec![Arc::clone(&context)],
        );

        let redeployer = HotRedeployer::new();
        let count = redeployer.tick(&host).await;
        assert_eq!(count, 0, "first observation must not trigger a redeploy");

        fs::remove_dir_all(&doc_base).ok();
    }

    #[tokio::test]
    async fn mtime_change_triggers_redeploy_and_starts_context() {
        let doc_base = make_webapp("mtime-change", TINY_WEB_XML);

        let context = Arc::new(Context::new("/app", doc_base.clone(), true, vec![]));
        let host = Host::new(
            "localhost",
            doc_base.clone(),
            vec![],
            vec![Arc::clone(&context)],
        );

        let redeployer = HotRedeployer::new();
        // First tick: just record the baseline.
        assert_eq!(redeployer.tick(&host).await, 0);

        // Mutate web.xml so its mtime advances. Sleep briefly to ensure the
        // filesystem reports a strictly greater modification timestamp even
        // on coarse-grained mtime backends.
        tokio::time::sleep(Duration::from_millis(20)).await;
        fs::write(doc_base.join("WEB-INF").join("web.xml"), TINY_WEB_XML).unwrap();
        // Some filesystems still resolve mtimes to the second; force a small
        // additional delay to make the test deterministic.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        // Touch by re-writing with slightly different content too, so the
        // directory mtime moves on filesystems with second-level resolution.
        fs::write(
            doc_base.join("WEB-INF").join("web.xml"),
            format!("{TINY_WEB_XML}\n<!-- bump -->"),
        )
        .unwrap();

        // Second tick: must detect change and redeploy.
        let count = redeployer.tick(&host).await;
        assert_eq!(
            count, 1,
            "an mtime advance must trigger exactly one redeploy"
        );

        // After redeploy the context is wired up and started.
        assert_eq!(
            context.state(),
            tomcatrs_core::LifecycleState::Started,
            "context must end the redeploy in Started"
        );
        assert!(
            !context.wrappers().is_empty(),
            "descriptor should have been wired in by the redeploy"
        );

        fs::remove_dir_all(&doc_base).ok();
    }

    #[tokio::test]
    async fn non_reloadable_context_is_ignored() {
        let doc_base = make_webapp("non-reloadable", TINY_WEB_XML);

        // reloadable = false
        let context = Arc::new(Context::new("/app", doc_base.clone(), false, vec![]));
        let host = Host::new(
            "localhost",
            doc_base.clone(),
            vec![],
            vec![Arc::clone(&context)],
        );

        let redeployer = HotRedeployer::new();
        assert_eq!(redeployer.tick(&host).await, 0);

        // Bump mtime.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        fs::write(
            doc_base.join("WEB-INF").join("web.xml"),
            format!("{TINY_WEB_XML}\n<!-- bump -->"),
        )
        .unwrap();

        // Still no redeploys — reloadable flag gates the whole pipeline.
        assert_eq!(redeployer.tick(&host).await, 0);

        fs::remove_dir_all(&doc_base).ok();
    }

    #[tokio::test]
    async fn redeploy_task_run_stops_promptly_on_shutdown() {
        let doc_base = make_webapp("task-shutdown", TINY_WEB_XML);
        let context = Arc::new(Context::new("/app", doc_base.clone(), true, vec![]));
        let host = Arc::new(Host::new(
            "localhost",
            doc_base.clone(),
            vec![],
            vec![Arc::clone(&context)],
        ));

        let (tx, rx) = watch::channel(false);
        let handle = tokio::spawn(RedeployTask::run(
            Arc::clone(&host),
            Duration::from_millis(50),
            rx,
        ));

        tokio::time::sleep(Duration::from_millis(80)).await;
        tx.send(true).unwrap();

        let joined = tokio::time::timeout(Duration::from_millis(500), handle).await;
        assert!(joined.is_ok(), "redeploy task did not stop within timeout");
        joined.unwrap().unwrap();

        fs::remove_dir_all(&doc_base).ok();
    }
}
