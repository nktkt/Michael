//! [`Context`] — a deployed web application.
//!
//! In Apache Tomcat a `Context` is one web application: it owns a context path
//! (the URL prefix it is mounted at), a document base (where its files live),
//! and a set of [`Wrapper`]s, one per servlet. This port keeps that shape;
//! classloading, filters and session management arrive in later crates.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tomcatrs_config::ContextConfig;
use tomcatrs_core::{Lifecycle, LifecycleContext, LifecycleState, Result};

use crate::state::StateCell;
use crate::wrapper::Wrapper;

/// A single deployed web application within a [`Host`](crate::Host).
#[derive(Debug)]
pub struct Context {
    /// Context path the application is mounted at (e.g. `/myapp`, or `""` for
    /// the ROOT context). Used as the key in the host's context map.
    path: String,
    /// Filesystem location of the exploded application or WAR.
    doc_base: PathBuf,
    /// Whether the context should be reloaded when its classes change.
    reloadable: bool,
    /// Servlet wrappers owned by this context.
    wrappers: Vec<Arc<Wrapper>>,
    /// Current lifecycle state.
    state: StateCell,
}

impl Context {
    /// Create a context mounted at `path`, served from `doc_base`, owning
    /// `wrappers`.
    pub fn new(
        path: impl Into<String>,
        doc_base: PathBuf,
        reloadable: bool,
        wrappers: Vec<Arc<Wrapper>>,
    ) -> Self {
        Self {
            path: path.into(),
            doc_base,
            reloadable,
            wrappers,
            state: StateCell::new(),
        }
    }

    /// Build a context from its parsed [`ContextConfig`].
    ///
    /// v0.1.0 has no `web.xml` parsing wired in here yet, so the context is
    /// created with no wrappers; servlets are added later by the deployer.
    pub fn from_config(config: &ContextConfig) -> Self {
        Context::new(
            config.path.clone(),
            config.doc_base.clone(),
            config.reloadable,
            Vec::new(),
        )
    }

    /// The context path this application is mounted at.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The filesystem document base of this application.
    pub fn doc_base(&self) -> &Path {
        &self.doc_base
    }

    /// Whether this context reloads on class changes.
    pub fn reloadable(&self) -> bool {
        self.reloadable
    }

    /// The servlet wrappers owned by this context.
    pub fn wrappers(&self) -> &[Arc<Wrapper>] {
        &self.wrappers
    }

    /// Look up a wrapper by its servlet name.
    pub fn wrapper(&self, servlet_name: &str) -> Option<&Arc<Wrapper>> {
        self.wrappers
            .iter()
            .find(|w| w.servlet_name() == servlet_name)
    }

    /// The context's current [`LifecycleState`].
    pub fn state(&self) -> LifecycleState {
        self.state.get()
    }

    /// A human-readable component name for log lines, e.g. `Catalina/[/myapp]`.
    fn component_name(&self, ctx: &LifecycleContext) -> String {
        let label = if self.path.is_empty() {
            "/"
        } else {
            &self.path
        };
        format!("{}/[{}]", ctx.name, label)
    }
}

#[async_trait]
impl Lifecycle for Context {
    async fn init(&self, ctx: &LifecycleContext) -> Result<()> {
        let name = self.component_name(ctx);
        tracing::info!(component = %name, doc_base = %self.doc_base.display(),
            reloadable = self.reloadable, wrappers = self.wrappers.len(), "context init");
        let child = LifecycleContext::new(&name);
        for wrapper in &self.wrappers {
            wrapper.init(&child).await?;
        }
        self.state.set(LifecycleState::Initialized);
        Ok(())
    }

    async fn start(&self, ctx: &LifecycleContext) -> Result<()> {
        let name = self.component_name(ctx);
        tracing::info!(component = %name, "context start");
        self.state.set(LifecycleState::Starting);
        let child = LifecycleContext::new(&name);
        for wrapper in &self.wrappers {
            wrapper.start(&child).await?;
        }
        self.state.set(LifecycleState::Started);
        Ok(())
    }

    async fn stop(&self, ctx: &LifecycleContext) -> Result<()> {
        let name = self.component_name(ctx);
        tracing::info!(component = %name, "context stop");
        self.state.set(LifecycleState::Stopping);
        let child = LifecycleContext::new(&name);
        for wrapper in &self.wrappers {
            wrapper.stop(&child).await?;
        }
        self.state.set(LifecycleState::Stopped);
        Ok(())
    }

    async fn destroy(&self, ctx: &LifecycleContext) -> Result<()> {
        let name = self.component_name(ctx);
        tracing::info!(component = %name, "context destroy");
        let child = LifecycleContext::new(&name);
        for wrapper in &self.wrappers {
            wrapper.destroy(&child).await?;
        }
        self.state.set(LifecycleState::Destroyed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapper::UrlPattern;

    #[tokio::test]
    async fn context_cascades_lifecycle_to_wrappers() {
        let w = Arc::new(Wrapper::new("s", "C", vec![UrlPattern::parse("/s")]));
        let ctx = Context::new(
            "/app",
            PathBuf::from("/tmp/app"),
            true,
            vec![Arc::clone(&w)],
        );
        let lc = LifecycleContext::new("Catalina/localhost");

        ctx.init(&lc).await.unwrap();
        assert_eq!(ctx.state(), LifecycleState::Initialized);
        assert_eq!(w.state(), LifecycleState::Initialized);

        ctx.start(&lc).await.unwrap();
        assert_eq!(ctx.state(), LifecycleState::Started);
        assert_eq!(w.state(), LifecycleState::Started);

        ctx.stop(&lc).await.unwrap();
        assert_eq!(ctx.state(), LifecycleState::Stopped);
        assert_eq!(w.state(), LifecycleState::Stopped);

        ctx.destroy(&lc).await.unwrap();
        assert_eq!(ctx.state(), LifecycleState::Destroyed);
        assert_eq!(w.state(), LifecycleState::Destroyed);
    }

    #[test]
    fn from_config_copies_fields() {
        let cfg = ContextConfig {
            path: "/shop".to_string(),
            doc_base: PathBuf::from("/var/www/shop"),
            reloadable: true,
        };
        let ctx = Context::from_config(&cfg);
        assert_eq!(ctx.path(), "/shop");
        assert_eq!(ctx.doc_base(), Path::new("/var/www/shop"));
        assert!(ctx.reloadable());
        assert!(ctx.wrappers().is_empty());
    }

    #[test]
    fn wrapper_lookup_by_name() {
        let w = Arc::new(Wrapper::new("find-me", "C", vec![]));
        let ctx = Context::new("", PathBuf::from("/tmp"), false, vec![w]);
        assert!(ctx.wrapper("find-me").is_some());
        assert!(ctx.wrapper("missing").is_none());
    }
}
