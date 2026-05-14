//! Tag Library Descriptor (`.tld`) parsing and discovery.
//!
//! A *Tag Library Descriptor* is an XML document (root element `<taglib>`)
//! that declares the custom JSP tags, EL functions, and lifecycle listeners a
//! tag library provides. JSP pages reference a library by its `<uri>` (via the
//! `<%@ taglib %>` directive); the container is responsible for resolving that
//! URI to a descriptor.
//!
//! This module provides:
//!
//! * [`TagLibrary`] and its component types ([`TagDef`], [`TagAttribute`],
//!   [`FunctionDef`]) — the in-memory model of a parsed `.tld`.
//! * [`TagLibrary::from_tld_str`] / [`TagLibrary::from_tld_file`] — real
//!   `quick-xml` parsing, tolerant of unknown/unmodelled elements (they are
//!   logged at `warn` and skipped rather than treated as errors).
//! * [`TldScanner`] — discovery of every `.tld` reachable from a deployed
//!   webapp: loose descriptors under `WEB-INF/` and descriptors packaged inside
//!   `WEB-INF/lib/*.jar` under `META-INF/`.
//! * [`TagLibraryMap`] — a `uri → TagLibrary` lookup table built by the scanner.
//!
//! The parser deliberately models only the subset of the TLD schema Tomcat-RS
//! needs today; descriptors are full of optional documentation elements
//! (`<description>`, `<display-name>`, `<icon>`, `<example>`, …) which carry no
//! runtime meaning and are skipped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::Reader;
use tomcatrs_core::{Error, Result};

/// A single attribute of a custom tag (`<attribute>` inside `<tag>`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagAttribute {
    /// Attribute name as written on the tag in a JSP page.
    pub name: String,
    /// Whether the attribute must be supplied (`<required>`); defaults to
    /// `false` when the element is absent.
    pub required: bool,
    /// Whether the attribute accepts a runtime expression value
    /// (`<rtexprvalue>`); defaults to `false` when the element is absent.
    pub rtexprvalue: bool,
    /// The declared Java type of the attribute (`<type>`), if specified.
    pub type_: Option<String>,
}

/// A scripting/EL variable a tag exposes into the page (`<variable>`).
///
/// Only the variable's name (`<name-given>`, falling back to
/// `<name-from-attribute>`) is modelled; the scope and type metadata are not
/// needed by the current runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagVariable {
    /// The variable name introduced into the page, or the name of the
    /// attribute that supplies it.
    pub name: String,
}

/// A custom tag declaration (`<tag>`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagDef {
    /// The tag's element name (`<name>`), used after the library prefix.
    pub name: String,
    /// Fully-qualified handler class (`<tag-class>`).
    pub tag_class: String,
    /// Body content model (`<body-content>`): `empty`, `scriptless`, `JSP`, or
    /// `tagdependent`. Empty when the descriptor omits it.
    pub body_content: String,
    /// Declared attributes, in document order.
    pub attributes: Vec<TagAttribute>,
    /// Scripting/EL variables the tag exposes, in document order.
    pub variables: Vec<TagVariable>,
    /// Whether the tag accepts arbitrary extra attributes
    /// (`<dynamic-attributes>`).
    pub dynamic_attributes: bool,
}

/// An EL function declaration (`<function>`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FunctionDef {
    /// Function name as used in EL (`<name>`).
    pub name: String,
    /// Fully-qualified class providing the implementation
    /// (`<function-class>`).
    pub function_class: String,
    /// The Java method signature of the implementation
    /// (`<function-signature>`).
    pub function_signature: String,
}

/// A parsed Tag Library Descriptor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagLibrary {
    /// The library's own version (`<tlib-version>`).
    pub tlib_version: String,
    /// A short, human-friendly name / default prefix (`<short-name>`).
    pub short_name: String,
    /// The canonical URI JSP pages use to reference this library (`<uri>`).
    /// May be empty: jar-packaged libraries are often resolved by location
    /// instead.
    pub uri: String,
    /// Every custom tag the library declares, in document order.
    pub tags: Vec<TagDef>,
    /// Every EL function the library declares, in document order.
    pub functions: Vec<FunctionDef>,
    /// Fully-qualified `<listener-class>` names registered by the library.
    pub listeners: Vec<String>,
}

impl TagLibrary {
    /// Parse a `.tld` descriptor held in memory.
    ///
    /// Unknown elements are logged at `warn` and skipped; the parser only
    /// fails on XML that cannot be tokenised or that ends unexpectedly inside
    /// an element being read.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] if the XML is malformed.
    pub fn from_tld_str(xml: &str) -> Result<TagLibrary> {
        parse_tld(xml)
    }

    /// Parse a `.tld` descriptor from a file on disk.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the file cannot be read, or [`Error::Config`]
    /// if its contents are malformed.
    pub fn from_tld_file(path: &Path) -> Result<TagLibrary> {
        let xml = std::fs::read_to_string(path)?;
        Self::from_tld_str(&xml)
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
            .map_err(|e| Error::config(format!("tld parse error: {e}")))?
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
                    "unexpected EOF inside <{tag}> in tld"
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
            .map_err(|e| Error::config(format!("tld parse error: {e}")))?
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
                    "unexpected EOF while skipping <{tag}> in tld"
                )))
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

/// Parse the trimmed text of an element as a TLD boolean.
///
/// The TLD schema accepts `true`/`false`/`yes`/`no`; anything else is logged
/// and treated as `false`.
fn read_bool(reader: &mut Reader<&[u8]>, tag: &str) -> Result<bool> {
    let raw = read_text(reader, tag)?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" => Ok(true),
        "false" | "no" => Ok(false),
        other => {
            tracing::warn!(
                element = tag,
                value = other,
                "invalid boolean in tld; treating as false"
            );
            Ok(false)
        }
    }
}

/// Parse an `<attribute>` subtree.
fn parse_attribute(reader: &mut Reader<&[u8]>) -> Result<TagAttribute> {
    let mut attr = TagAttribute::default();
    let mut buf = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("tld parse error: {e}")))?
        {
            Event::Start(s) => match local(s.name()).as_str() {
                "name" => attr.name = read_text(reader, "name")?,
                "required" => attr.required = read_bool(reader, "required")?,
                "rtexprvalue" => attr.rtexprvalue = read_bool(reader, "rtexprvalue")?,
                "type" => attr.type_ = Some(read_text(reader, "type")?),
                other => skip(reader, &other)?,
            },
            Event::End(e) if local(e.name()) == "attribute" => break,
            Event::Eof => return Err(Error::config("unexpected EOF inside <attribute> in tld")),
            _ => {}
        }
        buf.clear();
    }
    Ok(attr)
}

/// Parse a `<variable>` subtree.
fn parse_variable(reader: &mut Reader<&[u8]>) -> Result<TagVariable> {
    let mut var = TagVariable::default();
    let mut buf = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("tld parse error: {e}")))?
        {
            Event::Start(s) => match local(s.name()).as_str() {
                "name-given" => var.name = read_text(reader, "name-given")?,
                // Only used when `<name-given>` is absent.
                "name-from-attribute" => {
                    let from = read_text(reader, "name-from-attribute")?;
                    if var.name.is_empty() {
                        var.name = from;
                    }
                }
                other => skip(reader, &other)?,
            },
            Event::End(e) if local(e.name()) == "variable" => break,
            Event::Eof => return Err(Error::config("unexpected EOF inside <variable> in tld")),
            _ => {}
        }
        buf.clear();
    }
    Ok(var)
}

/// Parse a `<tag>` subtree.
fn parse_tag(reader: &mut Reader<&[u8]>) -> Result<TagDef> {
    let mut tag = TagDef::default();
    let mut buf = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("tld parse error: {e}")))?
        {
            Event::Start(s) => match local(s.name()).as_str() {
                "name" => tag.name = read_text(reader, "name")?,
                "tag-class" => tag.tag_class = read_text(reader, "tag-class")?,
                "body-content" => tag.body_content = read_text(reader, "body-content")?,
                "attribute" => tag.attributes.push(parse_attribute(reader)?),
                "variable" => tag.variables.push(parse_variable(reader)?),
                "dynamic-attributes" => {
                    tag.dynamic_attributes = read_bool(reader, "dynamic-attributes")?
                }
                other => {
                    tracing::warn!(
                        parent = "tag",
                        element = %other,
                        "unmodelled element inside <tag>; skipping"
                    );
                    skip(reader, &other)?;
                }
            },
            Event::End(e) if local(e.name()) == "tag" => break,
            Event::Eof => return Err(Error::config("unexpected EOF inside <tag> in tld")),
            _ => {}
        }
        buf.clear();
    }
    Ok(tag)
}

/// Parse a `<function>` subtree.
fn parse_function(reader: &mut Reader<&[u8]>) -> Result<FunctionDef> {
    let mut func = FunctionDef::default();
    let mut buf = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("tld parse error: {e}")))?
        {
            Event::Start(s) => match local(s.name()).as_str() {
                "name" => func.name = read_text(reader, "name")?,
                "function-class" => func.function_class = read_text(reader, "function-class")?,
                "function-signature" => {
                    func.function_signature = read_text(reader, "function-signature")?
                }
                other => skip(reader, &other)?,
            },
            Event::End(e) if local(e.name()) == "function" => break,
            Event::Eof => return Err(Error::config("unexpected EOF inside <function> in tld")),
            _ => {}
        }
        buf.clear();
    }
    Ok(func)
}

/// Parse a `<listener>` subtree into its `<listener-class>` name.
fn parse_listener(reader: &mut Reader<&[u8]>) -> Result<String> {
    let mut class = String::new();
    let mut buf = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("tld parse error: {e}")))?
        {
            Event::Start(s) => match local(s.name()).as_str() {
                "listener-class" => class = read_text(reader, "listener-class")?,
                other => skip(reader, &other)?,
            },
            Event::End(e) if local(e.name()) == "listener" => break,
            Event::Eof => return Err(Error::config("unexpected EOF inside <listener> in tld")),
            _ => {}
        }
        buf.clear();
    }
    Ok(class)
}

/// Parse a complete `.tld` document.
///
/// # Errors
///
/// Returns [`Error::Config`] when the document cannot be tokenised.
fn parse_tld(xml: &str) -> Result<TagLibrary> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut lib = TagLibrary::default();
    let mut buf = Vec::new();

    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::config(format!("tld parse error: {e}")))?
        {
            Event::Start(start) => {
                let name = local(start.name());
                match name.as_str() {
                    // The document root; descend into it.
                    "taglib" => {}
                    "tlib-version" => lib.tlib_version = read_text(&mut reader, "tlib-version")?,
                    "short-name" => lib.short_name = read_text(&mut reader, "short-name")?,
                    "uri" => lib.uri = read_text(&mut reader, "uri")?,
                    "tag" => lib.tags.push(parse_tag(&mut reader)?),
                    "function" => lib.functions.push(parse_function(&mut reader)?),
                    "listener" => {
                        let class = parse_listener(&mut reader)?;
                        if !class.is_empty() {
                            lib.listeners.push(class);
                        }
                    }
                    other => {
                        tracing::warn!(element = other, "unmodelled element in tld; skipping");
                        skip(&mut reader, other)?;
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(lib)
}

/// A `uri → `[`TagLibrary`] lookup table.
///
/// The map keys on the library URI: either the descriptor's own `<uri>`
/// element, or — for a jar-packaged descriptor with no `<uri>` — the archive
/// path of the `.tld` so the library is at least addressable.
#[derive(Debug, Clone, Default)]
pub struct TagLibraryMap {
    by_uri: HashMap<String, TagLibrary>,
}

impl TagLibraryMap {
    /// Create an empty map.
    pub fn new() -> TagLibraryMap {
        TagLibraryMap::default()
    }

    /// Insert a library, keyed by `uri`.
    ///
    /// A later insert with the same URI replaces the earlier one and is logged
    /// at `warn`, mirroring Tomcat's "last definition wins" behaviour.
    pub fn insert(&mut self, uri: impl Into<String>, lib: TagLibrary) {
        let uri = uri.into();
        if self.by_uri.insert(uri.clone(), lib).is_some() {
            tracing::warn!(uri = %uri, "duplicate tag library uri; overriding previous definition");
        }
    }

    /// Look up a library by its URI.
    pub fn get(&self, uri: &str) -> Option<&TagLibrary> {
        self.by_uri.get(uri)
    }

    /// Number of libraries registered.
    pub fn len(&self) -> usize {
        self.by_uri.len()
    }

    /// Whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.by_uri.is_empty()
    }

    /// Iterate over every `(uri, library)` pair.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &TagLibrary)> {
        self.by_uri.iter()
    }
}

/// Discovers Tag Library Descriptors in a deployed web application.
///
/// Per the JSP specification a container must locate `.tld` files in two
/// places:
///
/// * loose under `WEB-INF/` (and any subdirectory of it), and
/// * packaged inside `WEB-INF/lib/*.jar` under `META-INF/` (and any
///   subdirectory of it).
///
/// [`TldScanner::scan_webapp`] walks both and returns a [`TagLibraryMap`]. The
/// scanner is stateless and cheap to create.
#[derive(Debug, Clone, Copy, Default)]
pub struct TldScanner;

impl TldScanner {
    /// Create a new TLD scanner.
    pub fn new() -> TldScanner {
        TldScanner
    }

    /// Scan a webapp root directory for every reachable `.tld`.
    ///
    /// `webapp_root` is the exploded application directory (the one that
    /// contains `WEB-INF/`). A missing `WEB-INF/` directory yields an empty
    /// map. Descriptors that fail to parse are logged at `warn` and skipped —
    /// one broken library should not sink deployment.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if a directory cannot be traversed.
    pub fn scan_webapp(&self, webapp_root: &Path) -> Result<TagLibraryMap> {
        let mut map = TagLibraryMap::new();
        let web_inf = webapp_root.join("WEB-INF");
        if !web_inf.is_dir() {
            return Ok(map);
        }

        // 1. Loose `.tld` files anywhere under `WEB-INF/`.
        let mut loose = Vec::new();
        collect_tld_files(&web_inf, &mut loose)?;
        loose.sort();
        for path in loose {
            match TagLibrary::from_tld_file(&path) {
                Ok(lib) => {
                    let key = if lib.uri.is_empty() {
                        path.display().to_string()
                    } else {
                        lib.uri.clone()
                    };
                    map.insert(key, lib);
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "skipping unparseable tld");
                }
            }
        }

        // 2. `.tld` files packaged in `WEB-INF/lib/*.jar` under `META-INF/`.
        let lib_dir = web_inf.join("lib");
        if lib_dir.is_dir() {
            let mut jars = Vec::new();
            for entry in std::fs::read_dir(&lib_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_file()
                    && path
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("jar"))
                {
                    jars.push(path);
                }
            }
            jars.sort();
            for jar in jars {
                self.scan_jar(&jar, &mut map);
            }
        }

        Ok(map)
    }

    /// Scan a single `.jar` for `META-INF/**/*.tld` entries, inserting every
    /// successfully parsed library into `map`.
    ///
    /// Failures (an unreadable archive, a corrupt or unparseable entry) are
    /// logged at `warn` and skipped; this method never aborts a scan.
    fn scan_jar(&self, jar: &Path, map: &mut TagLibraryMap) {
        use std::io::Read;

        let file = match std::fs::File::open(jar) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(jar = %jar.display(), error = %e, "cannot open jar; skipping");
                return;
            }
        };
        let mut archive = match zip::ZipArchive::new(file) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(
                    jar = %jar.display(),
                    error = %e,
                    "not a readable jar/zip archive; skipping"
                );
                return;
            }
        };

        for i in 0..archive.len() {
            let mut entry = match archive.by_index(i) {
                Ok(entry) => entry,
                Err(e) => {
                    tracing::warn!(
                        jar = %jar.display(),
                        index = i,
                        error = %e,
                        "skipping unreadable jar entry"
                    );
                    continue;
                }
            };

            // Only `META-INF/**/*.tld` entries are tag library descriptors.
            let entry_name = entry.name().to_string();
            let is_tld = entry.is_file()
                && entry_name.starts_with("META-INF/")
                && entry_name.to_ascii_lowercase().ends_with(".tld");
            if !is_tld {
                continue;
            }

            let mut xml = String::new();
            if let Err(e) = entry.read_to_string(&mut xml) {
                tracing::warn!(
                    jar = %jar.display(),
                    entry = %entry_name,
                    error = %e,
                    "cannot read tld entry from jar; skipping"
                );
                continue;
            }

            match TagLibrary::from_tld_str(&xml) {
                Ok(lib) => {
                    // A jar-packaged descriptor without an explicit `<uri>` is
                    // addressed by its `jar!/entry` location.
                    let key = if lib.uri.is_empty() {
                        format!("{}!/{}", jar.display(), entry_name)
                    } else {
                        lib.uri.clone()
                    };
                    map.insert(key, lib);
                }
                Err(e) => {
                    tracing::warn!(
                        jar = %jar.display(),
                        entry = %entry_name,
                        error = %e,
                        "skipping unparseable tld in jar"
                    );
                }
            }
        }
    }
}

/// Recursively collect every `*.tld` file under `dir` into `out`.
fn collect_tld_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_tld_files(&path, out)?;
        } else if file_type.is_file()
            && path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("tld"))
        {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const SAMPLE_TLD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<taglib xmlns="http://java.sun.com/xml/ns/javaee" version="2.1">
  <description>A sample tag library for tests.</description>
  <display-name>Sample Tags</display-name>
  <tlib-version>1.2</tlib-version>
  <short-name>smpl</short-name>
  <uri>http://example.com/tags/sample</uri>
  <listener>
    <listener-class>com.example.tags.SampleListener</listener-class>
  </listener>
  <tag>
    <description>Greets someone.</description>
    <name>greet</name>
    <tag-class>com.example.tags.GreetTag</tag-class>
    <body-content>scriptless</body-content>
    <variable>
      <name-given>result</name-given>
    </variable>
    <attribute>
      <name>name</name>
      <required>true</required>
      <rtexprvalue>true</rtexprvalue>
      <type>java.lang.String</type>
    </attribute>
    <attribute>
      <name>polite</name>
      <required>false</required>
    </attribute>
    <dynamic-attributes>true</dynamic-attributes>
  </tag>
  <tag>
    <name>now</name>
    <tag-class>com.example.tags.NowTag</tag-class>
    <body-content>empty</body-content>
  </tag>
  <function>
    <name>upper</name>
    <function-class>com.example.tags.Functions</function-class>
    <function-signature>java.lang.String upper(java.lang.String)</function-signature>
  </function>
</taglib>
"#;

    /// A unique temp directory for one test, removed by the caller.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tomcatrs-taglib-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_sample_tld() {
        let lib = TagLibrary::from_tld_str(SAMPLE_TLD).expect("should parse");

        assert_eq!(lib.tlib_version, "1.2");
        assert_eq!(lib.short_name, "smpl");
        assert_eq!(lib.uri, "http://example.com/tags/sample");
        assert_eq!(lib.listeners, vec!["com.example.tags.SampleListener"]);

        assert_eq!(lib.tags.len(), 2);
        let greet = &lib.tags[0];
        assert_eq!(greet.name, "greet");
        assert_eq!(greet.tag_class, "com.example.tags.GreetTag");
        assert_eq!(greet.body_content, "scriptless");
        assert!(greet.dynamic_attributes);
        assert_eq!(greet.variables.len(), 1);
        assert_eq!(greet.variables[0].name, "result");

        assert_eq!(greet.attributes.len(), 2);
        let name_attr = &greet.attributes[0];
        assert_eq!(name_attr.name, "name");
        assert!(name_attr.required);
        assert!(name_attr.rtexprvalue);
        assert_eq!(name_attr.type_.as_deref(), Some("java.lang.String"));
        let polite_attr = &greet.attributes[1];
        assert_eq!(polite_attr.name, "polite");
        assert!(!polite_attr.required);
        assert!(!polite_attr.rtexprvalue);
        assert_eq!(polite_attr.type_, None);

        let now = &lib.tags[1];
        assert_eq!(now.name, "now");
        assert_eq!(now.body_content, "empty");
        assert!(!now.dynamic_attributes);

        assert_eq!(lib.functions.len(), 1);
        let func = &lib.functions[0];
        assert_eq!(func.name, "upper");
        assert_eq!(func.function_class, "com.example.tags.Functions");
        assert_eq!(
            func.function_signature,
            "java.lang.String upper(java.lang.String)"
        );
    }

    #[test]
    fn unknown_elements_are_tolerated() {
        let xml = r#"<taglib>
            <tlib-version>1.0</tlib-version>
            <some-future-element><nested>x</nested></some-future-element>
            <short-name>x</short-name>
        </taglib>"#;
        let lib = TagLibrary::from_tld_str(xml).expect("should parse");
        assert_eq!(lib.tlib_version, "1.0");
        assert_eq!(lib.short_name, "x");
    }

    #[test]
    fn malformed_xml_is_an_error() {
        let err = TagLibrary::from_tld_str("<taglib><tag>").unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn tag_library_map_lookup_by_uri() {
        let lib = TagLibrary::from_tld_str(SAMPLE_TLD).unwrap();
        let mut map = TagLibraryMap::new();
        map.insert(lib.uri.clone(), lib);

        assert_eq!(map.len(), 1);
        let found = map
            .get("http://example.com/tags/sample")
            .expect("library should be registered under its uri");
        assert_eq!(found.short_name, "smpl");
        assert!(map.get("http://example.com/tags/missing").is_none());
    }

    #[test]
    fn scan_webapp_finds_loose_and_jar_tlds() {
        let root = temp_dir("scan");
        let web_inf = root.join("WEB-INF");
        let tags_dir = web_inf.join("tags");
        let lib_dir = web_inf.join("lib");
        std::fs::create_dir_all(&tags_dir).unwrap();
        std::fs::create_dir_all(&lib_dir).unwrap();

        // A loose descriptor nested under WEB-INF/tags/.
        std::fs::write(tags_dir.join("sample.tld"), SAMPLE_TLD).unwrap();

        // A jar containing a descriptor under META-INF/.
        let jar_tld = r#"<taglib>
            <tlib-version>3.0</tlib-version>
            <short-name>jarlib</short-name>
            <uri>http://example.com/tags/jar</uri>
            <tag>
                <name>fromjar</name>
                <tag-class>com.example.JarTag</tag-class>
                <body-content>empty</body-content>
            </tag>
        </taglib>"#;
        let jar_path = lib_dir.join("tags.jar");
        let file = std::fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("META-INF/jar.tld", opts).unwrap();
        zip.write_all(jar_tld.as_bytes()).unwrap();
        // A non-tld entry must be ignored.
        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Manifest-Version: 1.0\n").unwrap();
        zip.finish().unwrap();

        let map = TldScanner::new().scan_webapp(&root).expect("scan");
        assert_eq!(map.len(), 2);

        let loose = map
            .get("http://example.com/tags/sample")
            .expect("loose tld registered by uri");
        assert_eq!(loose.short_name, "smpl");

        let from_jar = map
            .get("http://example.com/tags/jar")
            .expect("jar tld registered by uri");
        assert_eq!(from_jar.short_name, "jarlib");
        assert_eq!(from_jar.tags.len(), 1);
        assert_eq!(from_jar.tags[0].name, "fromjar");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn scan_webapp_without_web_inf_is_empty() {
        let root = temp_dir("nowebinf");
        let map = TldScanner::new().scan_webapp(&root).expect("scan");
        assert!(map.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }
}
