//! [`Wrapper`] — the leaf container that represents a single servlet.
//!
//! In Apache Tomcat a `Wrapper` is the container around exactly one servlet
//! instance: it carries the servlet's name, its implementing class, and the
//! set of URL patterns that route requests to it. This port keeps the same
//! shape; actual servlet *invocation* belongs to `tomcatrs-servlet-bridge` and
//! later crates.

use std::collections::HashMap;

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
    /// `<init-param>` name/value pairs declared for this servlet in `web.xml`.
    ///
    /// The Rust runtime never interprets these — they are handed verbatim to
    /// the JVM servlet bridge when the servlet is initialised.
    init_params: HashMap<String, String>,
    /// The `<load-on-startup>` ordering value, if the descriptor declared one.
    ///
    /// `Some(n)` requests eager initialisation, lower `n` first; `None` means
    /// the servlet is initialised lazily on first request.
    load_on_startup: Option<i32>,
    /// Current lifecycle state.
    state: StateCell,
}

impl Wrapper {
    /// Create a wrapper for `servlet_name` implemented by `servlet_class`,
    /// reachable via `mappings`.
    ///
    /// The wrapper starts with no `<init-param>`s and no `<load-on-startup>`
    /// value; use [`Wrapper::with_init_params`] and
    /// [`Wrapper::with_load_on_startup`] (or [`Wrapper::set_init_param`]) to
    /// attach descriptor-derived configuration after construction.
    pub fn new(
        servlet_name: impl Into<String>,
        servlet_class: impl Into<String>,
        mappings: Vec<UrlPattern>,
    ) -> Self {
        Self {
            servlet_name: servlet_name.into(),
            servlet_class: servlet_class.into(),
            mappings,
            init_params: HashMap::new(),
            load_on_startup: None,
            state: StateCell::new(),
        }
    }

    /// Builder-style setter that replaces the servlet's `<init-param>` map.
    pub fn with_init_params(mut self, init_params: HashMap<String, String>) -> Self {
        self.init_params = init_params;
        self
    }

    /// Builder-style setter for the `<load-on-startup>` ordering value.
    pub fn with_load_on_startup(mut self, load_on_startup: Option<i32>) -> Self {
        self.load_on_startup = load_on_startup;
        self
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

    /// The servlet's `<init-param>` name/value pairs.
    pub fn init_params(&self) -> &HashMap<String, String> {
        &self.init_params
    }

    /// The servlet's `<load-on-startup>` ordering value, if declared.
    pub fn load_on_startup(&self) -> Option<i32> {
        self.load_on_startup
    }

    /// The wrapper's current [`LifecycleState`].
    pub fn state(&self) -> LifecycleState {
        self.state.get()
    }

    /// Add a URL pattern to this wrapper after construction.
    pub fn add_mapping(&mut self, pattern: UrlPattern) {
        self.mappings.push(pattern);
    }

    /// Insert (or overwrite) a single `<init-param>` entry after construction.
    pub fn set_init_param(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.init_params.insert(name.into(), value.into());
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
        // A freshly built wrapper carries no descriptor configuration.
        assert!(w.init_params().is_empty());
        assert_eq!(w.load_on_startup(), None);
    }

    #[test]
    fn init_params_and_load_on_startup_are_carried() {
        let mut params = HashMap::new();
        params.insert("greeting".to_string(), "hi".to_string());

        let w = Wrapper::new("hello", "com.example.Hello", vec![])
            .with_init_params(params)
            .with_load_on_startup(Some(1));
        assert_eq!(
            w.init_params().get("greeting").map(String::as_str),
            Some("hi")
        );
        assert_eq!(w.load_on_startup(), Some(1));

        // The post-construction setter inserts additional entries.
        let mut w = w;
        w.set_init_param("debug", "true");
        assert_eq!(
            w.init_params().get("debug").map(String::as_str),
            Some("true")
        );
    }
}
