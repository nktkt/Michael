//! Servlet **filter-chain** infrastructure — the Rust port of the filter half of
//! Apache Tomcat's `org.apache.catalina.core.ApplicationFilterFactory` /
//! `ApplicationFilterChain`.
//!
//! A servlet filter intercepts a request before it reaches a servlet (and the
//! response on the way back out). The Servlet specification lets a deployment
//! descriptor declare any number of `<filter>`s and bind them to requests with
//! `<filter-mapping>`s, either by **URL pattern** or by **servlet name**, and
//! optionally restrict each mapping to a set of **dispatcher types**
//! (`REQUEST`, `FORWARD`, `INCLUDE`, `ASYNC`, `ERROR`).
//!
//! # What this module does — and does not — do
//!
//! The actual *execution* of a filter is Java code: it implements
//! `jakarta.servlet.Filter` and runs inside the JVM. The Rust runtime therefore
//! does **not** invoke filters itself. What it owns is the *resolution* problem:
//! given a request's mapped path (and/or servlet name) and dispatcher type,
//! which filters apply, and **in what order**?
//!
//! The Servlet specification fixes that order:
//!
//! 1. All `<filter-mapping>`s that match by `<url-pattern>`, in the order they
//!    appear in `web.xml`.
//! 2. Then all `<filter-mapping>`s that match by `<servlet-name>`, again in
//!    declaration order.
//!
//! A single filter may be named by several mappings; the spec says it still
//! runs once, at the position of its *first* matching mapping. [`FilterChain`]
//! reproduces exactly this behaviour.
//!
//! Once the ordered list is known, [`FilterInvoker`] models the "run this
//! filter, then call the next link" step. The bundled [`NoopFilterInvoker`]
//! simply chains straight through without doing any work — real invocation is
//! delegated to the JVM servlet bridge (`tomcatrs-servlet-bridge`), which
//! crosses the JNI boundary to call `Filter.doFilter`.
//!
//! # Example
//!
//! ```
//! use tomcatrs_catalina::filter::{
//!     DispatcherType, FilterChainBuilder, FilterDef, FilterMapping, FilterRegistry,
//! };
//! use tomcatrs_catalina::mapper::UrlPattern;
//!
//! let defs = vec![
//!     FilterDef::new("auth", "com.example.AuthFilter"),
//!     FilterDef::new("gzip", "com.example.GzipFilter"),
//! ];
//! let mappings = vec![
//!     FilterMapping::for_url("auth", UrlPattern::parse("/*")),
//!     FilterMapping::for_url("gzip", UrlPattern::parse("/*")),
//! ];
//! let registry = FilterRegistry::new(defs, mappings);
//! let builder = FilterChainBuilder::new(&registry);
//!
//! let chain = builder.build_for_path("/index.html", None, DispatcherType::Request);
//! assert_eq!(chain.filter_names(), ["auth", "gzip"]);
//! ```

use std::collections::HashMap;
use std::collections::HashSet;

use async_trait::async_trait;
use tomcatrs_core::Result;

use crate::mapper::UrlPattern;

/// The dispatch condition under which a request reaches a filter.
///
/// Mirrors `jakarta.servlet.DispatcherType`. A `<filter-mapping>` with no
/// explicit `<dispatcher>` elements defaults to [`DispatcherType::Request`]
/// only — see [`FilterMapping::default_dispatchers`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DispatcherType {
    /// A normal client request straight off the connector.
    Request,
    /// A `RequestDispatcher.forward` to another resource.
    Forward,
    /// A `RequestDispatcher.include` of another resource.
    Include,
    /// An async dispatch (`AsyncContext.dispatch`).
    Async,
    /// An error-page dispatch.
    Error,
}

impl DispatcherType {
    /// Parse a `<dispatcher>` element's text content (case-insensitively).
    ///
    /// Returns `None` for any token the Servlet specification does not define.
    pub fn parse(raw: &str) -> Option<DispatcherType> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "REQUEST" => Some(DispatcherType::Request),
            "FORWARD" => Some(DispatcherType::Forward),
            "INCLUDE" => Some(DispatcherType::Include),
            "ASYNC" => Some(DispatcherType::Async),
            "ERROR" => Some(DispatcherType::Error),
            _ => None,
        }
    }
}

/// A `<filter>` declaration: the filter's identity and configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterDef {
    /// Logical filter name (`<filter-name>`); the key mappings refer to.
    pub name: String,
    /// Fully-qualified Java filter class (`<filter-class>`). The Rust side never
    /// loads it — it is handed to the JVM bridge verbatim.
    pub class: String,
    /// `<init-param>` name/value pairs passed to `Filter.init`.
    pub init_params: HashMap<String, String>,
    /// Whether the filter declared `<async-supported>true</async-supported>`.
    pub async_supported: bool,
}

impl FilterDef {
    /// Create a filter definition with no init params and async unsupported.
    pub fn new(name: impl Into<String>, class: impl Into<String>) -> Self {
        FilterDef {
            name: name.into(),
            class: class.into(),
            init_params: HashMap::new(),
            async_supported: false,
        }
    }

    /// Builder-style setter for [`FilterDef::async_supported`].
    pub fn with_async_supported(mut self, async_supported: bool) -> Self {
        self.async_supported = async_supported;
        self
    }

    /// Builder-style setter that inserts one `<init-param>` entry.
    pub fn with_init_param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.init_params.insert(name.into(), value.into());
        self
    }
}

/// A `<filter-mapping>` binding a filter to the requests it applies to.
///
/// A mapping may match by URL pattern, by servlet name, or both (the spec
/// allows repeating either element). At least one `<url-pattern>` *or* one
/// `<servlet-name>` is expected; a mapping with neither matches nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterMapping {
    /// The `<filter-name>` this mapping targets — refers to a [`FilterDef`].
    pub filter_name: String,
    /// `<url-pattern>` entries, parsed into [`UrlPattern`]s.
    pub url_patterns: Vec<UrlPattern>,
    /// `<servlet-name>` entries this mapping targets.
    pub servlet_names: Vec<String>,
    /// `<dispatcher>` entries; an empty descriptor list defaults to
    /// `[Request]`, applied by the constructors below.
    pub dispatcher_types: Vec<DispatcherType>,
}

impl FilterMapping {
    /// The dispatcher set the Servlet spec uses when `<filter-mapping>` declares
    /// no `<dispatcher>` element: `REQUEST` only.
    pub fn default_dispatchers() -> Vec<DispatcherType> {
        vec![DispatcherType::Request]
    }

    /// A mapping that binds `filter_name` to a single URL pattern, for the
    /// default (`REQUEST`) dispatcher type.
    pub fn for_url(filter_name: impl Into<String>, pattern: UrlPattern) -> Self {
        FilterMapping {
            filter_name: filter_name.into(),
            url_patterns: vec![pattern],
            servlet_names: Vec::new(),
            dispatcher_types: Self::default_dispatchers(),
        }
    }

    /// A mapping that binds `filter_name` to a single servlet name, for the
    /// default (`REQUEST`) dispatcher type.
    pub fn for_servlet(filter_name: impl Into<String>, servlet_name: impl Into<String>) -> Self {
        FilterMapping {
            filter_name: filter_name.into(),
            url_patterns: Vec::new(),
            servlet_names: vec![servlet_name.into()],
            dispatcher_types: Self::default_dispatchers(),
        }
    }

    /// Builder-style replacement of the dispatcher-type set.
    ///
    /// An empty `types` is normalised back to the spec default (`[Request]`),
    /// so a mapping always matches at least one dispatcher type.
    pub fn with_dispatchers(mut self, types: Vec<DispatcherType>) -> Self {
        self.dispatcher_types = if types.is_empty() {
            Self::default_dispatchers()
        } else {
            types
        };
        self
    }

    /// Does this mapping apply under dispatcher type `dt`?
    fn covers_dispatcher(&self, dt: DispatcherType) -> bool {
        self.dispatcher_types.contains(&dt)
    }

    /// Does any of this mapping's URL patterns match the context-relative
    /// `path`?
    fn matches_path(&self, path: &str) -> bool {
        self.url_patterns.iter().any(|p| p.matches(path))
    }

    /// Does any of this mapping's servlet names equal `servlet_name`?
    fn matches_servlet(&self, servlet_name: &str) -> bool {
        self.servlet_names.iter().any(|s| s == servlet_name)
    }
}

/// Per-context store of every declared [`FilterDef`] and [`FilterMapping`].
///
/// One registry is built per deployed [`Context`](crate::Context) — it holds
/// the descriptor data in declaration order, which is the order the Servlet
/// specification's filter-chain rules depend on. [`FilterChainBuilder`] borrows
/// a registry to resolve concrete chains per request.
#[derive(Debug, Clone, Default)]
pub struct FilterRegistry {
    /// Filter definitions, keyed by name for O(1) lookup during chain builds.
    defs: HashMap<String, FilterDef>,
    /// Filter mappings, kept in `web.xml` declaration order — this ordering is
    /// load-bearing and must never be sorted.
    mappings: Vec<FilterMapping>,
}

impl FilterRegistry {
    /// Build a registry from raw filter definitions and mappings.
    ///
    /// `mappings` must be supplied in `web.xml` declaration order; the registry
    /// preserves it. A definition whose name is repeated keeps the *last*
    /// occurrence, matching how a `HashMap` insert behaves — `web.xml` is not
    /// supposed to declare a filter name twice.
    pub fn new(defs: Vec<FilterDef>, mappings: Vec<FilterMapping>) -> Self {
        let defs = defs
            .into_iter()
            .map(|d| (d.name.clone(), d))
            .collect::<HashMap<_, _>>();
        FilterRegistry { defs, mappings }
    }

    /// Build a registry from a parsed `web.xml` model
    /// ([`tomcatrs_config::web_xml::WebXml`]).
    ///
    /// The config crate's `FilterMapping` models only a single `<url-pattern>`
    /// per element and carries neither `<servlet-name>` nor `<dispatcher>`
    /// data, so every resulting [`FilterMapping`] here is a URL-pattern mapping
    /// for the default ([`DispatcherType::Request`]) dispatcher type. Mappings
    /// keep their `web.xml` order, which is exactly what [`FilterChain`]
    /// ordering requires.
    pub fn from_web_descriptor(web_xml: &tomcatrs_config::web_xml::WebXml) -> Self {
        let defs = web_xml
            .filters
            .iter()
            .map(|f| FilterDef {
                name: f.name.clone(),
                class: f.class.clone(),
                init_params: f.init_params.clone(),
                async_supported: false,
            })
            .collect::<Vec<_>>();

        let mappings = web_xml
            .filter_mappings
            .iter()
            .map(|m| {
                FilterMapping::for_url(m.filter_name.clone(), UrlPattern::parse(&m.url_pattern))
            })
            .collect::<Vec<_>>();

        FilterRegistry::new(defs, mappings)
    }

    /// Look up a filter definition by name.
    pub fn def(&self, name: &str) -> Option<&FilterDef> {
        self.defs.get(name)
    }

    /// The number of declared filter definitions.
    pub fn def_count(&self) -> usize {
        self.defs.len()
    }

    /// The filter mappings, in `web.xml` declaration order.
    pub fn mappings(&self) -> &[FilterMapping] {
        &self.mappings
    }
}

/// One resolved link in a [`FilterChain`]: a filter definition plus the
/// dispatcher type the request arrived under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterChainEntry {
    /// The filter to run at this position.
    pub def: FilterDef,
    /// The dispatcher type the owning request was resolved for.
    pub dispatcher_type: DispatcherType,
}

/// The ordered list of filters that apply to one request.
///
/// Produced by [`FilterChainBuilder`]; the order obeys the Servlet
/// specification (URL-pattern mappings first in declaration order, then
/// servlet-name mappings). Drive the chain with an [`FilterInvoker`] to walk it
/// link by link.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterChain {
    entries: Vec<FilterChainEntry>,
}

impl FilterChain {
    /// The resolved chain links, in execution order.
    pub fn entries(&self) -> &[FilterChainEntry] {
        &self.entries
    }

    /// The filter names, in execution order — handy for assertions and logs.
    pub fn filter_names(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.def.name.clone()).collect()
    }

    /// The number of filters in the chain.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the chain has no filters (the servlet runs directly).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Walk the whole chain through `invoker`, link by link, from the first
    /// filter to the end.
    ///
    /// Each call to [`FilterInvoker::invoke`] is expected to do that filter's
    /// work and then continue with the rest of the chain; [`run`](Self::run)
    /// itself only seeds the walk at index `0`. With [`NoopFilterInvoker`] this
    /// simply confirms every link is reachable.
    ///
    /// # Errors
    ///
    /// Propagates whatever error the `invoker` returns for a link.
    pub async fn run<I: FilterInvoker>(&self, invoker: &I) -> Result<()> {
        let cursor = FilterChainCursor {
            chain: self,
            index: 0,
        };
        cursor.proceed(invoker).await
    }
}

/// A position within a [`FilterChain`] walk.
///
/// Handed to [`FilterInvoker::invoke`] so the invoker can, after doing its
/// filter's work, call [`FilterChainCursor::proceed`] to advance to the next
/// link — the Rust analogue of Java's `FilterChain.doFilter(req, resp)`.
#[derive(Debug, Clone, Copy)]
pub struct FilterChainCursor<'a> {
    chain: &'a FilterChain,
    index: usize,
}

impl<'a> FilterChainCursor<'a> {
    /// The chain entry at the current position, or `None` once the cursor has
    /// walked off the end (i.e. the servlet itself would run next).
    pub fn current(&self) -> Option<&'a FilterChainEntry> {
        self.chain.entries.get(self.index)
    }

    /// Whether the cursor has reached the end of the chain.
    pub fn at_end(&self) -> bool {
        self.index >= self.chain.entries.len()
    }

    /// Advance the walk: run the filter at the current position through
    /// `invoker`, or stop if the end has been reached.
    ///
    /// # Errors
    ///
    /// Propagates whatever error the `invoker` returns.
    pub async fn proceed<I: FilterInvoker>(self, invoker: &I) -> Result<()> {
        match self.chain.entries.get(self.index) {
            Some(entry) => {
                let next = FilterChainCursor {
                    chain: self.chain,
                    index: self.index + 1,
                };
                invoker.invoke(entry, next).await
            }
            None => invoker.reached_end().await,
        }
    }
}

/// Strategy for *executing* a [`FilterChain`], one link at a time.
///
/// This trait deliberately models only the control flow — "run this filter,
/// then call the next link". The real work of a filter is Java code running in
/// the JVM, reached through `tomcatrs-servlet-bridge`; an implementation of
/// this trait there would, for each [`FilterChainEntry`], call the JVM's
/// `Filter.doFilter` and pass it a callback that resolves to
/// [`FilterChainCursor::proceed`]. The Rust core only decides *which* filters
/// run and *in what order* — [`FilterChain`] — and leaves invocation abstract.
#[async_trait]
pub trait FilterInvoker: Sync {
    /// Invoke `entry`'s filter, then continue the walk via `next`.
    ///
    /// An implementation must call [`FilterChainCursor::proceed`] on `next` to
    /// reach the rest of the chain (or deliberately short-circuit by not doing
    /// so — e.g. an auth filter rejecting a request).
    ///
    /// # Errors
    ///
    /// Returns an error if the filter (or any later link) fails.
    async fn invoke(&self, entry: &FilterChainEntry, next: FilterChainCursor<'_>) -> Result<()>;

    /// Called once the cursor walks off the end of the chain — the point at
    /// which the target servlet itself would run.
    ///
    /// # Errors
    ///
    /// Returns an error if end-of-chain handling fails. The default is `Ok`.
    async fn reached_end(&self) -> Result<()> {
        Ok(())
    }
}

/// A [`FilterInvoker`] that does nothing but chain straight through to the end.
///
/// It performs no filter work — real `Filter.doFilter` execution is delegated
/// to the JVM servlet bridge. This implementation exists so the Rust side can
/// be unit-tested in isolation: running a [`FilterChain`] through it proves the
/// chain is well-formed and every link is reachable.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopFilterInvoker;

#[async_trait]
impl FilterInvoker for NoopFilterInvoker {
    async fn invoke(&self, entry: &FilterChainEntry, next: FilterChainCursor<'_>) -> Result<()> {
        tracing::trace!(
            filter = %entry.def.name,
            dispatcher = ?entry.dispatcher_type,
            "noop filter invoker: passing through to next link"
        );
        // The whole point: do no work, just advance the walk.
        next.proceed(self).await
    }
}

/// Builds concrete [`FilterChain`]s for individual requests by consulting a
/// borrowed [`FilterRegistry`].
///
/// The builder is a thin, cheap view over the registry — make one per request
/// or cache it; it holds no mutable state.
#[derive(Debug, Clone, Copy)]
pub struct FilterChainBuilder<'a> {
    registry: &'a FilterRegistry,
}

impl<'a> FilterChainBuilder<'a> {
    /// Create a builder over `registry`.
    pub fn new(registry: &'a FilterRegistry) -> Self {
        FilterChainBuilder { registry }
    }

    /// Resolve the ordered filter chain for a request.
    ///
    /// `path` is the context-relative request path (the same string
    /// [`UrlPattern::matches`] expects). `servlet_name` is the name of the
    /// servlet the request mapped to, when known — pass `None` to skip
    /// servlet-name mappings entirely. `dispatcher_type` restricts the result
    /// to mappings that cover that dispatch condition.
    ///
    /// Ordering follows the Servlet specification exactly:
    ///
    /// 1. URL-pattern mappings whose pattern matches `path`, in `web.xml`
    ///    declaration order.
    /// 2. Then servlet-name mappings whose name equals `servlet_name`, again in
    ///    declaration order.
    ///
    /// A filter named by several matching mappings appears once, at the
    /// position of its first match. Mappings whose `filter_name` has no
    /// corresponding [`FilterDef`] are skipped (with a warning) rather than
    /// failing the build.
    pub fn build_for_path(
        &self,
        path: &str,
        servlet_name: Option<&str>,
        dispatcher_type: DispatcherType,
    ) -> FilterChain {
        let mut entries: Vec<FilterChainEntry> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();

        // Pass 1 — URL-pattern matches, in declaration order.
        for mapping in self.registry.mappings() {
            if !mapping.covers_dispatcher(dispatcher_type) {
                continue;
            }
            if mapping.url_patterns.is_empty() || !mapping.matches_path(path) {
                continue;
            }
            self.push_filter(&mut entries, &mut seen, mapping, dispatcher_type);
        }

        // Pass 2 — servlet-name matches, in declaration order, appended after
        // every URL-pattern match.
        if let Some(servlet_name) = servlet_name {
            for mapping in self.registry.mappings() {
                if !mapping.covers_dispatcher(dispatcher_type) {
                    continue;
                }
                if mapping.servlet_names.is_empty() || !mapping.matches_servlet(servlet_name) {
                    continue;
                }
                self.push_filter(&mut entries, &mut seen, mapping, dispatcher_type);
            }
        }

        FilterChain { entries }
    }

    /// Append the filter named by `mapping` to `entries`, unless it was already
    /// added by an earlier (higher-priority) mapping.
    fn push_filter<'r>(
        &'r self,
        entries: &mut Vec<FilterChainEntry>,
        seen: &mut HashSet<&'r str>,
        mapping: &'r FilterMapping,
        dispatcher_type: DispatcherType,
    ) {
        if seen.contains(mapping.filter_name.as_str()) {
            return;
        }
        match self.registry.def(&mapping.filter_name) {
            Some(def) => {
                seen.insert(mapping.filter_name.as_str());
                entries.push(FilterChainEntry {
                    def: def.clone(),
                    dispatcher_type,
                });
            }
            None => {
                tracing::warn!(
                    filter = %mapping.filter_name,
                    "filter-mapping references an undeclared <filter>; skipping"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three filters, all bound to `/*` by URL pattern in a known order.
    fn registry_url_ordered() -> FilterRegistry {
        let defs = vec![
            FilterDef::new("first", "com.example.First"),
            FilterDef::new("second", "com.example.Second"),
            FilterDef::new("third", "com.example.Third"),
        ];
        let mappings = vec![
            FilterMapping::for_url("first", UrlPattern::parse("/*")),
            FilterMapping::for_url("second", UrlPattern::parse("/*")),
            FilterMapping::for_url("third", UrlPattern::parse("/*")),
        ];
        FilterRegistry::new(defs, mappings)
    }

    #[test]
    fn url_pattern_filters_keep_declaration_order() {
        let registry = registry_url_ordered();
        let builder = FilterChainBuilder::new(&registry);

        let chain = builder.build_for_path("/anything", None, DispatcherType::Request);
        assert_eq!(chain.filter_names(), ["first", "second", "third"]);
    }

    #[test]
    fn non_matching_url_patterns_are_excluded() {
        let defs = vec![
            FilterDef::new("api", "com.example.Api"),
            FilterDef::new("all", "com.example.All"),
        ];
        let mappings = vec![
            FilterMapping::for_url("api", UrlPattern::parse("/api/*")),
            FilterMapping::for_url("all", UrlPattern::parse("/*")),
        ];
        let registry = FilterRegistry::new(defs, mappings);
        let builder = FilterChainBuilder::new(&registry);

        // Under /api/* only `api` then `all`...
        let api_chain = builder.build_for_path("/api/users", None, DispatcherType::Request);
        assert_eq!(api_chain.filter_names(), ["api", "all"]);

        // ...elsewhere only `all`.
        let other = builder.build_for_path("/index.html", None, DispatcherType::Request);
        assert_eq!(other.filter_names(), ["all"]);
    }

    #[test]
    fn servlet_name_filters_come_after_url_pattern_filters() {
        let defs = vec![
            FilterDef::new("byurl", "com.example.ByUrl"),
            FilterDef::new("byname", "com.example.ByName"),
        ];
        // Declare the servlet-name mapping FIRST to prove ordering is by
        // mapping *kind*, not by declaration position across kinds.
        let mappings = vec![
            FilterMapping::for_servlet("byname", "dispatcher-servlet"),
            FilterMapping::for_url("byurl", UrlPattern::parse("/*")),
        ];
        let registry = FilterRegistry::new(defs, mappings);
        let builder = FilterChainBuilder::new(&registry);

        let chain = builder.build_for_path(
            "/anything",
            Some("dispatcher-servlet"),
            DispatcherType::Request,
        );
        // URL-pattern match first, servlet-name match second.
        assert_eq!(chain.filter_names(), ["byurl", "byname"]);

        // Without the servlet name, only the URL-pattern filter resolves.
        let no_servlet = builder.build_for_path("/anything", None, DispatcherType::Request);
        assert_eq!(no_servlet.filter_names(), ["byurl"]);
    }

    #[test]
    fn a_filter_named_twice_appears_once_at_first_match() {
        let defs = vec![
            FilterDef::new("dup", "com.example.Dup"),
            FilterDef::new("other", "com.example.Other"),
        ];
        let mappings = vec![
            FilterMapping::for_url("dup", UrlPattern::parse("/*")),
            FilterMapping::for_url("other", UrlPattern::parse("/*")),
            // `dup` again, by servlet name — must NOT add a second entry.
            FilterMapping::for_servlet("dup", "s"),
        ];
        let registry = FilterRegistry::new(defs, mappings);
        let builder = FilterChainBuilder::new(&registry);

        let chain = builder.build_for_path("/x", Some("s"), DispatcherType::Request);
        assert_eq!(chain.filter_names(), ["dup", "other"]);
    }

    #[test]
    fn dispatcher_type_filtering_excludes_non_covered_mappings() {
        let defs = vec![
            FilterDef::new("req-only", "com.example.ReqOnly"),
            FilterDef::new("fwd-only", "com.example.FwdOnly"),
            FilterDef::new("both", "com.example.Both"),
        ];
        let mappings = vec![
            // Default dispatcher set => REQUEST only.
            FilterMapping::for_url("req-only", UrlPattern::parse("/*")),
            FilterMapping::for_url("fwd-only", UrlPattern::parse("/*"))
                .with_dispatchers(vec![DispatcherType::Forward]),
            FilterMapping::for_url("both", UrlPattern::parse("/*"))
                .with_dispatchers(vec![DispatcherType::Request, DispatcherType::Forward]),
        ];
        let registry = FilterRegistry::new(defs, mappings);
        let builder = FilterChainBuilder::new(&registry);

        let on_request = builder.build_for_path("/p", None, DispatcherType::Request);
        assert_eq!(on_request.filter_names(), ["req-only", "both"]);

        let on_forward = builder.build_for_path("/p", None, DispatcherType::Forward);
        assert_eq!(on_forward.filter_names(), ["fwd-only", "both"]);

        let on_include = builder.build_for_path("/p", None, DispatcherType::Include);
        assert!(on_include.is_empty());
    }

    #[test]
    fn mapping_with_empty_dispatchers_normalises_to_request() {
        let mapping = FilterMapping::for_url("f", UrlPattern::parse("/*")).with_dispatchers(vec![]);
        assert_eq!(mapping.dispatcher_types, vec![DispatcherType::Request]);
    }

    #[test]
    fn undeclared_filter_in_mapping_is_skipped() {
        let defs = vec![FilterDef::new("known", "com.example.Known")];
        let mappings = vec![
            FilterMapping::for_url("ghost", UrlPattern::parse("/*")),
            FilterMapping::for_url("known", UrlPattern::parse("/*")),
        ];
        let registry = FilterRegistry::new(defs, mappings);
        let builder = FilterChainBuilder::new(&registry);

        let chain = builder.build_for_path("/x", None, DispatcherType::Request);
        assert_eq!(chain.filter_names(), ["known"]);
    }

    #[tokio::test]
    async fn noop_invoker_walks_the_whole_chain_to_the_end() {
        let registry = registry_url_ordered();
        let builder = FilterChainBuilder::new(&registry);
        let chain = builder.build_for_path("/anything", None, DispatcherType::Request);
        assert_eq!(chain.len(), 3);

        // The NoopFilterInvoker chains straight through; reaching the end
        // without error proves every link is wired up.
        chain.run(&NoopFilterInvoker).await.unwrap();
    }

    #[tokio::test]
    async fn running_an_empty_chain_reaches_the_end_immediately() {
        let chain = FilterChain::default();
        assert!(chain.is_empty());
        chain.run(&NoopFilterInvoker).await.unwrap();
    }

    /// An invoker that records the order in which links are invoked, then
    /// continues the walk — proving the cursor advances correctly.
    struct RecordingInvoker {
        seen: parking_lot::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl FilterInvoker for RecordingInvoker {
        async fn invoke(
            &self,
            entry: &FilterChainEntry,
            next: FilterChainCursor<'_>,
        ) -> Result<()> {
            self.seen.lock().push(entry.def.name.clone());
            next.proceed(self).await
        }
    }

    #[tokio::test]
    async fn custom_invoker_observes_links_in_chain_order() {
        let registry = registry_url_ordered();
        let builder = FilterChainBuilder::new(&registry);
        let chain = builder.build_for_path("/anything", None, DispatcherType::Request);

        let invoker = RecordingInvoker {
            seen: parking_lot::Mutex::new(Vec::new()),
        };
        chain.run(&invoker).await.unwrap();
        assert_eq!(
            *invoker.seen.lock(),
            vec![
                "first".to_string(),
                "second".to_string(),
                "third".to_string()
            ]
        );
    }

    #[test]
    fn from_web_descriptor_builds_url_pattern_mappings() {
        use tomcatrs_config::web_xml::{
            FilterDef as XmlFilterDef, FilterMapping as XmlFilterMapping, WebXml,
        };

        let mut web = WebXml::default();
        web.filters.push(XmlFilterDef {
            name: "encoding".to_string(),
            class: "com.example.EncodingFilter".to_string(),
            init_params: HashMap::new(),
        });
        web.filter_mappings.push(XmlFilterMapping {
            filter_name: "encoding".to_string(),
            url_pattern: "/*".to_string(),
        });

        let registry = FilterRegistry::from_web_descriptor(&web);
        assert_eq!(registry.def_count(), 1);
        assert!(registry.def("encoding").is_some());

        let builder = FilterChainBuilder::new(&registry);
        let chain = builder.build_for_path("/index.html", None, DispatcherType::Request);
        assert_eq!(chain.filter_names(), ["encoding"]);
    }

    #[test]
    fn dispatcher_type_parses_known_tokens_case_insensitively() {
        assert_eq!(
            DispatcherType::parse("request"),
            Some(DispatcherType::Request)
        );
        assert_eq!(
            DispatcherType::parse("  FORWARD "),
            Some(DispatcherType::Forward)
        );
        assert_eq!(DispatcherType::parse("Async"), Some(DispatcherType::Async));
        assert_eq!(DispatcherType::parse("nonsense"), None);
    }
}
