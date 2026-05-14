//! `tomcatrs-coyote` — the Rust reimplementation of Apache Tomcat's **Coyote**
//! connector layer.
//!
//! Coyote is the protocol-facing edge of the runtime: it owns the listening
//! sockets, parses the wire protocol into a normalized [`Request`], hands that
//! request to an [`Adapter`] (the bridge into the servlet container), and
//! serializes the resulting [`Response`] back onto the socket.
//!
//! # Module map
//!
//! | Module        | Responsibility                                                   |
//! |---------------|------------------------------------------------------------------|
//! | [`acceptor`]  | `TcpListener` accept loop, one tokio task per connection.        |
//! | [`protocol`]  | Dispatch a freshly accepted connection to the right protocol.    |
//! | [`http1`]     | A real, working HTTP/1.1 parser and writer.                      |
//! | [`chunked`]   | HTTP/1.1 chunked transfer-encoding decoding and encoding.        |
//! | [`http2`]     | HTTP/2 scaffold (not implemented in v0.1.0).                     |
//! | [`ajp`]       | AJP scaffold (not implemented in v0.1.0).                        |
//! | [`tls`]       | TLS termination scaffold (intended `rustls` integration).        |
//! | [`upgrade`]   | `Connection: Upgrade` / WebSocket handoff scaffold.              |
//! | [`normalize`] | Real URI normalization: percent-decode, dot-segment collapsing.  |
//!
//! # Example
//!
//! ```no_run
//! use std::sync::Arc;
//! use tomcatrs_coyote::{Adapter, HttpConnector, Request, Response};
//!
//! struct Hello;
//!
//! #[async_trait::async_trait]
//! impl Adapter for Hello {
//!     async fn service(&self, _req: Request) -> Response {
//!         Response::with_body(200, "hello from coyote")
//!     }
//! }
//!
//! # async fn run(cfg: tomcatrs_config::ConnectorConfig) -> tomcatrs_core::Result<()> {
//! let connector = HttpConnector::new(cfg, Arc::new(Hello));
//! connector.serve().await
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod acceptor;
pub mod ajp;
pub mod chunked;
pub mod http1;
pub mod http2;
pub mod normalize;
pub mod protocol;
pub mod tls;
pub mod upgrade;

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;

use crate::acceptor::Acceptor;

/// A fully parsed, normalized inbound request.
///
/// This is the protocol-agnostic shape every connector produces. Whether the
/// bytes arrived over HTTP/1.1, HTTP/2, or AJP, the [`Adapter`] only ever sees
/// a `Request`.
#[derive(Debug, Clone)]
pub struct Request {
    /// The request method, upper-cased as received (e.g. `GET`, `POST`).
    pub method: String,
    /// The raw request target exactly as it appeared on the request line
    /// (e.g. `/app/x%2Fy?q=1`). Useful for logging and diagnostics.
    pub uri: String,
    /// The normalized, percent-decoded path with dot-segments collapsed
    /// (e.g. `/app/file`). Guaranteed not to escape the document root.
    pub path: String,
    /// The query string, if any, without the leading `?`.
    pub query: Option<String>,
    /// The HTTP version token from the request line (e.g. `HTTP/1.1`).
    pub version: String,
    /// Request headers, in arrival order, with original casing preserved.
    pub headers: Vec<(String, String)>,
    /// The request body. Empty when there is no entity.
    pub body: Bytes,
    /// The remote peer's socket address.
    pub peer_addr: SocketAddr,
}

impl Request {
    /// Look up a header value by name, case-insensitively.
    ///
    /// Returns the first matching header's value, or `None` if absent.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// An outbound response produced by an [`Adapter`].
#[derive(Debug, Clone)]
pub struct Response {
    /// The HTTP status code.
    pub status: u16,
    /// Response headers, in insertion order.
    pub headers: Vec<(String, String)>,
    /// The response body. May be empty.
    pub body: Bytes,
}

impl Response {
    /// Create an empty response with the given status code.
    pub fn new(status: u16) -> Self {
        Response {
            status,
            headers: Vec::new(),
            body: Bytes::new(),
        }
    }

    /// Create a response with the given status code and body.
    pub fn with_body(status: u16, body: impl Into<Bytes>) -> Self {
        Response {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    /// Set (replacing any existing occurrence of) a header, returning `&mut Self`
    /// so calls can be chained.
    pub fn set_header(&mut self, name: &str, value: &str) -> &mut Self {
        if let Some(slot) = self
            .headers
            .iter_mut()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
        {
            slot.1 = value.to_string();
        } else {
            self.headers.push((name.to_string(), value.to_string()));
        }
        self
    }

    /// Look up a header value by name, case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// The handoff point between Coyote and the servlet container.
///
/// In Tomcat this is `org.apache.coyote.Adapter`; an implementation routes the
/// request through the `Engine` → `Host` → `Context` → `Wrapper` pipeline. The
/// connector layer treats it as an opaque async function.
#[async_trait::async_trait]
pub trait Adapter: Send + Sync {
    /// Service a single request and produce a response.
    async fn service(&self, req: Request) -> Response;
}

/// An HTTP connector: binds a TCP socket and serves requests through an
/// [`Adapter`].
///
/// Construct with [`HttpConnector::new`], then drive it with
/// [`HttpConnector::serve`]. The connector is bound lazily inside `serve` so
/// that construction is infallible; call [`HttpConnector::local_port`] after a
/// successful bind to discover the actual port (useful when configured with
/// port `0`).
pub struct HttpConnector {
    cfg: tomcatrs_config::ConnectorConfig,
    adapter: Arc<dyn Adapter>,
    /// The actually-bound port. Equals `cfg.port` until a port-0 bind resolves
    /// it; updated in-place by `serve` once the listener exists.
    local_port: std::sync::atomic::AtomicU16,
}

impl HttpConnector {
    /// Create a new connector from a [`tomcatrs_config::ConnectorConfig`] and an
    /// [`Adapter`]. This does not touch the network.
    pub fn new(cfg: tomcatrs_config::ConnectorConfig, adapter: Arc<dyn Adapter>) -> Self {
        let port = cfg.port;
        HttpConnector {
            cfg,
            adapter,
            local_port: std::sync::atomic::AtomicU16::new(port),
        }
    }

    /// Bind the configured `address:port` and run the accept loop until the
    /// listener errors.
    ///
    /// # Errors
    ///
    /// Returns [`tomcatrs_core::Error::Protocol`] if the configuration requests
    /// a protocol or TLS mode that is not yet supported in v0.1.0, or
    /// [`tomcatrs_core::Error::Io`] if the socket cannot be bound.
    pub async fn serve(self) -> tomcatrs_core::Result<()> {
        let acceptor = Acceptor::bind(&self.cfg, self.adapter.clone()).await?;
        self.local_port.store(
            acceptor.local_addr().port(),
            std::sync::atomic::Ordering::SeqCst,
        );
        acceptor.run().await
    }

    /// The port the connector is (or will be) bound on.
    ///
    /// Before [`serve`](Self::serve) has bound the socket this returns the
    /// configured port, which may be `0`. After a successful bind it returns the
    /// concrete port assigned by the OS.
    pub fn local_port(&self) -> u16 {
        self.local_port.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_header_lookup_is_case_insensitive() {
        let req = Request {
            method: "GET".into(),
            uri: "/".into(),
            path: "/".into(),
            query: None,
            version: "HTTP/1.1".into(),
            headers: vec![("Content-Type".into(), "text/plain".into())],
            body: Bytes::new(),
            peer_addr: "127.0.0.1:0".parse().unwrap(),
        };
        assert_eq!(req.header("content-type"), Some("text/plain"));
        assert_eq!(req.header("CONTENT-TYPE"), Some("text/plain"));
        assert_eq!(req.header("missing"), None);
    }

    #[test]
    fn response_builders_and_set_header() {
        let mut resp = Response::with_body(201, "created");
        resp.set_header("Location", "/new")
            .set_header("X-Trace", "abc");
        assert_eq!(resp.status, 201);
        assert_eq!(&resp.body[..], b"created");
        assert_eq!(resp.header("location"), Some("/new"));

        // set_header replaces rather than appends.
        resp.set_header("location", "/moved");
        assert_eq!(resp.header("Location"), Some("/moved"));
        assert_eq!(
            resp.headers
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case("location"))
                .count(),
            1
        );
    }

    #[test]
    fn response_new_is_empty() {
        let resp = Response::new(204);
        assert_eq!(resp.status, 204);
        assert!(resp.body.is_empty());
        assert!(resp.headers.is_empty());
    }

    /// A trivial adapter that echoes the request path back in the body.
    struct EchoAdapter;

    #[async_trait::async_trait]
    impl Adapter for EchoAdapter {
        async fn service(&self, req: Request) -> Response {
            let mut resp = Response::with_body(200, format!("path={}", req.path));
            resp.set_header("Content-Type", "text/plain");
            resp
        }
    }

    #[tokio::test]
    async fn connector_serves_a_real_http11_request_over_tcp() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Bind on port 0 so the OS picks a free port.
        let cfg = tomcatrs_config::ConnectorConfig {
            protocol: tomcatrs_config::Protocol::Http11,
            address: Some("127.0.0.1".parse().unwrap()),
            port: 0,
            tls: None,
            limits: tomcatrs_config::RequestLimits::default(),
        };
        let connector = HttpConnector::new(cfg, Arc::new(EchoAdapter));

        // Bind explicitly first so we know the port, then run the accept loop.
        let acceptor = crate::acceptor::Acceptor::bind(&connector.cfg, connector.adapter.clone())
            .await
            .expect("bind should succeed on 127.0.0.1:0");
        let addr = acceptor.local_addr();
        let server = tokio::spawn(async move {
            let _ = acceptor.run().await;
        });

        // Connect and send a raw HTTP/1.1 request.
        let mut client = tokio::net::TcpStream::connect(addr)
            .await
            .expect("client should connect");
        client
            .write_all(b"GET /hello/world HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();

        // Read the whole response (server closes the connection).
        let mut buf = Vec::new();
        client.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8(buf).unwrap();

        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "got: {text:?}");
        assert!(text.contains("Content-Type: text/plain\r\n"));
        assert!(text.contains("Connection: close\r\n"));
        assert!(text.ends_with("path=/hello/world"));

        server.abort();
    }

    #[tokio::test]
    async fn connector_rejects_path_traversal_with_400() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let cfg = tomcatrs_config::ConnectorConfig {
            protocol: tomcatrs_config::Protocol::Http11,
            address: Some("127.0.0.1".parse().unwrap()),
            port: 0,
            tls: None,
            limits: tomcatrs_config::RequestLimits::default(),
        };
        let acceptor = crate::acceptor::Acceptor::bind(&cfg, Arc::new(EchoAdapter))
            .await
            .unwrap();
        let addr = acceptor.local_addr();
        let server = tokio::spawn(async move {
            let _ = acceptor.run().await;
        });

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /../../etc/passwd HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        client.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text:?}");

        server.abort();
    }
}
