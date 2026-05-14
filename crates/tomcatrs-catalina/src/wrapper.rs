//! [`Wrapper`] — the leaf container that represents a single servlet.
//!
//! In Apache Tomcat a `Wrapper` is the container around exactly one servlet
//! instance: it carries the servlet's name, its implementing class, and the
//! set of URL patterns that route requests to it. This port keeps the same
//! shape; actual servlet *invocation* belongs to `tomcatrs-servlet-bridge` and
//! later crates.

use async_trait::async_trait;
use tomcatrs_core::{Lifecycle, LifecycleContext, LifecycleState, Result};

use crate::mapper::UrlPattern;
use crate::state::StateCell;

/// A single servlet registration within a [`Context`](crate::Context).
#[derive(Debug)]
pub struct Wrapper {
    /// The servlet's registered name (the `<servlet-name>` value).
    servlet_name: String,
    /// Fully-qualified implementing class (the `<servlet-class>` value).
    servlet_class: String,
    /// URL patterns that route to this servlet, parsed from `<servlet-mapping>`.
    mappings: Vec<UrlPattern>,
    /// Current lifecycle state.
    state: StateCell,
}

impl Wrapper {
    /// Create a wrapper for `servlet_name` implemented by `servlet_class`,
    /// reachable via `mappings`.
    pub fn new(
        servlet_name: impl Into<String>,
        servlet_class: impl Into<String>,
        mappings: Vec<UrlPattern>,
    ) -> Self {
        Self {
            servlet_name: servlet_name.into(),
            servlet_class: servlet_class.into(),
            mappings,
            state: StateCell::new(),
        }
    }

    /// The servlet's registered name.
    pub fn servlet_name(&self) -> &str {
        &self.servlet_name
    }

    /// The servlet's implementing class name.
    pub fn servlet_class(&self) -> &str {
        &self.servlet_class
    }

    /// The URL patterns routing to this servlet.
    pub fn mappings(&self) -> &[UrlPattern] {
        &self.mappings
    }

    /// The wrapper's current [`LifecycleState`].
    pub fn state(&self) -> LifecycleState {
        self.state.get()
    }

    /// Add a URL pattern to this wrapper after construction.
    pub fn add_mapping(&mut self, pattern: UrlPattern) {
        self.mappings.push(pattern);
    }
}

#[async_trait]
impl Lifecycle for Wrapper {
    async fn init(&self, ctx: &LifecycleContext) -> Result<()> {
        tracing::debug!(component = %ctx.name, servlet = %self.servlet_name,
            class = %self.servlet_class, "wrapper init");
        self.state.set(LifecycleState::Initialized);
        Ok(())
    }

    async fn start(&self, ctx: &LifecycleContext) -> Result<()> {
        tracing::debug!(component = %ctx.name, servlet = %self.servlet_name,
            mappings = self.mappings.len(), "wrapper start");
        self.state.set(LifecycleState::Started);
        Ok(())
    }

    async fn stop(&self, ctx: &LifecycleContext) -> Result<()> {
        tracing::debug!(component = %ctx.name, servlet = %self.servlet_name, "wrapper stop");
        self.state.set(LifecycleState::Stopped);
        Ok(())
    }

    async fn destroy(&self, ctx: &LifecycleContext) -> Result<()> {
        tracing::debug!(component = %ctx.name, servlet = %self.servlet_name, "wrapper destroy");
        self.state.set(LifecycleState::Destroyed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wrapper_lifecycle_transitions() {
        let w = Wrapper::new(
            "hello",
            "com.example.HelloServlet",
            vec![UrlPattern::parse("/hello")],
        );
        let ctx = LifecycleContext::new("Catalina/localhost/app/hello");
        assert_eq!(w.state(), LifecycleState::New);
        w.init(&ctx).await.unwrap();
        assert_eq!(w.state(), LifecycleState::Initialized);
        w.start(&ctx).await.unwrap();
        assert_eq!(w.state(), LifecycleState::Started);
        w.stop(&ctx).await.unwrap();
        assert_eq!(w.state(), LifecycleState::Stopped);
        w.destroy(&ctx).await.unwrap();
        assert_eq!(w.state(), LifecycleState::Destroyed);
    }

    #[test]
    fn accessors_reflect_construction() {
        let w = Wrapper::new("s", "C", vec![UrlPattern::parse("*.jsp")]);
        assert_eq!(w.servlet_name(), "s");
        assert_eq!(w.servlet_class(), "C");
        assert_eq!(w.mappings().len(), 1);
    }
}
