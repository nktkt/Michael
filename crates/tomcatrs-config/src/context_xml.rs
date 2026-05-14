//! Parser for a web application's `META-INF/context.xml`.
//!
//! A `context.xml` describes a single deployed application: the path it is
//! mounted at, where its document base lives, and whether it should be hot
//! reloaded. This module turns that file into a [`ContextConfig`]. As with
//! [`crate::server_xml`], unknown attributes and nested elements (`<Resource>`,
//! `<Parameter>`, `<Manager>`, …) are logged and skipped rather than rejected.

use quick_xml::events::Event;
use quick_xml::Reader;
use tomcatrs_core::{Error, Result};

use crate::server_xml;
use crate::ContextConfig;

impl ContextConfig {
    /// Parse a `context.xml` document held in memory into a [`ContextConfig`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] if the XML is malformed or contains no
    /// `<Context>` element.
    pub fn from_xml_str(xml: &str) -> Result<ContextConfig> {
        parse_context_xml(xml)
    }

    /// Parse a `context.xml` document from a file on disk.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the file cannot be read, or [`Error::Config`]
    /// if its contents are malformed.
    pub fn from_xml_file(path: impl AsRef<std::path::Path>) -> Result<ContextConfig> {
        let xml = std::fs::read_to_string(path.as_ref())?;
        Self::from_xml_str(&xml)
    }
}

/// Parse a `context.xml` document, returning the first `<Context>` found.
///
/// # Errors
///
/// Returns [`Error::Config`] when the document cannot be tokenised or has no
/// `<Context>` element.
pub fn parse_context_xml(xml: &str) -> Result<ContextConfig> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut context: Option<ContextConfig> = None;
    let mut buf = Vec::new();

    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("context.xml parse error: {e}")))?
        {
            Event::Start(start) | Event::Empty(start) => {
                let name = String::from_utf8_lossy(start.name().local_name().as_ref()).into_owned();
                if name == "Context" {
                    if context.is_none() {
                        context = Some(server_xml::parse_context_attrs(&start)?);
                    } else {
                        tracing::warn!(
                            "multiple <Context> elements in context.xml; using the first"
                        );
                    }
                } else if context.is_some() {
                    tracing::warn!(
                        element = %name,
                        "unmodelled element inside <Context>; skipping"
                    );
                } else {
                    tracing::warn!(
                        element = %name,
                        "unexpected element in context.xml before <Context>; skipping"
                    );
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    context.ok_or_else(|| Error::config("context.xml contains no <Context> element"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parses_context_xml() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
            <Context path="/shop" docBase="shop" reloadable="true" antiResourceLocking="false">
                <Resource name="jdbc/Shop" auth="Container" type="javax.sql.DataSource" />
                <Parameter name="theme" value="dark" />
            </Context>"#;
        let ctx = parse_context_xml(xml).expect("should parse");
        assert_eq!(ctx.path, "/shop");
        assert_eq!(ctx.doc_base, PathBuf::from("shop"));
        assert!(ctx.reloadable);
    }

    #[test]
    fn defaults_when_attributes_absent() {
        let ctx = parse_context_xml("<Context/>").expect("should parse");
        assert_eq!(ctx.path, "");
        assert_eq!(ctx.doc_base, PathBuf::new());
        assert!(!ctx.reloadable);
    }

    #[test]
    fn missing_context_element_is_an_error() {
        assert!(parse_context_xml("<NotAContext/>").is_err());
    }
}
