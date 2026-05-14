//! A minimal static-file [`Adapter`] for the `tomcatrs` binary.
//!
//! [`StaticAdapter`] is the v0.1.0 stand-in for Tomcat's `DefaultServlet`: it
//! serves plain files out of a document root and nothing more. Real
//! Servlet/JSP execution is delegated to the JVM bridge (`tomcatrs-servlet-bridge`
//! / `tomcatrs-jsp`), which is *not* wired into this MVP — the adapter exists so
//! that `tomcatrs run` produces a working, observable HTTP server today.
//!
//! # Security
//!
//! The adapter is path-traversal hardened: the request path is rejected
//! outright if, after normalization, any component is `..` or the resolved
//! path would escape [`app_base`](StaticAdapter::app_base). Tomcat's Coyote
//! layer already normalizes URIs, but defence-in-depth is cheap here.

use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use tomcatrs_coyote::{Adapter, Request, Response};

/// Serves static files from a document root directory.
///
/// Construct one per HTTP/1.1 connector, wrap it in an [`std::sync::Arc`], and
/// hand it to a `tomcatrs_coyote::HttpConnector`.
#[derive(Debug, Clone)]
pub struct StaticAdapter {
    /// The document root every request path is resolved against.
    pub app_base: PathBuf,
}

impl StaticAdapter {
    /// Create an adapter rooted at `app_base`.
    pub fn new(app_base: impl Into<PathBuf>) -> Self {
        Self {
            app_base: app_base.into(),
        }
    }

    /// Resolve a request path against [`app_base`](Self::app_base).
    ///
    /// Returns `None` if the path attempts traversal (`..`), is absolute in a
    /// way that would escape the root, or otherwise resolves outside the
    /// document root. The returned path is always a descendant of `app_base`.
    fn resolve(&self, request_path: &str) -> Option<PathBuf> {
        // Strip the leading slash; treat the remainder as relative.
        let rel = request_path.trim_start_matches('/');
        let candidate = Path::new(rel);

        // Walk components, rejecting anything that could escape the root. We do
        // this lexically rather than via `canonicalize` so that a missing file
        // still produces a clean 404 rather than an I/O error, and so the check
        // does not depend on the filesystem state.
        let mut resolved = self.app_base.clone();
        for component in candidate.components() {
            match component {
                Component::Normal(part) => resolved.push(part),
                // A bare `/`, `./` — harmless, skip.
                Component::CurDir | Component::RootDir => {}
                // `..`, drive prefixes, or anything else: refuse.
                Component::ParentDir | Component::Prefix(_) => return None,
            }
        }

        // Defence-in-depth: the lexical result must still be under `app_base`.
        if resolved.starts_with(&self.app_base) {
            Some(resolved)
        } else {
            None
        }
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
/// We avoid pulling in a date crate for the MVP; this hand-rolled formatter is
/// good enough for a `Date:` header and is correct for all dates after the
/// Unix epoch.
fn http_date_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Civil-from-days algorithm (Howard Hinnant), epoch = 1970-01-01.
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
         <p>This is the built-in static file server. Drop files into the document\n\
         root and they will be served from here.</p>\n\
         <p><strong>Note:</strong> Servlet and JSP execution is delegated to the\n\
         JVM bridge, which is <em>not</em> wired into this MVP. Only static\n\
         content is served in v0.1.0.</p>\n\
         </body>\n\
         </html>\n"
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

#[async_trait]
impl Adapter for StaticAdapter {
    async fn service(&self, req: Request) -> Response {
        // Only GET and HEAD are meaningful for a static file server.
        if req.method != "GET" && req.method != "HEAD" {
            return finalize(
                Response::with_body(405, "405 Method Not Allowed"),
                "text/plain; charset=utf-8",
            );
        }

        // Reject traversal / escape attempts up front.
        let Some(resolved) = self.resolve(&req.path) else {
            return finalize(
                Response::with_body(403, "403 Forbidden"),
                "text/plain; charset=utf-8",
            );
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
                let body: Bytes = Bytes::from(bytes);
                let mut resp = Response::with_body(200, body);
                // For HEAD, drop the body but keep the status/headers.
                if req.method == "HEAD" {
                    resp.body = Bytes::new();
                }
                finalize(resp, ct)
            }
            Err(_) => {
                // Special case: a request for "/" with no index.html gets the
                // friendly landing page instead of a bare 404.
                if (req.path == "/" || req.path.is_empty())
                    || (resolved == self.app_base && !resolved.join("index.html").exists())
                {
                    let mut resp = Response::with_body(200, landing_page());
                    if req.method == "HEAD" {
                        resp.body = Bytes::new();
                    }
                    return finalize(resp, "text/html; charset=utf-8");
                }
                finalize(
                    Response::with_body(404, "404 Not Found"),
                    "text/plain; charset=utf-8",
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_guessing() {
        assert_eq!(
            content_type_for(Path::new("index.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            content_type_for(Path::new("a/b/style.css")),
            "text/css; charset=utf-8"
        );
        assert_eq!(
            content_type_for(Path::new("app.js")),
            "application/javascript; charset=utf-8"
        );
        assert_eq!(content_type_for(Path::new("data.json")), "application/json");
        assert_eq!(content_type_for(Path::new("logo.png")), "image/png");
        assert_eq!(content_type_for(Path::new("photo.JPG")), "image/jpeg");
        assert_eq!(content_type_for(Path::new("icon.svg")), "image/svg+xml");
        // Unknown / no extension falls back to text/plain.
        assert_eq!(
            content_type_for(Path::new("README")),
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            content_type_for(Path::new("archive.tar.gz")),
            "text/plain; charset=utf-8"
        );
    }

    #[test]
    fn traversal_is_rejected() {
        let adapter = StaticAdapter::new("/srv/webapps");

        // Plain `..` traversal is refused.
        assert!(adapter.resolve("/../etc/passwd").is_none());
        assert!(adapter.resolve("/../../etc/passwd").is_none());
        assert!(adapter.resolve("/foo/../../bar").is_none());

        // A legitimate nested path resolves *inside* the root.
        let ok = adapter.resolve("/css/site.css").expect("should resolve");
        assert!(ok.starts_with("/srv/webapps"));
        assert_eq!(ok, PathBuf::from("/srv/webapps/css/site.css"));

        // The root itself resolves to the document base.
        assert_eq!(
            adapter.resolve("/").expect("root resolves"),
            PathBuf::from("/srv/webapps")
        );
    }

    #[test]
    fn resolved_paths_never_escape_app_base() {
        let adapter = StaticAdapter::new("/srv/webapps");
        for probe in [
            "/ok/file.txt",
            "/deep/nested/dir/x",
            "/./also/ok",
            "/index.html",
        ] {
            let resolved = adapter.resolve(probe).expect("benign path resolves");
            assert!(
                resolved.starts_with("/srv/webapps"),
                "{probe} escaped app_base: {resolved:?}"
            );
        }
    }
}
