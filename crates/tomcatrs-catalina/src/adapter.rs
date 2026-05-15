//! [`CatalinaAdapter`] — the bridge from Coyote into the Catalina container
//! tree.
//!
//! This is the Rust analogue of `org.apache.catalina.connector.CoyoteAdapter`:
//! it implements [`tomcatrs_coyote::Adapter`], and for every inbound request it
//! drives the [`Mapper`] to resolve the `Engine` ➜ `Host` ➜ `Context` ➜
//! `Wrapper` chain from the request's `Host` header and path.
//!
//! # Routing outcomes
//!
//! For each request the adapter produces one of three responses:
//!
//! 1. **A servlet matched.** The JVM servlet bridge is not wired into v1.0.0,
//!    so instead of invoking the servlet the adapter returns a `501 Not
//!    Implemented` placeholder whose body names the resolved context, servlet,
//!    servlet-path and path-info. A `X-Tomcatrs-Mapped: <context>;<servlet>`
//!    header is attached so that callers (and tests) can confirm routing
//!    worked. The JVM-bridge agent will later replace this branch with a real
//!    servlet invocation.
//! 2. **No servlet, but a context owns the URL.** The request is served as a
//!    static file out of the context's `doc_base`, exactly
//!    as Tomcat's `DefaultServlet` would. The static-serving helper lives in
//!    this module ([`serve_static`]).
//! 3. **Nothing matched.** A `404 Not Found` is returned with an
//!    error-report-style HTML body, mirroring Tomcat's error page.
//!
//! # Sharing
//!
//! [`CatalinaAdapter`] holds an [`Arc<Engine>`] (the natural anchor for the
//! [`Mapper`]) plus a fallback document root. It is therefore `Send + Sync` and
//! cheap to `clone` — construct one and hand `Arc`-wrapped clones to every
//! connector.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use tomcatrs_coyote::{Adapter, Request, Response};

use crate::engine::Engine;
use crate::mapper::{Mapper, MappingResult};

/// An [`Adapter`] that routes Coyote requests through the Catalina [`Mapper`].
///
/// Construct with [`CatalinaAdapter::new`] from a live [`Arc<Engine>`] (e.g.
/// `server.service(name).engine()`), wrap it in an [`Arc`], and hand clones to
/// each `tomcatrs_coyote::HttpConnector`.
#[derive(Debug, Clone)]
pub struct CatalinaAdapter {
    /// The routing component, anchored on the engine's host/context tree.
    mapper: Mapper,
    /// Fallback document root used when a request resolves to no context at
    /// all (no host, or a host with no matching context). This keeps the
    /// server able to serve a default landing page and produce sensible 404s
    /// even before any application is deployed.
    app_base: PathBuf,
}

impl CatalinaAdapter {
    /// Create an adapter that routes against `engine`'s host/context tree,
    /// falling back to `app_base` as a document root when nothing matches.
    pub fn new(engine: Arc<Engine>, app_base: impl Into<PathBuf>) -> Self {
        Self {
            mapper: Mapper::new(engine),
            app_base: app_base.into(),
        }
    }

    /// The engine this adapter routes against.
    pub fn engine(&self) -> &Arc<Engine> {
        self.mapper.engine()
    }

    /// The fallback document root.
    pub fn app_base(&self) -> &Path {
        &self.app_base
    }

    /// Build the `501` placeholder response for a request that mapped onto a
    /// servlet wrapper.
    ///
    /// v1.0.0 has no JVM servlet bridge wired in, so rather than invoking the
    /// servlet the adapter returns a clear, self-describing placeholder. The
    /// `X-Tomcatrs-Mapped` header carries `<context-path>;<servlet-name>` so
    /// routing can be asserted without parsing the body.
    fn mapped_placeholder(mapping: &MappingResult) -> Response {
        let context_path = display_context_path(mapping.context.path());
        let servlet_name = mapping.wrapper.servlet_name();
        let servlet_class = mapping.wrapper.servlet_class();
        let path_info = mapping.path_info.as_deref().unwrap_or("");

        let body = format!(
            "<!DOCTYPE html>\n\
             <html lang=\"en\">\n\
             <head><meta charset=\"utf-8\"><title>501 Not Implemented</title></head>\n\
             <body style=\"font-family: system-ui, sans-serif;\">\n\
             <h1>501 &mdash; servlet routing succeeded</h1>\n\
             <p>The Catalina mapper resolved this request, but the JVM servlet\n\
             bridge is not wired into this build, so the servlet was not\n\
             invoked.</p>\n\
             <ul>\n\
             <li><strong>Host:</strong> {host}</li>\n\
             <li><strong>Context:</strong> {context}</li>\n\
             <li><strong>Servlet:</strong> {servlet} ({class})</li>\n\
             <li><strong>Servlet path:</strong> {servlet_path}</li>\n\
             <li><strong>Path info:</strong> {path_info}</li>\n\
             <li><strong>Matched pattern:</strong> {pattern}</li>\n\
             </ul>\n\
             </body>\n\
             </html>\n",
            host = mapping.host.name(),
            context = context_path,
            servlet = servlet_name,
            class = servlet_class,
            servlet_path = mapping.servlet_path,
            path_info = path_info,
            pattern = mapping.matched_pattern.as_pattern_string(),
        );

        let mut resp = Response::with_body(501, body);
        resp.set_header(
            "X-Tomcatrs-Mapped",
            &format!("{context_path};{servlet_name}"),
        );
        finalize(resp, "text/html; charset=utf-8")
    }
}

#[async_trait]
impl Adapter for CatalinaAdapter {
    async fn service(&self, req: Request) -> Response {
        // The mapper keys host resolution off the `Host` header; an absent
        // header is treated as the empty string, which falls through to the
        // engine's default host.
        let host_header = req.header("Host").unwrap_or("");

        match self.mapper.map(host_header, &req.path) {
            // A servlet wrapper matched — return the routing placeholder until
            // the JVM bridge is wired in.
            Some(mapping) => {
                let resp = Self::mapped_placeholder(&mapping);
                if req.method == "HEAD" {
                    let mut resp = resp;
                    resp.body = Bytes::new();
                    resp
                } else {
                    resp
                }
            }
            // No servlet matched. If a context still owns the URL, serve the
            // request as a static file from that context's document base;
            // otherwise fall back to the adapter's own document root so the
            // default ROOT page and 404s still work.
            None => {
                let doc_base = self
                    .mapper
                    .resolve_host(host_header)
                    .and_then(|host| self.mapper.resolve_context(&host, &req.path))
                    .map(|ctx| ctx.doc_base().to_path_buf())
                    .unwrap_or_else(|| self.app_base.clone());

                serve_static(&doc_base, &req).await
            }
        }
    }
}

/// Serve `req` as a static file resolved against `doc_base`.
///
/// This is the shared, path-traversal-hardened static handler used both for
/// the no-servlet-but-context case and for the no-context fallback. It mirrors
/// the behaviour of the CLI's original `StaticAdapter`:
///
/// * only `GET` and `HEAD` are accepted (`405` otherwise);
/// * `..` / prefix components are rejected lexically (`403`);
/// * a directory is served via its `index.html`;
/// * a request for `/` with no `index.html` gets a friendly landing page;
/// * anything else missing is a `404` error-report page.
pub async fn serve_static(doc_base: &Path, req: &Request) -> Response {
    if req.method != "GET" && req.method != "HEAD" {
        let body = error_report(405, "Method Not Allowed", &req.path);
        return finalize(Response::with_body(405, body), "text/html; charset=utf-8");
    }

    let Some(resolved) = resolve_under(doc_base, &req.path) else {
        let body = error_report(403, "Forbidden", &req.path);
        return finalize(Response::with_body(403, body), "text/html; charset=utf-8");
    };

    // Directory handling: look for an index.html inside it.
    let target = if resolved.is_dir() {
        resolved.join("index.html")
    } else {
        resolved.clone()
    };

    match tokio::fs::read(&target).await {
        Ok(bytes) => {
            let ct = content_type_for(&target);
            let mut resp = Response::with_body(200, Bytes::from(bytes));
            if req.method == "HEAD" {
                resp.body = Bytes::new();
            }
            finalize(resp, ct)
        }
        Err(_) => {
            // A request for the document-root index with no index.html on disk
            // gets the friendly landing page rather than a bare 404.
            if (req.path == "/" || req.path.is_empty())
                || (resolved == doc_base && !resolved.join("index.html").exists())
            {
                let mut resp = Response::with_body(200, landing_page());
                if req.method == "HEAD" {
                    resp.body = Bytes::new();
                }
                return finalize(resp, "text/html; charset=utf-8");
            }
            let body = error_report(404, "Not Found", &req.path);
            let mut resp = Response::with_body(404, body);
            if req.method == "HEAD" {
                resp.body = Bytes::new();
            }
            finalize(resp, "text/html; charset=utf-8")
        }
    }
}

/// Lexically resolve `request_path` against `doc_base`, refusing traversal.
///
/// Returns `None` if any component is `..` or a path prefix, or if the lexical
/// result would escape `doc_base`. The check is purely lexical so a missing
/// file still yields a clean `404` rather than an I/O error.
fn resolve_under(doc_base: &Path, request_path: &str) -> Option<PathBuf> {
    let rel = request_path.trim_start_matches('/');
    let candidate = Path::new(rel);

    let mut resolved = doc_base.to_path_buf();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => resolved.push(part),
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }

    if resolved.starts_with(doc_base) {
        Some(resolved)
    } else {
        None
    }
}

/// Render a context path for display: the ROOT context (`""`) shows as `/`.
fn display_context_path(path: &str) -> String {
    if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    }
}

/// Guess a `Content-Type` from a file-name extension.
///
/// Deliberately tiny — just the handful of types a smoke-test landing page
/// needs. Anything unrecognised is served as `text/plain`.
fn content_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") | Some("mjs") => "application/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("svg") => "image/svg+xml",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "text/plain; charset=utf-8",
    }
}

/// Format an HTTP `Date` header value (RFC 7231 IMF-fixdate) for "now".
///
/// Hand-rolled to avoid a date-crate dependency; correct for all dates after
/// the Unix epoch.
fn http_date_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let dow = ((days % 7) + 4) % 7; // 1970-01-01 was a Thursday (=4).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };

    const DOW: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        DOW[dow as usize],
        day,
        MON[(month - 1) as usize],
        year,
        hour,
        minute,
        second,
    )
}

/// The HTML landing page shown for `/` when no `index.html` exists.
fn landing_page() -> String {
    let version = tomcatrs_core::VERSION;
    format!(
        "<!DOCTYPE html>\n\
         <html lang=\"en\">\n\
         <head><meta charset=\"utf-8\"><title>Tomcat-RS Compatibility Runtime</title></head>\n\
         <body style=\"font-family: system-ui, sans-serif; max-width: 40rem; margin: 4rem auto;\">\n\
         <h1>Tomcat-RS Compatibility Runtime v{version} &mdash; it works!</h1>\n\
         <p>This request was routed through the Catalina mapper. No servlet or\n\
         static file claimed it, so this default page is served from the\n\
         document root.</p>\n\
         <p><strong>Note:</strong> Servlet and JSP execution is delegated to the\n\
         JVM bridge, which is <em>not</em> wired into this build.</p>\n\
         </body>\n\
         </html>\n"
    )
}

/// Render a Tomcat-style error-report HTML page.
fn error_report(status: u16, reason: &str, path: &str) -> String {
    format!(
        "<!DOCTYPE html>\n\
         <html lang=\"en\">\n\
         <head><meta charset=\"utf-8\"><title>{status} {reason}</title></head>\n\
         <body style=\"font-family: system-ui, sans-serif;\">\n\
         <h1>HTTP Status {status} &mdash; {reason}</h1>\n\
         <hr>\n\
         <p><strong>Path:</strong> {path}</p>\n\
         <p><strong>Description:</strong> The origin server did not find a\n\
         current representation for the target resource or is not willing to\n\
         disclose that one exists.</p>\n\
         <hr>\n\
         <h3>Tomcat-RS Compatibility Runtime v{version}</h3>\n\
         </body>\n\
         </html>\n",
        version = tomcatrs_core::VERSION,
    )
}

/// Apply the response headers every reply from this adapter carries.
fn finalize(mut resp: Response, content_type: &str) -> Response {
    let server = format!("Tomcat-RS/{}", tomcatrs_core::VERSION);
    resp.set_header("Server", &server)
        .set_header("Date", &http_date_now())
        .set_header("Content-Type", content_type);
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::path::PathBuf;

    use crate::context::Context;
    use crate::host::Host;
    use crate::mapper::UrlPattern;
    use crate::wrapper::Wrapper;

    /// Build a bare-bones `Request` for `host` + `path`.
    fn request(host: &str, path: &str) -> Request {
        let peer: SocketAddr = "127.0.0.1:0".parse().unwrap();
        Request {
            method: "GET".to_string(),
            uri: path.to_string(),
            path: path.to_string(),
            query: None,
            version: "HTTP/1.1".to_string(),
            headers: vec![("Host".to_string(), host.to_string())],
            body: Bytes::new(),
            peer_addr: peer,
        }
    }

    /// Construct an `Engine` ➜ `Host` ➜ `Context` ➜ `Wrapper` tree by hand.
    fn sample_engine() -> Arc<Engine> {
        // ROOT context with one exact-match servlet at `/hello`.
        let wrapper = Arc::new(Wrapper::new(
            "hello",
            "com.example.HelloServlet",
            vec![UrlPattern::parse("/hello")],
        ));
        let root_ctx = Arc::new(Context::new(
            "",
            PathBuf::from("/tmp/webapps/ROOT"),
            false,
            vec![wrapper],
        ));
        let host = Arc::new(Host::new(
            "localhost",
            PathBuf::from("/tmp/webapps"),
            vec!["127.0.0.1".to_string()],
            vec![root_ctx],
        ));
        let engine = Engine::new("Catalina", "localhost");
        engine.add_host(host);
        Arc::new(engine)
    }

    #[tokio::test]
    async fn mapped_request_returns_routing_placeholder() {
        let adapter = CatalinaAdapter::new(sample_engine(), "/tmp/webapps");
        let resp = adapter.service(request("localhost", "/hello")).await;

        // Routing succeeded: 501 placeholder + the X-Tomcatrs-Mapped header.
        assert_eq!(resp.status, 501);
        assert_eq!(resp.header("X-Tomcatrs-Mapped"), Some("/;hello"));

        let body = String::from_utf8(resp.body.to_vec()).unwrap();
        assert!(body.contains("hello"));
        assert!(body.contains("com.example.HelloServlet"));
    }

    #[tokio::test]
    async fn mapped_request_works_through_host_alias() {
        let adapter = CatalinaAdapter::new(sample_engine(), "/tmp/webapps");
        // `127.0.0.1` is an alias of `localhost`.
        let resp = adapter.service(request("127.0.0.1", "/hello")).await;
        assert_eq!(resp.status, 501);
        assert_eq!(resp.header("X-Tomcatrs-Mapped"), Some("/;hello"));
    }

    #[tokio::test]
    async fn unknown_path_in_known_host_is_404() {
        let adapter = CatalinaAdapter::new(sample_engine(), "/tmp/webapps");
        // The ROOT context owns the URL but no servlet matches and no static
        // file exists under the (non-existent) doc_base.
        let resp = adapter
            .service(request("localhost", "/no/such/servlet"))
            .await;
        assert_eq!(resp.status, 404);
        assert!(resp.header("X-Tomcatrs-Mapped").is_none());

        let body = String::from_utf8(resp.body.to_vec()).unwrap();
        assert!(body.contains("404"));
        assert!(body.contains("Not Found"));
    }

    #[tokio::test]
    async fn unknown_host_falls_back_and_404s() {
        // An engine whose default host has no contexts at all: every request
        // resolves to no context, falls back to `app_base`, and 404s on a
        // missing file.
        let host = Arc::new(Host::new(
            "localhost",
            PathBuf::from("/tmp/webapps"),
            vec![],
            vec![],
        ));
        let engine = Engine::new("Catalina", "localhost");
        engine.add_host(host);
        let adapter = CatalinaAdapter::new(Arc::new(engine), "/tmp/definitely/missing");

        let resp = adapter
            .service(request("nonexistent.invalid", "/whatever.txt"))
            .await;
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn static_file_under_context_is_served() {
        // A real temp directory acts as the context doc_base.
        let dir =
            std::env::temp_dir().join(format!("tomcatrs-adapter-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("note.txt");
        std::fs::write(&file, b"static body").unwrap();

        // ROOT context, no servlets, doc_base = our temp dir.
        let root_ctx = Arc::new(Context::new("", dir.clone(), false, vec![]));
        let host = Arc::new(Host::new(
            "localhost",
            PathBuf::from("/tmp/webapps"),
            vec![],
            vec![root_ctx],
        ));
        let engine = Engine::new("Catalina", "localhost");
        engine.add_host(host);
        let adapter = CatalinaAdapter::new(Arc::new(engine), "/tmp/webapps");

        let resp = adapter.service(request("localhost", "/note.txt")).await;
        assert_eq!(resp.status, 200);
        assert_eq!(&resp.body[..], b"static body");

        // Cleanup.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_under_rejects_traversal() {
        let base = Path::new("/srv/webapps");
        assert!(resolve_under(base, "/../etc/passwd").is_none());
        assert!(resolve_under(base, "/a/../../b").is_none());
        assert_eq!(
            resolve_under(base, "/css/site.css").unwrap(),
            PathBuf::from("/srv/webapps/css/site.css")
        );
        assert_eq!(
            resolve_under(base, "/").unwrap(),
            PathBuf::from("/srv/webapps")
        );
    }
}
