//! `tomcatrs` — the command-line entry point for the **Tomcat-RS Compatibility
//! Runtime**.
//!
//! This binary crate ties the workspace together: it parses the CLI, loads a
//! [`tomcatrs_config::ServerConfig`], boots a [`tomcatrs_catalina::Server`]
//! through its [`tomcatrs_core::Lifecycle`], and stands up a
//! [`tomcatrs_coyote::HttpConnector`] per HTTP/1.1 connector backed by a
//! [`tomcatrs_catalina::adapter::CatalinaAdapter`], which routes every request
//! through the Catalina mapper (`Engine` ➜ `Host` ➜ `Context` ➜ `Wrapper`).
//!
//! ```text
//! tomcatrs run          [--config server.xml] [--port N] [--app-base DIR] [--log-level L]
//! tomcatrs check-config <server.xml>
//! tomcatrs version
//! ```
//!
//! Servlet/JSP execution is delegated to the JVM bridge and is intentionally
//! *not* wired into this MVP — a mapped servlet currently yields a `501`
//! routing placeholder, while contexts with static resources are served from
//! their document base.

#![deny(missing_docs)]

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use tomcatrs_catalina::adapter::CatalinaAdapter;
use tomcatrs_config::{Protocol, ServerConfig};
use tomcatrs_core::{Lifecycle, LifecycleContext};
use tomcatrs_coyote::HttpConnector;

/// The `tomcatrs` command-line interface.
#[derive(Debug, Parser)]
#[command(
    name = "tomcatrs",
    version,
    about = "Tomcat-RS Compatibility Runtime — an incremental Rust rewrite of Apache Tomcat",
    long_about = None,
)]
struct Cli {
    /// The subcommand to run.
    #[command(subcommand)]
    command: Command,
}

/// The set of `tomcatrs` subcommands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Start the server and serve traffic until interrupted.
    Run(RunArgs),

    /// Parse a `server.xml` and print the resolved configuration model.
    CheckConfig {
        /// Path to the `server.xml` file to validate.
        path: PathBuf,
    },

    /// Print the runtime version and exit.
    Version,
}

/// Flags accepted by the `run` subcommand.
#[derive(Debug, Parser)]
struct RunArgs {
    /// Path to a `server.xml`. When omitted, the built-in development
    /// configuration ([`ServerConfig::default_dev`]) is used.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Override the port of the first HTTP/1.1 connector.
    #[arg(long, value_name = "PORT")]
    port: Option<u16>,

    /// Override the document root served by the static adapter.
    #[arg(long, value_name = "DIR", default_value = "webapps")]
    app_base: PathBuf,

    /// Logging verbosity (`trace`, `debug`, `info`, `warn`, `error`).
    #[arg(long, value_name = "LEVEL", default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Run(args) => run(args).await,
        Command::CheckConfig { path } => check_config(&path),
        Command::Version => {
            println!(
                "Tomcat-RS Compatibility Runtime v{}",
                tomcatrs_core::VERSION
            );
            Ok(())
        }
    }
}

/// Implements `tomcatrs run`.
async fn run(args: RunArgs) -> anyhow::Result<()> {
    // 1. Structured logging first, so every subsequent step is observable.
    tomcatrs_observability::init_tracing(&args.log_level);

    // 2. Load the configuration model, then apply the `--port` override to the
    //    first HTTP/1.1 connector we find.
    let mut config = match &args.config {
        Some(path) => ServerConfig::from_xml_file(path)
            .with_context(|| format!("failed to load server.xml from {}", path.display()))?,
        None => {
            tracing::info!("no --config given; using built-in development configuration");
            ServerConfig::default_dev()
        }
    };

    if let Some(port) = args.port {
        match first_http11_connector_mut(&mut config) {
            Some(connector) => {
                tracing::info!(
                    old_port = connector.port,
                    new_port = port,
                    "overriding first HTTP/1.1 connector port via --port",
                );
                connector.port = port;
            }
            None => {
                tracing::warn!(
                    "--port {port} given but the configuration has no HTTP/1.1 connector to override"
                );
            }
        }
    }

    // 3. Bring up the Catalina container tree.
    let server = tomcatrs_catalina::Server::from_config(&config)
        .context("failed to build the Catalina server from configuration")?;
    let ctx = LifecycleContext::new("Catalina");
    server
        .init(&ctx)
        .await
        .context("Catalina server failed to initialize")?;
    server
        .start(&ctx)
        .await
        .context("Catalina server failed to start")?;
    log_container_tree(&config);

    // 4. Stand up one HTTP/1.1 connector per matching `<Connector>`, each
    //    backed by a `CatalinaAdapter` that routes through the live container
    //    tree. Other protocols are recognised but not served in v0.1.0.
    let app_base = args.app_base.clone();
    let mut connector_tasks = Vec::new();
    let mut http11_count = 0usize;

    for service in server.services() {
        // The adapter for this service routes against its engine's host tree;
        // unmatched requests fall back to `--app-base` so the default
        // `webapps/ROOT` page and proper 404s still work.
        let adapter: Arc<CatalinaAdapter> = Arc::new(CatalinaAdapter::new(
            Arc::clone(service.engine()),
            app_base.clone(),
        ));

        for connector_cfg in service.connector_configs() {
            match connector_cfg.protocol {
                Protocol::Http11 => {
                    http11_count += 1;
                    let connector =
                        HttpConnector::new(connector_cfg.clone(), Arc::clone(&adapter) as Arc<_>);
                    tracing::info!(
                        service = %service.name(),
                        port = connector_cfg.port,
                        app_base = %app_base.display(),
                        "starting HTTP/1.1 connector (CatalinaAdapter)",
                    );
                    connector_tasks.push(tokio::spawn(async move { connector.serve().await }));
                }
                Protocol::Http2 => {
                    tracing::warn!(
                        service = %service.name(),
                        port = connector_cfg.port,
                        "HTTP/2 connector is not served in v0.1.0 — skipping",
                    );
                }
                Protocol::Ajp => {
                    tracing::warn!(
                        service = %service.name(),
                        port = connector_cfg.port,
                        "AJP connector is not served in v0.1.0 — skipping",
                    );
                }
            }
        }
    }

    if http11_count == 0 {
        tracing::warn!("no HTTP/1.1 connectors configured — the server will accept no traffic");
    }

    // 5. Block until Ctrl-C, then shut the container down in an orderly way.
    tracing::info!("Tomcat-RS is up; press Ctrl-C to shut down");
    tokio::signal::ctrl_c()
        .await
        .context("failed to install Ctrl-C handler")?;
    tracing::info!("shutdown signal received; stopping Tomcat-RS");

    // Connector tasks hold `serve()` futures that run until the process exits;
    // abort them so their listening sockets are released promptly.
    for task in &connector_tasks {
        task.abort();
    }

    server
        .stop(&ctx)
        .await
        .context("Catalina server failed to stop cleanly")?;
    server
        .destroy(&ctx)
        .await
        .context("Catalina server failed to release resources")?;
    tracing::info!("Tomcat-RS shut down cleanly");

    Ok(())
}

/// Implements `tomcatrs check-config <PATH>`.
///
/// Parses the document and prints the resolved model. Returns an error (which
/// `main` turns into a non-zero exit code) on any parse failure.
fn check_config(path: &std::path::Path) -> anyhow::Result<()> {
    let config = ServerConfig::from_xml_file(path)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    println!("server.xml: {}", path.display());
    println!("  shutdown port : {}", config.port);
    println!("  shutdown token: {}", config.shutdown);
    println!("  services      : {}", config.services.len());

    for service in &config.services {
        println!("  Service \"{}\"", service.name);
        println!("    connectors: {}", service.connectors.len());
        for connector in &service.connectors {
            let addr = connector
                .address
                .map(|a| a.to_string())
                .unwrap_or_else(|| "*".to_string());
            let tls = if connector.tls.is_some() {
                " (TLS)"
            } else {
                ""
            };
            println!(
                "      - {:?} {}:{}{}",
                connector.protocol, addr, connector.port, tls
            );
        }
        println!(
            "    Engine \"{}\" (default-host: {})",
            service.engine.name, service.engine.default_host
        );
        for host in &service.engine.hosts {
            println!(
                "      Host \"{}\" (app-base: {}, auto-deploy: {})",
                host.name,
                host.app_base.display(),
                host.auto_deploy
            );
            if !host.aliases.is_empty() {
                println!("        aliases: {}", host.aliases.join(", "));
            }
            for context in &host.contexts {
                let path = if context.path.is_empty() {
                    "\"\" (ROOT)"
                } else {
                    context.path.as_str()
                };
                println!(
                    "        Context {} -> {} (reloadable: {})",
                    path,
                    context.doc_base.display(),
                    context.reloadable
                );
            }
        }
    }

    println!("configuration is valid");
    Ok(())
}

/// Find the first HTTP/1.1 connector in the configuration tree, mutably.
fn first_http11_connector_mut(
    config: &mut ServerConfig,
) -> Option<&mut tomcatrs_config::ConnectorConfig> {
    config
        .services
        .iter_mut()
        .flat_map(|s| s.connectors.iter_mut())
        .find(|c| c.protocol == Protocol::Http11)
}

/// Log the resolved Host/Context tree at `start` time, mirroring Tomcat's
/// boot-time deployment log lines.
fn log_container_tree(config: &ServerConfig) {
    for service in &config.services {
        let engine = &service.engine;
        tracing::info!(
            service = %service.name,
            engine = %engine.name,
            default_host = %engine.default_host,
            "Catalina service started",
        );
        for host in &engine.hosts {
            tracing::info!(
                host = %host.name,
                app_base = %host.app_base.display(),
                auto_deploy = host.auto_deploy,
                contexts = host.contexts.len(),
                "host online",
            );
            for context in &host.contexts {
                let path = if context.path.is_empty() {
                    "\"\" (ROOT)".to_string()
                } else {
                    context.path.clone()
                };
                tracing::info!(
                    host = %host.name,
                    context = %path,
                    doc_base = %context.doc_base.display(),
                    reloadable = context.reloadable,
                    "context deployed",
                );
            }
        }
    }
}
