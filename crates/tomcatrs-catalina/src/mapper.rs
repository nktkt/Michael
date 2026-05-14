//! [`Mapper`] — request routing, the Rust port of Tomcat's
//! `org.apache.catalina.mapper.Mapper`.
//!
//! Given a `Host` header value and a request URI, the mapper resolves the
//! `(Host, Context, Wrapper)` triple that should service the request, plus the
//! decomposition of the URI into `servlet_path` / `path_info`.
//!
//! The algorithm mirrors the Servlet specification and Tomcat's implementation:
//!
//! 1. **Host resolution** — match the host name exactly, then against each
//!    host's aliases; if nothing matches, fall back to the engine's
//!    `default_host`.
//! 2. **Context resolution** — choose the context whose path is the *longest*
//!    prefix of the request URI (the empty-path "ROOT" context matches
//!    everything and always loses to a more specific path).
//! 3. **Wrapper resolution** — within the chosen context, match the
//!    context-relative path against every servlet [`UrlPattern`] and pick the
//!    winner by precedence: **exact** ➜ **longest path-prefix** ➜
//!    **extension** ➜ **default (`/`)**.

use std::sync::Arc;

use crate::context::Context;
use crate::engine::Engine;
use crate::host::Host;
use crate::wrapper::Wrapper;

/// A parsed servlet URL pattern, as found in a `<url-pattern>` element.
///
/// The four variants are the only kinds the Servlet specification defines, and
/// they form a strict precedence order used by [`UrlPattern::specificity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlPattern {
    /// An exact match, e.g. `/status` — matches that path and nothing else.
    Exact(String),
    /// A path prefix, e.g. `/api/*` — the stored string is the prefix *without*
    /// the trailing `/*` (so `/api/*` is stored as `/api`).
    Prefix(String),
    /// An extension match, e.g. `*.jsp` — the stored string is the extension
    /// *without* the leading `*.` (so `*.jsp` is stored as `jsp`).
    Extension(String),
    /// The default pattern `/` — matches anything no other pattern claims.
    Default,
}

impl UrlPattern {
    /// Parse a raw `<url-pattern>` string into a [`UrlPattern`].
    ///
    /// Parsing is total — it never fails. Inputs that do not look like a
    /// prefix, extension, or the default pattern are treated as [`Exact`]
    /// matches, which is the spec-mandated fallback.
    ///
    /// [`Exact`]: UrlPattern::Exact
    pub fn parse(raw: &str) -> UrlPattern {
        if raw == "/" || raw.is_empty() {
            // An empty pattern denotes the context root; Tomcat treats the
            // bare `/` and `""` as the default-servlet pattern.
            return UrlPattern::Default;
        }
        if raw == "/*" {
            // `/*` is a path prefix whose prefix is the empty string: it
            // matches every path but still loses to exact and extension
            // matches. Stored as a zero-length prefix.
            return UrlPattern::Prefix(String::new());
        }
        if let Some(prefix) = raw.strip_suffix("/*") {
            return UrlPattern::Prefix(prefix.to_string());
        }
        if let Some(ext) = raw.strip_prefix("*.") {
            return UrlPattern::Extension(ext.to_string());
        }
        UrlPattern::Exact(raw.to_string())
    }

    /// Test whether this pattern matches the context-relative `path`.
    ///
    /// `path` is expected to be the request URI with the context path already
    /// stripped (e.g. for URI `/app/api/users` under context `/app`, `path` is
    /// `/api/users`).
    pub fn matches(&self, path: &str) -> bool {
        match self {
            UrlPattern::Exact(p) => p == path,
            UrlPattern::Prefix(prefix) => {
                if prefix.is_empty() {
                    // `/*` — matches everything.
                    return true;
                }
                // `/api` matches `/api` exactly and `/api/<anything>`.
                path == prefix
                    || (path.starts_with(prefix)
                        && path.as_bytes().get(prefix.len()) == Some(&b'/'))
            }
            UrlPattern::Extension(ext) => {
                // Extension matches apply only to the final path segment.
                match path.rsplit('/').next() {
                    Some(last) => last
                        .rsplit_once('.')
                        .map(|(_, suffix)| suffix == ext)
                        .unwrap_or(false),
                    None => false,
                }
            }
            UrlPattern::Default => true,
        }
    }

    /// A precedence score for this pattern *kind*, independent of the request.
    ///
    /// Higher wins. Used to break ties between patterns of different kinds:
    /// exact (`3`) ➜ prefix (`2`) ➜ extension (`1`) ➜ default (`0`).
    pub fn specificity(&self) -> u8 {
        match self {
            UrlPattern::Exact(_) => 3,
            UrlPattern::Prefix(_) => 2,
            UrlPattern::Extension(_) => 1,
            UrlPattern::Default => 0,
        }
    }

    /// The full ranking score of a *successful* match against `path`.
    ///
    /// Returns `None` when the pattern does not match. When it does, the score
    /// is `(specificity, match_length)`: the kind dominates, and within the
    /// prefix kind a longer matched prefix wins (so `/a/b/*` beats `/a/*`).
    fn match_score(&self, path: &str) -> Option<(u8, usize)> {
        if !self.matches(path) {
            return None;
        }
        let length = match self {
            UrlPattern::Exact(p) => p.len(),
            UrlPattern::Prefix(prefix) => prefix.len(),
            UrlPattern::Extension(ext) => ext.len(),
            UrlPattern::Default => 0,
        };
        Some((self.specificity(), length))
    }

    /// Render the pattern back to its canonical `<url-pattern>` string form.
    pub fn as_pattern_string(&self) -> String {
        match self {
            UrlPattern::Exact(p) => p.clone(),
            UrlPattern::Prefix(prefix) if prefix.is_empty() => "/*".to_string(),
            UrlPattern::Prefix(prefix) => format!("{prefix}/*"),
            UrlPattern::Extension(ext) => format!("*.{ext}"),
            UrlPattern::Default => "/".to_string(),
        }
    }
}

/// The outcome of routing a request through the [`Mapper`].
///
/// All three container handles are reference-counted clones of the live
/// component tree, so a `MappingResult` can outlive the mapping call cheaply.
#[derive(Debug, Clone)]
pub struct MappingResult {
    /// The virtual host that claimed the request.
    pub host: Arc<Host>,
    /// The web application context the request was routed into.
    pub context: Arc<Context>,
    /// The servlet wrapper selected to service the request.
    pub wrapper: Arc<Wrapper>,
    /// The portion of the URI mapped to the servlet (the "servlet path").
    ///
    /// For an exact or extension match this is the whole context-relative
    /// path; for a prefix match it is the prefix; for the default servlet it
    /// is empty.
    pub servlet_path: String,
    /// Extra path information after the servlet path, or `None` when there is
    /// none. Corresponds to `HttpServletRequest::getPathInfo`.
    pub path_info: Option<String>,
    /// The matched servlet pattern, kept for diagnostics and JSP dispatch.
    pub matched_pattern: UrlPattern,
}

/// Stateless request router over a borrowed [`Engine`].
///
/// The mapper holds no mutable state of its own — it is a thin, cheaply
/// constructed view over the live container tree, so a fresh one can be made
/// per request (or cached, since it is `Send + Sync`).
#[derive(Debug, Clone)]
pub struct Mapper {
    engine: Arc<Engine>,
}

impl Mapper {
    /// Create a mapper that routes against `engine`'s host/context tree.
    pub fn new(engine: Arc<Engine>) -> Self {
        Self { engine }
    }

    /// The engine this mapper routes against.
    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }

    /// Resolve `host_header` + `uri` to a [`MappingResult`].
    ///
    /// `host_header` is matched case-insensitively (DNS names are
    /// case-insensitive); a port suffix, if present, is ignored. `uri` is the
    /// decoded request path beginning with `/`.
    ///
    /// Returns `None` when no context or no servlet can be found — callers
    /// translate that into a `404`.
    pub fn map(&self, host_header: &str, uri: &str) -> Option<MappingResult> {
        let host = self.resolve_host(host_header)?;
        let context = self.resolve_context(&host, uri)?;

        // The path the servlet patterns are matched against is the request URI
        // with the context path removed.
        let relative = strip_context_path(context.path(), uri);
        let (wrapper, matched_pattern) = self.resolve_wrapper(&context, &relative)?;

        let (servlet_path, path_info) = split_servlet_path(&matched_pattern, &relative);

        Some(MappingResult {
            host,
            context,
            wrapper,
            servlet_path,
            path_info,
            matched_pattern,
        })
    }

    /// Step 1 — host resolution: exact name, then aliases, then `default_host`.
    pub fn resolve_host(&self, host_header: &str) -> Option<Arc<Host>> {
        let name = normalize_host(host_header);

        if let Some(host) = self.engine.host(&name) {
            return Some(host);
        }
        // Alias scan — aliases are also case-insensitive DNS names.
        for entry in self.engine.hosts().iter() {
            let host = entry.value();
            if host.aliases().iter().any(|a| a.eq_ignore_ascii_case(&name)) {
                return Some(Arc::clone(host));
            }
        }
        // Fallback to the configured default host.
        self.engine.host(self.engine.default_host())
    }

    /// Step 2 — context resolution: the context whose path is the longest
    /// prefix of `uri`.
    pub fn resolve_context(&self, host: &Host, uri: &str) -> Option<Arc<Context>> {
        let mut best: Option<Arc<Context>> = None;
        let mut best_len = 0usize;

        for entry in host.contexts().iter() {
            let ctx = entry.value();
            let path = ctx.path();
            if !uri_under_context(path, uri) {
                continue;
            }
            // Longest matching context path wins; on a tie the existing pick
            // is kept (paths are unique keys, so ties cannot actually occur).
            if best.is_none() || path.len() > best_len {
                best_len = path.len();
                best = Some(Arc::clone(ctx));
            }
        }
        best
    }

    /// Step 3 — wrapper resolution: the highest-scoring servlet pattern within
    /// `context` for the context-relative `relative_path`.
    pub fn resolve_wrapper(
        &self,
        context: &Context,
        relative_path: &str,
    ) -> Option<(Arc<Wrapper>, UrlPattern)> {
        let mut best: Option<(Arc<Wrapper>, UrlPattern, (u8, usize))> = None;

        for wrapper in context.wrappers() {
            for pattern in wrapper.mappings() {
                if let Some(score) = pattern.match_score(relative_path) {
                    let better = match &best {
                        None => true,
                        Some((_, _, best_score)) => score > *best_score,
                    };
                    if better {
                        best = Some((Arc::clone(wrapper), pattern.clone(), score));
                    }
                }
            }
        }
        best.map(|(w, p, _)| (w, p))
    }
}

/// Lower-case a host header and drop any `:port` suffix.
fn normalize_host(host_header: &str) -> String {
    let without_port = host_header.split(':').next().unwrap_or(host_header);
    without_port.trim().to_ascii_lowercase()
}

/// Is `uri` served by the context mounted at `context_path`?
///
/// The ROOT context (`""` or `"/"`) serves every URI. A context at `/app`
/// serves `/app` itself and anything under `/app/`.
fn uri_under_context(context_path: &str, uri: &str) -> bool {
    if context_path.is_empty() || context_path == "/" {
        return true;
    }
    uri == context_path
        || (uri.starts_with(context_path) && uri.as_bytes().get(context_path.len()) == Some(&b'/'))
}

/// Remove the context path prefix from `uri`, yielding the context-relative
/// path (always beginning with `/`, or `/` itself for the bare context root).
fn strip_context_path(context_path: &str, uri: &str) -> String {
    let rel = if context_path.is_empty() || context_path == "/" {
        uri
    } else {
        uri.strip_prefix(context_path).unwrap_or(uri)
    };
    if rel.is_empty() {
        "/".to_string()
    } else {
        rel.to_string()
    }
}

/// Split a context-relative path into `(servlet_path, path_info)` according to
/// the kind of pattern that matched it, per the Servlet specification.
fn split_servlet_path(pattern: &UrlPattern, relative: &str) -> (String, Option<String>) {
    match pattern {
        // Exact and extension matches consume the whole path; no path info.
        UrlPattern::Exact(_) | UrlPattern::Extension(_) => (relative.to_string(), None),
        // A prefix match: the prefix is the servlet path, the remainder (if
        // any, and non-empty) is the path info.
        UrlPattern::Prefix(prefix) => {
            if prefix.is_empty() {
                // `/*` — servlet path is empty, the whole path is path info.
                let info = if relative.is_empty() {
                    None
                } else {
                    Some(relative.to_string())
                };
                (String::new(), info)
            } else {
                let rest = &relative[prefix.len()..];
                let info = if rest.is_empty() {
                    None
                } else {
                    Some(rest.to_string())
                };
                (prefix.clone(), info)
            }
        }
        // The default servlet: servlet path empty, whole path is path info.
        UrlPattern::Default => {
            let info = if relative.is_empty() || relative == "/" {
                Some(relative.to_string())
            } else {
                Some(relative.to_string())
            };
            (String::new(), info)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Context;
    use crate::engine::Engine;
    use crate::host::Host;
    use crate::wrapper::Wrapper;
    use std::path::PathBuf;

    // ---- UrlPattern::parse --------------------------------------------------

    #[test]
    fn parse_classifies_every_pattern_kind() {
        assert_eq!(
            UrlPattern::parse("/status"),
            UrlPattern::Exact("/status".into())
        );
        assert_eq!(
            UrlPattern::parse("/api/*"),
            UrlPattern::Prefix("/api".into())
        );
        assert_eq!(UrlPattern::parse("/*"), UrlPattern::Prefix(String::new()));
        assert_eq!(
            UrlPattern::parse("*.jsp"),
            UrlPattern::Extension("jsp".into())
        );
        assert_eq!(UrlPattern::parse("/"), UrlPattern::Default);
        assert_eq!(UrlPattern::parse(""), UrlPattern::Default);
    }

    #[test]
    fn pattern_roundtrips_through_string_form() {
        for raw in ["/status", "/api/*", "/*", "*.jsp", "/"] {
            let p = UrlPattern::parse(raw);
            assert_eq!(UrlPattern::parse(&p.as_pattern_string()), p);
        }
    }

    // ---- UrlPattern::matches ------------------------------------------------

    #[test]
    fn exact_pattern_matches_only_itself() {
        let p = UrlPattern::parse("/foo");
        assert!(p.matches("/foo"));
        assert!(!p.matches("/foo/"));
        assert!(!p.matches("/foo/bar"));
        assert!(!p.matches("/foobar"));
    }

    #[test]
    fn prefix_pattern_matches_prefix_and_descendants() {
        let p = UrlPattern::parse("/foo/*");
        assert!(p.matches("/foo")); // the prefix itself
        assert!(p.matches("/foo/bar"));
        assert!(p.matches("/foo/bar/baz"));
        assert!(!p.matches("/foobar")); // not a path-segment boundary
        assert!(!p.matches("/bar"));
    }

    #[test]
    fn extension_pattern_matches_final_segment_suffix() {
        let p = UrlPattern::parse("*.jsp");
        assert!(p.matches("/index.jsp"));
        assert!(p.matches("/a/b/c.jsp"));
        assert!(!p.matches("/index.jspx"));
        assert!(!p.matches("/index.html"));
        assert!(!p.matches("/jsp"));
    }

    #[test]
    fn default_pattern_matches_anything() {
        let p = UrlPattern::parse("/");
        assert!(p.matches("/"));
        assert!(p.matches("/anything/at/all"));
    }

    // ---- precedence: exact > prefix > extension > default ------------------

    fn ctx_with_servlets(path: &str, mappings: &[(&str, &str)]) -> Arc<Context> {
        let wrappers = mappings
            .iter()
            .map(|(name, pat)| {
                Arc::new(Wrapper::new(
                    *name,
                    format!("com.example.{name}"),
                    vec![UrlPattern::parse(pat)],
                ))
            })
            .collect();
        Arc::new(Context::new(path, PathBuf::from("/tmp"), false, wrappers))
    }

    fn engine_with(hosts: Vec<Arc<Host>>, default_host: &str) -> Arc<Engine> {
        let engine = Engine::new("Catalina", default_host);
        for h in hosts {
            engine.add_host(h);
        }
        Arc::new(engine)
    }

    #[test]
    fn exact_beats_prefix_extension_and_default() {
        let ctx = ctx_with_servlets(
            "",
            &[
                ("def", "/"),
                ("ext", "*.do"),
                ("pre", "/foo/*"),
                ("exact", "/foo/bar.do"),
            ],
        );
        let host = Arc::new(Host::new(
            "localhost",
            PathBuf::from("/w"),
            vec![],
            vec![ctx],
        ));
        let engine = engine_with(vec![host], "localhost");
        let mapper = Mapper::new(engine);

        let r = mapper.map("localhost", "/foo/bar.do").unwrap();
        assert_eq!(r.wrapper.servlet_name(), "exact");
        assert_eq!(r.matched_pattern, UrlPattern::Exact("/foo/bar.do".into()));
        assert_eq!(r.servlet_path, "/foo/bar.do");
        assert_eq!(r.path_info, None);
    }

    #[test]
    fn prefix_beats_extension_and_default() {
        let ctx = ctx_with_servlets("", &[("def", "/"), ("ext", "*.do"), ("pre", "/foo/*")]);
        let host = Arc::new(Host::new(
            "localhost",
            PathBuf::from("/w"),
            vec![],
            vec![ctx],
        ));
        let mapper = Mapper::new(engine_with(vec![host], "localhost"));

        let r = mapper.map("localhost", "/foo/bar.do").unwrap();
        assert_eq!(r.wrapper.servlet_name(), "pre");
        assert_eq!(r.servlet_path, "/foo");
        assert_eq!(r.path_info.as_deref(), Some("/bar.do"));
    }

    #[test]
    fn longer_prefix_beats_shorter_prefix() {
        let ctx = ctx_with_servlets("", &[("short", "/foo/*"), ("long", "/foo/bar/*")]);
        let host = Arc::new(Host::new(
            "localhost",
            PathBuf::from("/w"),
            vec![],
            vec![ctx],
        ));
        let mapper = Mapper::new(engine_with(vec![host], "localhost"));

        let r = mapper.map("localhost", "/foo/bar/baz").unwrap();
        assert_eq!(r.wrapper.servlet_name(), "long");
        assert_eq!(r.servlet_path, "/foo/bar");
        assert_eq!(r.path_info.as_deref(), Some("/baz"));
    }

    #[test]
    fn extension_beats_default() {
        let ctx = ctx_with_servlets("", &[("def", "/"), ("ext", "*.jsp")]);
        let host = Arc::new(Host::new(
            "localhost",
            PathBuf::from("/w"),
            vec![],
            vec![ctx],
        ));
        let mapper = Mapper::new(engine_with(vec![host], "localhost"));

        let r = mapper.map("localhost", "/pages/index.jsp").unwrap();
        assert_eq!(r.wrapper.servlet_name(), "ext");
        assert_eq!(r.servlet_path, "/pages/index.jsp");
        assert_eq!(r.path_info, None);
    }

    #[test]
    fn default_servlet_is_last_resort() {
        let ctx = ctx_with_servlets("", &[("def", "/"), ("ext", "*.jsp")]);
        let host = Arc::new(Host::new(
            "localhost",
            PathBuf::from("/w"),
            vec![],
            vec![ctx],
        ));
        let mapper = Mapper::new(engine_with(vec![host], "localhost"));

        let r = mapper.map("localhost", "/static/logo.png").unwrap();
        assert_eq!(r.wrapper.servlet_name(), "def");
        assert_eq!(r.servlet_path, "");
        assert_eq!(r.path_info.as_deref(), Some("/static/logo.png"));
    }

    // ---- context path resolution -------------------------------------------

    #[test]
    fn longest_context_path_wins() {
        let root = ctx_with_servlets("", &[("rootsv", "/")]);
        let app = ctx_with_servlets("/app", &[("appsv", "/")]);
        let app_v2 = ctx_with_servlets("/app/v2", &[("v2sv", "/")]);
        let host = Arc::new(Host::new(
            "localhost",
            PathBuf::from("/w"),
            vec![],
            vec![root, app, app_v2],
        ));
        let mapper = Mapper::new(engine_with(vec![host], "localhost"));

        assert_eq!(
            mapper
                .map("localhost", "/app/v2/page")
                .unwrap()
                .context
                .path(),
            "/app/v2"
        );
        assert_eq!(
            mapper
                .map("localhost", "/app/other")
                .unwrap()
                .context
                .path(),
            "/app"
        );
        assert_eq!(
            mapper
                .map("localhost", "/somewhere")
                .unwrap()
                .context
                .path(),
            ""
        );
    }

    #[test]
    fn context_path_must_match_segment_boundary() {
        let app = ctx_with_servlets("/app", &[("appsv", "/")]);
        let host = Arc::new(Host::new(
            "localhost",
            PathBuf::from("/w"),
            vec![],
            vec![app],
        ));
        let mapper = Mapper::new(engine_with(vec![host], "localhost"));

        // `/application` must NOT route into context `/app`.
        assert!(mapper.map("localhost", "/application").is_none());
        // `/app` and `/app/x` must.
        assert_eq!(
            mapper.map("localhost", "/app").unwrap().context.path(),
            "/app"
        );
        assert_eq!(
            mapper.map("localhost", "/app/x").unwrap().context.path(),
            "/app"
        );
    }

    // ---- host resolution ----------------------------------------------------

    #[test]
    fn host_resolves_by_exact_alias_and_default() {
        let h1 = Arc::new(Host::new(
            "localhost",
            PathBuf::from("/w"),
            vec!["127.0.0.1".to_string(), "tomcat.local".to_string()],
            vec![ctx_with_servlets("", &[("s", "/")])],
        ));
        let h2 = Arc::new(Host::new(
            "example.com",
            PathBuf::from("/w2"),
            vec![],
            vec![ctx_with_servlets("", &[("s", "/")])],
        ));
        let mapper = Mapper::new(engine_with(vec![h1, h2], "localhost"));

        // exact
        assert_eq!(
            mapper.resolve_host("example.com").unwrap().name(),
            "example.com"
        );
        // case-insensitive + port stripped
        assert_eq!(
            mapper.resolve_host("EXAMPLE.COM:8080").unwrap().name(),
            "example.com"
        );
        // alias
        assert_eq!(
            mapper.resolve_host("tomcat.local").unwrap().name(),
            "localhost"
        );
        // unknown -> default host
        assert_eq!(
            mapper.resolve_host("nope.invalid").unwrap().name(),
            "localhost"
        );
    }

    #[test]
    fn map_returns_none_when_no_servlet_matches() {
        // A context with only an exact pattern; an unrelated URI matches nothing.
        let ctx = ctx_with_servlets("", &[("only", "/exact")]);
        let host = Arc::new(Host::new(
            "localhost",
            PathBuf::from("/w"),
            vec![],
            vec![ctx],
        ));
        let mapper = Mapper::new(engine_with(vec![host], "localhost"));

        assert!(mapper.map("localhost", "/exact").is_some());
        assert!(mapper.map("localhost", "/something-else").is_none());
    }
}
