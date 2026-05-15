//! `tomcatrs-catalina` — the **Catalina** servlet-container component model for
//! the Tomcat-RS Compatibility Runtime.
//!
//! Catalina is the heart of Apache Tomcat: the nested tree of containers that
//! accepts a parsed request and routes it to a servlet. This crate ports that
//! tree to Rust, one component per module, each implementing the shared
//! [`tomcatrs_core::Lifecycle`] contract:
//!
//! ```text
//! Server                       (server.rs)  — top-level, owns Services
//!  └─ Service                  (service.rs) — binds connectors to an Engine
//!      └─ Engine               (engine.rs)  — request-processing engine
//!          └─ Host             (host.rs)    — a virtual host
//!              └─ Context      (context.rs) — a deployed web application
//!                  └─ Wrapper  (wrapper.rs) — a single servlet registration
//! ```
//!
//! [`Mapper`] is the routing component: given a host header and
//! a request URI it resolves the `(Host, Context, Wrapper)` triple exactly the
//! way Tomcat's `org.apache.catalina.mapper.Mapper` does — exact host/alias
//! match with a `default_host` fallback, longest-prefix context-path matching,
//! and servlet [`UrlPattern`] matching with the canonical
//! precedence (exact ➜ path-prefix ➜ extension ➜ default).
//!
//! # Lifecycle cascade
//!
//! Every transition on a parent container cascades to its children in tree
//! order. `init`/`start` descend the tree; `stop`/`destroy` likewise descend
//! (children are stopped before — conceptually alongside — their parent). Each
//! component owns a small [`LifecycleState`](tomcatrs_core::LifecycleState)
//! cell guarded by a [`parking_lot::Mutex`] for interior mutability, since the
//! `Lifecycle` trait takes `&self`.
//!
//! # Example
//!
//! ```no_run
//! use tomcatrs_catalina::Server;
//! use tomcatrs_config::ServerConfig;
//! use tomcatrs_core::{Lifecycle, LifecycleContext};
//!
//! # async fn run() -> tomcatrs_core::Result<()> {
//! let config = ServerConfig::default_dev();
//! let server = Server::from_config(&config)?;
//! let ctx = LifecycleContext::new("Catalina");
//! server.init(&ctx).await?;
//! server.start(&ctx).await?;
//! // ... serve traffic ...
//! server.stop(&ctx).await?;
//! server.destroy(&ctx).await?;
//! # Ok(())
//! # }
//! ```

#![deny(missing_docs)]

pub mod adapter;
pub mod background;
pub mod context;
pub mod default_servlet;
pub mod deployer;
pub mod engine;
pub mod filter;
pub mod host;
pub mod manager;
pub mod manager_ui;
pub mod mapper;
pub mod pipeline;
pub mod redeploy;
pub mod security_valve;
pub mod server;
pub mod service;
pub mod startup;
pub mod state;
pub mod valve;
pub mod wrapper;

pub use adapter::CatalinaAdapter;
pub use context::Context;
pub use engine::Engine;
pub use host::Host;
pub use mapper::{Mapper, MappingResult, UrlPattern};
pub use pipeline::StandardPipeline;
pub use server::Server;
pub use service::Service;
pub use state::StateCell;
pub use valve::Valve;
pub use wrapper::Wrapper;
