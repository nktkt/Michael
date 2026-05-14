//! A lightweight, self-contained model of `WEB-INF/web.xml`.
//!
//! The authoritative configuration model lives in `tomcatrs-config`, but that
//! crate builds in parallel with this one. To avoid a hard ordering dependency
//! — and because the webapp layer only needs a handful of fields — this module
//! defines its own minimal types and parses `web.xml` directly with
//! `quick-xml`.
//!
//! The parser is intentionally tolerant: elements it does not recognise are
//! skipped rather than treated as errors, matching Tomcat's own forgiving
//! deployment-descriptor handling. Genuinely malformed XML surfaces as
//! [`Error::Deployment`].

use std::path::Path;

use quick_xml::events::Event;
use quick_xml::reader::Reader;
use tomcatrs_core::{Error, Result};

/// A `<servlet>` declaration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServletDef {
    /// The `<servlet-name>`.
    pub name: String,
    /// The `<servlet-class>`, if declared (JSP servlets use `<jsp-file>`).
    pub class: Option<String>,
    /// The `<jsp-file>`, if this is a JSP-backed servlet.
    pub jsp_file: Option<String>,
    /// Whether `<load-on-startup>` was present with a non-negative value.
    pub load_on_startup: bool,
}

/// A `<servlet-mapping>`: a servlet name bound to one URL pattern.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServletMapping {
    /// The `<servlet-name>` this mapping targets.
    pub servlet_name: String,
    /// The `<url-pattern>` that routes to it.
    pub url_pattern: String,
}

/// A `<filter>` declaration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterDef {
    /// The `<filter-name>`.
    pub name: String,
    /// The `<filter-class>`, if declared.
    pub class: Option<String>,
}

/// A `<filter-mapping>`: a filter name bound to one URL pattern.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterMapping {
    /// The `<filter-name>` this mapping targets.
    pub filter_name: String,
    /// The `<url-pattern>` the filter intercepts.
    pub url_pattern: String,
}

/// The parsed, webapp-relevant subset of a `WEB-INF/web.xml` descriptor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebDescriptor {
    /// Declared servlets, in document order.
    pub servlets: Vec<ServletDef>,
    /// Declared servlet mappings, in document order.
    pub servlet_mappings: Vec<ServletMapping>,
    /// Declared filters, in document order.
    pub filters: Vec<FilterDef>,
    /// Declared filter mappings, in document order.
    pub filter_mappings: Vec<FilterMapping>,
    /// Declared `<listener-class>` values, in document order.
    pub listeners: Vec<String>,
    /// `<welcome-file>` entries from `<welcome-file-list>`.
    pub welcome_files: Vec<String>,
    /// `true` if the root `<web-app>` element carried `metadata-complete="true"`.
    pub metadata_complete: bool,
}

impl WebDescriptor {
    /// Parse a `web.xml` document from an in-memory string.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Deployment`] if the XML is not well-formed.
    pub fn from_xml_str(xml: &str) -> Result<WebDescriptor> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);

        let mut desc = WebDescriptor::default();
        let mut buf = Vec::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) => {
                    let name = local_name(e.name().as_ref());
                    match name.as_str() {
                        "web-app" => {
                            for attr in e.attributes().flatten() {
                                if local_name(attr.key.as_ref()) == "metadata-complete" {
                                    let v = attr
                                        .unescape_value()
                                        .map(|c| c.into_owned())
                                        .unwrap_or_default();
                                    desc.metadata_complete = v.eq_ignore_ascii_case("true");
                                }
                            }
                        }
                        "servlet" => {
                            desc.servlets.push(parse_servlet(&mut reader)?);
                        }
                        "servlet-mapping" => {
                            desc.servlet_mappings
                                .push(parse_servlet_mapping(&mut reader)?);
                        }
                        "filter" => {
                            desc.filters.push(parse_filter(&mut reader)?);
                        }
                        "filter-mapping" => {
                            desc.filter_mappings
                                .push(parse_filter_mapping(&mut reader)?);
                        }
                        "listener" => {
                            if let Some(class) =
                                parse_simple_child(&mut reader, "listener", "listener-class")?
                            {
                                desc.listeners.push(class);
                            }
                        }
                        "welcome-file-list" => {
                            desc.welcome_files
                                .extend(parse_welcome_file_list(&mut reader)?);
                        }
                        _ => {}
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    return Err(Error::Deployment(format!("malformed web.xml: {e}")));
                }
                _ => {}
            }
            buf.clear();
        }

        Ok(desc)
    }

    /// Parse a `web.xml` document from a file on disk.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the file cannot be read, or
    /// [`Error::Deployment`] if its contents are not well-formed XML.
    pub fn from_xml_file(path: impl AsRef<Path>) -> Result<WebDescriptor> {
        let xml = std::fs::read_to_string(path.as_ref())?;
        Self::from_xml_str(&xml)
    }
}

/// Strip any XML namespace prefix from a raw element/attribute name.
fn local_name(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    match s.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => s.into_owned(),
    }
}

/// Read character data until the matching end tag of `parent`, returning the
/// text content of the *first* direct child named `child`.
fn parse_simple_child(
    reader: &mut Reader<&[u8]>,
    parent: &str,
    child: &str,
) -> Result<Option<String>> {
    let mut buf = Vec::new();
    let mut value: Option<String> = None;
    let mut in_child = false;
    let mut depth = 0usize;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                if depth == 0 && name == child {
                    in_child = true;
                }
                depth += 1;
            }
            Ok(Event::Text(t)) if in_child => {
                let text = t.unescape().map(|c| c.into_owned()).unwrap_or_default();
                if value.is_none() {
                    value = Some(text);
                }
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref());
                if depth == 0 && name == parent {
                    break;
                }
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    in_child = false;
                }
            }
            Ok(Event::Eof) => {
                return Err(Error::Deployment(format!(
                    "malformed web.xml: unexpected end of document inside <{parent}>"
                )));
            }
            Err(e) => {
                return Err(Error::Deployment(format!("malformed web.xml: {e}")));
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(value)
}

/// Parse the children of a `<servlet>` element (cursor is past `<servlet>`).
fn parse_servlet(reader: &mut Reader<&[u8]>) -> Result<ServletDef> {
    let fields = collect_fields(reader, "servlet")?;
    let mut def = ServletDef::default();
    for (key, val) in fields {
        match key.as_str() {
            "servlet-name" => def.name = val,
            "servlet-class" => def.class = Some(val),
            "jsp-file" => def.jsp_file = Some(val),
            "load-on-startup" => {
                def.load_on_startup = val.trim().parse::<i32>().map(|n| n >= 0).unwrap_or(false);
            }
            _ => {}
        }
    }
    Ok(def)
}

/// Parse the children of a `<servlet-mapping>` element.
fn parse_servlet_mapping(reader: &mut Reader<&[u8]>) -> Result<ServletMapping> {
    let fields = collect_fields(reader, "servlet-mapping")?;
    let mut m = ServletMapping::default();
    for (key, val) in fields {
        match key.as_str() {
            "servlet-name" => m.servlet_name = val,
            "url-pattern" => m.url_pattern = val,
            _ => {}
        }
    }
    Ok(m)
}

/// Parse the children of a `<filter>` element.
fn parse_filter(reader: &mut Reader<&[u8]>) -> Result<FilterDef> {
    let fields = collect_fields(reader, "filter")?;
    let mut def = FilterDef::default();
    for (key, val) in fields {
        match key.as_str() {
            "filter-name" => def.name = val,
            "filter-class" => def.class = Some(val),
            _ => {}
        }
    }
    Ok(def)
}

/// Parse the children of a `<filter-mapping>` element.
fn parse_filter_mapping(reader: &mut Reader<&[u8]>) -> Result<FilterMapping> {
    let fields = collect_fields(reader, "filter-mapping")?;
    let mut m = FilterMapping::default();
    for (key, val) in fields {
        match key.as_str() {
            "filter-name" => m.filter_name = val,
            "url-pattern" => m.url_pattern = val,
            _ => {}
        }
    }
    Ok(m)
}

/// Parse a `<welcome-file-list>`, returning every `<welcome-file>` value.
fn parse_welcome_file_list(reader: &mut Reader<&[u8]>) -> Result<Vec<String>> {
    let fields = collect_fields(reader, "welcome-file-list")?;
    Ok(fields
        .into_iter()
        .filter(|(k, _)| k == "welcome-file")
        .map(|(_, v)| v)
        .collect())
}

/// Collect every direct-child `(local-name, text)` pair of the element named
/// `parent`, consuming events up to and including `</parent>`.
///
/// Nested grandchildren are skipped; only the text of direct children is
/// captured. This is sufficient for the flat shape of the `web.xml` elements
/// this crate cares about.
fn collect_fields(reader: &mut Reader<&[u8]>, parent: &str) -> Result<Vec<(String, String)>> {
    let mut buf = Vec::new();
    let mut fields = Vec::new();
    let mut depth = 0usize;
    let mut current: Option<(String, String)> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                if depth == 0 {
                    current = Some((name, String::new()));
                }
                depth += 1;
            }
            Ok(Event::Text(t)) => {
                if depth == 1 {
                    if let Some((_, ref mut val)) = current {
                        let text = t.unescape().map(|c| c.into_owned()).unwrap_or_default();
                        val.push_str(&text);
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref());
                if depth == 0 && name == parent {
                    break;
                }
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    if let Some(pair) = current.take() {
                        fields.push((pair.0, pair.1.trim().to_string()));
                    }
                }
            }
            Ok(Event::Eof) => {
                return Err(Error::Deployment(format!(
                    "malformed web.xml: unexpected end of document inside <{parent}>"
                )));
            }
            Err(e) => {
                return Err(Error::Deployment(format!("malformed web.xml: {e}")));
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<web-app xmlns="http://xmlns.jcp.org/xml/ns/javaee" version="4.0" metadata-complete="true">
  <servlet>
    <servlet-name>hello</servlet-name>
    <servlet-class>com.example.HelloServlet</servlet-class>
    <load-on-startup>1</load-on-startup>
  </servlet>
  <servlet>
    <servlet-name>index</servlet-name>
    <jsp-file>/index.jsp</jsp-file>
  </servlet>
  <servlet-mapping>
    <servlet-name>hello</servlet-name>
    <url-pattern>/hello/*</url-pattern>
  </servlet-mapping>
  <filter>
    <filter-name>enc</filter-name>
    <filter-class>com.example.EncodingFilter</filter-class>
  </filter>
  <filter-mapping>
    <filter-name>enc</filter-name>
    <url-pattern>/*</url-pattern>
  </filter-mapping>
  <listener>
    <listener-class>com.example.AppListener</listener-class>
  </listener>
  <welcome-file-list>
    <welcome-file>index.html</welcome-file>
    <welcome-file>index.jsp</welcome-file>
  </welcome-file-list>
</web-app>
"#;

    #[test]
    fn parses_full_descriptor() {
        let d = WebDescriptor::from_xml_str(SAMPLE).expect("parse");
        assert!(d.metadata_complete);

        assert_eq!(d.servlets.len(), 2);
        assert_eq!(d.servlets[0].name, "hello");
        assert_eq!(
            d.servlets[0].class.as_deref(),
            Some("com.example.HelloServlet")
        );
        assert!(d.servlets[0].load_on_startup);
        assert_eq!(d.servlets[1].name, "index");
        assert_eq!(d.servlets[1].jsp_file.as_deref(), Some("/index.jsp"));
        assert!(!d.servlets[1].load_on_startup);

        assert_eq!(d.servlet_mappings.len(), 1);
        assert_eq!(d.servlet_mappings[0].servlet_name, "hello");
        assert_eq!(d.servlet_mappings[0].url_pattern, "/hello/*");

        assert_eq!(d.filters.len(), 1);
        assert_eq!(d.filters[0].name, "enc");
        assert_eq!(d.filter_mappings.len(), 1);
        assert_eq!(d.filter_mappings[0].url_pattern, "/*");

        assert_eq!(d.listeners, vec!["com.example.AppListener".to_string()]);
        assert_eq!(
            d.welcome_files,
            vec!["index.html".to_string(), "index.jsp".to_string()]
        );
    }

    #[test]
    fn empty_web_app_is_ok() {
        let d = WebDescriptor::from_xml_str("<web-app></web-app>").expect("parse");
        assert!(d.servlets.is_empty());
        assert!(!d.metadata_complete);
    }

    #[test]
    fn malformed_xml_is_deployment_error() {
        let err = WebDescriptor::from_xml_str("<web-app><servlet>").unwrap_err();
        assert!(matches!(err, Error::Deployment(_)));
    }
}
