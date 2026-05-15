//! `tomcatrs-compat-tests` — a differential / compatibility test harness for the
//! **Tomcat-RS Compatibility Runtime**.
//!
//! This crate is a small framework that drives a live Tomcat-RS HTTP/1.1 stack
//! end-to-end (`Coyote` ➜ `CatalinaAdapter` ➜ `Engine` ➜ `Host` ➜ `Context`)
//! over a real TCP socket, captures the response, and diffs it against a
//! known-good ("golden") expectation.
//!
//! # Methodology
//!
//! The compatibility methodology described in `tests/README.md` is, in full:
//!
//! 1. Deploy an application twice with equivalent configuration: once on a
//!    stock **Apache Tomcat 11.0.x** instance, once on **Tomcat-RS**.
//! 2. Drive the same sequence of requests at both.
//! 3. Diff status code, response headers, response body and access logs.
//! 4. A divergence is either a bug or a documented deliberate difference.
//!
//! This harness is the Rust side of step (2). The "stock Tomcat" side is
//! out-of-process by definition — you cannot link Tomcat into a Rust crate —
//! so this v1.0.0 harness records the Tomcat-RS responses as **goldens** under
//! `golden/` and asserts against them. To run a real differential pass against
//! a live Tomcat:
//!
//! 1. Bring up Apache Tomcat with the same `server.xml` / web application.
//! 2. Implement [`Server`] for an `ExternalTomcat { base_url: String }` type
//!    that just turns [`Server::start`] into an HTTP client pointed at
//!    `base_url`.
//! 3. Run the same [`CompatScenario`] list against both servers, store the
//!    Tomcat output as the golden, and re-run the battery.
//!
//! Until that real Tomcat is available the goldens checked in here pin the
//! Tomcat-RS behaviour and serve as regression fences.
//!
//! # What's in the box
//!
//! * [`CompatScenario`] — name + closure that produces a [`CompatResult`].
//! * [`CompatResult`] — the captured triple of `(status, headers, body)`,
//!   plus free-form `notes` for human-readable expectations.
//! * [`Server`] — the trait every backend (Tomcat-RS, an external Tomcat, a
//!   stub) implements; [`start`](Server::start) binds and returns a
//!   [`Handle`] (base URL + shutdown).
//! * [`start_tomcatrs`] — the canonical way to stand up an in-process
//!   Tomcat-RS for a test: builds a [`tomcatrs_catalina::Engine`] from a
//!   [`TomcatrsConfig`] and serves it through a real
//!   [`tomcatrs_coyote::HttpConnector`].
//! * [`raw_http_request`] — a deliberately tiny HTTP/1.1 client that speaks
//!   plain TCP; it lets the harness drive the connector with byte-exact
//!   request sequences (so we can test chunked bodies, large headers, etc.).
//!
//! # Example
//!
//! ```no_run
//! use tomcatrs_compat_tests::{start_tomcatrs, TomcatrsConfig, raw_http_request};
//!
//! # async fn run() {
//! let handle = start_tomcatrs(TomcatrsConfig::default()).await;
//! let (status, _headers, body) = raw_http_request(
//!     handle.addr(),
//!     b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
//! )
//! .await
//! .unwrap();
//! assert_eq!(status, 200);
//! assert!(String::from_utf8_lossy(&body).contains("Tomcat-RS"));
//! handle.shutdown().await;
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

use tomcatrs_catalina::adapter::CatalinaAdapter;
use tomcatrs_catalina::{Context as CatContext, Engine, Host};
use tomcatrs_config::{ConnectorConfig, Protocol, RequestLimits};
use tomcatrs_coyote::HttpConnector;

pub mod goldens;

/// One scenario in a compatibility battery: a name and a factory that produces
/// a future yielding the observable response triple.
///
/// Scenarios are deliberately not generic in their result type — every
/// backend, including a future "point this at a live Tomcat 11" backend, must
/// produce a [`CompatResult`] so that goldens are directly comparable.
pub struct CompatScenario {
    /// Short, stable identifier — also the golden file's basename.
    pub name: &'static str,
    /// The work that produces the response. Boxed to keep the struct object-safe.
    pub run:
        Box<dyn Fn() -> Pin<Box<dyn Future<Output = CompatResult> + Send>> + Send + Sync + 'static>,
}

impl CompatScenario {
    /// Construct a scenario from a name and an async factory.
    pub fn new<F, Fut>(name: &'static str, run: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = CompatResult> + Send + 'static,
    {
        CompatScenario {
            name,
            run: Box::new(move || Box::pin(run())),
        }
    }
}

/// The observable outcome of one scenario.
#[derive(Debug, Clone)]
pub struct CompatResult {
    /// HTTP status code returned to the client.
    pub status: u16,
    /// Response headers, lower-cased and in arrival order (so two responses
    /// that happen to use different casing still compare equal).
    pub headers: Vec<(String, String)>,
    /// Response body bytes.
    pub body: Bytes,
    /// Free-form notes captured by the scenario — what it tested, why a
    /// particular header was elided, etc. Not compared against goldens.
    pub notes: String,
}

impl CompatResult {
    /// Build a [`CompatResult`] from `(status, headers, body, notes)`,
    /// canonicalizing headers to lower-case names.
    pub fn from_parts(
        status: u16,
        headers: Vec<(String, String)>,
        body: Bytes,
        notes: impl Into<String>,
    ) -> Self {
        let headers = headers
            .into_iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect();
        CompatResult {
            status,
            headers,
            body,
            notes: notes.into(),
        }
    }

    /// Look up the first header value with `name`, case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        let needle = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == needle)
            .map(|(_, v)| v.as_str())
    }
}

/// A running server bound to a real TCP socket.
///
/// Returned by [`Server::start`] / [`start_tomcatrs`]. Drop or
/// [`Handle::shutdown`] tears the listener down; the underlying connector task
/// is aborted either way.
pub struct Handle {
    addr: SocketAddr,
    base_url: String,
    server_task: Option<JoinHandle<()>>,
}

impl Handle {
    /// The concrete socket address the listener is bound to (port is resolved
    /// even when the configuration requested port `0`).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `http://host:port` form of [`Self::addr`] — handy for HTTP clients.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Abort the listener task and wait for it to wind down.
    pub async fn shutdown(mut self) {
        if let Some(task) = self.server_task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if let Some(task) = self.server_task.take() {
            task.abort();
        }
    }
}

/// Common interface every compatibility backend implements.
///
/// In v1.0.0 only [`start_tomcatrs`] is shipped; an external-Tomcat backend
/// would also implement this trait so a generic battery can drive both.
#[async_trait]
pub trait Server: Send + Sync {
    /// Bind a socket and return a [`Handle`] pointing at it.
    async fn start(&self) -> Handle;
}

/// Configuration knobs for [`start_tomcatrs`].
///
/// A minimal `Engine` ➜ `Host` ➜ `Context` tree is built automatically; this
/// struct just lets a test override the small handful of values that matter
/// for the kind of scenarios in this harness.
#[derive(Debug, Clone)]
pub struct TomcatrsConfig {
    /// Document base for the ROOT context. When the directory does not exist
    /// the static handler still serves a friendly landing page for `/`.
    pub doc_base: PathBuf,
    /// Fallback document root for the [`CatalinaAdapter`].
    pub app_base: PathBuf,
    /// Connector request limits. Defaults to [`RequestLimits::default`].
    pub limits: RequestLimits,
}

impl Default for TomcatrsConfig {
    fn default() -> Self {
        // A path that almost certainly does not exist on disk, so requests fall
        // through to the friendly landing page / 404 path rather than reading
        // arbitrary filesystem content.
        let doc_base = std::env::temp_dir().join(format!(
            "tomcatrs-compat-tests-{}-docbase",
            std::process::id()
        ));
        let app_base = doc_base.clone();
        TomcatrsConfig {
            doc_base,
            app_base,
            limits: RequestLimits::default(),
        }
    }
}

/// Stand up a real Tomcat-RS stack (Coyote ➜ Catalina) on `127.0.0.1:0`.
///
/// The returned [`Handle`] owns the listener task; dropping the handle aborts
/// the task, which closes the socket.
pub async fn start_tomcatrs(config: TomcatrsConfig) -> Handle {
    // Minimal container tree: one engine, one host, one ROOT context with no
    // servlets. Static requests fall through to the fallback `app_base`.
    let root_ctx = Arc::new(CatContext::new("", config.doc_base.clone(), false, vec![]));
    let host = Arc::new(Host::new(
        "localhost",
        config.doc_base.clone(),
        vec!["127.0.0.1".to_string()],
        vec![root_ctx],
    ));
    let engine = Engine::new("Catalina", "localhost");
    engine.add_host(host);
    let adapter: Arc<CatalinaAdapter> = Arc::new(CatalinaAdapter::new(
        Arc::new(engine),
        config.app_base.clone(),
    ));

    let connector_cfg = ConnectorConfig {
        protocol: Protocol::Http11,
        address: Some("127.0.0.1".parse().unwrap()),
        port: 0,
        tls: None,
        limits: config.limits.clone(),
    };

    // Bind first so we know the resolved port before returning.
    let acceptor = tomcatrs_coyote::acceptor::Acceptor::bind(&connector_cfg, adapter.clone())
        .await
        .expect("bind on 127.0.0.1:0 should succeed");
    let addr = acceptor.local_addr();

    let server_task = tokio::spawn(async move {
        let _ = acceptor.run().await;
    });

    // Touch `HttpConnector::new` so the public constructor stays exercised by
    // this harness even though we drive the acceptor directly to control bind
    // ordering (so we know the port before returning).
    let _ = HttpConnector::new(connector_cfg, adapter as Arc<_>);

    Handle {
        addr,
        base_url: format!("http://{addr}"),
        server_task: Some(server_task),
    }
}

/// A pre-built [`Server`] implementation backed by [`start_tomcatrs`].
///
/// Useful when a generic battery wants `Box<dyn Server>` rather than a free
/// function.
pub struct TomcatrsServer {
    /// Configuration applied to every [`Server::start`] call.
    pub config: TomcatrsConfig,
}

#[async_trait]
impl Server for TomcatrsServer {
    async fn start(&self) -> Handle {
        start_tomcatrs(self.config.clone()).await
    }
}

/// Send a raw HTTP/1.1 request and return the parsed `(status, headers, body)`.
///
/// The request bytes must include the terminating `CRLFCRLF`. The function
/// reads the peer to EOF (so the request *should* set `Connection: close`),
/// parses the status line, headers, and body. Chunked transfer-encoding on the
/// **response** side is decoded; otherwise `Content-Length` is trusted.
///
/// Header names in the result are lower-cased.
pub async fn raw_http_request(
    addr: SocketAddr,
    request: &[u8],
) -> std::io::Result<(u16, Vec<(String, String)>, Bytes)> {
    let mut stream = TcpStream::connect(addr).await?;
    stream.write_all(request).await?;
    stream.flush().await?;

    let mut buf = Vec::with_capacity(4096);
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await;
    match read {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "raw_http_request: server did not close the connection within 5s",
            ))
        }
    }

    parse_http_response(&buf)
}

/// Parse a complete HTTP/1.1 response buffer into `(status, headers, body)`.
fn parse_http_response(buf: &[u8]) -> std::io::Result<(u16, Vec<(String, String)>, Bytes)> {
    let header_end = find_subslice(buf, b"\r\n\r\n").ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "no CRLFCRLF in response")
    })?;
    let head = std::str::from_utf8(&buf[..header_end])
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let body_bytes = &buf[header_end + 4..];

    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "empty response"))?;

    let mut parts = status_line.splitn(3, ' ');
    let _version = parts.next().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "missing HTTP version")
    })?;
    let code_str = parts.next().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "missing status code")
    })?;
    let status: u16 = code_str.parse().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("non-numeric status: {code_str}"),
        )
    })?;

    let mut headers = Vec::new();
    let mut transfer_encoding_chunked = false;
    let mut content_length: Option<usize> = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked") {
            transfer_encoding_chunked = true;
        }
        if name == "content-length" {
            content_length = value.parse().ok();
        }
        headers.push((name, value));
    }

    let body = if transfer_encoding_chunked {
        decode_chunked(body_bytes)?
    } else if let Some(len) = content_length {
        let len = len.min(body_bytes.len());
        Bytes::copy_from_slice(&body_bytes[..len])
    } else {
        Bytes::copy_from_slice(body_bytes)
    };

    Ok((status, headers, body))
}

/// Decode a chunked transfer-encoded body. Trailers (if any) are discarded.
fn decode_chunked(mut input: &[u8]) -> std::io::Result<Bytes> {
    let mut out = Vec::new();
    loop {
        let line_end = find_subslice(input, b"\r\n").ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "chunked body: missing size CRLF",
            )
        })?;
        let size_line = std::str::from_utf8(&input[..line_end])
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let size_str = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("chunked body: bad size {size_str:?}"),
            )
        })?;
        input = &input[line_end + 2..];
        if size == 0 {
            return Ok(Bytes::from(out));
        }
        if input.len() < size + 2 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "chunked body: truncated chunk",
            ));
        }
        out.extend_from_slice(&input[..size]);
        input = &input[size + 2..];
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compat_result_from_parts_lowercases_headers() {
        let r = CompatResult::from_parts(
            200,
            vec![
                ("Content-Type".to_string(), "text/plain".to_string()),
                ("X-Mixed-Case".to_string(), "v".to_string()),
            ],
            Bytes::from_static(b"ok"),
            "test",
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.header("content-type"), Some("text/plain"));
        assert_eq!(r.header("X-MIXED-CASE"), Some("v"));
        assert_eq!(&r.body[..], b"ok");
        assert_eq!(r.notes, "test");
    }

    #[test]
    fn parse_response_decodes_chunked_body() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let (status, headers, body) = parse_http_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(&body[..], b"hello");
        assert!(headers
            .iter()
            .any(|(k, v)| k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked")));
    }

    #[test]
    fn parse_response_honors_content_length() {
        let raw = b"HTTP/1.1 404 Not Found\r\nContent-Length: 3\r\n\r\nbadEXTRA-GARBAGE-IGNORED";
        let (status, _h, body) = parse_http_response(raw).unwrap();
        assert_eq!(status, 404);
        assert_eq!(&body[..], b"bad");
    }
}
