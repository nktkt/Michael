//! [`HostDeployer`] / [`DeploymentWatcher`] — automatic WAR deployment for a
//! [`Host`].
//!
//! In Apache Tomcat the `HostConfig` lifecycle listener is what makes a virtual
//! host "live": at startup it scans the host's `appBase` and deploys whatever
//! it finds, and while running it periodically re-scans to pick up newly
//! dropped applications and notice removed ones. This module ports that
//! behaviour on top of [`tomcatrs_webapp::DeploymentScanner`].
//!
//! * [`HostDeployer`] is the one-shot side: [`HostDeployer::deploy_all`] scans
//!   the host's `app_base` once and registers a [`Context`] for every
//!   discovered [`DeploymentUnit`].
//! * [`DeploymentWatcher`] is the background side: [`DeploymentWatcher::watch`]
//!   re-runs the scan on a fixed interval, deploying applications that have
//!   appeared and (when the host has auto-deploy enabled) logging applications
//!   that have disappeared. It is cancel-safe — it returns promptly when its
//!   shutdown channel fires.
//!
//! Packed `.war` archives are discovered by the scanner but cannot be opened by
//! `tomcatrs_webapp::Webapp` in `v0.1.0`; the deployer logs and skips them
//! rather than failing the whole scan.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tomcatrs_core::{Error, Result};
use tomcatrs_webapp::{DeploymentKind, DeploymentScanner, DeploymentUnit, Webapp};

use crate::context::Context;
use crate::host::Host;

/// One-shot automatic deployer for a [`Host`].
///
/// The deployer is stateless: it owns a [`DeploymentScanner`] and nothing else,
/// so a single instance can be reused across hosts and across repeated scans.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostDeployer {
    scanner: DeploymentScanner,
}

impl HostDeployer {
    /// Create a new deployer.
    pub fn new() -> HostDeployer {
        HostDeployer {
            scanner: DeploymentScanner::new(),
        }
    }

    /// Scan `host`'s `app_base` once and deploy every application found,
    /// returning the number of contexts newly registered on the host.
    ///
    /// Each discovered [`DeploymentUnit`] is turned into a [`Context`] (see
    /// [`HostDeployer::deploy_unit`]) and registered via
    /// [`Host::register_context`]. A context whose path is already deployed is
    /// left untouched and not counted — re-running `deploy_all` is therefore
    /// cheap and idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Deployment`] if the host's `app_base` does not exist or
    /// is not a directory, or [`Error::Io`] if it cannot be read. Individual
    /// units that fail to open (for example packed `.war` archives, unsupported
    /// in `v0.1.0`) are logged and skipped rather than aborting the scan.
    pub fn deploy_all(&self, host: &Host) -> Result<usize> {
        let units = self.scanner.scan(host.app_base())?;
        let mut deployed = 0usize;

        for unit in units {
            if host.context(&unit.context_path).is_some() {
                tracing::debug!(
                    context_path = %unit.context_path,
                    "skipping already-deployed context"
                );
                continue;
            }
            match self.deploy_unit(host, &unit) {
                Ok(()) => deployed += 1,
                Err(err) => {
                    tracing::warn!(
                        context_path = %unit.context_path,
                        doc_base = %unit.doc_base.display(),
                        error = %err,
                        "skipping deployment unit that failed to deploy"
                    );
                }
            }
        }

        tracing::info!(
            host = %host.name(),
            deployed,
            "host auto-deployment complete"
        );
        Ok(deployed)
    }

    /// Deploy a single [`DeploymentUnit`] onto `host`.
    ///
    /// The unit is validated by opening it as a [`Webapp`] (which checks for a
    /// `WEB-INF/` directory and parses `web.xml` when present), then a
    /// [`Context`] mounted at the unit's context path is registered on the
    /// host.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Deployment`] if the unit is a packed `.war`
    /// ([`DeploymentKind::PackedWar`]) — unsupported in `v0.1.0` — or if the
    /// exploded directory is not a valid web application.
    pub fn deploy_unit(&self, host: &Host, unit: &DeploymentUnit) -> Result<()> {
        if unit.kind == DeploymentKind::PackedWar {
            return Err(Error::Deployment(format!(
                "packed .war deployment {} not supported in v0.1.0",
                unit.doc_base.display()
            )));
        }

        // Validate the on-disk layout before mounting anything.
        let webapp = Webapp::open(unit.context_path.clone(), &unit.doc_base)?;

        let context = Arc::new(Context::new(
            unit.context_path.clone(),
            webapp.doc_base().to_path_buf(),
            false,
            Vec::new(),
        ));
        host.register_context(Arc::clone(&context));

        // Wire the application's `WEB-INF/web.xml` (servlets, filters, welcome
        // files, listeners) into the freshly registered context. A webapp with
        // no `web.xml` deploys successfully with an empty report.
        let report = context.deploy()?;

        tracing::info!(
            host = %host.name(),
            context_path = %unit.context_path,
            doc_base = %unit.doc_base.display(),
            had_web_xml = report.had_web_xml,
            servlets = report.servlet_count,
            filters = report.filter_count,
            "deployed web application"
        );
        Ok(())
    }
}

/// Background re-scanner that keeps a [`Host`]'s deployments in sync with its
/// `app_base` directory.
///
/// Created with [`DeploymentWatcher::new`] and driven by
/// [`DeploymentWatcher::watch`], which loops on a [`tokio::time::interval`]
/// until its shutdown channel is signalled.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeploymentWatcher {
    deployer: HostDeployer,
}

impl DeploymentWatcher {
    /// Create a new watcher.
    pub fn new() -> DeploymentWatcher {
        DeploymentWatcher {
            deployer: HostDeployer::new(),
        }
    }

    /// Periodically re-scan `host`'s `app_base`, deploying applications that
    /// have appeared and — when the host has auto-deploy enabled — logging
    /// applications that have been removed from disk.
    ///
    /// The loop ticks every `interval`. Each tick:
    ///
    /// * runs [`HostDeployer::deploy_all`], registering any newly-appeared
    ///   web applications;
    /// * if [`Host::auto_deploy`] is `true`, compares the set of currently
    ///   registered context paths against what is still on disk and logs every
    ///   context whose document base has disappeared.
    ///
    /// # Cancel safety
    ///
    /// `watch` is cancel-safe: it `select!`s the interval tick against
    /// `shutdown`, and returns as soon as `shutdown` yields `true` or its
    /// sender is dropped. A scan in progress always runs to completion before
    /// the next tick, so the host's context map is never left half-updated.
    pub async fn watch(
        &self,
        host: Arc<Host>,
        interval: Duration,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let mut ticker = tokio::time::interval(interval);
        // The first tick fires immediately; skipping missed ticks keeps a slow
        // scan from causing a burst of catch-up ticks afterwards.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // If shutdown was already requested before we even started, do nothing.
        if *shutdown.borrow() {
            return;
        }

        tracing::info!(
            host = %host.name(),
            interval_ms = interval.as_millis() as u64,
            "deployment watcher started"
        );

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    self.scan_once(&host);
                }
                changed = shutdown.changed() => {
                    // `Err` means the sender was dropped — treat as shutdown.
                    if changed.is_err() || *shutdown.borrow() {
                        tracing::info!(
                            host = %host.name(),
                            "deployment watcher stopping"
                        );
                        return;
                    }
                }
            }
        }
    }

    /// Run a single watch iteration against `host`: deploy new applications,
    /// then (if the host auto-deploys) log any that have vanished from disk.
    fn scan_once(&self, host: &Host) {
        match self.deployer.deploy_all(host) {
            Ok(0) => {}
            Ok(n) => {
                tracing::info!(host = %host.name(), deployed = n, "watcher deployed new applications")
            }
            Err(err) => {
                tracing::warn!(host = %host.name(), error = %err, "deployment watcher scan failed");
                return;
            }
        }

        if host.auto_deploy() {
            for entry in host.contexts().iter() {
                let doc_base = entry.value().doc_base().to_path_buf();
                if !doc_base.exists() {
                    tracing::warn!(
                        host = %host.name(),
                        context_path = %entry.key(),
                        doc_base = %doc_base.display(),
                        "deployed application removed from disk; manual undeploy required"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// A unique temp directory for one test, cleaned up by the caller.
    fn unique_app_base(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tomcatrs-catalina-deployer-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    /// Create a minimal exploded webapp `name/` (with `WEB-INF/`) under
    /// `app_base`, the layout `Webapp::open` requires.
    fn make_exploded(app_base: &Path, name: &str) {
        fs::create_dir_all(app_base.join(name).join("WEB-INF")).unwrap();
    }

    #[test]
    fn deploy_all_registers_a_context_per_webapp() {
        let app_base = unique_app_base("deploy-all");
        fs::create_dir_all(&app_base).unwrap();
        make_exploded(&app_base, "ROOT");
        make_exploded(&app_base, "foo");
        // A directory without WEB-INF/ must not become a context.
        fs::create_dir_all(app_base.join("not-a-webapp")).unwrap();

        let host = Host::new("localhost", app_base.clone(), vec![], vec![]);
        let count = HostDeployer::new().deploy_all(&host).unwrap();

        assert_eq!(count, 2);
        assert!(host.context("/").is_some(), "ROOT should mount at /");
        assert!(host.context("/foo").is_some(), "foo should mount at /foo");
        assert!(host.context("/not-a-webapp").is_none());

        fs::remove_dir_all(&app_base).ok();
    }

    #[test]
    fn deploy_all_is_idempotent() {
        let app_base = unique_app_base("idempotent");
        fs::create_dir_all(&app_base).unwrap();
        make_exploded(&app_base, "foo");

        let host = Host::new("localhost", app_base.clone(), vec![], vec![]);
        let deployer = HostDeployer::new();

        assert_eq!(deployer.deploy_all(&host).unwrap(), 1);
        // Second pass deploys nothing new but does not error or duplicate.
        assert_eq!(deployer.deploy_all(&host).unwrap(), 0);
        assert_eq!(host.contexts().len(), 1);

        fs::remove_dir_all(&app_base).ok();
    }

    #[test]
    fn deploy_all_rejects_missing_app_base() {
        let host = Host::new("localhost", unique_app_base("missing"), vec![], vec![]);
        let err = HostDeployer::new().deploy_all(&host).unwrap_err();
        assert!(matches!(err, Error::Deployment(_)));
    }

    #[tokio::test]
    async fn watcher_deploys_then_stops_on_shutdown() {
        let app_base = unique_app_base("watch");
        fs::create_dir_all(&app_base).unwrap();
        make_exploded(&app_base, "foo");

        let host = Arc::new(Host::new("localhost", app_base.clone(), vec![], vec![]));
        let (tx, rx) = watch::channel(false);

        let watcher = DeploymentWatcher::new();
        let host_for_task = Arc::clone(&host);
        let handle = tokio::spawn(async move {
            watcher
                .watch(host_for_task, Duration::from_millis(20), rx)
                .await;
        });

        // Give the immediate first tick time to run its scan.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(host.context("/foo").is_some(), "watcher should deploy foo");

        // Signal shutdown; the watcher must return promptly.
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_millis(500), handle)
            .await
            .expect("watcher did not stop within timeout")
            .expect("watcher task panicked");

        fs::remove_dir_all(&app_base).ok();
    }
}
