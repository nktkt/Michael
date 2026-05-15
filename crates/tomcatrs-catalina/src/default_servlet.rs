//! [`DefaultServlet`] — the static-resource handler for a [`Context`].
//!
//! This is the Rust analogue of `org.apache.catalina.servlets.DefaultServlet`:
//! the servlet Tomcat maps at `/` in every web application to serve the static
//! files that live under the context's document base. Where the
//! [`adapter`](crate::adapter) module's [`serve_static`](crate::adapter::serve_static)
//! helper is a deliberately tiny smoke-test handler, `DefaultServlet` implements
//! the real HTTP semantics a browser (and the HTTP spec) expect:
//!
//! * **path safety** — request paths are resolved lexically under the document
//!   base; `..` traversal, path prefixes, and the protected `/WEB-INF` and
//!   `/META-INF` trees are rejected;
//! * **welcome files** — a request for a directory tries each configured
//!   welcome file in order; failing that it either renders an HTML directory
//!   listing (when [`DefaultServletConfig::listings`] is on) or returns
//!   `404`/`403`;
//! * **conditional GET** — `If-Modified-Since`, `If-None-Match`,
//!   `If-Unmodified-Since` and `If-Match` are all honoured, with a weak
//!   [`ETag`](https://www.rfc-editor.org/rfc/rfc7232) derived from the file's
//!   size and mtime;
//! * **range requests** — `Range: bytes=...` is parsed; a single range yields a
//!   `206` with `Content-Range`, multiple ranges yield a `206`
//!   `multipart/byteranges`, and an unsatisfiable range yields `416`.
//!   `Accept-Ranges: bytes` is always advertised;
//! * **content typing** — a reasonably complete extension → MIME map;
//! * **caching/identity headers** — `Last-Modified`, `ETag`, `Content-Length`,
//!   `Date` and `Server` are set on every successful response.
//!
//! # Synchronous I/O
//!
//! [`DefaultServlet::serve`] performs blocking file I/O with [`std::fs`]. That
//! is acceptable for v1.0.0 — the bodies are read fully into memory — but large
//! files should later move to an async, zero-copy `sendfile` path. As a bridge
//! to that future, when a file's size exceeds
//! [`DefaultServletConfig::sendfile_threshold`] the response carries an
//! `X-Tomcatrs-Sendfile` header naming the on-disk path; a connector that
//! understands the marker may stream the file itself instead of buffering the
//! body. The body is *still* produced correctly in v1.0.0 so the marker is a
//! pure optimisation hint.

use std::fmt::Write as _;
use std::fs::{self, Metadata};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use tomcatrs_coyote::{Request, Response};

use crate::context::Context;

/// Tunable behaviour for a [`DefaultServlet`].
///
/// Mirrors the subset of `DefaultServlet`'s `<init-param>`s that affect static
/// serving. Construct with [`DefaultServletConfig::default`] and override
/// fields as needed.
#[derive(Debug, Clone)]
pub struct DefaultServletConfig {
    /// Whether a directory with no matching welcome file is rendered as an HTML
    /// listing. When `false` such a request is answered with `403 Forbidden`,
    /// matching Tomcat's default (`listings = false`).
    pub listings: bool,
    /// Whether write methods (`PUT`, `DELETE`) are permitted. When `true` they
    /// are accepted by the dispatcher but answered with `501 Not Implemented`
    /// (the write path is scaffolded, not implemented, in v1.0.0). When `false`
    /// they are rejected up front with `405 Method Not Allowed`.
    pub read_only: bool,
    /// File-size threshold, in bytes, at or above which a successful response
    /// carries the `X-Tomcatrs-Sendfile` hint header. `0` disables the hint.
    pub sendfile_threshold: usize,
}

impl Default for DefaultServletConfig {
    /// Tomcat-compatible defaults: no directory listings, read-only, and a
    /// 48&nbsp;KiB sendfile threshold.
    fn default() -> Self {
        Self {
            listings: false,
            read_only: true,
            sendfile_threshold: 48 * 1024,
        }
    }
}

/// A static-resource handler bound to one context's document base.
///
/// Build one with [`DefaultServlet::new`] (explicit configuration) or
/// [`DefaultServlet::for_context`] (reads the document base and welcome-file
/// list straight off a deployed [`Context`]). Then call
/// [`DefaultServlet::serve`] per request.
#[derive(Debug, Clone)]
pub struct DefaultServlet {
    /// Filesystem root every request path is resolved under.
    doc_base: PathBuf,
    /// Behaviour knobs.
    config: DefaultServletConfig,
    /// Welcome files tried, in order, for a request that resolves to a
    /// directory.
    welcome_files: Vec<String>,
}

impl DefaultServlet {
    /// Create a `DefaultServlet` serving `doc_base` with the given `config` and
    /// `welcome_files` (tried in order for directory requests).
    pub fn new(
        doc_base: impl Into<PathBuf>,
        config: DefaultServletConfig,
        welcome_files: Vec<String>,
    ) -> Self {
        Self {
            doc_base: doc_base.into(),
            config,
            welcome_files,
        }
    }

    /// Build a `DefaultServlet` for `ctx`, reading its
    /// [`doc_base`](Context::doc_base) and [`welcome_files`](Context::welcome_files).
    ///
    /// The behaviour [`DefaultServletConfig`] is left at its Tomcat-compatible
    /// default; callers that need listings or write methods should build with
    /// [`DefaultServlet::new`] instead. When the context declared no welcome
    /// files a sensible default list (`index.html`, `index.htm`) is used so a
    /// bare directory request still resolves.
    pub fn for_context(ctx: &Context) -> Self {
        let welcome_files = if ctx.welcome_files().is_empty() {
            vec!["index.html".to_string(), "index.htm".to_string()]
        } else {
            ctx.welcome_files().to_vec()
        };
        Self::new(
            ctx.doc_base().to_path_buf(),
            DefaultServletConfig::default(),
            welcome_files,
        )
    }

    /// The document base this servlet serves from.
    pub fn doc_base(&self) -> &Path {
        &self.doc_base
    }

    /// The effective configuration.
    pub fn config(&self) -> &DefaultServletConfig {
        &self.config
    }

    /// The welcome-file list, in order.
    pub fn welcome_files(&self) -> &[String] {
        &self.welcome_files
    }

    /// Service one request and produce a [`Response`].
    ///
    /// This is the full GET/HEAD static path; see the module docs for the HTTP
    /// semantics implemented. `PUT`/`DELETE` are answered with `501` when
    /// [`DefaultServletConfig::read_only`] is `false`, `405` otherwise; every
    /// other method is `405`.
    pub fn serve(&self, req: &Request) -> Response {
        match req.method.as_str() {
            "GET" | "HEAD" => self.serve_get(req),
            "PUT" | "DELETE" => {
                if self.config.read_only {
                    self.error(405, "Method Not Allowed", &req.path)
                } else {
                    // The write path is scaffolded but not implemented in
                    // v1.0.0: a non-read-only DefaultServlet would create or
                    // remove the resource here. Until that lands, answer with a
                    // clear 501 rather than silently succeeding.
                    let mut resp = self.error(
                        501,
                        "Not Implemented",
                        &format!(
                            "{} is accepted (servlet is not read-only) but the write \
                             path is not implemented in this build",
                            req.method
                        ),
                    );
                    resp.set_header("Allow", "GET, HEAD, PUT, DELETE");
                    resp
                }
            }
            _ => {
                let mut resp = self.error(405, "Method Not Allowed", &req.path);
                let allow = if self.config.read_only {
                    "GET, HEAD"
                } else {
                    "GET, HEAD, PUT, DELETE"
                };
                resp.set_header("Allow", allow);
                resp
            }
        }
    }

    /// The GET/HEAD path. `HEAD` is handled identically to `GET` except the
    /// body is stripped at the very end (after `Content-Length` is computed).
    fn serve_get(&self, req: &Request) -> Response {
        let is_head = req.method == "HEAD";

        // 1. Resolve the request path safely under the document base.
        let resolved = match self.resolve(&req.path) {
            Resolved::Ok(path) => path,
            Resolved::Protected => {
                return self.head_aware(self.error(404, "Not Found", &req.path), is_head)
            }
            Resolved::Traversal => {
                return self.head_aware(self.error(403, "Forbidden", &req.path), is_head)
            }
        };

        // 2. Directory handling: welcome files, then listing or 403/404.
        let metadata = match fs::metadata(&resolved) {
            Ok(md) => md,
            Err(_) => return self.head_aware(self.error(404, "Not Found", &req.path), is_head),
        };

        if metadata.is_dir() {
            return self.serve_directory(req, &resolved, is_head);
        }

        self.serve_file(req, &resolved, &metadata, is_head)
    }

    /// Serve a request that resolved to a directory: try each welcome file,
    /// then fall back to a listing (if enabled) or an error.
    fn serve_directory(&self, req: &Request, dir: &Path, is_head: bool) -> Response {
        for welcome in &self.welcome_files {
            let candidate = dir.join(welcome);
            if let Ok(md) = fs::metadata(&candidate) {
                if md.is_file() {
                    return self.serve_file(req, &candidate, &md, is_head);
                }
            }
        }

        if self.config.listings {
            match self.render_listing(&req.path, dir) {
                Ok(html) => {
                    let body = Bytes::from(html);
                    let mut resp = Response::with_body(200, body.clone());
                    self.finalize(&mut resp, "text/html; charset=utf-8", body.len());
                    self.head_aware(resp, is_head)
                }
                Err(_) => self.head_aware(self.error(404, "Not Found", &req.path), is_head),
            }
        } else {
            // Listings disabled and no welcome file: Tomcat answers 404 here,
            // but a directory that demonstrably exists is more honestly a 403.
            self.head_aware(self.error(403, "Forbidden", &req.path), is_head)
        }
    }

    /// Serve a concrete regular file, applying conditional-GET and range logic.
    fn serve_file(
        &self,
        req: &Request,
        path: &Path,
        metadata: &Metadata,
        is_head: bool,
    ) -> Response {
        let len = metadata.len();
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let etag = weak_etag(len, mtime);
        let content_type = content_type_for(path);

        // --- Conditional request evaluation (RFC 7232 precedence). ---
        // If-Match / If-Unmodified-Since gate the request entirely.
        if let Some(if_match) = req.header("If-Match") {
            if !etag_matches(if_match, &etag, false) {
                return self.head_aware(self.precondition_failed(&req.path, &etag, mtime), is_head);
            }
        } else if let Some(ius) = req.header("If-Unmodified-Since") {
            if let Some(since) = parse_http_date(ius) {
                if mtime > since {
                    return self
                        .head_aware(self.precondition_failed(&req.path, &etag, mtime), is_head);
                }
            }
        }

        // If-None-Match / If-Modified-Since can short-circuit to 304.
        let not_modified = if let Some(inm) = req.header("If-None-Match") {
            etag_matches(inm, &etag, true)
        } else if let Some(ims) = req.header("If-Modified-Since") {
            parse_http_date(ims)
                .map(|since| mtime <= since)
                .unwrap_or(false)
        } else {
            false
        };
        if not_modified {
            let mut resp = Response::new(304);
            resp.set_header("ETag", &etag)
                .set_header("Last-Modified", &http_date(mtime))
                .set_header("Accept-Ranges", "bytes");
            self.stamp(&mut resp);
            // 304 carries no body regardless of method.
            resp.body = Bytes::new();
            return resp;
        }

        // --- Read the file. ---
        let data = match fs::read(path) {
            Ok(d) => d,
            Err(_) => return self.head_aware(self.error(404, "Not Found", &req.path), is_head),
        };

        // --- Range handling. ---
        // A Range header is only honoured when If-Range (if present) still
        // matches; we keep it simple and always honour Range here since
        // If-Range support is out of scope for v1.0.0.
        if let Some(range_header) = req.header("Range") {
            match parse_range(range_header, len) {
                RangeParse::None => { /* not a bytes range — ignore, serve full */ }
                RangeParse::Unsatisfiable => {
                    let mut resp = self.error(416, "Range Not Satisfiable", &req.path);
                    resp.set_header("Content-Range", &format!("bytes */{len}"))
                        .set_header("Accept-Ranges", "bytes");
                    return self.head_aware(resp, is_head);
                }
                RangeParse::Single(start, end) => {
                    let slice = data[start as usize..=end as usize].to_vec();
                    let body = Bytes::from(slice);
                    let mut resp = Response::with_body(206, body.clone());
                    resp.set_header("Content-Range", &format!("bytes {start}-{end}/{len}"));
                    self.finalize(&mut resp, content_type, body.len());
                    resp.set_header("ETag", &etag)
                        .set_header("Last-Modified", &http_date(mtime))
                        .set_header("Accept-Ranges", "bytes");
                    self.maybe_sendfile(&mut resp, path, len);
                    return self.head_aware(resp, is_head);
                }
                RangeParse::Multiple(ranges) => {
                    let boundary = multipart_boundary(&etag, mtime);
                    let body = build_multipart(&data, &ranges, content_type, len, &boundary);
                    let body = Bytes::from(body);
                    let mut resp = Response::with_body(206, body.clone());
                    let ct = format!("multipart/byteranges; boundary={boundary}");
                    self.finalize(&mut resp, &ct, body.len());
                    resp.set_header("ETag", &etag)
                        .set_header("Last-Modified", &http_date(mtime))
                        .set_header("Accept-Ranges", "bytes");
                    self.maybe_sendfile(&mut resp, path, len);
                    return self.head_aware(resp, is_head);
                }
            }
        }

        // --- Full body, 200 OK. ---
        let body = Bytes::from(data);
        let mut resp = Response::with_body(200, body.clone());
        self.finalize(&mut resp, content_type, body.len());
        resp.set_header("ETag", &etag)
            .set_header("Last-Modified", &http_date(mtime))
            .set_header("Accept-Ranges", "bytes");
        self.maybe_sendfile(&mut resp, path, len);
        self.head_aware(resp, is_head)
    }

    /// Resolve `request_path` to an absolute path under [`Self::doc_base`].
    ///
    /// The check is purely lexical (no `canonicalize`) so a missing file still
    /// yields a clean `404` rather than an I/O error, and symlink races cannot
    /// influence the traversal verdict. `/WEB-INF` and `/META-INF` (and anything
    /// beneath them) are rejected as protected.
    fn resolve(&self, request_path: &str) -> Resolved {
        let rel = request_path.trim_start_matches('/');
        let candidate = Path::new(rel);

        let mut resolved = self.doc_base.clone();
        let mut first: Option<String> = None;
        for component in candidate.components() {
            match component {
                Component::Normal(part) => {
                    if first.is_none() {
                        first = Some(part.to_string_lossy().to_ascii_uppercase());
                    }
                    resolved.push(part);
                }
                Component::CurDir | Component::RootDir => {}
                Component::ParentDir | Component::Prefix(_) => return Resolved::Traversal,
            }
        }

        // Reject the protected metadata trees, case-insensitively, by their
        // first path segment.
        if let Some(seg) = first.as_deref() {
            if seg == "WEB-INF" || seg == "META-INF" {
                return Resolved::Protected;
            }
        }

        if resolved.starts_with(&self.doc_base) {
            Resolved::Ok(resolved)
        } else {
            Resolved::Traversal
        }
    }

    /// Render an HTML directory listing for `dir` (mounted at `url_path`).
    fn render_listing(&self, url_path: &str, dir: &Path) -> std::io::Result<String> {
        let mut entries: Vec<(String, bool, u64)> = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let md = entry.metadata()?;
            entries.push((name, md.is_dir(), md.len()));
        }
        // Directories first, then files; each group alphabetical.
        entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        let mut html = String::new();
        let _ = write!(
            html,
            "<!DOCTYPE html>\n<html lang=\"en\">\n<head><meta charset=\"utf-8\">\
             <title>Directory listing for {url_path}</title></head>\n\
             <body style=\"font-family: system-ui, sans-serif;\">\n\
             <h1>Directory listing for {url_path}</h1>\n<hr>\n<ul>\n"
        );
        let base = url_path.trim_end_matches('/');
        for (name, is_dir, size) in entries {
            let suffix = if is_dir { "/" } else { "" };
            if is_dir {
                let _ = writeln!(html, "<li><a href=\"{base}/{name}/\">{name}/</a></li>");
            } else {
                let _ = writeln!(
                    html,
                    "<li><a href=\"{base}/{name}{suffix}\">{name}</a> &mdash; {size} bytes</li>"
                );
            }
        }
        let _ = write!(
            html,
            "</ul>\n<hr>\n<h3>Tomcat-RS Compatibility Runtime v{}</h3>\n</body>\n</html>\n",
            tomcatrs_core::VERSION
        );
        Ok(html)
    }

    /// Build a `412 Precondition Failed` response.
    fn precondition_failed(&self, path: &str, etag: &str, mtime: u64) -> Response {
        let mut resp = self.error(412, "Precondition Failed", path);
        resp.set_header("ETag", etag)
            .set_header("Last-Modified", &http_date(mtime))
            .set_header("Accept-Ranges", "bytes");
        resp
    }

    /// Attach the `X-Tomcatrs-Sendfile` hint when `len` is at or above the
    /// configured threshold. The body is left intact; the marker is advisory.
    fn maybe_sendfile(&self, resp: &mut Response, path: &Path, len: u64) {
        let threshold = self.config.sendfile_threshold;
        if threshold > 0 && len as usize >= threshold {
            resp.set_header("X-Tomcatrs-Sendfile", &path.to_string_lossy());
        }
    }

    /// Build a Tomcat-style error-report response (HTML body, status headers).
    fn error(&self, status: u16, reason: &str, detail: &str) -> Response {
        let body = error_report(status, reason, detail);
        let body = Bytes::from(body);
        let mut resp = Response::with_body(status, body.clone());
        self.finalize(&mut resp, "text/html; charset=utf-8", body.len());
        resp
    }

    /// Apply `Content-Type` + `Content-Length` and the standard identity
    /// headers (`Date`, `Server`).
    fn finalize(&self, resp: &mut Response, content_type: &str, content_length: usize) {
        resp.set_header("Content-Type", content_type)
            .set_header("Content-Length", &content_length.to_string());
        self.stamp(resp);
    }

    /// Apply the `Date` and `Server` headers carried by every response.
    fn stamp(&self, resp: &mut Response) {
        let server = format!("Tomcat-RS/{}", tomcatrs_core::VERSION);
        resp.set_header("Server", &server)
            .set_header("Date", &http_date_now());
    }

    /// Strip the body for `HEAD` requests while leaving headers (including the
    /// already-computed `Content-Length`) intact.
    fn head_aware(&self, mut resp: Response, is_head: bool) -> Response {
        if is_head {
            resp.body = Bytes::new();
        }
        resp
    }
}

/// Outcome of resolving a request path under the document base.
enum Resolved {
    /// A safe path inside the document base (the file may or may not exist).
    Ok(PathBuf),
    /// The path tried to escape the document base (`..` / prefix component).
    Traversal,
    /// The path targets a protected tree (`/WEB-INF`, `/META-INF`).
    Protected,
}

/// Outcome of parsing a `Range` header against a known resource length.
enum RangeParse {
    /// Not a `bytes=` range — caller should serve the full entity.
    None,
    /// A single satisfiable range, as inclusive `(start, end)` byte offsets.
    Single(u64, u64),
    /// Multiple satisfiable ranges, each inclusive `(start, end)`.
    Multiple(Vec<(u64, u64)>),
    /// The header was a `bytes=` range but no part of it overlaps the entity.
    Unsatisfiable,
}

/// Parse an HTTP `Range` header value against an entity of `len` bytes.
///
/// Supports the `bytes=` unit with `start-end`, `start-`, and `-suffix` forms,
/// comma-separated. Any individually unsatisfiable range is dropped; if *every*
/// range is unsatisfiable the result is [`RangeParse::Unsatisfiable`].
fn parse_range(header: &str, len: u64) -> RangeParse {
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        return RangeParse::None;
    };
    if len == 0 {
        return RangeParse::Unsatisfiable;
    }

    let mut ranges: Vec<(u64, u64)> = Vec::new();
    let mut saw_any = false;
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        saw_any = true;
        let Some((start_s, end_s)) = part.split_once('-') else {
            return RangeParse::None;
        };
        let (start, end) = match (start_s.trim(), end_s.trim()) {
            ("", "") => return RangeParse::None,
            // Suffix range: last `n` bytes.
            ("", suffix) => {
                let Ok(n) = suffix.parse::<u64>() else {
                    return RangeParse::None;
                };
                if n == 0 {
                    continue; // unsatisfiable individually
                }
                let n = n.min(len);
                (len - n, len - 1)
            }
            // Open-ended range: `start-`.
            (start, "") => {
                let Ok(s) = start.parse::<u64>() else {
                    return RangeParse::None;
                };
                if s >= len {
                    continue;
                }
                (s, len - 1)
            }
            // Closed range: `start-end`.
            (start, end) => {
                let (Ok(s), Ok(e)) = (start.parse::<u64>(), end.parse::<u64>()) else {
                    return RangeParse::None;
                };
                if s > e || s >= len {
                    continue;
                }
                (s, e.min(len - 1))
            }
        };
        ranges.push((start, end));
    }

    if !saw_any {
        return RangeParse::None;
    }
    match ranges.len() {
        0 => RangeParse::Unsatisfiable,
        1 => RangeParse::Single(ranges[0].0, ranges[0].1),
        _ => RangeParse::Multiple(ranges),
    }
}

/// Build the body of a `multipart/byteranges` response.
fn build_multipart(
    data: &[u8],
    ranges: &[(u64, u64)],
    content_type: &str,
    total: u64,
    boundary: &str,
) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    for (start, end) in ranges {
        out.extend_from_slice(b"\r\n--");
        out.extend_from_slice(boundary.as_bytes());
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
        out.extend_from_slice(
            format!("Content-Range: bytes {start}-{end}/{total}\r\n\r\n").as_bytes(),
        );
        out.extend_from_slice(&data[*start as usize..=*end as usize]);
    }
    out.extend_from_slice(b"\r\n--");
    out.extend_from_slice(boundary.as_bytes());
    out.extend_from_slice(b"--\r\n");
    out
}

/// Build a stable multipart boundary token from the resource identity.
fn multipart_boundary(etag: &str, mtime: u64) -> String {
    let digest: String = etag.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    format!("tomcatrs_byteranges_{mtime:x}_{digest}")
}

/// Compute a weak `ETag` from a file's size and mtime, in Tomcat's
/// `W/"<size>-<mtime>"` shape.
fn weak_etag(size: u64, mtime_secs: u64) -> String {
    format!("W/\"{size:x}-{mtime_secs:x}\"")
}

/// Test an `If-Match` / `If-None-Match` header value against `etag`.
///
/// `weak_ok` selects weak comparison (used for `If-None-Match`): when `false`
/// (strong comparison, used for `If-Match`) a weak-tagged candidate never
/// matches. `*` matches any current representation.
fn etag_matches(header: &str, etag: &str, weak_ok: bool) -> bool {
    let header = header.trim();
    if header == "*" {
        return true;
    }
    // Strip the optional weak prefix from our own tag for comparison.
    let our_opaque = etag.strip_prefix("W/").unwrap_or(etag);
    for candidate in header.split(',') {
        let candidate = candidate.trim();
        let cand_is_weak = candidate.starts_with("W/");
        let cand_opaque = candidate.strip_prefix("W/").unwrap_or(candidate);
        if cand_opaque == our_opaque && (weak_ok || !cand_is_weak) {
            return true;
        }
    }
    false
}

/// Guess a `Content-Type` from a file-name extension.
///
/// A reasonably complete map covering the common web asset, image, font and
/// document types. Anything unrecognised falls back to
/// `application/octet-stream`, the safe default for an unknown binary.
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
        Some("map") => "application/json",
        Some("xml") => "application/xml; charset=utf-8",
        Some("txt") | Some("text") => "text/plain; charset=utf-8",
        Some("md") => "text/markdown; charset=utf-8",
        Some("csv") => "text/csv; charset=utf-8",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("bmp") => "image/bmp",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("eot") => "application/vnd.ms-fontobject",
        Some("pdf") => "application/pdf",
        Some("wasm") => "application/wasm",
        Some("zip") => "application/zip",
        Some("gz") => "application/gzip",
        Some("tar") => "application/x-tar",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mp3") => "audio/mpeg",
        Some("ogg") => "audio/ogg",
        Some("wav") => "audio/wav",
        _ => "application/octet-stream",
    }
}

/// Render a Tomcat-style error-report HTML page.
fn error_report(status: u16, reason: &str, detail: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n\
         <head><meta charset=\"utf-8\"><title>{status} {reason}</title></head>\n\
         <body style=\"font-family: system-ui, sans-serif;\">\n\
         <h1>HTTP Status {status} &mdash; {reason}</h1>\n<hr>\n\
         <p><strong>Detail:</strong> {detail}</p>\n<hr>\n\
         <h3>Tomcat-RS Compatibility Runtime v{version}</h3>\n\
         </body>\n</html>\n",
        version = tomcatrs_core::VERSION,
    )
}

/// Format a Unix-epoch second count as an RFC 7231 IMF-fixdate string.
///
/// Hand-rolled to avoid a date-crate dependency; correct for all timestamps at
/// or after the Unix epoch. Mirrors the helper in [`crate::adapter`] — kept
/// local here so `default_servlet.rs` is self-contained.
fn http_date(secs: u64) -> String {
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

/// The current time as an RFC 7231 IMF-fixdate string.
fn http_date_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    http_date(secs)
}

/// Parse an RFC 7231 IMF-fixdate (and the two obsolete formats Tomcat also
/// accepts, leniently) into a Unix-epoch second count.
///
/// Only the IMF-fixdate form (`Sun, 06 Nov 1994 08:49:37 GMT`) is parsed
/// precisely; the parser is deliberately small and rejects anything it does not
/// fully understand by returning `None`, which makes the conditional-GET checks
/// degrade safely (the request is treated as unconditional).
fn parse_http_date(value: &str) -> Option<u64> {
    // IMF-fixdate: "Sun, 06 Nov 1994 08:49:37 GMT"
    let v = value.trim();
    let v = v.split_once(", ").map(|(_, rest)| rest).unwrap_or(v);
    let mut parts = v.split_whitespace();
    let day: i64 = parts.next()?.parse().ok()?;
    let month = month_index(parts.next()?)?;
    let year: i64 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    let mut hms = time.split(':');
    let hour: i64 = hms.next()?.parse().ok()?;
    let minute: i64 = hms.next()?.parse().ok()?;
    let second: i64 = hms.next()?.parse().ok()?;

    // days_from_civil (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    let total = days * 86_400 + hour * 3600 + minute * 60 + second;
    if total < 0 {
        None
    } else {
        Some(total as u64)
    }
}

/// Map a three-letter English month abbreviation to its 1-based index.
fn month_index(name: &str) -> Option<i64> {
    Some(match name {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    /// A unique temp directory for one test.
    fn unique_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tomcatrs-default-servlet-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build a `Request` for `method` + `path` with the given headers.
    fn request(method: &str, path: &str, headers: &[(&str, &str)]) -> Request {
        let peer: SocketAddr = "127.0.0.1:0".parse().unwrap();
        Request {
            method: method.to_string(),
            uri: path.to_string(),
            path: path.to_string(),
            query: None,
            version: "HTTP/1.1".to_string(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: Bytes::new(),
            peer_addr: peer,
        }
    }

    /// A servlet with default config over a fresh temp doc base.
    fn servlet(dir: &Path) -> DefaultServlet {
        DefaultServlet::new(
            dir.to_path_buf(),
            DefaultServletConfig::default(),
            vec!["index.html".to_string()],
        )
    }

    #[test]
    fn serves_a_real_file_with_type_and_length() {
        let dir = unique_dir("serve");
        fs::write(dir.join("hello.txt"), b"hello world").unwrap();
        let ds = servlet(&dir);

        let resp = ds.serve(&request("GET", "/hello.txt", &[]));
        assert_eq!(resp.status, 200);
        assert_eq!(&resp.body[..], b"hello world");
        assert_eq!(
            resp.header("Content-Type"),
            Some("text/plain; charset=utf-8")
        );
        assert_eq!(resp.header("Content-Length"), Some("11"));
        assert_eq!(resp.header("Accept-Ranges"), Some("bytes"));
        assert!(resp.header("ETag").is_some());
        assert!(resp.header("Last-Modified").is_some());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_is_404() {
        let dir = unique_dir("missing");
        let ds = servlet(&dir);
        let resp = ds.serve(&request("GET", "/nope.txt", &[]));
        assert_eq!(resp.status, 404);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn directory_with_welcome_file_is_served() {
        let dir = unique_dir("welcome");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/index.html"), b"<h1>welcome</h1>").unwrap();
        let ds = servlet(&dir);

        let resp = ds.serve(&request("GET", "/sub/", &[]));
        assert_eq!(resp.status, 200);
        assert_eq!(&resp.body[..], b"<h1>welcome</h1>");
        assert_eq!(
            resp.header("Content-Type"),
            Some("text/html; charset=utf-8")
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn directory_listing_when_enabled() {
        let dir = unique_dir("listing");
        fs::write(dir.join("a.txt"), b"a").unwrap();
        fs::write(dir.join("b.txt"), b"b").unwrap();
        let ds = DefaultServlet::new(
            dir.clone(),
            DefaultServletConfig {
                listings: true,
                ..DefaultServletConfig::default()
            },
            // No welcome file present, so a listing is rendered.
            vec!["index.html".to_string()],
        );

        let resp = ds.serve(&request("GET", "/", &[]));
        assert_eq!(resp.status, 200);
        let body = String::from_utf8(resp.body.to_vec()).unwrap();
        assert!(body.contains("a.txt"));
        assert!(body.contains("b.txt"));
        assert!(body.contains("Directory listing"));

        // With listings off the same request is a 403.
        let ds_off = servlet(&dir);
        let resp = ds_off.serve(&request("GET", "/", &[]));
        assert_eq!(resp.status, 403);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn traversal_is_rejected() {
        let dir = unique_dir("traversal");
        let ds = servlet(&dir);
        let resp = ds.serve(&request("GET", "/../../etc/passwd", &[]));
        assert_eq!(resp.status, 403);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn web_inf_is_rejected() {
        let dir = unique_dir("web-inf");
        fs::create_dir_all(dir.join("WEB-INF")).unwrap();
        fs::write(dir.join("WEB-INF/web.xml"), b"<web-app/>").unwrap();
        let ds = servlet(&dir);

        let resp = ds.serve(&request("GET", "/WEB-INF/web.xml", &[]));
        assert_eq!(resp.status, 404);
        // Case-insensitively too.
        let resp = ds.serve(&request("GET", "/web-inf/web.xml", &[]));
        assert_eq!(resp.status, 404);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn if_modified_since_yields_304() {
        let dir = unique_dir("ims");
        let path = dir.join("cached.txt");
        fs::write(&path, b"cacheable").unwrap();
        let ds = servlet(&dir);

        // First fetch to learn the Last-Modified value.
        let first = ds.serve(&request("GET", "/cached.txt", &[]));
        let last_mod = first.header("Last-Modified").unwrap().to_string();

        let resp = ds.serve(&request(
            "GET",
            "/cached.txt",
            &[("If-Modified-Since", &last_mod)],
        ));
        assert_eq!(resp.status, 304);
        assert!(resp.body.is_empty());
        assert!(resp.header("ETag").is_some());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn if_none_match_yields_304() {
        let dir = unique_dir("inm");
        fs::write(dir.join("etagged.txt"), b"body").unwrap();
        let ds = servlet(&dir);

        let first = ds.serve(&request("GET", "/etagged.txt", &[]));
        let etag = first.header("ETag").unwrap().to_string();

        let resp = ds.serve(&request("GET", "/etagged.txt", &[("If-None-Match", &etag)]));
        assert_eq!(resp.status, 304);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn single_range_yields_206_with_content_range() {
        let dir = unique_dir("range");
        fs::write(dir.join("data.bin"), b"0123456789").unwrap();
        let ds = servlet(&dir);

        let resp = ds.serve(&request("GET", "/data.bin", &[("Range", "bytes=2-5")]));
        assert_eq!(resp.status, 206);
        assert_eq!(&resp.body[..], b"2345");
        assert_eq!(resp.header("Content-Range"), Some("bytes 2-5/10"));
        assert_eq!(resp.header("Content-Length"), Some("4"));
        assert_eq!(resp.header("Accept-Ranges"), Some("bytes"));

        // Suffix range: last 3 bytes.
        let resp = ds.serve(&request("GET", "/data.bin", &[("Range", "bytes=-3")]));
        assert_eq!(resp.status, 206);
        assert_eq!(&resp.body[..], b"789");
        assert_eq!(resp.header("Content-Range"), Some("bytes 7-9/10"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn multiple_ranges_yield_multipart() {
        let dir = unique_dir("multirange");
        fs::write(dir.join("data.bin"), b"0123456789").unwrap();
        let ds = servlet(&dir);

        let resp = ds.serve(&request("GET", "/data.bin", &[("Range", "bytes=0-1,8-9")]));
        assert_eq!(resp.status, 206);
        let ct = resp.header("Content-Type").unwrap();
        assert!(ct.starts_with("multipart/byteranges; boundary="));
        let body = String::from_utf8(resp.body.to_vec()).unwrap();
        assert!(body.contains("Content-Range: bytes 0-1/10"));
        assert!(body.contains("Content-Range: bytes 8-9/10"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unsatisfiable_range_yields_416() {
        let dir = unique_dir("unsat");
        fs::write(dir.join("data.bin"), b"0123456789").unwrap();
        let ds = servlet(&dir);

        let resp = ds.serve(&request("GET", "/data.bin", &[("Range", "bytes=50-60")]));
        assert_eq!(resp.status, 416);
        assert_eq!(resp.header("Content-Range"), Some("bytes */10"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn head_has_no_body_but_keeps_headers() {
        let dir = unique_dir("head");
        fs::write(dir.join("page.html"), b"<h1>hi</h1>").unwrap();
        let ds = servlet(&dir);

        let resp = ds.serve(&request("HEAD", "/page.html", &[]));
        assert_eq!(resp.status, 200);
        assert!(resp.body.is_empty());
        // Content-Length still reflects the real entity size.
        assert_eq!(resp.header("Content-Length"), Some("11"));
        assert_eq!(
            resp.header("Content-Type"),
            Some("text/html; charset=utf-8")
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn non_get_on_read_only_is_405() {
        let dir = unique_dir("readonly");
        let ds = servlet(&dir);

        let resp = ds.serve(&request("PUT", "/x.txt", &[]));
        assert_eq!(resp.status, 405);
        let resp = ds.serve(&request("DELETE", "/x.txt", &[]));
        assert_eq!(resp.status, 405);
        let resp = ds.serve(&request("POST", "/x.txt", &[]));
        assert_eq!(resp.status, 405);
    }

    #[test]
    fn write_methods_are_501_when_not_read_only() {
        let dir = unique_dir("writable");
        let ds = DefaultServlet::new(
            dir.clone(),
            DefaultServletConfig {
                read_only: false,
                ..DefaultServletConfig::default()
            },
            vec![],
        );
        let resp = ds.serve(&request("PUT", "/x.txt", &[]));
        assert_eq!(resp.status, 501);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sendfile_hint_for_large_files() {
        let dir = unique_dir("sendfile");
        let big = vec![b'x'; 100 * 1024];
        fs::write(dir.join("big.bin"), &big).unwrap();
        let ds = servlet(&dir); // 48 KiB threshold

        let resp = ds.serve(&request("GET", "/big.bin", &[]));
        assert_eq!(resp.status, 200);
        assert!(resp.header("X-Tomcatrs-Sendfile").is_some());
        // Body is still produced correctly in v1.0.0.
        assert_eq!(resp.body.len(), 100 * 1024);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn for_context_reads_doc_base_and_welcome_files() {
        let dir = unique_dir("for-context");
        let ctx = Context::new("/app", dir.clone(), false, vec![]);
        let ds = DefaultServlet::for_context(&ctx);
        assert_eq!(ds.doc_base(), dir.as_path());
        // No welcome files declared -> sensible default list.
        assert_eq!(ds.welcome_files(), ["index.html", "index.htm"]);
        fs::remove_dir_all(&dir).ok();
    }
}
