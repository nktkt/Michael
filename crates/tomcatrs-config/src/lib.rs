//! `tomcatrs-config` — configuration model and parsers for the
//! **Tomcat-RS Compatibility Runtime**.
//!
//! This crate owns the strongly-typed, in-memory representation of the classic
//! Apache Tomcat configuration files and the parsers that build it:
//!
//! * [`ServerConfig`] mirrors `conf/server.xml` — the `Server` → `Service` →
//!   `Engine` → `Host` → `Context` component tree, parsed by [`server_xml`].
//! * [`ContextConfig`] mirrors a per-application `META-INF/context.xml`,
//!   parsed by [`context_xml`].
//! * [`WebXml`] mirrors a web application deployment descriptor
//!   (`WEB-INF/web.xml`), parsed by [`web_xml`].
//! * [`CatalinaProperties`] mirrors `conf/catalina.properties`, parsed by
//!   [`catalina_properties`].
//!
//! All parsers are tolerant: unknown attributes and elements are logged with
//! `tracing::warn!` and skipped rather than turned into hard errors, matching
//! Tomcat's own forgiving behaviour. Genuine syntax errors are surfaced as
//! [`tomcatrs_core::Error::Config`].

#![deny(missing_docs)]

pub mod catalina_properties;
pub mod context_xml;
pub mod server_xml;
pub mod web_xml;

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use catalina_properties::CatalinaProperties;
pub use web_xml::{FilterDef, FilterMapping, ServletDef, ServletMapping, WebXml};

/// Wire protocol spoken by a [`ConnectorConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// HTTP/1.1 — the default Coyote connector protocol.
    Http11,
    /// HTTP/2 (typically negotiated over TLS via ALPN).
    Http2,
    /// Apache JServ Protocol, used behind a fronting `httpd`/`nginx`.
    Ajp,
}

impl Default for Protocol {
    fn default() -> Self {
        Protocol::Http11
    }
}

impl Protocol {
    /// Best-effort parse of a Tomcat `protocol="..."` attribute value.
    ///
    /// Recognises the historical class names (`org.apache.coyote.http11.*`),
    /// the friendly aliases (`HTTP/1.1`, `AJP/1.3`), and HTTP/2 upgrade
    /// protocols. Unrecognised values fall back to [`Protocol::Http11`] and are
    /// logged by the caller.
    fn from_attr(value: &str) -> Option<Protocol> {
        let v = value.to_ascii_lowercase();
        if v.contains("ajp") {
            Some(Protocol::Ajp)
        } else if v.contains("http/2") || v.contains("h2") || v.contains("http2") {
            Some(Protocol::Http2)
        } else if v.contains("http") {
            Some(Protocol::Http11)
        } else {
            None
        }
    }
}

/// TLS material for a secured connector.
#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// Path to the PEM-encoded server certificate (chain).
    pub cert_file: PathBuf,
    /// Path to the PEM-encoded private key.
    pub key_file: PathBuf,
}

/// Request-processing limits enforced by a connector.
///
/// Defaults track Apache Tomcat's out-of-the-box values closely enough to be a
/// safe drop-in starting point.
#[derive(Debug, Clone)]
pub struct RequestLimits {
    /// Maximum number of request headers accepted.
    pub max_header_count: usize,
    /// Maximum size, in bytes, of the request header block.
    pub max_header_size: usize,
    /// Maximum number of request parameters (query + form) parsed.
    pub max_parameter_count: usize,
    /// Maximum size, in bytes, of a buffered `POST` body.
    pub max_post_size: usize,
    /// Maximum number of parts in a `multipart/form-data` request.
    pub max_part_count: usize,
    /// Maximum length, in bytes, of the request URI.
    pub max_uri_len: usize,
    /// Maximum time to wait for a complete request to arrive.
    pub request_timeout: Duration,
    /// Idle timeout for a kept-alive connection between requests.
    pub keep_alive_timeout: Duration,
}

impl Default for RequestLimits {
    fn default() -> Self {
        RequestLimits {
            max_header_count: 100,
            max_header_size: 8192,
            max_parameter_count: 10_000,
            max_post_size: 2 * 1024 * 1024,
            max_part_count: 10,
            max_uri_len: 8192,
            request_timeout: Duration::from_secs(60),
            keep_alive_timeout: Duration::from_secs(20),
        }
    }
}

/// A single network listener — one `<Connector>` element in `server.xml`.
#[derive(Debug, Clone)]
pub struct ConnectorConfig {
    /// Protocol spoken on this connector.
    pub protocol: Protocol,
    /// Bind address; `None` means "all interfaces".
    pub address: Option<IpAddr>,
    /// TCP port to listen on.
    pub port: u16,
    /// TLS configuration; `Some` when `SSLEnabled="true"`.
    pub tls: Option<TlsConfig>,
    /// Request-processing limits for this connector.
    pub limits: RequestLimits,
}

/// A web application context — one `<Context>` element.
#[derive(Debug, Clone)]
pub struct ContextConfig {
    /// Context path the application is mounted at (e.g. `/myapp`, or `""`).
    pub path: String,
    /// Filesystem location of the exploded application or WAR.
    pub doc_base: PathBuf,
    /// Whether the context should be reloaded when classes change.
    pub reloadable: bool,
}

/// A virtual host — one `<Host>` element.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Canonical host name (e.g. `localhost`).
    pub name: String,
    /// Directory scanned for deployable applications.
    pub app_base: PathBuf,
    /// Additional DNS aliases routed to this host.
    pub aliases: Vec<String>,
    /// Whether applications dropped into `app_base` are auto-deployed.
    pub auto_deploy: bool,
    /// Explicitly declared contexts nested under this host.
    pub contexts: Vec<ContextConfig>,
}

/// A request-processing engine — one `<Engine>` element.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Engine name (conventionally `Catalina`).
    pub name: String,
    /// Name of the [`HostConfig`] used when no other host matches.
    pub default_host: String,
    /// Virtual hosts served by this engine.
    pub hosts: Vec<HostConfig>,
}

/// A service binding connectors to an engine — one `<Service>` element.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// Service name (conventionally `Catalina`).
    pub name: String,
    /// Connectors owned by this service.
    pub connectors: Vec<ConnectorConfig>,
    /// The engine all connectors feed requests into.
    pub engine: EngineConfig,
}

/// The root of the configuration tree — the `<Server>` element.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Port the shutdown listener binds to.
    pub port: u16,
    /// Magic string a client must send to trigger an orderly shutdown.
    pub shutdown: String,
    /// Services hosted by this server.
    pub services: Vec<ServiceConfig>,
}

impl ServerConfig {
    /// Parse a `server.xml` document from an in-memory string.
    ///
    /// # Errors
    ///
    /// Returns [`tomcatrs_core::Error::Config`] if the XML is malformed.
    /// Unknown attributes and elements are *not* errors — they are logged and
    /// skipped.
    pub fn from_xml_str(xml: &str) -> tomcatrs_core::Result<ServerConfig> {
        server_xml::parse_server_xml(xml)
    }

    /// Parse a `server.xml` document from a file on disk.
    ///
    /// # Errors
    ///
    /// Returns [`tomcatrs_core::Error::Io`] if the file cannot be read, or
    /// [`tomcatrs_core::Error::Config`] if its contents are malformed.
    pub fn from_xml_file(path: impl AsRef<Path>) -> tomcatrs_core::Result<ServerConfig> {
        let path = path.as_ref();
        let xml = std::fs::read_to_string(path)?;
        Self::from_xml_str(&xml)
    }

    /// Build the canonical development-mode configuration.
    ///
    /// This is equivalent to a freshly-unpacked Tomcat with the comments and
    /// optional connectors removed: a shutdown listener on `8005`, a single
    /// `Catalina` service exposing one HTTP/1.1 connector on `0.0.0.0:8080`,
    /// and a `Catalina` engine whose only (and default) host is `localhost`
    /// serving applications from `webapps`.
    pub fn default_dev() -> ServerConfig {
        ServerConfig {
            port: 8005,
            shutdown: "SHUTDOWN".to_string(),
            services: vec![ServiceConfig {
                name: "Catalina".to_string(),
                connectors: vec![ConnectorConfig {
                    protocol: Protocol::Http11,
                    address: None,
                    port: 8080,
                    tls: None,
                    limits: RequestLimits::default(),
                }],
                engine: EngineConfig {
                    name: "Catalina".to_string(),
                    default_host: "localhost".to_string(),
                    hosts: vec![HostConfig {
                        name: "localhost".to_string(),
                        app_base: PathBuf::from("webapps"),
                        aliases: Vec::new(),
                        auto_deploy: true,
                        contexts: Vec::new(),
                    }],
                },
            }],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_dev_matches_canonical_tomcat() {
        let cfg = ServerConfig::default_dev();
        assert_eq!(cfg.port, 8005);
        assert_eq!(cfg.shutdown, "SHUTDOWN");
        assert_eq!(cfg.services.len(), 1);

        let svc = &cfg.services[0];
        assert_eq!(svc.name, "Catalina");
        assert_eq!(svc.connectors.len(), 1);
        assert_eq!(svc.connectors[0].protocol, Protocol::Http11);
        assert_eq!(svc.connectors[0].port, 8080);
        assert!(svc.connectors[0].address.is_none());
        assert!(svc.connectors[0].tls.is_none());

        assert_eq!(svc.engine.name, "Catalina");
        assert_eq!(svc.engine.default_host, "localhost");
        assert_eq!(svc.engine.hosts.len(), 1);
        assert_eq!(svc.engine.hosts[0].name, "localhost");
        assert_eq!(svc.engine.hosts[0].app_base, PathBuf::from("webapps"));
        assert!(svc.engine.hosts[0].auto_deploy);
    }

    #[test]
    fn request_limits_defaults_are_tomcat_like() {
        let l = RequestLimits::default();
        assert_eq!(l.max_header_count, 100);
        assert_eq!(l.max_header_size, 8192);
        assert_eq!(l.max_parameter_count, 10_000);
        assert_eq!(l.max_post_size, 2 * 1024 * 1024);
        assert_eq!(l.max_part_count, 10);
        assert_eq!(l.max_uri_len, 8192);
        assert_eq!(l.request_timeout, Duration::from_secs(60));
        assert_eq!(l.keep_alive_timeout, Duration::from_secs(20));
    }

    #[test]
    fn protocol_default_is_http11() {
        assert_eq!(Protocol::default(), Protocol::Http11);
    }
}
