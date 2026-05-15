//! [`Server`] — the top-level Catalina container.
//!
//! In Apache Tomcat the `Server` is the root of the component tree and the
//! single point of process-wide lifecycle control: it owns the [`Service`]s,
//! the shutdown port, and the orderly start/stop sequence. [`Server::from_config`]
//! turns a parsed [`ServerConfig`] into the entire live tree.

use std::sync::Arc;

use async_trait::async_trait;
use tomcatrs_config::ServerConfig;
use tomcatrs_core::{Lifecycle, LifecycleContext, LifecycleState, Result};

use crate::service::Service;
use crate::state::StateCell;

/// The root container of a Tomcat-RS process.
#[derive(Debug)]
pub struct Server {
    /// Port the shutdown listener binds to (driven by the CLI in v0.1.0).
    shutdown_port: u16,
    /// Magic string a client must send to trigger an orderly shutdown.
    shutdown_command: String,
    /// The services hosted by this server.
    services: Vec<Arc<Service>>,
    /// Current lifecycle state.
    state: StateCell,
}

impl Server {
    /// Build the entire Catalina component tree from a parsed [`ServerConfig`].
    ///
    /// This constructs, for each [`ServiceConfig`](tomcatrs_config::ServiceConfig),
    /// a [`Service`] ➜ [`Engine`](crate::Engine) ➜ [`Host`](crate::Host) ➜
    /// [`Context`](crate::Context) sub-tree. No lifecycle transition is run —
    /// the returned `Server` is in [`LifecycleState::New`]; call
    /// [`Lifecycle::init`] and [`Lifecycle::start`] to bring it up.
    ///
    /// # Errors
    ///
    /// Returns [`tomcatrs_core::Error::Config`] if the configuration describes
    /// no services, since a server with nothing to serve is almost certainly a
    /// mistake.
    pub fn from_config(config: &ServerConfig) -> Result<Server> {
        if config.services.is_empty() {
            return Err(tomcatrs_core::Error::config(
                "server configuration defines no <Service> elements",
            ));
        }
        let services = config
            .services
            .iter()
            .map(|svc| Arc::new(Service::from_config(svc)))
            .collect();
        Ok(Server {
            shutdown_port: config.port,
            shutdown_command: config.shutdown.clone(),
            services,
            state: StateCell::new(),
        })
    }

    /// The configured shutdown port.
    pub fn shutdown_port(&self) -> u16 {
        self.shutdown_port
    }

    /// The configured shutdown command string.
    pub fn shutdown_command(&self) -> &str {
        &self.shutdown_command
    }

    /// The services hosted by this server.
    pub fn services(&self) -> &[Arc<Service>] {
        &self.services
    }

    /// Look up a service by name.
    pub fn service(&self, name: &str) -> Option<&Arc<Service>> {
        self.services.iter().find(|s| s.name() == name)
    }

    /// The server's current [`LifecycleState`].
    pub fn state(&self) -> LifecycleState {
        self.state.get()
    }
}

#[async_trait]
impl Lifecycle for Server {
    async fn init(&self, ctx: &LifecycleContext) -> Result<()> {
        tracing::info!(component = %ctx.name, shutdown_port = self.shutdown_port,
            services = self.services.len(), "server init");
        for service in &self.services {
            service.init(ctx).await?;
        }
        self.state.set(LifecycleState::Initialized);
        Ok(())
    }

    async fn start(&self, ctx: &LifecycleContext) -> Result<()> {
        tracing::info!(component = %ctx.name, "server start");
        self.state.set(LifecycleState::Starting);
        for service in &self.services {
            service.start(ctx).await?;
        }
        self.state.set(LifecycleState::Started);
        tracing::info!(component = %ctx.name, "server started");
        Ok(())
    }

    async fn stop(&self, ctx: &LifecycleContext) -> Result<()> {
        tracing::info!(component = %ctx.name, "server stop");
        self.state.set(LifecycleState::Stopping);
        for service in &self.services {
            service.stop(ctx).await?;
        }
        self.state.set(LifecycleState::Stopped);
        Ok(())
    }

    async fn destroy(&self, ctx: &LifecycleContext) -> Result<()> {
        tracing::info!(component = %ctx.name, "server destroy");
        for service in &self.services {
            service.destroy(ctx).await?;
        }
        self.state.set(LifecycleState::Destroyed);

        // Make the shutdown cascade visible to operators reading the log:
        // count every level of the container tree so it's clear what was
        // taken down in this `destroy()` call. The numbers are derived
        // directly from the live tree we just shut down, not re-read from
        // config, so they reflect the actual cascade.
        let mut service_count = 0usize;
        let mut host_count = 0usize;
        let mut context_count = 0usize;
        let mut wrapper_count = 0usize;
        for service in &self.services {
            service_count += 1;
            let engine = service.engine();
            let hosts = engine.hosts();
            host_count += hosts.len();
            for host_entry in hosts.iter() {
                let host = host_entry.value();
                let contexts = host.contexts();
                context_count += contexts.len();
                for context_entry in contexts.iter() {
                    wrapper_count += context_entry.value().wrappers().len();
                }
            }
        }
        tracing::info!(
            component = %ctx.name,
            services = service_count,
            hosts = host_count,
            contexts = context_count,
            wrappers = wrapper_count,
            "server destroy complete: Server → Service → Engine → Host → Context → Wrapper cascade finished",
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tomcatrs_config::{
        ConnectorConfig, ContextConfig, EngineConfig, HostConfig, Protocol, RequestLimits,
        ServiceConfig,
    };

    /// A hand-constructed multi-host, multi-context configuration.
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
                        aliases: vec!["127.0.0.1".to_string()],
                        auto_deploy: true,
                        contexts: vec![
                            ContextConfig {
                                path: "".to_string(),
                                doc_base: PathBuf::from("webapps/ROOT"),
                                reloadable: false,
                            },
                            ContextConfig {
                                path: "/app".to_string(),
                                doc_base: PathBuf::from("webapps/app"),
                                reloadable: true,
                            },
                        ],
                    }],
                },
            }],
        }
    }

    #[test]
    fn from_config_builds_the_whole_tree() {
        let server = Server::from_config(&sample_config()).unwrap();
        assert_eq!(server.shutdown_port(), 8005);
        assert_eq!(server.shutdown_command(), "SHUTDOWN");
        assert_eq!(server.services().len(), 1);

        let service = server.service("Catalina").unwrap();
        assert_eq!(service.connector_configs().len(), 1);

        let engine = service.engine();
        assert_eq!(engine.name(), "Catalina");
        assert_eq!(engine.default_host(), "localhost");

        let host = engine.host("localhost").unwrap();
        assert_eq!(host.aliases(), &["127.0.0.1".to_string()]);
        assert_eq!(host.contexts().len(), 2);
        assert!(host.context("").is_some());
        assert!(host.context("/app").is_some());
    }

    #[test]
    fn from_config_rejects_empty_server() {
        let cfg = ServerConfig {
            port: 8005,
            shutdown: "SHUTDOWN".to_string(),
            services: vec![],
        };
        let err = Server::from_config(&cfg).unwrap_err();
        assert!(matches!(err, tomcatrs_core::Error::Config(_)));
    }

    #[tokio::test]
    async fn server_lifecycle_cascades_through_whole_tree() {
        let server = Server::from_config(&sample_config()).unwrap();
        let ctx = LifecycleContext::new("Catalina");

        assert_eq!(server.state(), LifecycleState::New);

        server.init(&ctx).await.unwrap();
        assert_eq!(server.state(), LifecycleState::Initialized);

        server.start(&ctx).await.unwrap();
        assert_eq!(server.state(), LifecycleState::Started);
        assert!(server.state().is_available());

        // Every leaf of the tree should be Started too.
        let host = server
            .service("Catalina")
            .unwrap()
            .engine()
            .host("localhost")
            .unwrap();
        assert_eq!(host.state(), LifecycleState::Started);
        let app = host.context("/app").unwrap();
        assert_eq!(app.state(), LifecycleState::Started);

        server.stop(&ctx).await.unwrap();
        assert_eq!(server.state(), LifecycleState::Stopped);

        server.destroy(&ctx).await.unwrap();
        assert_eq!(server.state(), LifecycleState::Destroyed);
        assert!(server.state().is_terminal());
    }

    #[tokio::test]
    async fn default_dev_config_starts_cleanly() {
        let server = Server::from_config(&ServerConfig::default_dev()).unwrap();
        let ctx = LifecycleContext::new("Catalina");
        server.init(&ctx).await.unwrap();
        server.start(&ctx).await.unwrap();
        assert!(server.state().is_available());
    }
}
