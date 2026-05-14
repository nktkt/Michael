//! [`Context`] — a deployed web application.
//!
//! In Apache Tomcat a `Context` is one web application: it owns a context path
//! (the URL prefix it is mounted at), a document base (where its files live),
//! and a set of [`Wrapper`]s, one per servlet. This port keeps that shape.
//!
//! # Descriptor deployment
//!
//! A context is created "empty" (no servlets) and is then *deployed*: its
//! `WEB-INF/web.xml` deployment descriptor is parsed and wired in.
//! [`Context::deploy`] is the full per-context step — it opens the on-disk
//! [`Webapp`](tomcatrs_webapp::Webapp), parses `web.xml` (if present), and calls
//! [`Context::deploy_descriptor`] to populate the context from it:
//!
//! * one [`Wrapper`] per `<servlet>`, carrying its `<init-param>`s and
//!   `<load-on-startup>` value, with `<servlet-mapping>` URL patterns applied;
//! * a [`FilterRegistry`] built from the `<filter>` / `<filter-mapping>`
//!   declarations;
//! * the `<welcome-file>` list and the `<listener-class>` names.
//!
//! Descriptor-derived state is populated *after* construction, so it lives in
//! [`std::sync::OnceLock`] cells: each is written exactly once (either by the
//! constructor, for pre-built wrappers, or by [`Context::deploy_descriptor`]),
//! which keeps [`Context`] `Send + Sync` while letting the deployer operate on
//! a shared `&Context`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use tomcatrs_config::ContextConfig;
use tomcatrs_core::{Error, Lifecycle, LifecycleContext, LifecycleState, Result};
use tomcatrs_webapp::Webapp;

use crate::filter::{FilterDef, FilterMapping, FilterRegistry};
use crate::mapper::UrlPattern;
use crate::state::StateCell;
use crate::wrapper::Wrapper;

/// A summary of what [`Context::deploy`] wired into a context.
///
/// Returned by [`Context::deploy`] so callers (and tests) can assert on the
/// outcome without re-reading the context's internal state. A context with no
/// `WEB-INF/web.xml` deploys successfully with an all-zero report — that is the
/// valid "annotation-only / no servlets" case, not an error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeploymentReport {
    /// Whether a `WEB-INF/web.xml` descriptor was found and parsed.
    pub had_web_xml: bool,
    /// Number of `<servlet>` wrappers created.
    pub servlet_count: usize,
    /// Number of `<servlet-mapping>` URL patterns applied across all wrappers.
    pub mapping_count: usize,
    /// Number of `<filter>` definitions registered.
    pub filter_count: usize,
    /// Number of `<welcome-file>` entries recorded.
    pub welcome_file_count: usize,
    /// Number of `<listener-class>` entries recorded.
    pub listener_count: usize,
}

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
    ///
    /// Written exactly once: by [`Context::new`] when pre-built wrappers are
    /// supplied, or by [`Context::deploy_descriptor`] when the descriptor is
    /// wired in. Unset until then — [`Context::wrappers`] reports an empty
    /// slice in that case.
    wrappers: OnceLock<Vec<Arc<Wrapper>>>,
    /// The filter registry built from the descriptor's `<filter>` /
    /// `<filter-mapping>` declarations. Set by [`Context::deploy_descriptor`].
    filter_registry: OnceLock<FilterRegistry>,
    /// The `<welcome-file>` list, in document order. Set by
    /// [`Context::deploy_descriptor`].
    welcome_files: OnceLock<Vec<String>>,
    /// The `<listener-class>` names. Set by [`Context::deploy_descriptor`].
    listeners: OnceLock<Vec<String>>,
    /// Current lifecycle state.
    state: StateCell,
}

impl Context {
    /// Create a context mounted at `path`, served from `doc_base`, owning
    /// `wrappers`.
    ///
    /// When `wrappers` is non-empty it is taken as the context's final servlet
    /// set (the path used by the mapper's unit tests and by callers that wire
    /// servlets manually). When it is empty the context is left "undeployed":
    /// a later [`Context::deploy`] / [`Context::deploy_descriptor`] call may
    /// populate it from a `web.xml` descriptor.
    pub fn new(
        path: impl Into<String>,
        doc_base: PathBuf,
        reloadable: bool,
        wrappers: Vec<Arc<Wrapper>>,
    ) -> Self {
        let wrapper_cell = OnceLock::new();
        // Only seed the cell when wrappers were supplied; an empty vec leaves
        // the context open to descriptor deployment.
        if !wrappers.is_empty() {
            // `set` on a fresh `OnceLock` cannot fail.
            let _ = wrapper_cell.set(wrappers);
        }
        Self {
            path: path.into(),
            doc_base,
            reloadable,
            wrappers: wrapper_cell,
            filter_registry: OnceLock::new(),
            welcome_files: OnceLock::new(),
            listeners: OnceLock::new(),
            state: StateCell::new(),
        }
    }

    /// Build a context from its parsed [`ContextConfig`].
    ///
    /// The context is created with no wrappers; servlets, filters and the
    /// welcome-file list are wired in later by [`Context::deploy`], which reads
    /// the application's `WEB-INF/web.xml`.
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
    ///
    /// Returns an empty slice until the context has been deployed (either with
    /// pre-built wrappers via [`Context::new`] or from a descriptor via
    /// [`Context::deploy_descriptor`]).
    pub fn wrappers(&self) -> &[Arc<Wrapper>] {
        self.wrappers.get().map(Vec::as_slice).unwrap_or(&[])
    }

    /// Look up a wrapper by its servlet name.
    pub fn wrapper(&self, servlet_name: &str) -> Option<&Arc<Wrapper>> {
        self.wrappers()
            .iter()
            .find(|w| w.servlet_name() == servlet_name)
    }

    /// Look up a wrapper by its servlet name.
    ///
    /// An intention-revealing alias for [`Context::wrapper`], matching the
    /// `find_*` naming used elsewhere in the deployment story.
    pub fn find_wrapper(&self, servlet_name: &str) -> Option<&Arc<Wrapper>> {
        self.wrapper(servlet_name)
    }

    /// The context's [`FilterRegistry`], if a descriptor has been deployed.
    ///
    /// Returns `None` for a context that has not been deployed, or one whose
    /// `web.xml` declared no filters at all is still `Some` but empty — the
    /// registry is created whenever [`Context::deploy_descriptor`] runs.
    pub fn filter_registry(&self) -> Option<&FilterRegistry> {
        self.filter_registry.get()
    }

    /// The context's `<welcome-file>` list, in document order.
    ///
    /// Returns an empty slice for a context whose descriptor has not been
    /// deployed or declared no welcome files.
    pub fn welcome_files(&self) -> &[String] {
        self.welcome_files.get().map(Vec::as_slice).unwrap_or(&[])
    }

    /// The context's `<listener-class>` names, in document order.
    ///
    /// Returns an empty slice for a context whose descriptor has not been
    /// deployed or declared no listeners.
    pub fn listeners(&self) -> &[String] {
        self.listeners.get().map(Vec::as_slice).unwrap_or(&[])
    }

    /// The context's current [`LifecycleState`].
    pub fn state(&self) -> LifecycleState {
        self.state.get()
    }

    /// Wire a parsed `web.xml` model into this context.
    ///
    /// For each `<servlet>` a [`Wrapper`] is created carrying the servlet name,
    /// class, `<init-param>`s and `<load-on-startup>` value; every
    /// `<servlet-mapping>` whose `<servlet-name>` matches a declared servlet
    /// has its `<url-pattern>` parsed (via [`UrlPattern::parse`]) and applied to
    /// that wrapper's mappings. A [`FilterRegistry`] is built from the
    /// `<filter>` / `<filter-mapping>` declarations, and the `<welcome-file>`
    /// list and `<listener-class>` names are recorded.
    ///
    /// This is the *additive* core of deployment: it does not touch the
    /// filesystem. [`Context::deploy`] is the higher-level entry point that
    /// opens the webapp and finds the descriptor first.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Deployment`] if the context has already had its
    /// descriptor wired in (each context is deployed exactly once) or if it was
    /// constructed with a pre-built, non-empty wrapper set.
    pub fn deploy_descriptor(&self, web_xml: &tomcatrs_config::web_xml::WebXml) -> Result<()> {
        // Build one mutable wrapper per <servlet>, keyed by servlet name so
        // <servlet-mapping> entries can be attached.
        let mut wrappers: Vec<Wrapper> = web_xml
            .servlets
            .iter()
            .map(|servlet| {
                Wrapper::new(servlet.name.clone(), servlet.class.clone(), Vec::new())
                    .with_init_params(servlet.init_params.clone())
                    .with_load_on_startup(servlet.load_on_startup)
            })
            .collect();

        // Apply <servlet-mapping>s: parse each <url-pattern> and push it onto
        // the wrapper whose servlet name it targets.
        let mut applied_mappings = 0usize;
        for mapping in &web_xml.servlet_mappings {
            match wrappers
                .iter_mut()
                .find(|w| w.servlet_name() == mapping.servlet_name)
            {
                Some(wrapper) => {
                    wrapper.add_mapping(UrlPattern::parse(&mapping.url_pattern));
                    applied_mappings += 1;
                }
                None => tracing::warn!(
                    context = %self.path,
                    servlet_name = %mapping.servlet_name,
                    url_pattern = %mapping.url_pattern,
                    "<servlet-mapping> references an undeclared <servlet>; skipping"
                ),
            }
        }

        let wrappers: Vec<Arc<Wrapper>> = wrappers.into_iter().map(Arc::new).collect();

        // Build the filter registry from <filter> / <filter-mapping>.
        let filter_defs: Vec<FilterDef> = web_xml
            .filters
            .iter()
            .map(|f| {
                let mut def = FilterDef::new(f.name.clone(), f.class.clone());
                def.init_params = f.init_params.clone();
                def
            })
            .collect();
        let filter_mappings: Vec<FilterMapping> = web_xml
            .filter_mappings
            .iter()
            .map(|m| {
                FilterMapping::for_url(m.filter_name.clone(), UrlPattern::parse(&m.url_pattern))
            })
            .collect();
        let filter_registry = FilterRegistry::new(filter_defs, filter_mappings);

        // Commit every descriptor-derived field. Each `OnceLock::set` fails if
        // the cell was already written — which means this context was already
        // deployed (or built with pre-wired wrappers); that is a caller error.
        self.wrappers.set(wrappers).map_err(|_| {
            Error::Deployment(format!(
                "context [{}] already has servlets wired in; deploy_descriptor is single-shot",
                self.path
            ))
        })?;
        // The remaining cells can only be set by this method, so once the
        // wrapper cell was claimed above these are guaranteed unset.
        let _ = self.filter_registry.set(filter_registry);
        let _ = self.welcome_files.set(web_xml.welcome_files.clone());
        let _ = self.listeners.set(web_xml.listeners.clone());

        tracing::info!(
            context = %self.path,
            servlets = self.wrappers().len(),
            servlet_mappings = applied_mappings,
            filters = self.filter_registry().map(FilterRegistry::def_count).unwrap_or(0),
            welcome_files = self.welcome_files().len(),
            listeners = self.listeners().len(),
            "deployed web.xml descriptor into context"
        );

        Ok(())
    }

    /// Open this context's on-disk web application and wire its descriptor in.
    ///
    /// This is the full per-context deployment step:
    ///
    /// 1. open the [`Webapp`] rooted at [`Context::doc_base`] (validating the
    ///    exploded layout and parsing `WEB-INF/web.xml` if present);
    /// 2. when a `web.xml` is present, parse it into a
    ///    [`WebXml`](tomcatrs_config::web_xml::WebXml) and hand it to
    ///    [`Context::deploy_descriptor`];
    /// 3. log and return a [`DeploymentReport`] summarising the result.
    ///
    /// A web application with **no** `WEB-INF/web.xml` is deployed successfully
    /// with an empty report — that is the valid annotation-only / no-servlets
    /// case, not an error.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Deployment`] if the document base is not a valid
    /// exploded web application (see [`Webapp::open`]), if `web.xml` exists but
    /// is malformed, or if the context has already been deployed.
    pub fn deploy(&self) -> Result<DeploymentReport> {
        let webapp = Webapp::open(self.path.clone(), &self.doc_base)?;

        let Some(web_xml_path) = webapp.web_xml_path() else {
            // No descriptor at all — a valid, empty deployment.
            tracing::info!(
                context = %self.path,
                doc_base = %self.doc_base.display(),
                "no WEB-INF/web.xml; deployed with an empty descriptor"
            );
            return Ok(DeploymentReport::default());
        };

        let web_xml = tomcatrs_config::web_xml::WebXml::from_xml_file(web_xml_path)?;
        self.deploy_descriptor(&web_xml)?;

        let report = DeploymentReport {
            had_web_xml: true,
            servlet_count: self.wrappers().len(),
            mapping_count: self.wrappers().iter().map(|w| w.mappings().len()).sum(),
            filter_count: self
                .filter_registry()
                .map(FilterRegistry::def_count)
                .unwrap_or(0),
            welcome_file_count: self.welcome_files().len(),
            listener_count: self.listeners().len(),
        };

        tracing::info!(
            context = %self.path,
            doc_base = %self.doc_base.display(),
            servlets = report.servlet_count,
            mappings = report.mapping_count,
            filters = report.filter_count,
            "context deployment complete"
        );

        Ok(report)
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
            reloadable = self.reloadable, wrappers = self.wrappers().len(), "context init");
        let child = LifecycleContext::new(&name);
        for wrapper in self.wrappers() {
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
        for wrapper in self.wrappers() {
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
        for wrapper in self.wrappers() {
            wrapper.stop(&child).await?;
        }
        self.state.set(LifecycleState::Stopped);
        Ok(())
    }

    async fn destroy(&self, ctx: &LifecycleContext) -> Result<()> {
        let name = self.component_name(ctx);
        tracing::info!(component = %name, "context destroy");
        let child = LifecycleContext::new(&name);
        for wrapper in self.wrappers() {
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
    use std::collections::HashMap;
    use std::fs;

    /// A unique temp directory for one test, cleaned up by the caller.
    fn unique_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tomcatrs-catalina-context-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    /// Build a `WebXml` with two servlets + mappings and one filter + mapping.
    fn sample_web_xml() -> tomcatrs_config::web_xml::WebXml {
        use tomcatrs_config::web_xml::{
            FilterDef as XmlFilterDef, FilterMapping as XmlFilterMapping, ServletDef,
            ServletMapping, WebXml,
        };

        let mut web = WebXml::default();

        let mut hello_params = HashMap::new();
        hello_params.insert("greeting".to_string(), "hi".to_string());
        web.servlets.push(ServletDef {
            name: "hello".to_string(),
            class: "com.example.HelloServlet".to_string(),
            load_on_startup: Some(1),
            init_params: hello_params,
        });
        web.servlets.push(ServletDef {
            name: "api".to_string(),
            class: "com.example.ApiServlet".to_string(),
            load_on_startup: None,
            init_params: HashMap::new(),
        });

        web.servlet_mappings.push(ServletMapping {
            servlet_name: "hello".to_string(),
            url_pattern: "/hello".to_string(),
        });
        web.servlet_mappings.push(ServletMapping {
            servlet_name: "api".to_string(),
            url_pattern: "/api/*".to_string(),
        });

        web.filters.push(XmlFilterDef {
            name: "encoding".to_string(),
            class: "com.example.EncodingFilter".to_string(),
            init_params: HashMap::new(),
        });
        web.filter_mappings.push(XmlFilterMapping {
            filter_name: "encoding".to_string(),
            url_pattern: "/*".to_string(),
        });

        web.welcome_files = vec!["index.html".to_string(), "index.jsp".to_string()];
        web.listeners = vec!["com.example.AppContextListener".to_string()];
        web
    }

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
        // `find_wrapper` is the intention-revealing alias.
        assert!(ctx.find_wrapper("find-me").is_some());
        assert!(ctx.find_wrapper("missing").is_none());
    }

    #[test]
    fn deploy_descriptor_wires_servlets_mappings_and_filters() {
        let web = sample_web_xml();
        let ctx = Context::new("/app", PathBuf::from("/tmp/app"), false, Vec::new());

        ctx.deploy_descriptor(&web).expect("descriptor deploys");

        // Two servlets, each as a wrapper carrying its descriptor config.
        assert_eq!(ctx.wrappers().len(), 2);
        let hello = ctx.find_wrapper("hello").expect("hello wrapper");
        assert_eq!(hello.servlet_class(), "com.example.HelloServlet");
        assert_eq!(hello.load_on_startup(), Some(1));
        assert_eq!(
            hello.init_params().get("greeting").map(String::as_str),
            Some("hi")
        );
        // Servlet mappings were parsed and attached to the right wrappers.
        assert_eq!(hello.mappings(), &[UrlPattern::Exact("/hello".to_string())]);
        let api = ctx.find_wrapper("api").expect("api wrapper");
        assert_eq!(api.mappings(), &[UrlPattern::Prefix("/api".to_string())]);
        assert_eq!(api.load_on_startup(), None);

        // The filter registry was built from <filter> / <filter-mapping>.
        let registry = ctx.filter_registry().expect("filter registry built");
        assert_eq!(registry.def_count(), 1);
        assert!(registry.def("encoding").is_some());
        assert_eq!(registry.mappings().len(), 1);

        // Welcome files and listeners were recorded in document order.
        assert_eq!(ctx.welcome_files(), ["index.html", "index.jsp"]);
        assert_eq!(ctx.listeners(), ["com.example.AppContextListener"]);
    }

    #[test]
    fn deploy_descriptor_is_single_shot() {
        let web = sample_web_xml();
        let ctx = Context::new("/app", PathBuf::from("/tmp/app"), false, Vec::new());
        ctx.deploy_descriptor(&web).expect("first deploy succeeds");
        // A second deploy must be rejected rather than silently overwriting.
        let err = ctx.deploy_descriptor(&web).unwrap_err();
        assert!(matches!(err, Error::Deployment(_)));
    }

    #[test]
    fn deploy_reads_real_web_xml_from_exploded_webapp() {
        let doc_base = unique_dir("deploy-real");
        let web_inf = doc_base.join("WEB-INF");
        fs::create_dir_all(&web_inf).unwrap();
        fs::write(
            web_inf.join("web.xml"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<web-app>
  <filter>
    <filter-name>encoding</filter-name>
    <filter-class>com.example.EncodingFilter</filter-class>
  </filter>
  <filter-mapping>
    <filter-name>encoding</filter-name>
    <url-pattern>/*</url-pattern>
  </filter-mapping>
  <servlet>
    <servlet-name>hello</servlet-name>
    <servlet-class>com.example.HelloServlet</servlet-class>
    <load-on-startup>1</load-on-startup>
  </servlet>
  <servlet>
    <servlet-name>api</servlet-name>
    <servlet-class>com.example.ApiServlet</servlet-class>
  </servlet>
  <servlet-mapping>
    <servlet-name>hello</servlet-name>
    <url-pattern>/hello</url-pattern>
  </servlet-mapping>
  <servlet-mapping>
    <servlet-name>api</servlet-name>
    <url-pattern>/api/*</url-pattern>
  </servlet-mapping>
  <welcome-file-list>
    <welcome-file>index.html</welcome-file>
  </welcome-file-list>
</web-app>
"#,
        )
        .unwrap();

        let ctx = Context::new("/app", doc_base.clone(), false, Vec::new());
        let report = ctx.deploy().expect("deploy succeeds");

        assert_eq!(
            report,
            DeploymentReport {
                had_web_xml: true,
                servlet_count: 2,
                mapping_count: 2,
                filter_count: 1,
                welcome_file_count: 1,
                listener_count: 0,
            }
        );
        assert!(ctx.find_wrapper("hello").is_some());
        assert!(ctx.find_wrapper("api").is_some());
        assert_eq!(ctx.welcome_files(), ["index.html"]);

        fs::remove_dir_all(&doc_base).ok();
    }

    #[test]
    fn deploy_succeeds_with_empty_report_when_no_web_xml() {
        let doc_base = unique_dir("deploy-no-web-xml");
        // A valid exploded webapp layout, but with no web.xml at all.
        fs::create_dir_all(doc_base.join("WEB-INF")).unwrap();

        let ctx = Context::new("/bare", doc_base.clone(), false, Vec::new());
        let report = ctx.deploy().expect("deploy with no web.xml succeeds");

        assert_eq!(report, DeploymentReport::default());
        assert!(!report.had_web_xml);
        assert!(ctx.wrappers().is_empty());
        assert!(ctx.filter_registry().is_none());
        assert!(ctx.welcome_files().is_empty());

        fs::remove_dir_all(&doc_base).ok();
    }
}
