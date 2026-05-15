//! Parser for the servlet deployment descriptor, `WEB-INF/web.xml`.
//!
//! This module models the subset of the descriptor Tomcat-RS currently needs:
//! servlet and filter declarations, their URL mappings, lifecycle listeners,
//! and the welcome-file list. The structure mirrors the Servlet specification's
//! element names. Unknown elements (`<session-config>`, `<error-page>`,
//! `<security-constraint>`, …) are logged and skipped.

use std::collections::HashMap;

use quick_xml::events::Event;
use quick_xml::Reader;
use tomcatrs_core::{Error, Result};

/// A `<servlet>` declaration.
#[derive(Debug, Clone, Default)]
pub struct ServletDef {
    /// Logical servlet name (`<servlet-name>`).
    pub name: String,
    /// Fully-qualified servlet class (`<servlet-class>`).
    pub class: String,
    /// `<load-on-startup>` ordering value, if present.
    pub load_on_startup: Option<i32>,
    /// `<init-param>` name/value pairs.
    pub init_params: HashMap<String, String>,
}

/// A `<servlet-mapping>` binding a servlet name to a URL pattern.
#[derive(Debug, Clone, Default)]
pub struct ServletMapping {
    /// The `<servlet-name>` this mapping targets.
    pub servlet_name: String,
    /// The `<url-pattern>` requests are matched against.
    pub url_pattern: String,
}

/// A `<filter>` declaration.
#[derive(Debug, Clone, Default)]
pub struct FilterDef {
    /// Logical filter name (`<filter-name>`).
    pub name: String,
    /// Fully-qualified filter class (`<filter-class>`).
    pub class: String,
    /// `<init-param>` name/value pairs.
    pub init_params: HashMap<String, String>,
}

/// A `<filter-mapping>` binding a filter name to a URL pattern.
#[derive(Debug, Clone, Default)]
pub struct FilterMapping {
    /// The `<filter-name>` this mapping targets.
    pub filter_name: String,
    /// The `<url-pattern>` requests are matched against.
    pub url_pattern: String,
}

/// The parsed contents of a `web.xml` deployment descriptor.
#[derive(Debug, Clone, Default)]
pub struct WebXml {
    /// All declared servlets.
    pub servlets: Vec<ServletDef>,
    /// All servlet-to-URL mappings.
    pub servlet_mappings: Vec<ServletMapping>,
    /// All declared filters.
    pub filters: Vec<FilterDef>,
    /// All filter-to-URL mappings.
    pub filter_mappings: Vec<FilterMapping>,
    /// Fully-qualified `<listener-class>` names.
    pub listeners: Vec<String>,
    /// `<welcome-file>` entries, in document order.
    pub welcome_files: Vec<String>,
}

impl WebXml {
    /// Parse a `web.xml` document held in memory.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] if the XML cannot be tokenised.
    pub fn from_xml_str(xml: &str) -> Result<WebXml> {
        parse_web_xml(xml)
    }

    /// Parse a `web.xml` document from a file on disk.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the file cannot be read, or [`Error::Config`]
    /// if its contents are malformed.
    pub fn from_xml_file(path: impl AsRef<std::path::Path>) -> Result<WebXml> {
        let xml = std::fs::read_to_string(path.as_ref())?;
        Self::from_xml_str(&xml)
    }
}

/// Local name of an element as an owned `String`.
fn local(name: quick_xml::name::QName<'_>) -> String {
    String::from_utf8_lossy(name.local_name().as_ref()).into_owned()
}

/// Read the trimmed text content of the element whose end tag is `tag`.
fn read_text(reader: &mut Reader<&[u8]>, tag: &str) -> Result<String> {
    let mut buf = Vec::new();
    let mut text = String::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("web.xml parse error: {e}")))?
        {
            Event::Text(t) => {
                let chunk = t
                    .unescape()
                    .map_err(|e| Error::config(format!("invalid text in <{tag}>: {e}")))?;
                text.push_str(&chunk);
            }
            Event::CData(c) => {
                text.push_str(&String::from_utf8_lossy(&c));
            }
            Event::End(e) if local(e.name()) == tag => break,
            Event::Eof => {
                return Err(Error::config(format!(
                    "unexpected EOF inside <{tag}> in web.xml"
                )))
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(text.trim().to_string())
}

/// Skip every event up to the matching end tag for `tag`.
fn skip(reader: &mut Reader<&[u8]>, tag: &str) -> Result<()> {
    let mut depth = 1usize;
    let mut buf = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("web.xml parse error: {e}")))?
        {
            Event::Start(s) if local(s.name()) == tag => depth += 1,
            Event::End(e) if local(e.name()) == tag => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Event::Eof => {
                return Err(Error::config(format!(
                    "unexpected EOF while skipping <{tag}> in web.xml"
                )))
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

/// Parse an `<init-param>` subtree into a `(name, value)` pair.
fn parse_init_param(reader: &mut Reader<&[u8]>) -> Result<(String, String)> {
    let mut name = String::new();
    let mut value = String::new();
    let mut buf = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("web.xml parse error: {e}")))?
        {
            Event::Start(s) => match local(s.name()).as_str() {
                "param-name" => name = read_text(reader, "param-name")?,
                "param-value" => value = read_text(reader, "param-value")?,
                other => skip(reader, other)?,
            },
            Event::End(e) if local(e.name()) == "init-param" => break,
            Event::Eof => {
                return Err(Error::config(
                    "unexpected EOF inside <init-param> in web.xml",
                ))
            }
            _ => {}
        }
        buf.clear();
    }
    Ok((name, value))
}

/// Parse a `<servlet>` subtree.
fn parse_servlet(reader: &mut Reader<&[u8]>) -> Result<ServletDef> {
    let mut def = ServletDef::default();
    let mut buf = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("web.xml parse error: {e}")))?
        {
            Event::Start(s) => match local(s.name()).as_str() {
                "servlet-name" => def.name = read_text(reader, "servlet-name")?,
                "servlet-class" => def.class = read_text(reader, "servlet-class")?,
                "load-on-startup" => {
                    let raw = read_text(reader, "load-on-startup")?;
                    match raw.parse::<i32>() {
                        Ok(v) => def.load_on_startup = Some(v),
                        Err(_) => tracing::warn!(
                            value = %raw,
                            "invalid <load-on-startup> value in web.xml; ignoring"
                        ),
                    }
                }
                "init-param" => {
                    let (k, v) = parse_init_param(reader)?;
                    def.init_params.insert(k, v);
                }
                other => {
                    tracing::warn!(
                        parent = "servlet",
                        element = %other,
                        "unmodelled element inside <servlet>; skipping"
                    );
                    skip(reader, other)?;
                }
            },
            Event::End(e) if local(e.name()) == "servlet" => break,
            Event::Eof => return Err(Error::config("unexpected EOF inside <servlet> in web.xml")),
            _ => {}
        }
        buf.clear();
    }
    Ok(def)
}

/// Parse a `<filter>` subtree.
fn parse_filter(reader: &mut Reader<&[u8]>) -> Result<FilterDef> {
    let mut def = FilterDef::default();
    let mut buf = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("web.xml parse error: {e}")))?
        {
            Event::Start(s) => match local(s.name()).as_str() {
                "filter-name" => def.name = read_text(reader, "filter-name")?,
                "filter-class" => def.class = read_text(reader, "filter-class")?,
                "init-param" => {
                    let (k, v) = parse_init_param(reader)?;
                    def.init_params.insert(k, v);
                }
                other => {
                    tracing::warn!(
                        parent = "filter",
                        element = %other,
                        "unmodelled element inside <filter>; skipping"
                    );
                    skip(reader, other)?;
                }
            },
            Event::End(e) if local(e.name()) == "filter" => break,
            Event::Eof => return Err(Error::config("unexpected EOF inside <filter> in web.xml")),
            _ => {}
        }
        buf.clear();
    }
    Ok(def)
}

/// Parse a `<servlet-mapping>` subtree.
fn parse_servlet_mapping(reader: &mut Reader<&[u8]>) -> Result<ServletMapping> {
    let mut m = ServletMapping::default();
    let mut buf = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("web.xml parse error: {e}")))?
        {
            Event::Start(s) => match local(s.name()).as_str() {
                "servlet-name" => m.servlet_name = read_text(reader, "servlet-name")?,
                "url-pattern" => m.url_pattern = read_text(reader, "url-pattern")?,
                other => skip(reader, other)?,
            },
            Event::End(e) if local(e.name()) == "servlet-mapping" => break,
            Event::Eof => {
                return Err(Error::config(
                    "unexpected EOF inside <servlet-mapping> in web.xml",
                ))
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(m)
}

/// Parse a `<filter-mapping>` subtree.
fn parse_filter_mapping(reader: &mut Reader<&[u8]>) -> Result<FilterMapping> {
    let mut m = FilterMapping::default();
    let mut buf = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("web.xml parse error: {e}")))?
        {
            Event::Start(s) => match local(s.name()).as_str() {
                "filter-name" => m.filter_name = read_text(reader, "filter-name")?,
                "url-pattern" => m.url_pattern = read_text(reader, "url-pattern")?,
                other => skip(reader, other)?,
            },
            Event::End(e) if local(e.name()) == "filter-mapping" => break,
            Event::Eof => {
                return Err(Error::config(
                    "unexpected EOF inside <filter-mapping> in web.xml",
                ))
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(m)
}

/// Parse a `<listener>` subtree into its `<listener-class>` name.
fn parse_listener(reader: &mut Reader<&[u8]>) -> Result<String> {
    let mut class = String::new();
    let mut buf = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("web.xml parse error: {e}")))?
        {
            Event::Start(s) => match local(s.name()).as_str() {
                "listener-class" => class = read_text(reader, "listener-class")?,
                other => skip(reader, other)?,
            },
            Event::End(e) if local(e.name()) == "listener" => break,
            Event::Eof => return Err(Error::config("unexpected EOF inside <listener> in web.xml")),
            _ => {}
        }
        buf.clear();
    }
    Ok(class)
}

/// Parse a `<welcome-file-list>` subtree into its `<welcome-file>` entries.
fn parse_welcome_files(reader: &mut Reader<&[u8]>) -> Result<Vec<String>> {
    let mut files = Vec::new();
    let mut buf = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("web.xml parse error: {e}")))?
        {
            Event::Start(s) => match local(s.name()).as_str() {
                "welcome-file" => {
                    let f = read_text(reader, "welcome-file")?;
                    if !f.is_empty() {
                        files.push(f);
                    }
                }
                other => skip(reader, other)?,
            },
            Event::End(e) if local(e.name()) == "welcome-file-list" => break,
            Event::Eof => {
                return Err(Error::config(
                    "unexpected EOF inside <welcome-file-list> in web.xml",
                ))
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(files)
}

/// Parse a complete `web.xml` document.
///
/// # Errors
///
/// Returns [`Error::Config`] when the document cannot be tokenised.
pub fn parse_web_xml(xml: &str) -> Result<WebXml> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut web = WebXml::default();
    let mut buf = Vec::new();

    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("web.xml parse error: {e}")))?
        {
            Event::Start(start) => {
                let name = local(start.name());
                match name.as_str() {
                    // The document root; descend into it.
                    "web-app" => {}
                    "servlet" => web.servlets.push(parse_servlet(&mut reader)?),
                    "servlet-mapping" => web
                        .servlet_mappings
                        .push(parse_servlet_mapping(&mut reader)?),
                    "filter" => web.filters.push(parse_filter(&mut reader)?),
                    "filter-mapping" => {
                        web.filter_mappings.push(parse_filter_mapping(&mut reader)?)
                    }
                    "listener" => {
                        let class = parse_listener(&mut reader)?;
                        if !class.is_empty() {
                            web.listeners.push(class);
                        }
                    }
                    "welcome-file-list" => web.welcome_files = parse_welcome_files(&mut reader)?,
                    other => {
                        tracing::warn!(element = other, "unmodelled element in web.xml; skipping");
                        skip(&mut reader, other)?;
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(web)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<web-app xmlns="http://xmlns.jcp.org/xml/ns/javaee" version="4.0">
  <display-name>Sample App</display-name>
  <filter>
    <filter-name>encoding</filter-name>
    <filter-class>com.example.EncodingFilter</filter-class>
    <init-param>
      <param-name>charset</param-name>
      <param-value>UTF-8</param-value>
    </init-param>
  </filter>
  <filter-mapping>
    <filter-name>encoding</filter-name>
    <url-pattern>/*</url-pattern>
  </filter-mapping>
  <listener>
    <listener-class>com.example.AppContextListener</listener-class>
  </listener>
  <servlet>
    <servlet-name>hello</servlet-name>
    <servlet-class>com.example.HelloServlet</servlet-class>
    <load-on-startup>1</load-on-startup>
    <init-param>
      <param-name>greeting</param-name>
      <param-value>hi</param-value>
    </init-param>
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
  <session-config>
    <session-timeout>30</session-timeout>
  </session-config>
  <welcome-file-list>
    <welcome-file>index.html</welcome-file>
    <welcome-file>index.jsp</welcome-file>
  </welcome-file-list>
</web-app>
"#;

    #[test]
    fn parses_sample_web_xml() {
        let web = parse_web_xml(SAMPLE).expect("should parse");

        assert_eq!(web.servlets.len(), 2);
        let hello = &web.servlets[0];
        assert_eq!(hello.name, "hello");
        assert_eq!(hello.class, "com.example.HelloServlet");
        assert_eq!(hello.load_on_startup, Some(1));
        assert_eq!(
            hello.init_params.get("greeting").map(String::as_str),
            Some("hi")
        );
        assert_eq!(web.servlets[1].name, "api");
        assert_eq!(web.servlets[1].load_on_startup, None);

        assert_eq!(web.servlet_mappings.len(), 2);
        assert_eq!(web.servlet_mappings[0].servlet_name, "hello");
        assert_eq!(web.servlet_mappings[0].url_pattern, "/hello");
        assert_eq!(web.servlet_mappings[1].url_pattern, "/api/*");

        assert_eq!(web.filters.len(), 1);
        assert_eq!(web.filters[0].name, "encoding");
        assert_eq!(
            web.filters[0]
                .init_params
                .get("charset")
                .map(String::as_str),
            Some("UTF-8")
        );
        assert_eq!(web.filter_mappings.len(), 1);
        assert_eq!(web.filter_mappings[0].filter_name, "encoding");
        assert_eq!(web.filter_mappings[0].url_pattern, "/*");

        assert_eq!(
            web.listeners,
            vec!["com.example.AppContextListener".to_string()]
        );
        assert_eq!(
            web.welcome_files,
            vec!["index.html".to_string(), "index.jsp".to_string()]
        );
    }

    #[test]
    fn empty_web_app_yields_empty_struct() {
        let web = parse_web_xml("<web-app></web-app>").expect("should parse");
        assert!(web.servlets.is_empty());
        assert!(web.filters.is_empty());
        assert!(web.listeners.is_empty());
        assert!(web.welcome_files.is_empty());
    }
}
