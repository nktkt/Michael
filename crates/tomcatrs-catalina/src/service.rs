//! [`Service`] — binds connectors to an engine.
//!
//! In Apache Tomcat a `Service` groups one or more connectors with a single
//! [`Engine`]: connectors accept bytes off the wire, the engine turns mapped
//! requests into servlet invocations. In Tomcat-RS v0.1.0 the connectors
//! themselves are owned and driven by the `tomcatrs-cli` binary (they live in
//! `tomcatrs-coyote`), so a `Service` keeps the parsed
//! [`ConnectorConfig`]s purely for reference and drives the engine's
//! lifecycle.

use std::sync::Arc;

use async_trait::async_trait;
use tomcatrs_config::{ConnectorConfig, ServiceConfig};
use tomcatrs_core::{Lifecycle, LifecycleContext, LifecycleState, Result};

use crate::engine::Engine;
use crate::state::StateCell;

/// A service binding connectors to an [`Engine`] within a
/// [`Server`](crate::Server).
#[derive(Debug)]
pub struct Service {
    /// Service name (conventionally `Catalina`).
    name: String,
    /// The engine all of this service's connectors feed requests into.
    engine: Arc<Engine>,
    /// Parsed connector configuration, kept for reference; the connectors are
    /// constructed and run by the CLI in v0.1.0.
    connector_configs: Vec<ConnectorConfig>,
    /// Current lifecycle state.
    state: StateCell,
}

impl Service {
    /// Create a service named `name` driving `engine`, with `connector_configs`
    /// kept for reference.
    pub fn new(
        name: impl Into<String>,
        engine: Arc<Engine>,
        connector_configs: Vec<ConnectorConfig>,
    ) -> Self {
        Self {
            name: name.into(),
            engine,
            connector_configs,
            state: StateCell::new(),
        }
    }

    /// Build a service from its parsed [`ServiceConfig`].
    pub fn from_config(config: &ServiceConfig) -> Self {
        let engine = Arc::new(Engine::from_config(&config.engine));
        Service::new(config.name.clone(), engine, config.connectors.clone())
    }

    /// The service name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The engine driven by this service.
    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }

    /// The connector configurations attached to this service.
    pub fn connector_configs(&self) -> &[ConnectorConfig] {
        &self.connector_configs
    }

    /// The service's current [`LifecycleState`].
    pub fn state(&self) -> LifecycleState {
        self.state.get()
    }

    /// A human-readable component name for log lines.
    fn component_name(&self, ctx: &LifecycleContext) -> String {
        format!("{}/{}", ctx.name, self.name)
    }
}

#[async_trait]
impl Lifecycle for Service {
    async fn init(&self, ctx: &LifecycleContext) -> Result<()> {
        let name = self.component_name(ctx);
        tracing::info!(component = %name, connectors = self.connector_configs.len(),
            "service init");
        let child = LifecycleContext::new(&name);
        self.engine.init(&child).await?;
        self.state.set(LifecycleState::Initialized);
        Ok(())
    }

    async fn start(&self, ctx: &LifecycleContext) -> Result<()> {
        let name = self.component_name(ctx);
        tracing::info!(component = %name, "service start");
        self.state.set(LifecycleState::Starting);
        let child = LifecycleContext::new(&name);
        self.engine.start(&child).await?;
        self.state.set(LifecycleState::Started);
        Ok(())
    }

    async fn stop(&self, ctx: &LifecycleContext) -> Result<()> {
        let name = self.component_name(ctx);
        tracing::info!(component = %name, "service stop");
        self.state.set(LifecycleState::Stopping);
        let child = LifecycleContext::new(&name);
        self.engine.stop(&child).await?;
        self.state.set(LifecycleState::Stopped);
        Ok(())
    }

    async fn destroy(&self, ctx: &LifecycleContext) -> Result<()> {
        let name = self.component_name(ctx);
        tracing::info!(component = %name, "service destroy");
        let child = LifecycleContext::new(&name);
        self.engine.destroy(&child).await?;
        self.state.set(LifecycleState::Destroyed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn service_cascades_lifecycle_to_engine() {
        let engine = Arc::new(Engine::new("Catalina", "localhost"));
        let service = Service::new("Catalina", Arc::clone(&engine), vec![]);
        let lc = LifecycleContext::new("Catalina-server");

        service.init(&lc).await.unwrap();
        assert_eq!(service.state(), LifecycleState::Initialized);
        assert_eq!(engine.state(), LifecycleState::Initialized);

        service.start(&lc).await.unwrap();
        assert_eq!(service.state(), LifecycleState::Started);
        assert_eq!(engine.state(), LifecycleState::Started);

        service.stop(&lc).await.unwrap();
        service.destroy(&lc).await.unwrap();
        assert_eq!(service.state(), LifecycleState::Destroyed);
        assert_eq!(engine.state(), LifecycleState::Destroyed);
    }
}
