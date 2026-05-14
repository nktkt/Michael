//! JSP configuration: the `<jsp-config>` element of `web.xml`.
//!
//! The Servlet deployment descriptor may carry a `<jsp-config>` element that
//! tunes how the container treats JSP pages. It has two kinds of children:
//!
//! * `<jsp-property-group>` — applies JSP page properties (EL handling,
//!   scripting, page encoding, implicit includes, …) to the set of pages
//!   matched by one or more `<url-pattern>` elements.
//! * `<taglib>` — maps a tag library `<taglib-uri>` to the `<taglib-location>`
//!   of its descriptor, overriding the URI a `.tld` declares for itself.
//!
//! This module models that element ([`JspConfigDescriptor`] and its components
//! [`JspPropertyGroup`] / [`TaglibMapping`]) and parses just the `<jsp-config>`
//! subtree out of a full `web.xml` document with `quick-xml`. Unknown elements
//! are logged at `warn` and skipped.
//!
//! [`JspConfigDescriptor::property_group_for`] resolves the property group
//! that applies to a given page path, using the same three url-pattern shapes
//! the Servlet spec defines: exact match, `*.ext` extension match, and
//! `/prefix/*` path-prefix match.

use quick_xml::events::Event;
use quick_xml::Reader;
use tomcatrs_core::{Error, Result};

/// A `<jsp-property-group>`: JSP page properties applied to every page whose
/// path matches one of [`url_patterns`](JspPropertyGroup::url_patterns).
///
/// Every property other than the patterns is optional: an absent element
/// leaves the corresponding field `None`, meaning "container default".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JspPropertyGroup {
    /// The `<url-pattern>` values this group applies to.
    pub url_patterns: Vec<String>,
    /// `<el-ignored>` — disable EL evaluation for matched pages.
    pub el_ignored: Option<bool>,
    /// `<scripting-invalid>` — forbid `<% %>` scriptlets in matched pages.
    pub scripting_invalid: Option<bool>,
    /// `<page-encoding>` — the character encoding of matched pages.
    pub page_encoding: Option<String>,
    /// `<include-prelude>` — paths included at the top of every matched page.
    pub include_preludes: Vec<String>,
    /// `<include-coda>` — paths included at the bottom of every matched page.
    pub include_codas: Vec<String>,
    /// `<is-xml>` — treat matched pages as JSP documents (XML syntax).
    pub is_xml: Option<bool>,
    /// `<trim-directive-whitespaces>` — strip whitespace-only template text
    /// left by directives.
    pub trim_directive_whitespaces: Option<bool>,
    /// `<default-content-type>` — the response content type for matched pages.
    pub default_content_type: Option<String>,
    /// `<buffer>` — the page output buffer size (e.g. `8kb`, `none`).
    pub buffer: Option<String>,
}

impl JspPropertyGroup {
    /// Whether `path` is matched by any of this group's url patterns.
    pub fn matches(&self, path: &str) -> bool {
        self.url_patterns
            .iter()
            .any(|p| url_pattern_matches(p, path))
    }
}

/// A `<taglib>` entry inside `<jsp-config>`: an explicit `uri → location`
/// mapping for a tag library descriptor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaglibMapping {
    /// The `<taglib-uri>` JSP pages use in their `taglib` directive.
    pub taglib_uri: String,
    /// The `<taglib-location>` of the `.tld`, relative to the webapp root.
    pub taglib_location: String,
}

/// The parsed `<jsp-config>` element of a `web.xml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JspConfigDescriptor {
    /// Every `<jsp-property-group>`, in document order.
    pub property_groups: Vec<JspPropertyGroup>,
    /// Every `<taglib>` mapping, in document order.
    pub taglib_mappings: Vec<TaglibMapping>,
}

impl JspConfigDescriptor {
    /// Parse the `<jsp-config>` subtree out of a full `web.xml` document.
    ///
    /// A `web.xml` with no `<jsp-config>` element yields an empty descriptor.
    /// Unknown elements are logged at `warn` and skipped.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] if the XML cannot be tokenised.
    pub fn from_web_xml_str(xml: &str) -> Result<JspConfigDescriptor> {
        parse_jsp_config(xml)
    }

    /// Parse the `<jsp-config>` element from a `web.xml` file on disk.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the file cannot be read, or [`Error::Config`]
    /// if its contents are malformed.
    pub fn from_web_xml_file(path: impl AsRef<std::path::Path>) -> Result<JspConfigDescriptor> {
        let xml = std::fs::read_to_string(path.as_ref())?;
        Self::from_web_xml_str(&xml)
    }

    /// Find the property group that applies to the JSP page at `path`.
    ///
    /// Matching follows the Servlet specification's url-pattern precedence:
    /// an exact match wins over a path-prefix (`/prefix/*`) match, which wins
    /// over an extension (`*.ext`) match. Among path-prefix matches the
    /// longest pattern wins. When nothing matches, returns `None`.
    pub fn property_group_for(&self, path: &str) -> Option<&JspPropertyGroup> {
        let mut best: Option<(&JspPropertyGroup, MatchKind, usize)> = None;
        for group in &self.property_groups {
            for pattern in &group.url_patterns {
                if let Some(kind) = classify_match(pattern, path) {
                    let len = pattern.len();
                    let better = match &best {
                        None => true,
                        Some((_, best_kind, best_len)) => {
                            kind > *best_kind || (kind == *best_kind && len > *best_len)
                        }
                    };
                    if better {
                        best = Some((group, kind, len));
                    }
                }
            }
        }
        best.map(|(group, _, _)| group)
    }
}

/// Relative precedence of the url-pattern match shapes; higher wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MatchKind {
    /// `*.ext` extension match — lowest precedence.
    Extension,
    /// `/prefix/*` path-prefix match.
    Prefix,
    /// Exact, character-for-character match — highest precedence.
    Exact,
}

/// Classify how `pattern` matches `path`, or `None` if it does not match.
fn classify_match(pattern: &str, path: &str) -> Option<MatchKind> {
    if let Some(ext) = pattern.strip_prefix("*.") {
        // Extension pattern: the path must end with `.ext`.
        if path.len() > ext.len() + 1
            && path.ends_with(ext)
            && path.as_bytes()[path.len() - ext.len() - 1] == b'.'
        {
            return Some(MatchKind::Extension);
        }
        return None;
    }
    if let Some(prefix) = pattern.strip_suffix("/*") {
        // Path-prefix pattern: `/dir/*` matches `/dir` and anything under it.
        if path == prefix || path.starts_with(&format!("{prefix}/")) {
            return Some(MatchKind::Prefix);
        }
        return None;
    }
    // Anything else is treated as an exact match.
    if pattern == path {
        return Some(MatchKind::Exact);
    }
    None
}

/// Whether `pattern` matches `path` under the supported url-pattern shapes.
fn url_pattern_matches(pattern: &str, path: &str) -> bool {
    classify_match(pattern, path).is_some()
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
            Event::CData(c) => text.push_str(&String::from_utf8_lossy(&c)),
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

/// Parse the trimmed text of an element as a deployment-descriptor boolean.
///
/// The schema accepts `true`/`false`/`yes`/`no`; anything else is logged and
/// treated as `false`.
fn read_bool(reader: &mut Reader<&[u8]>, tag: &str) -> Result<bool> {
    let raw = read_text(reader, tag)?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" => Ok(true),
        "false" | "no" => Ok(false),
        other => {
            tracing::warn!(
                element = tag,
                value = other,
                "invalid boolean in web.xml jsp-config; treating as false"
            );
            Ok(false)
        }
    }
}

/// Parse a `<jsp-property-group>` subtree.
fn parse_property_group(reader: &mut Reader<&[u8]>) -> Result<JspPropertyGroup> {
    let mut group = JspPropertyGroup::default();
    let mut buf = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("web.xml parse error: {e}")))?
        {
            Event::Start(s) => match local(s.name()).as_str() {
                "url-pattern" => group.url_patterns.push(read_text(reader, "url-pattern")?),
                "el-ignored" => group.el_ignored = Some(read_bool(reader, "el-ignored")?),
                "scripting-invalid" => {
                    group.scripting_invalid = Some(read_bool(reader, "scripting-invalid")?)
                }
                "page-encoding" => group.page_encoding = Some(read_text(reader, "page-encoding")?),
                "include-prelude" => group
                    .include_preludes
                    .push(read_text(reader, "include-prelude")?),
                "include-coda" => group.include_codas.push(read_text(reader, "include-coda")?),
                "is-xml" => group.is_xml = Some(read_bool(reader, "is-xml")?),
                "trim-directive-whitespaces" => {
                    group.trim_directive_whitespaces =
                        Some(read_bool(reader, "trim-directive-whitespaces")?)
                }
                "default-content-type" => {
                    group.default_content_type = Some(read_text(reader, "default-content-type")?)
                }
                "buffer" => group.buffer = Some(read_text(reader, "buffer")?),
                other => {
                    tracing::warn!(
                        parent = "jsp-property-group",
                        element = %other,
                        "unmodelled element inside <jsp-property-group>; skipping"
                    );
                    skip(reader, &other)?;
                }
            },
            Event::End(e) if local(e.name()) == "jsp-property-group" => break,
            Event::Eof => {
                return Err(Error::config(
                    "unexpected EOF inside <jsp-property-group> in web.xml",
                ))
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(group)
}

/// Parse a `<taglib>` subtree inside `<jsp-config>`.
fn parse_taglib_mapping(reader: &mut Reader<&[u8]>) -> Result<TaglibMapping> {
    let mut mapping = TaglibMapping::default();
    let mut buf = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("web.xml parse error: {e}")))?
        {
            Event::Start(s) => match local(s.name()).as_str() {
                "taglib-uri" => mapping.taglib_uri = read_text(reader, "taglib-uri")?,
                "taglib-location" => {
                    mapping.taglib_location = read_text(reader, "taglib-location")?
                }
                other => skip(reader, &other)?,
            },
            Event::End(e) if local(e.name()) == "taglib" => break,
            Event::Eof => return Err(Error::config("unexpected EOF inside <taglib> in web.xml")),
            _ => {}
        }
        buf.clear();
    }
    Ok(mapping)
}

/// Parse a `<jsp-config>` subtree into a [`JspConfigDescriptor`].
fn parse_jsp_config_subtree(reader: &mut Reader<&[u8]>) -> Result<JspConfigDescriptor> {
    let mut descriptor = JspConfigDescriptor::default();
    let mut buf = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("web.xml parse error: {e}")))?
        {
            Event::Start(s) => match local(s.name()).as_str() {
                "jsp-property-group" => descriptor
                    .property_groups
                    .push(parse_property_group(reader)?),
                "taglib" => descriptor
                    .taglib_mappings
                    .push(parse_taglib_mapping(reader)?),
                other => {
                    tracing::warn!(
                        parent = "jsp-config",
                        element = %other,
                        "unmodelled element inside <jsp-config>; skipping"
                    );
                    skip(reader, &other)?;
                }
            },
            Event::End(e) if local(e.name()) == "jsp-config" => break,
            Event::Eof => {
                return Err(Error::config(
                    "unexpected EOF inside <jsp-config> in web.xml",
                ))
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(descriptor)
}

/// Walk a full `web.xml` document and extract its `<jsp-config>` element.
///
/// # Errors
///
/// Returns [`Error::Config`] when the document cannot be tokenised.
fn parse_jsp_config(xml: &str) -> Result<JspConfigDescriptor> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut descriptor = JspConfigDescriptor::default();
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
                    "jsp-config" => descriptor = parse_jsp_config_subtree(&mut reader)?,
                    // Every other top-level element is irrelevant here.
                    other => skip(&mut reader, other)?,
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(descriptor)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<web-app xmlns="http://xmlns.jcp.org/xml/ns/javaee" version="4.0">
  <display-name>JSP Config Sample</display-name>
  <servlet>
    <servlet-name>hello</servlet-name>
    <servlet-class>com.example.HelloServlet</servlet-class>
  </servlet>
  <jsp-config>
    <taglib>
      <taglib-uri>http://example.com/tags/core</taglib-uri>
      <taglib-location>/WEB-INF/tld/core.tld</taglib-location>
    </taglib>
    <taglib>
      <taglib-uri>http://example.com/tags/fmt</taglib-uri>
      <taglib-location>/WEB-INF/tld/fmt.tld</taglib-location>
    </taglib>
    <jsp-property-group>
      <url-pattern>*.jspx</url-pattern>
      <is-xml>true</is-xml>
      <el-ignored>false</el-ignored>
      <page-encoding>UTF-8</page-encoding>
      <include-prelude>/WEB-INF/jsp/prelude.jspf</include-prelude>
      <include-coda>/WEB-INF/jsp/coda.jspf</include-coda>
      <trim-directive-whitespaces>true</trim-directive-whitespaces>
    </jsp-property-group>
    <jsp-property-group>
      <url-pattern>/secure/*</url-pattern>
      <scripting-invalid>true</scripting-invalid>
      <default-content-type>text/html</default-content-type>
      <buffer>16kb</buffer>
    </jsp-property-group>
  </jsp-config>
  <welcome-file-list>
    <welcome-file>index.jsp</welcome-file>
  </welcome-file-list>
</web-app>
"#;

    #[test]
    fn parses_jsp_config_block() {
        let cfg = JspConfigDescriptor::from_web_xml_str(SAMPLE).expect("should parse");

        assert_eq!(cfg.taglib_mappings.len(), 2);
        assert_eq!(
            cfg.taglib_mappings[0].taglib_uri,
            "http://example.com/tags/core"
        );
        assert_eq!(
            cfg.taglib_mappings[0].taglib_location,
            "/WEB-INF/tld/core.tld"
        );
        assert_eq!(
            cfg.taglib_mappings[1].taglib_uri,
            "http://example.com/tags/fmt"
        );

        assert_eq!(cfg.property_groups.len(), 2);

        let jspx = &cfg.property_groups[0];
        assert_eq!(jspx.url_patterns, vec!["*.jspx"]);
        assert_eq!(jspx.is_xml, Some(true));
        assert_eq!(jspx.el_ignored, Some(false));
        assert_eq!(jspx.page_encoding.as_deref(), Some("UTF-8"));
        assert_eq!(jspx.include_preludes, vec!["/WEB-INF/jsp/prelude.jspf"]);
        assert_eq!(jspx.include_codas, vec!["/WEB-INF/jsp/coda.jspf"]);
        assert_eq!(jspx.trim_directive_whitespaces, Some(true));
        assert_eq!(jspx.scripting_invalid, None);

        let secure = &cfg.property_groups[1];
        assert_eq!(secure.url_patterns, vec!["/secure/*"]);
        assert_eq!(secure.scripting_invalid, Some(true));
        assert_eq!(secure.default_content_type.as_deref(), Some("text/html"));
        assert_eq!(secure.buffer.as_deref(), Some("16kb"));
        assert_eq!(secure.is_xml, None);
    }

    #[test]
    fn web_xml_without_jsp_config_is_empty() {
        let cfg = JspConfigDescriptor::from_web_xml_str(
            "<web-app><servlet><servlet-name>x</servlet-name></servlet></web-app>",
        )
        .expect("should parse");
        assert!(cfg.property_groups.is_empty());
        assert!(cfg.taglib_mappings.is_empty());
    }

    #[test]
    fn malformed_xml_is_an_error() {
        let err = JspConfigDescriptor::from_web_xml_str("<web-app><jsp-config>").unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn property_group_for_extension_pattern() {
        let cfg = JspConfigDescriptor::from_web_xml_str(SAMPLE).unwrap();

        let group = cfg
            .property_group_for("/views/report.jspx")
            .expect("*.jspx should match");
        assert_eq!(group.is_xml, Some(true));

        // A plain `.jsp` page is not covered by the `*.jspx` group.
        assert!(cfg.property_group_for("/views/report.jsp").is_none());
    }

    #[test]
    fn property_group_for_prefix_pattern() {
        let cfg = JspConfigDescriptor::from_web_xml_str(SAMPLE).unwrap();

        let nested = cfg
            .property_group_for("/secure/admin/panel.jsp")
            .expect("/secure/* should match nested paths");
        assert_eq!(nested.scripting_invalid, Some(true));

        // `/secure/*` also matches the bare prefix path itself.
        let bare = cfg
            .property_group_for("/secure")
            .expect("/secure/* should match the prefix itself");
        assert_eq!(bare.buffer.as_deref(), Some("16kb"));

        // An unrelated path matches nothing.
        assert!(cfg.property_group_for("/public/home.html").is_none());
    }

    #[test]
    fn exact_match_beats_prefix_and_extension() {
        let cfg = JspConfigDescriptor {
            property_groups: vec![
                JspPropertyGroup {
                    url_patterns: vec!["*.jsp".to_string()],
                    buffer: Some("ext".to_string()),
                    ..Default::default()
                },
                JspPropertyGroup {
                    url_patterns: vec!["/app/*".to_string()],
                    buffer: Some("prefix".to_string()),
                    ..Default::default()
                },
                JspPropertyGroup {
                    url_patterns: vec!["/app/index.jsp".to_string()],
                    buffer: Some("exact".to_string()),
                    ..Default::default()
                },
            ],
            taglib_mappings: Vec::new(),
        };

        let group = cfg.property_group_for("/app/index.jsp").unwrap();
        assert_eq!(group.buffer.as_deref(), Some("exact"));

        // No exact match: the path-prefix group wins over the extension one.
        let group = cfg.property_group_for("/app/other.jsp").unwrap();
        assert_eq!(group.buffer.as_deref(), Some("prefix"));
    }
}
