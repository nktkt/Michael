//! [`Host`] — a virtual host.
//!
//! In Apache Tomcat a `Host` is a virtual host: a canonical name, a set of DNS
//! aliases, an `appBase` directory scanned for applications, and the
//! [`Context`]s deployed under it. This port keeps that shape; auto-deployment
//! scanning of `app_base` is left to the deployer in a later crate.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
#[cfg(test)]
use tomcatrs_config::ContextConfig;
use tomcatrs_config::HostConfig;
use tomcatrs_core::{Lifecycle, LifecycleContext, LifecycleState, Result};

use crate::context::Context;
use crate::state::StateCell;

/// A virtual host within an [`Engine`](crate::Engine).
#[derive(Debug)]
pub struct Host {
    /// Canonical host name (e.g. `localhost`).
    name: String,
    /// Additional DNS aliases that also route to this host.
    aliases: Vec<String>,
    /// Directory scanned for deployable applications.
    app_base: PathBuf,
    /// Whether applications dropped into `app_base` are auto-deployed and
    /// auto-undeployed by a deployment watcher.
    auto_deploy: bool,
    /// Deployed contexts, keyed by their context path.
    contexts: DashMap<String, Arc<Context>>,
    /// Current lifecycle state.
    state: StateCell,
}

impl Host {
    /// Create a host named `name` serving applications from `app_base`, with
    /// `aliases` and pre-built `contexts`.
    ///
    /// `auto_deploy` defaults to `true`, matching Tomcat's `Host` default; use
    /// [`Host::with_auto_deploy`] to override it, or build from a
    /// [`HostConfig`] with [`Host::from_config`].
    pub fn new(
        name: impl Into<String>,
        app_base: PathBuf,
        aliases: Vec<String>,
        contexts: Vec<Arc<Context>>,
    ) -> Self {
        let map = DashMap::new();
        for ctx in contexts {
            map.insert(ctx.path().to_string(), ctx);
        }
        Self {
            name: name.into(),
            aliases,
            app_base,
            auto_deploy: true,
            contexts: map,
            state: StateCell::new(),
        }
    }

    /// Set whether this host auto-deploys applications dropped into its
    /// `app_base`, returning `self` for builder-style chaining.
    pub fn with_auto_deploy(mut self, auto_deploy: bool) -> Self {
        self.auto_deploy = auto_deploy;
        self
    }

    /// Build a host from its parsed [`HostConfig`], constructing one
    /// [`Context`] per declared `ContextConfig`.
    pub fn from_config(config: &HostConfig) -> Self {
        let contexts = config
            .contexts
            .iter()
            .map(|c| Arc::new(Context::from_config(c)))
            .collect();
        Host::new(
            config.name.clone(),
            config.app_base.clone(),
            config.aliases.clone(),
            contexts,
        )
        .with_auto_deploy(config.auto_deploy)
    }

    /// The canonical host name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The DNS aliases routed to this host.
    pub fn aliases(&self) -> &[String] {
        &self.aliases
    }

    /// The directory scanned for deployable applications.
    pub fn app_base(&self) -> &Path {
        &self.app_base
    }

    /// Whether applications dropped into `app_base` are auto-deployed (and, by
    /// a [`DeploymentWatcher`](crate::deployer::DeploymentWatcher),
    /// auto-undeployed).
    pub fn auto_deploy(&self) -> bool {
        self.auto_deploy
    }

    /// The map of deployed contexts, keyed by context path.
    pub fn contexts(&self) -> &DashMap<String, Arc<Context>> {
        &self.contexts
    }

    /// Look up a context by its exact context path.
    pub fn context(&self, path: &str) -> Option<Arc<Context>> {
        self.contexts.get(path).map(|c| Arc::clone(c.value()))
    }

    /// Deploy (or replace) a context under this host.
    pub fn add_context(&self, context: Arc<Context>) {
        self.contexts.insert(context.path().to_string(), context);
    }

    /// Register a context under this host, keyed by its context path.
    ///
    /// This is the entry point used by the [`deployer`](crate::deployer) when
    /// it discovers a web application in `app_base`; it is an alias for
    /// [`Host::add_context`] kept as a distinct, intention-revealing name for
    /// auto-deployment call sites.
    pub fn register_context(&self, context: Arc<Context>) {
        self.add_context(context);
    }

    /// Remove the context mounted at `path`, returning it if one was deployed.
    pub fn remove_context(&self, path: &str) -> Option<Arc<Context>> {
        self.contexts.remove(path).map(|(_, ctx)| ctx)
    }

    /// The host's current [`LifecycleState`].
    pub fn state(&self) -> LifecycleState {
        self.state.get()
    }

    /// A human-readable component name for log lines.
    fn component_name(&self, ctx: &LifecycleContext) -> String {
        format!("{}/{}", ctx.name, self.name)
    }
}

#[async_trait]
impl Lifecycle for Host {
    async fn init(&self, ctx: &LifecycleContext) -> Result<()> {
        let name = self.component_name(ctx);
        tracing::info!(component = %name, app_base = %self.app_base.display(),
            aliases = ?self.aliases, contexts = self.contexts.len(), "host init");
        let child = LifecycleContext::new(&name);
        for entry in self.contexts.iter() {
            entry.value().init(&child).await?;
        }
        self.state.set(LifecycleState::Initialized);
        Ok(())
    }

    async fn start(&self, ctx: &LifecycleContext) -> Result<()> {
        let name = self.component_name(ctx);
        tracing::info!(component = %name, "host start");
        self.state.set(LifecycleState::Starting);
        let child = LifecycleContext::new(&name);
        for entry in self.contexts.iter() {
            entry.value().start(&child).await?;
        }
        self.state.set(LifecycleState::Started);
        Ok(())
    }

    async fn stop(&self, ctx: &LifecycleContext) -> Result<()> {
        let name = self.component_name(ctx);
        tracing::info!(component = %name, "host stop");
        self.state.set(LifecycleState::Stopping);
        let child = LifecycleContext::new(&name);
        for entry in self.contexts.iter() {
            entry.value().stop(&child).await?;
        }
        self.state.set(LifecycleState::Stopped);
        Ok(())
    }

    async fn destroy(&self, ctx: &LifecycleContext) -> Result<()> {
        let name = self.component_name(ctx);
        tracing::info!(component = %name, "host destroy");
        let child = LifecycleContext::new(&name);
        for entry in self.contexts.iter() {
            entry.value().destroy(&child).await?;
        }
        self.state.set(LifecycleState::Destroyed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn host_cascades_lifecycle_to_contexts() {
        let ctx = Arc::new(Context::new(
            "/app",
            PathBuf::from("/tmp/app"),
            false,
            vec![],
        ));
        let host = Host::new(
            "localhost",
            PathBuf::from("/webapps"),
            vec!["127.0.0.1".to_string()],
            vec![Arc::clone(&ctx)],
        );
        let lc = LifecycleContext::new("Catalina");

        host.init(&lc).await.unwrap();
        assert_eq!(host.state(), LifecycleState::Initialized);
        assert_eq!(ctx.state(), LifecycleState::Initialized);

        host.start(&lc).await.unwrap();
        assert_eq!(host.state(), LifecycleState::Started);
        assert_eq!(ctx.state(), LifecycleState::Started);

        host.stop(&lc).await.unwrap();
        assert_eq!(host.state(), LifecycleState::Stopped);

        host.destroy(&lc).await.unwrap();
        assert_eq!(host.state(), LifecycleState::Destroyed);
        assert_eq!(ctx.state(), LifecycleState::Destroyed);
    }

    #[test]
    fn from_config_builds_contexts_and_aliases() {
        let cfg = HostConfig {
            name: "localhost".to_string(),
            app_base: PathBuf::from("webapps"),
            aliases: vec!["www.localhost".to_string()],
            auto_deploy: true,
            contexts: vec![ContextConfig {
                path: "/app".to_string(),
                doc_base: PathBuf::from("app"),
                reloadable: false,
            }],
        };
        let host = Host::from_config(&cfg);
        assert_eq!(host.name(), "localhost");
        assert_eq!(host.aliases(), &["www.localhost".to_string()]);
        assert!(host.context("/app").is_some());
    }
}
