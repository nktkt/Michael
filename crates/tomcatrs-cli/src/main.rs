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
//! tomcatrs preflight    --config server.xml [--strict]
//! tomcatrs version
//! ```
//!
//! Servlet/JSP execution is delegated to the JVM bridge and is intentionally
//! *not* wired into this MVP — a mapped servlet currently yields a `501`
//! routing placeholder, while contexts with static resources are served from
//! their document base.

#![deny(missing_docs)]

mod preflight;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use tomcatrs_catalina::adapter::CatalinaAdapter;
use tomcatrs_config::{Protocol, ServerConfig};
use tomcatrs_core::{Lifecycle, LifecycleContext};
use tomcatrs_coyote::{HttpConnector, Shutdown};

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

    /// Run production-readiness checks against a `server.xml`.
    ///
    /// Each check prints one `[OK] / [WARN] / [FAIL]: <message>` line and
    /// the command exits non-zero on any failure (or, with `--strict`, on
    /// any warning).
    Preflight(PreflightArgs),

    /// Print the runtime version and exit.
    Version,
}

/// Flags accepted by the `preflight` subcommand.
#[derive(Debug, Parser)]
struct PreflightArgs {
    /// Path to the `server.xml` to validate.
    #[arg(long, value_name = "PATH")]
    config: PathBuf,

    /// Treat warnings as failures (non-zero exit if any `WARN` rows appear).
    #[arg(long)]
    strict: bool,
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

    /// Graceful-shutdown drain timeout, in seconds.
    ///
    /// On `SIGINT` (Ctrl-C) or, on Unix, `SIGTERM`, every HTTP/1.1 connector
    /// stops accepting new connections immediately and waits this long for
    /// in-flight requests to complete. Requests still running when the
    /// timeout fires are aborted (the operator sees a `forced shutdown` log
    /// line). A second signal during the drain forces an immediate abort.
    #[arg(long, value_name = "SECS", default_value_t = 30)]
    shutdown_timeout: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Run(args) => run(args).await,
        Command::CheckConfig { path } => check_config(&path),
        Command::Preflight(args) => {
            let code = preflight::run_preflight(&args.config, args.strict)?;
            // `main` is `anyhow::Result<()>`; convert a non-zero preflight
            // code into an explicit process exit so the shell sees it.
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
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
    //
    //    Every connector subscribes to a single shared `Shutdown` handle. When
    //    we trigger it (below) each accept loop stops accepting *and* drains
    //    its in-flight requests within `--shutdown-timeout` seconds.
    let app_base = args.app_base.clone();
    let shutdown = Shutdown::new();
    let drain_timeout = Duration::from_secs(args.shutdown_timeout);
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
                        shutdown_timeout_secs = args.shutdown_timeout,
                        "starting HTTP/1.1 connector (CatalinaAdapter)",
                    );
                    let rx = shutdown.subscribe();
                    connector_tasks.push(tokio::spawn(async move {
                        connector.serve_with_shutdown(rx, drain_timeout).await
                    }));
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

    // 5. Block until a shutdown signal arrives, then shut the container down
    //    in an orderly way.
    //
    //    Unix: wait for *either* `SIGINT` (Ctrl-C) or `SIGTERM` — the signal
    //    Docker / systemd / Kubernetes send for a graceful stop. A *second*
    //    signal during the drain forces an immediate abort: useful when a
    //    misbehaving handler is hung and the operator wants the process gone.
    //
    //    Windows: only `Ctrl-C` is portably available; the same "second
    //    signal forces" semantics apply.
    tracing::info!(
        shutdown_timeout_secs = args.shutdown_timeout,
        "Tomcat-RS is up; send SIGINT/SIGTERM (Ctrl-C) to shut down",
    );
    wait_for_shutdown_signal()
        .await
        .context("failed to install signal handlers")?;
    tracing::info!(
        shutdown_timeout_secs = args.shutdown_timeout,
        "shutdown signal received; draining in-flight requests then stopping Tomcat-RS",
    );

    // Signal every connector to drain. This drops their listening sockets
    // immediately (so a kernel `SYN` to the port gets `ECONNREFUSED`) and
    // gives in-flight per-connection tasks up to `drain_timeout` to finish.
    shutdown.trigger();

    // Race the connectors' drain against either (a) a second OS signal
    // (forced shutdown) or (b) `--shutdown-timeout` plus a small slop. The
    // connectors honour `drain_timeout` themselves; the outer budget here is
    // purely a safety net so a runaway connector can't pin the process open.
    let drain_all = async {
        for task in connector_tasks.drain(..) {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!(error = %e, "connector exited with error"),
                Err(e) if e.is_cancelled() => {}
                Err(e) => tracing::warn!(error = %e, "connector task panicked or was aborted"),
            }
        }
    };
    let outer_budget = drain_timeout + Duration::from_secs(2);
    tokio::select! {
        biased;

        // A second signal during the drain: forced shutdown. We don't abort
        // connector tasks here — they're still tracked above — we just stop
        // waiting and proceed to Server::stop()/destroy().
        _ = wait_for_shutdown_signal() => {
            tracing::warn!("forced shutdown — in-flight requests may be cut");
        }

        // Happy path: every connector reported a clean drain.
        _ = drain_all => {
            tracing::info!("all connectors drained");
        }

        // Belt-and-braces: don't let a buggy connector pin the process open.
        _ = tokio::time::sleep(outer_budget) => {
            tracing::warn!(
                budget_secs = outer_budget.as_secs(),
                "connector drain exceeded its outer budget — proceeding with Server::stop()",
            );
        }
    }

    server
        .stop(&ctx)
        .await
        .context("Catalina server failed to stop cleanly")?;
    server
        .destroy(&ctx)
        .await
        .context("Catalina server failed to release resources")?;

    // Note: a `JvmRuntime`, when used, drains and detaches its worker pool
    // from `impl Drop` (see `tomcatrs_servlet_bridge::jvm`). The CLI doesn't
    // currently own one directly — webapps that wire one up rely on RAII to
    // tear it down as the owner goes out of scope. If a future revision
    // gives the CLI an explicit `JvmRuntime`, call `JvmRuntime::shutdown()`
    // here before returning.
    tracing::info!("Tomcat-RS shut down cleanly");

    Ok(())
}

/// Wait for the first OS signal that should trigger a graceful shutdown.
///
/// * On Unix this resolves when either `SIGINT` (Ctrl-C, when run from a
///   terminal) or `SIGTERM` (the default Docker / systemd / Kubernetes
///   stop signal) is delivered to the process.
/// * On non-Unix targets (Windows) only Ctrl-C is portably available, so
///   that's all we listen for.
///
/// Each call installs a *fresh* handler, so calling this twice — once before
/// the drain and once during — gives the operator the "second signal forces
/// abort" semantics the CLI advertises.
async fn wait_for_shutdown_signal() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigint =
            signal(SignalKind::interrupt()).context("failed to install SIGINT handler")?;
        let mut sigterm =
            signal(SignalKind::terminate()).context("failed to install SIGTERM handler")?;
        tokio::select! {
            _ = sigint.recv() => {
                tracing::info!(signal = "SIGINT", "received OS signal");
            }
            _ = sigterm.recv() => {
                tracing::info!(signal = "SIGTERM", "received OS signal");
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .context("failed to install Ctrl-C handler")?;
        tracing::info!(signal = "CTRL_C", "received OS signal");
        Ok(())
    }
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
