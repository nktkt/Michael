//! Parser for Apache Tomcat's `conf/server.xml`.
//!
//! The implementation is a streaming pass over the document using
//! [`quick_xml::Reader`]. It understands the structural elements
//! (`Server`, `Service`, `Connector`, `Engine`, `Host`, `Context`) and the
//! attributes Tomcat-RS currently models. Anything it does not recognise — a
//! `<Listener>`, a `<Realm>`, a `<Valve>`, an unmodelled attribute — is logged
//! at `warn` level and skipped, so a stock `server.xml` parses cleanly.

use std::net::IpAddr;
use std::path::PathBuf;

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use tomcatrs_core::{Error, Result};

use crate::{
    ConnectorConfig, EngineConfig, HostConfig, Protocol, RequestLimits, ServerConfig,
    ServiceConfig, TlsConfig,
};

/// Decode a single attribute's value to an owned `String`.
fn attr_value(attr: &quick_xml::events::attributes::Attribute<'_>) -> Result<String> {
    let bytes = attr
        .decode_and_unescape_value(quick_xml::reader::Reader::from_str("").decoder())
        .map_err(|e| Error::config(format!("invalid attribute value in server.xml: {e}")))?;
    Ok(bytes.into_owned())
}

/// Local name of an element/attribute as a UTF-8 `String`.
fn local_name(name: quick_xml::name::QName<'_>) -> String {
    String::from_utf8_lossy(name.local_name().as_ref()).into_owned()
}

/// Collect every attribute of a start tag into `(name, value)` pairs.
fn collect_attrs(start: &BytesStart<'_>) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for attr in start.attributes() {
        let attr =
            attr.map_err(|e| Error::config(format!("malformed attribute in server.xml: {e}")))?;
        let key = local_name(attr.key.into());
        let value = attr_value(&attr)?;
        out.push((key, value));
    }
    Ok(out)
}

/// Parse a `server.xml` document held entirely in memory.
///
/// # Errors
///
/// Returns [`Error::Config`] when the XML cannot be tokenised or when an
/// expected numeric attribute (such as `port`) is not a valid number.
pub fn parse_server_xml(xml: &str) -> Result<ServerConfig> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut server: Option<ServerConfig> = None;
    let mut buf = Vec::new();

    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("server.xml parse error: {e}")))?
        {
            Event::Start(start) => {
                let name = local_name(start.name());
                match name.as_str() {
                    "Server" => {
                        server = Some(parse_server_element(&mut reader, &start)?);
                    }
                    other => {
                        tracing::warn!(
                            element = other,
                            "unexpected top-level element in server.xml; skipping"
                        );
                        skip_element(&mut reader, &start)?;
                    }
                }
            }
            Event::Empty(start) => {
                let name = local_name(start.name());
                if name != "Server" {
                    tracing::warn!(
                        element = %name,
                        "unexpected empty top-level element in server.xml; skipping"
                    );
                } else {
                    server = Some(ServerConfig {
                        port: 8005,
                        shutdown: "SHUTDOWN".to_string(),
                        services: Vec::new(),
                    });
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    server.ok_or_else(|| Error::config("server.xml contains no <Server> element"))
}

/// Parse a `<Server>` subtree, with `start` being its opening tag.
fn parse_server_element(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart<'_>,
) -> Result<ServerConfig> {
    let mut port = 8005u16;
    let mut shutdown = "SHUTDOWN".to_string();

    for (key, value) in collect_attrs(start)? {
        match key.as_str() {
            "port" => {
                port = value.parse().map_err(|_| {
                    Error::config(format!("invalid Server port `{value}` in server.xml"))
                })?
            }
            "shutdown" => shutdown = value,
            other => tracing::warn!(
                element = "Server",
                attribute = other,
                "unknown attribute on <Server>; ignoring"
            ),
        }
    }

    let mut services = Vec::new();
    let mut buf = Vec::new();

    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("server.xml parse error: {e}")))?
        {
            Event::Start(child) => {
                let name = local_name(child.name());
                match name.as_str() {
                    "Service" => services.push(parse_service_element(reader, &child)?),
                    other => {
                        tracing::warn!(
                            parent = "Server",
                            element = other,
                            "unmodelled element under <Server>; skipping"
                        );
                        skip_element(reader, &child)?;
                    }
                }
            }
            Event::Empty(child) => {
                tracing::warn!(
                    parent = "Server",
                    element = %local_name(child.name()),
                    "unmodelled empty element under <Server>; skipping"
                );
            }
            Event::End(end) if local_name(end.name()) == "Server" => break,
            Event::Eof => {
                return Err(Error::config(
                    "unexpected EOF inside <Server> in server.xml",
                ))
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(ServerConfig {
        port,
        shutdown,
        services,
    })
}

/// Parse a `<Service>` subtree.
fn parse_service_element(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart<'_>,
) -> Result<ServiceConfig> {
    let mut name = "Catalina".to_string();
    for (key, value) in collect_attrs(start)? {
        match key.as_str() {
            "name" => name = value,
            other => tracing::warn!(
                element = "Service",
                attribute = other,
                "unknown attribute on <Service>; ignoring"
            ),
        }
    }

    let mut connectors = Vec::new();
    let mut engine: Option<EngineConfig> = None;
    let mut buf = Vec::new();

    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("server.xml parse error: {e}")))?
        {
            Event::Start(child) => {
                let cname = local_name(child.name());
                match cname.as_str() {
                    "Connector" => {
                        connectors.push(parse_connector_attrs(&child)?);
                        skip_element(reader, &child)?;
                    }
                    "Engine" => engine = Some(parse_engine_element(reader, &child)?),
                    other => {
                        tracing::warn!(
                            parent = "Service",
                            element = other,
                            "unmodelled element under <Service>; skipping"
                        );
                        skip_element(reader, &child)?;
                    }
                }
            }
            Event::Empty(child) => {
                let cname = local_name(child.name());
                if cname == "Connector" {
                    connectors.push(parse_connector_attrs(&child)?);
                } else {
                    tracing::warn!(
                        parent = "Service",
                        element = %cname,
                        "unmodelled empty element under <Service>; skipping"
                    );
                }
            }
            Event::End(end) if local_name(end.name()) == "Service" => break,
            Event::Eof => {
                return Err(Error::config(
                    "unexpected EOF inside <Service> in server.xml",
                ))
            }
            _ => {}
        }
        buf.clear();
    }

    let engine = engine.ok_or_else(|| {
        Error::config(format!(
            "<Service name=\"{name}\"> has no <Engine> in server.xml"
        ))
    })?;

    Ok(ServiceConfig {
        name,
        connectors,
        engine,
    })
}

/// Build a [`ConnectorConfig`] from the attributes of a `<Connector>` tag.
fn parse_connector_attrs(start: &BytesStart<'_>) -> Result<ConnectorConfig> {
    let mut protocol = Protocol::Http11;
    let mut address: Option<IpAddr> = None;
    let mut port = 0u16;
    let mut ssl_enabled = false;
    let mut cert_file: Option<PathBuf> = None;
    let mut key_file: Option<PathBuf> = None;
    let mut limits = RequestLimits::default();

    for (key, value) in collect_attrs(start)? {
        match key.as_str() {
            "protocol" => match Protocol::from_attr(&value) {
                Some(p) => protocol = p,
                None => tracing::warn!(
                    element = "Connector",
                    value = %value,
                    "unrecognised protocol; defaulting to HTTP/1.1"
                ),
            },
            "port" => {
                port = value.parse().map_err(|_| {
                    Error::config(format!("invalid Connector port `{value}` in server.xml"))
                })?
            }
            "address" => match value.parse::<IpAddr>() {
                Ok(ip) => address = Some(ip),
                Err(_) => tracing::warn!(
                    element = "Connector",
                    value = %value,
                    "invalid Connector address; binding to all interfaces"
                ),
            },
            "SSLEnabled" => ssl_enabled = value.eq_ignore_ascii_case("true"),
            "certificateFile" | "SSLCertificateFile" => cert_file = Some(PathBuf::from(value)),
            "certificateKeyFile" | "SSLCertificateKeyFile" => key_file = Some(PathBuf::from(value)),
            "maxHeaderCount" => {
                if let Ok(v) = value.parse() {
                    limits.max_header_count = v;
                }
            }
            "maxHttpHeaderSize" => {
                if let Ok(v) = value.parse() {
                    limits.max_header_size = v;
                }
            }
            "maxParameterCount" => {
                if let Ok(v) = value.parse() {
                    limits.max_parameter_count = v;
                }
            }
            "maxPostSize" => {
                if let Ok(v) = value.parse() {
                    limits.max_post_size = v;
                }
            }
            "maxPartCount" => {
                if let Ok(v) = value.parse() {
                    limits.max_part_count = v;
                }
            }
            "maxHttpRequestHeaderSize" => {
                if let Ok(v) = value.parse() {
                    limits.max_uri_len = v;
                }
            }
            "connectionTimeout" => {
                if let Ok(ms) = value.parse::<u64>() {
                    limits.request_timeout = std::time::Duration::from_millis(ms);
                }
            }
            "keepAliveTimeout" => {
                if let Ok(ms) = value.parse::<u64>() {
                    limits.keep_alive_timeout = std::time::Duration::from_millis(ms);
                }
            }
            other => tracing::warn!(
                element = "Connector",
                attribute = other,
                "unknown attribute on <Connector>; ignoring"
            ),
        }
    }

    let tls = if ssl_enabled {
        Some(TlsConfig {
            cert_file: cert_file.unwrap_or_default(),
            key_file: key_file.unwrap_or_default(),
        })
    } else {
        None
    };

    Ok(ConnectorConfig {
        protocol,
        address,
        port,
        tls,
        limits,
    })
}

/// Parse an `<Engine>` subtree.
fn parse_engine_element(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart<'_>,
) -> Result<EngineConfig> {
    let mut name = "Catalina".to_string();
    let mut default_host = "localhost".to_string();

    for (key, value) in collect_attrs(start)? {
        match key.as_str() {
            "name" => name = value,
            "defaultHost" => default_host = value,
            other => tracing::warn!(
                element = "Engine",
                attribute = other,
                "unknown attribute on <Engine>; ignoring"
            ),
        }
    }

    let mut hosts = Vec::new();
    let mut buf = Vec::new();

    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("server.xml parse error: {e}")))?
        {
            Event::Start(child) => {
                let cname = local_name(child.name());
                match cname.as_str() {
                    "Host" => hosts.push(parse_host_element(reader, &child)?),
                    other => {
                        tracing::warn!(
                            parent = "Engine",
                            element = other,
                            "unmodelled element under <Engine>; skipping"
                        );
                        skip_element(reader, &child)?;
                    }
                }
            }
            Event::Empty(child) => {
                let cname = local_name(child.name());
                if cname == "Host" {
                    hosts.push(parse_host_element_from_attrs(&child)?);
                } else {
                    tracing::warn!(
                        parent = "Engine",
                        element = %cname,
                        "unmodelled empty element under <Engine>; skipping"
                    );
                }
            }
            Event::End(end) if local_name(end.name()) == "Engine" => break,
            Event::Eof => {
                return Err(Error::config(
                    "unexpected EOF inside <Engine> in server.xml",
                ))
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(EngineConfig {
        name,
        default_host,
        hosts,
    })
}

/// Parse the attributes shared by both empty and non-empty `<Host>` tags.
fn parse_host_element_from_attrs(start: &BytesStart<'_>) -> Result<HostConfig> {
    let mut name = "localhost".to_string();
    let mut app_base = PathBuf::from("webapps");
    let mut auto_deploy = true;

    for (key, value) in collect_attrs(start)? {
        match key.as_str() {
            "name" => name = value,
            "appBase" => app_base = PathBuf::from(value),
            "autoDeploy" => auto_deploy = value.eq_ignore_ascii_case("true"),
            "unpackWARs" | "deployOnStartup" => { /* modelled implicitly; ignore quietly */ }
            other => tracing::warn!(
                element = "Host",
                attribute = other,
                "unknown attribute on <Host>; ignoring"
            ),
        }
    }

    Ok(HostConfig {
        name,
        app_base,
        aliases: Vec::new(),
        auto_deploy,
        contexts: Vec::new(),
    })
}

/// Parse a `<Host>` subtree, including nested `<Alias>` and `<Context>` tags.
fn parse_host_element(reader: &mut Reader<&[u8]>, start: &BytesStart<'_>) -> Result<HostConfig> {
    let mut host = parse_host_element_from_attrs(start)?;
    let mut buf = Vec::new();

    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("server.xml parse error: {e}")))?
        {
            Event::Start(child) => {
                let cname = local_name(child.name());
                match cname.as_str() {
                    "Context" => {
                        host.contexts.push(parse_context_attrs(&child)?);
                        skip_element(reader, &child)?;
                    }
                    "Alias" => {
                        let alias = read_text(reader, "Alias")?;
                        if !alias.is_empty() {
                            host.aliases.push(alias);
                        }
                    }
                    other => {
                        tracing::warn!(
                            parent = "Host",
                            element = other,
                            "unmodelled element under <Host>; skipping"
                        );
                        skip_element(reader, &child)?;
                    }
                }
            }
            Event::Empty(child) => {
                let cname = local_name(child.name());
                if cname == "Context" {
                    host.contexts.push(parse_context_attrs(&child)?);
                } else {
                    tracing::warn!(
                        parent = "Host",
                        element = %cname,
                        "unmodelled empty element under <Host>; skipping"
                    );
                }
            }
            Event::End(end) if local_name(end.name()) == "Host" => break,
            Event::Eof => return Err(Error::config("unexpected EOF inside <Host> in server.xml")),
            _ => {}
        }
        buf.clear();
    }

    Ok(host)
}

/// Build a [`crate::ContextConfig`] from the attributes of a `<Context>` tag.
pub(crate) fn parse_context_attrs(start: &BytesStart<'_>) -> Result<crate::ContextConfig> {
    let mut path = String::new();
    let mut doc_base = PathBuf::new();
    let mut reloadable = false;

    for (key, value) in collect_attrs(start)? {
        match key.as_str() {
            "path" => path = value,
            "docBase" => doc_base = PathBuf::from(value),
            "reloadable" => reloadable = value.eq_ignore_ascii_case("true"),
            other => tracing::warn!(
                element = "Context",
                attribute = other,
                "unknown attribute on <Context>; ignoring"
            ),
        }
    }

    Ok(crate::ContextConfig {
        path,
        doc_base,
        reloadable,
    })
}

/// Read the text content of an element whose end tag has local name `tag`.
fn read_text(reader: &mut Reader<&[u8]>, tag: &str) -> Result<String> {
    let mut buf = Vec::new();
    let mut text = String::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("server.xml parse error: {e}")))?
        {
            Event::Text(t) => {
                let chunk = t
                    .unescape()
                    .map_err(|e| Error::config(format!("invalid text in <{tag}>: {e}")))?;
                text.push_str(chunk.trim());
            }
            Event::End(end) if local_name(end.name()) == tag => break,
            Event::Eof => {
                return Err(Error::config(format!(
                    "unexpected EOF inside <{tag}> in server.xml"
                )))
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(text)
}

/// Consume and discard every event up to the matching end tag of `start`.
fn skip_element(reader: &mut Reader<&[u8]>, start: &BytesStart<'_>) -> Result<()> {
    let target = local_name(start.name());
    let mut depth = 1usize;
    let mut buf = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("server.xml parse error: {e}")))?
        {
            Event::Start(s) if local_name(s.name()) == target => depth += 1,
            Event::End(e) if local_name(e.name()) == target => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Event::Eof => {
                return Err(Error::config(format!(
                    "unexpected EOF while skipping <{target}> in server.xml"
                )))
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Server port="8005" shutdown="SHUTDOWN">
  <Listener className="org.apache.catalina.startup.VersionLoggerListener" />
  <GlobalNamingResources>
    <Resource name="UserDatabase" auth="Container" />
  </GlobalNamingResources>
  <Service name="Catalina">
    <Connector port="8080" protocol="HTTP/1.1" connectionTimeout="20000" redirectPort="8443" />
    <Connector port="8443" protocol="org.apache.coyote.http11.Http11AprProtocol"
               SSLEnabled="true" certificateFile="/etc/tls/cert.pem"
               certificateKeyFile="/etc/tls/key.pem" />
    <Connector port="8009" protocol="AJP/1.3" address="127.0.0.1" />
    <Engine name="Catalina" defaultHost="localhost">
      <Realm className="org.apache.catalina.realm.LockOutRealm" />
      <Host name="localhost" appBase="webapps" unpackWARs="true" autoDeploy="true">
        <Alias>www.example.com</Alias>
        <Context path="/app" docBase="myapp" reloadable="true" />
        <Valve className="org.apache.catalina.valves.AccessLogValve" />
      </Host>
    </Engine>
  </Service>
</Server>
"#;

    #[test]
    fn parses_realistic_server_xml() {
        let cfg = parse_server_xml(SAMPLE).expect("should parse");
        assert_eq!(cfg.port, 8005);
        assert_eq!(cfg.shutdown, "SHUTDOWN");
        assert_eq!(cfg.services.len(), 1);

        let svc = &cfg.services[0];
        assert_eq!(svc.name, "Catalina");
        assert_eq!(svc.connectors.len(), 3);

        assert_eq!(svc.connectors[0].protocol, Protocol::Http11);
        assert_eq!(svc.connectors[0].port, 8080);
        assert!(svc.connectors[0].tls.is_none());
        assert_eq!(
            svc.connectors[0].limits.request_timeout,
            std::time::Duration::from_millis(20000)
        );

        assert_eq!(svc.connectors[1].protocol, Protocol::Http11);
        assert_eq!(svc.connectors[1].port, 8443);
        let tls = svc.connectors[1].tls.as_ref().expect("tls connector");
        assert_eq!(tls.cert_file, PathBuf::from("/etc/tls/cert.pem"));
        assert_eq!(tls.key_file, PathBuf::from("/etc/tls/key.pem"));

        assert_eq!(svc.connectors[2].protocol, Protocol::Ajp);
        assert_eq!(svc.connectors[2].port, 8009);
        assert_eq!(
            svc.connectors[2].address,
            Some("127.0.0.1".parse().unwrap())
        );

        let engine = &svc.engine;
        assert_eq!(engine.name, "Catalina");
        assert_eq!(engine.default_host, "localhost");
        assert_eq!(engine.hosts.len(), 1);

        let host = &engine.hosts[0];
        assert_eq!(host.name, "localhost");
        assert_eq!(host.app_base, PathBuf::from("webapps"));
        assert!(host.auto_deploy);
        assert_eq!(host.aliases, vec!["www.example.com".to_string()]);
        assert_eq!(host.contexts.len(), 1);
        assert_eq!(host.contexts[0].path, "/app");
        assert_eq!(host.contexts[0].doc_base, PathBuf::from("myapp"));
        assert!(host.contexts[0].reloadable);
    }

    #[test]
    fn missing_server_element_is_an_error() {
        let err = parse_server_xml("<NotServer/>").unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn unknown_attributes_do_not_fail() {
        let xml = r#"<Server port="9000" shutdown="BYE" frobnicate="yes">
            <Service name="S"><Engine name="E" defaultHost="h">
            <Host name="h" appBase="apps" mystery="42"/></Engine></Service></Server>"#;
        let cfg = parse_server_xml(xml).expect("unknown attrs must be tolerated");
        assert_eq!(cfg.port, 9000);
        assert_eq!(cfg.shutdown, "BYE");
        assert_eq!(cfg.services[0].engine.hosts[0].name, "h");
    }
}
