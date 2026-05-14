//! [`Engine`] — the request-processing engine.
//!
//! In Apache Tomcat an `Engine` is the top container of a [`Service`]: it owns
//! the set of virtual [`Host`]s and names the host used when an incoming
//! request matches none of them. This port keeps that shape; the engine is
//! also the natural anchor for the [`Mapper`](crate::Mapper).

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use tomcatrs_config::EngineConfig;
use tomcatrs_core::{Lifecycle, LifecycleContext, LifecycleState, Result};

use crate::host::Host;
use crate::state::StateCell;

/// The request-processing engine within a [`Service`](crate::Service).
#[derive(Debug)]
pub struct Engine {
    /// Engine name (conventionally `Catalina`).
    name: String,
    /// Name of the [`Host`] used when no other host matches a request.
    default_host: String,
    /// Virtual hosts served by this engine, keyed by canonical host name.
    hosts: DashMap<String, Arc<Host>>,
    /// Current lifecycle state.
    state: StateCell,
}

impl Engine {
    /// Create an empty engine named `name` with the given `default_host`.
    pub fn new(name: impl Into<String>, default_host: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            default_host: default_host.into(),
            hosts: DashMap::new(),
            state: StateCell::new(),
        }
    }

    /// Build an engine from its parsed [`EngineConfig`], constructing one
    /// [`Host`] per declared `HostConfig`.
    pub fn from_config(config: &EngineConfig) -> Self {
        let engine = Engine::new(config.name.clone(), config.default_host.clone());
        for host_cfg in &config.hosts {
            engine.add_host(Arc::new(Host::from_config(host_cfg)));
        }
        engine
    }

    /// The engine name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The name of the default host.
    pub fn default_host(&self) -> &str {
        &self.default_host
    }

    /// The map of virtual hosts, keyed by canonical host name.
    pub fn hosts(&self) -> &DashMap<String, Arc<Host>> {
        &self.hosts
    }

    /// Look up a host by its canonical name (case-insensitive).
    pub fn host(&self, name: &str) -> Option<Arc<Host>> {
        let key = name.to_ascii_lowercase();
        self.hosts.get(&key).map(|h| Arc::clone(h.value()))
    }

    /// Register a virtual host with this engine.
    ///
    /// The host is keyed by its lower-cased canonical name, since DNS names are
    /// case-insensitive and the [`Mapper`](crate::Mapper) looks them up that
    /// way.
    pub fn add_host(&self, host: Arc<Host>) {
        let key = host.name().to_ascii_lowercase();
        self.hosts.insert(key, host);
    }

    /// The engine's current [`LifecycleState`].
    pub fn state(&self) -> LifecycleState {
        self.state.get()
    }

    /// A human-readable component name for log lines.
    fn component_name(&self, ctx: &LifecycleContext) -> String {
        format!("{}/{}", ctx.name, self.name)
    }
}

#[async_trait]
impl Lifecycle for Engine {
    async fn init(&self, ctx: &LifecycleContext) -> Result<()> {
        let name = self.component_name(ctx);
        tracing::info!(component = %name, default_host = %self.default_host,
            hosts = self.hosts.len(), "engine init");

        // The default host must actually exist — surface a misconfiguration
        // early rather than 404-ing every unmatched request at runtime.
        if !self.hosts.is_empty() && self.host(&self.default_host).is_none() {
            self.state.set(LifecycleState::Failed);
            return Err(tomcatrs_core::Error::lifecycle(format!(
                "engine '{}' default host '{}' is not defined",
                self.name, self.default_host
            )));
        }

        let child = LifecycleContext::new(&name);
        for entry in self.hosts.iter() {
            entry.value().init(&child).await?;
        }
        self.state.set(LifecycleState::Initialized);
        Ok(())
    }

    async fn start(&self, ctx: &LifecycleContext) -> Result<()> {
        let name = self.component_name(ctx);
        tracing::info!(component = %name, "engine start");
        self.state.set(LifecycleState::Starting);
        let child = LifecycleContext::new(&name);
        for entry in self.hosts.iter() {
            entry.value().start(&child).await?;
        }
        self.state.set(LifecycleState::Started);
        Ok(())
    }

    async fn stop(&self, ctx: &LifecycleContext) -> Result<()> {
        let name = self.component_name(ctx);
        tracing::info!(component = %name, "engine stop");
        self.state.set(LifecycleState::Stopping);
        let child = LifecycleContext::new(&name);
        for entry in self.hosts.iter() {
            entry.value().stop(&child).await?;
        }
        self.state.set(LifecycleState::Stopped);
        Ok(())
    }

    async fn destroy(&self, ctx: &LifecycleContext) -> Result<()> {
        let name = self.component_name(ctx);
        tracing::info!(component = %name, "engine destroy");
        let child = LifecycleContext::new(&name);
        for entry in self.hosts.iter() {
            entry.value().destroy(&child).await?;
        }
        self.state.set(LifecycleState::Destroyed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn engine_cascades_lifecycle_to_hosts() {
        let host = Arc::new(Host::new("localhost", PathBuf::from("/w"), vec![], vec![]));
        let engine = Engine::new("Catalina", "localhost");
        engine.add_host(Arc::clone(&host));
        let lc = LifecycleContext::new("Catalina-service");

        engine.init(&lc).await.unwrap();
        assert_eq!(engine.state(), LifecycleState::Initialized);
        assert_eq!(host.state(), LifecycleState::Initialized);

        engine.start(&lc).await.unwrap();
        assert_eq!(engine.state(), LifecycleState::Started);

        engine.stop(&lc).await.unwrap();
        engine.destroy(&lc).await.unwrap();
        assert_eq!(engine.state(), LifecycleState::Destroyed);
        assert_eq!(host.state(), LifecycleState::Destroyed);
    }

    #[tokio::test]
    async fn init_fails_when_default_host_missing() {
        let host = Arc::new(Host::new("localhost", PathBuf::from("/w"), vec![], vec![]));
        let engine = Engine::new("Catalina", "ghost");
        engine.add_host(host);
        let lc = LifecycleContext::new("svc");

        let err = engine.init(&lc).await.unwrap_err();
        assert!(matches!(err, tomcatrs_core::Error::Lifecycle(_)));
        assert_eq!(engine.state(), LifecycleState::Failed);
    }

    #[test]
    fn host_lookup_is_case_insensitive() {
        let engine = Engine::new("Catalina", "localhost");
        engine.add_host(Arc::new(Host::new(
            "LocalHost",
            PathBuf::from("/w"),
            vec![],
            vec![],
        )));
        assert!(engine.host("localhost").is_some());
        assert!(engine.host("LOCALHOST").is_some());
    }
}
