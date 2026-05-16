//! Annotation model, Java class-file parser, and annotation index for
//! Servlet-spec class annotations.
//!
//! The Servlet specification lets applications declare components with
//! annotations instead of `web.xml`: `@WebServlet`, `@WebFilter`,
//! `@WebListener`, and so on. Discovering them requires reading Java class
//! files (or `.jar` entries) on the application classpath.
//!
//! Tomcat-RS performs this discovery **without a JVM**: a Java `.class` file
//! is a well-specified binary format whose constant pool and
//! `RuntimeVisibleAnnotations` attribute carry everything needed to find and
//! decode `@WebServlet` / `@WebFilter` / `@WebListener`. This module contains:
//!
//! * the data model — [`AnnotationIndex`] and the per-annotation info structs;
//! * a focused, hand-rolled [`ClassFile`] parser that reads the magic, version,
//!   constant pool, `this_class`, and class-level attributes;
//! * [`ClassFile::servlet_annotations`], which walks `RuntimeVisibleAnnotations`
//!   and turns the relevant annotations into the info structs.
//!
//! The [`ClassScanner`](crate::scanner::ClassScanner) in the sibling
//! [`scanner`](crate::scanner) module drives this parser across a webapp's
//! `WEB-INF/classes` tree and `WEB-INF/lib/*.jar` files.
//!
//! ## Scope of the parser
//!
//! Only the subset of the class-file format required for class-level annotation
//! discovery is implemented. Method/field tables and the code attribute are
//! skipped wholesale by length. Constant-pool entry kinds that cannot appear in
//! the bytes we need to resolve (method handles, dynamics, …) are recorded as
//! opaque placeholders so indices stay aligned, but are never dereferenced.

use tomcatrs_core::{Error, Result};

/// Key/value initialisation parameter, as carried by `@WebServlet` /
/// `@WebFilter` `initParams` (each `@WebInitParam` has a `name` and `value`).
pub type InitParam = (String, String);

/// Metadata extracted from one `@WebServlet` annotation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WebServletInfo {
    /// Fully-qualified (dotted) name of the annotated servlet class.
    pub class_name: String,
    /// The servlet name (`name` element, defaulting to the class name).
    pub servlet_name: String,
    /// URL patterns the servlet is mapped to (`value` / `urlPatterns`).
    pub url_patterns: Vec<String>,
    /// `loadOnStartup` order, if specified.
    pub load_on_startup: Option<i32>,
    /// Whether the servlet declared `asyncSupported = true`.
    pub async_supported: bool,
    /// `@WebInitParam` entries, in declaration order.
    pub init_params: Vec<InitParam>,
}

/// Metadata extracted from one `@WebFilter` annotation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WebFilterInfo {
    /// Fully-qualified (dotted) name of the annotated filter class.
    pub class_name: String,
    /// The filter name (`filterName`, defaulting to the class name).
    pub filter_name: String,
    /// URL patterns the filter intercepts (`value` / `urlPatterns`).
    pub url_patterns: Vec<String>,
    /// Servlet names the filter intercepts (`servletNames`).
    pub servlet_names: Vec<String>,
    /// Whether the filter declared `asyncSupported = true`.
    pub async_supported: bool,
    /// `@WebInitParam` entries, in declaration order.
    pub init_params: Vec<InitParam>,
}

/// Metadata extracted from one `@WebListener` annotation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WebListenerInfo {
    /// Fully-qualified (dotted) name of the annotated listener class.
    pub class_name: String,
}

/// Compact, classpath-graph-friendly view of a parsed Java class.
///
/// Carries the four facts the Servlet 6 `@HandlesTypes` rule needs:
/// the class's own name, the name of its direct superclass (if any),
/// the names of every interface it directly implements (or, for an
/// interface, directly extends), and the names of every class-level
/// annotation on it.
///
/// All names use the dotted, fully-qualified form — `com.example.Foo`,
/// `java.lang.Object` — matching `java.lang.Class.getName()` and the
/// existing [`ClassFile::class_name`] convention. This is the FQCN
/// format the [`crate::scanner::ClassgraphIndex`] queries against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassMeta {
    /// Dotted FQCN of this class, e.g. `com.example.Foo`.
    pub name: String,
    /// Dotted FQCN of this class's direct superclass, e.g.
    /// `java.lang.Object`. `None` for `java.lang.Object` itself.
    pub super_name: Option<String>,
    /// Dotted FQCNs of every interface this class directly implements
    /// (for an interface, directly extends).
    pub interfaces: Vec<String>,
    /// Dotted FQCNs of every class-level annotation on this class.
    pub annotations: Vec<String>,
}

/// The aggregate result of scanning a web application's classpath for
/// Servlet-spec annotations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnnotationIndex {
    /// Every discovered `@WebServlet`.
    pub web_servlets: Vec<WebServletInfo>,
    /// Every discovered `@WebFilter`.
    pub web_filters: Vec<WebFilterInfo>,
    /// Every discovered `@WebListener`.
    pub web_listeners: Vec<WebListenerInfo>,
}

impl AnnotationIndex {
    /// Create an empty index.
    pub fn new() -> AnnotationIndex {
        AnnotationIndex::default()
    }

    /// Whether the index contains no discovered annotations.
    pub fn is_empty(&self) -> bool {
        self.web_servlets.is_empty() && self.web_filters.is_empty() && self.web_listeners.is_empty()
    }

    /// Total number of annotated components recorded across all categories.
    pub fn len(&self) -> usize {
        self.web_servlets.len() + self.web_filters.len() + self.web_listeners.len()
    }

    /// Merge `other` into `self`, appending all of its discovered components.
    ///
    /// Ordering within each category is preserved: `self`'s entries come first,
    /// then `other`'s. No de-duplication is performed — callers that scan
    /// overlapping classpath roots are responsible for not doing so.
    pub fn merge(&mut self, other: AnnotationIndex) {
        self.web_servlets.extend(other.web_servlets);
        self.web_filters.extend(other.web_filters);
        self.web_listeners.extend(other.web_listeners);
    }

    /// Consume two indexes and return their merge. Convenience over
    /// [`AnnotationIndex::merge`] for fold-style accumulation.
    pub fn merged(mut self, other: AnnotationIndex) -> AnnotationIndex {
        self.merge(other);
        self
    }
}

// ---------------------------------------------------------------------------
// Java `.class` file parser
// ---------------------------------------------------------------------------

/// Fully-qualified internal names of the Servlet-spec annotations this parser
/// recognises. These are the `jakarta.*` (Servlet 5+) names; the legacy
/// `javax.*` equivalents are also accepted for applications that have not yet
/// migrated.
const WEBSERVLET_DESCRIPTORS: &[&str] = &[
    "Ljakarta/servlet/annotation/WebServlet;",
    "Ljavax/servlet/annotation/WebServlet;",
];
const WEBFILTER_DESCRIPTORS: &[&str] = &[
    "Ljakarta/servlet/annotation/WebFilter;",
    "Ljavax/servlet/annotation/WebFilter;",
];
const WEBLISTENER_DESCRIPTORS: &[&str] = &[
    "Ljakarta/servlet/annotation/WebListener;",
    "Ljavax/servlet/annotation/WebListener;",
];
const WEBINITPARAM_DESCRIPTORS: &[&str] = &[
    "Ljakarta/servlet/annotation/WebInitParam;",
    "Ljavax/servlet/annotation/WebInitParam;",
];

/// One entry in a class file's constant pool.
///
/// Only the kinds this parser needs to resolve names are decoded; every other
/// kind is kept as [`Constant::Other`] purely to preserve index alignment
/// (`Long`/`Double` additionally occupy two slots, per the JVM spec).
///
/// `StringRef` and `NameAndType` are decoded for spec-completeness and so that
/// pool indices stay correctly aligned; the Servlet annotations themselves
/// reference `Utf8` and `Class` constants directly, so the inner indices of
/// these two variants are not currently dereferenced.
#[derive(Debug, Clone)]
#[allow(dead_code)]
enum Constant {
    /// A modified-UTF8 string (already decoded to a Rust `String`).
    Utf8(String),
    /// `CONSTANT_Class`: index of the UTF8 holding the internal class name.
    Class(u16),
    /// `CONSTANT_String`: index of the UTF8 holding the string value.
    StringRef(u16),
    /// `CONSTANT_NameAndType`: (name UTF8 index, descriptor UTF8 index).
    NameAndType(u16, u16),
    /// `CONSTANT_Integer`.
    Integer(i32),
    /// `CONSTANT_Long`.
    Long(i64),
    /// `CONSTANT_Float`.
    Float(f32),
    /// `CONSTANT_Double`.
    Double(f64),
    /// Any other constant kind — present only to keep pool indices aligned.
    Other,
    /// The unused second slot of a `Long`/`Double` entry.
    Unusable,
}

/// A decoded element-value of an annotation (JVMS §4.7.16.1).
///
/// Only the value shapes that the Servlet annotations actually use are decoded
/// in detail; richer shapes (nested annotations other than `@WebInitParam`,
/// class literals) are kept but not deeply inspected.
#[derive(Debug, Clone)]
enum ElementValue {
    /// A constant: string, int, bool-as-int, etc., rendered to a `String`
    /// where it makes sense plus the raw integer when it was an integer.
    Const {
        string: Option<String>,
        int: Option<i64>,
    },
    /// An array of element-values.
    Array(Vec<ElementValue>),
    /// A nested annotation: its type descriptor plus its element pairs.
    Annotation(DecodedAnnotation),
    /// An enum constant, a class literal, or anything else we keep opaque.
    Other,
}

/// A decoded `RuntimeVisibleAnnotations` entry: a type descriptor plus its
/// `(element-name, element-value)` pairs.
#[derive(Debug, Clone)]
struct DecodedAnnotation {
    /// The annotation's field descriptor, e.g.
    /// `Ljakarta/servlet/annotation/WebServlet;`.
    descriptor: String,
    /// Element name → value pairs, in declaration order.
    elements: Vec<(String, ElementValue)>,
}

impl DecodedAnnotation {
    /// Look up a named element's value.
    fn get(&self, name: &str) -> Option<&ElementValue> {
        self.elements
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }
}

/// A parsed Java `.class` file — only the parts needed for class-level
/// annotation discovery.
#[derive(Debug, Clone)]
pub struct ClassFile {
    /// Major bytecode version (e.g. `52` for Java 8, `65` for Java 21).
    pub major_version: u16,
    /// Minor bytecode version.
    pub minor_version: u16,
    /// The dotted, fully-qualified name of this class (e.g. `com.example.Foo`).
    pub class_name: String,
    /// The dotted, fully-qualified name of this class's direct superclass
    /// (e.g. `java.lang.Object`), or `None` for `java.lang.Object` itself.
    pub super_name: Option<String>,
    /// The dotted, fully-qualified names of every interface this class
    /// directly implements (or, for an interface, directly extends).
    pub interface_names: Vec<String>,
    /// The decoded constant pool, indexed from 1 (slot 0 is a placeholder).
    ///
    /// Retained on the parsed `ClassFile` for inspection and testing; the
    /// annotation-decoding paths resolve everything they need while the pool is
    /// still in scope during [`ClassFile::parse`].
    #[allow(dead_code)]
    constant_pool: Vec<Constant>,
    /// Class-level `RuntimeVisibleAnnotations`, already decoded.
    annotations: Vec<DecodedAnnotation>,
}

/// A little-endian-free cursor over a class file's big-endian byte stream.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Cursor<'a> {
        Cursor { bytes, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }

    fn u8(&mut self) -> Result<u8> {
        let b = *self.bytes.get(self.pos).ok_or_else(|| truncated("u8"))?;
        self.pos += 1;
        Ok(b)
    }

    fn u16(&mut self) -> Result<u16> {
        if self.remaining() < 2 {
            return Err(truncated("u16"));
        }
        let v = u16::from_be_bytes([self.bytes[self.pos], self.bytes[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    fn u32(&mut self) -> Result<u32> {
        if self.remaining() < 4 {
            return Err(truncated("u32"));
        }
        let v = u32::from_be_bytes([
            self.bytes[self.pos],
            self.bytes[self.pos + 1],
            self.bytes[self.pos + 2],
            self.bytes[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(v)
    }

    fn i32(&mut self) -> Result<i32> {
        self.u32().map(|v| v as i32)
    }

    fn i64(&mut self) -> Result<i64> {
        let hi = self.u32()? as u64;
        let lo = self.u32()? as u64;
        Ok(((hi << 32) | lo) as i64)
    }

    fn f32(&mut self) -> Result<f32> {
        self.u32().map(f32::from_bits)
    }

    fn f64(&mut self) -> Result<f64> {
        let hi = self.u32()? as u64;
        let lo = self.u32()? as u64;
        Ok(f64::from_bits((hi << 32) | lo))
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(truncated("byte slice"));
        }
        let slice = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn skip(&mut self, n: usize) -> Result<()> {
        if self.remaining() < n {
            return Err(truncated("skip"));
        }
        self.pos += n;
        Ok(())
    }
}

/// Build a "class file truncated" deployment error.
fn truncated(what: &str) -> Error {
    Error::Deployment(format!(
        "malformed class file: truncated while reading {what}"
    ))
}

/// Build a generic "malformed class file" deployment error.
fn malformed(msg: impl Into<String>) -> Error {
    Error::Deployment(format!("malformed class file: {}", msg.into()))
}

impl ClassFile {
    /// Parse a Java `.class` file from its raw bytes.
    ///
    /// This reads the magic number, the version, the full constant pool, the
    /// access flags, `this_class`, `super_class`, the interface list, then
    /// skips the field and method tables by length and finally walks the
    /// class-level attributes to capture `RuntimeVisibleAnnotations`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Deployment`] if `bytes` is not a well-formed class file
    /// (bad magic, truncated, dangling constant-pool references, …).
    pub fn parse(bytes: &[u8]) -> Result<ClassFile> {
        let mut c = Cursor::new(bytes);

        let magic = c.u32()?;
        if magic != 0xCAFE_BABE {
            return Err(malformed(format!(
                "bad magic 0x{magic:08X} (expected 0xCAFEBABE)"
            )));
        }

        let minor_version = c.u16()?;
        let major_version = c.u16()?;

        let constant_pool = parse_constant_pool(&mut c)?;

        let _access_flags = c.u16()?;
        let this_class = c.u16()?;
        let super_class = c.u16()?;

        let interfaces_count = c.u16()?;
        let mut interface_indices = Vec::with_capacity(interfaces_count as usize);
        for _ in 0..interfaces_count {
            interface_indices.push(c.u16()?);
        }

        skip_member_table(&mut c, &constant_pool)?; // fields
        skip_member_table(&mut c, &constant_pool)?; // methods

        let annotations = parse_class_attributes(&mut c, &constant_pool)?;

        let class_name = resolve_class_name(&constant_pool, this_class)?;
        // `super_class` is 0 only for `java.lang.Object` (and module-info).
        let super_name = if super_class == 0 {
            None
        } else {
            Some(resolve_class_name(&constant_pool, super_class)?)
        };
        let mut interface_names = Vec::with_capacity(interface_indices.len());
        for idx in interface_indices {
            interface_names.push(resolve_class_name(&constant_pool, idx)?);
        }

        Ok(ClassFile {
            major_version,
            minor_version,
            class_name,
            super_name,
            interface_names,
            constant_pool,
            annotations,
        })
    }

    /// Whether this class carries any of the Servlet-spec annotations this
    /// parser recognises.
    pub fn has_servlet_annotations(&self) -> bool {
        self.annotations.iter().any(|a| {
            is_descriptor(&a.descriptor, WEBSERVLET_DESCRIPTORS)
                || is_descriptor(&a.descriptor, WEBFILTER_DESCRIPTORS)
                || is_descriptor(&a.descriptor, WEBLISTENER_DESCRIPTORS)
        })
    }

    /// Decode every Servlet-spec annotation on this class into the
    /// corresponding info struct and fold them into `index`.
    ///
    /// A class may legitimately carry more than one of these annotations
    /// (e.g. an endpoint that is both a `@WebServlet` and a `@WebListener`),
    /// so each is handled independently.
    pub fn collect_into(&self, index: &mut AnnotationIndex) {
        for ann in &self.annotations {
            if is_descriptor(&ann.descriptor, WEBSERVLET_DESCRIPTORS) {
                index.web_servlets.push(self.decode_web_servlet(ann));
            } else if is_descriptor(&ann.descriptor, WEBFILTER_DESCRIPTORS) {
                index.web_filters.push(self.decode_web_filter(ann));
            } else if is_descriptor(&ann.descriptor, WEBLISTENER_DESCRIPTORS) {
                index.web_listeners.push(WebListenerInfo {
                    class_name: self.class_name.clone(),
                });
            }
        }
    }

    /// Decode the Servlet-spec annotations on this class into a fresh
    /// [`AnnotationIndex`]. Convenience wrapper over [`Self::collect_into`].
    pub fn servlet_annotations(&self) -> AnnotationIndex {
        let mut index = AnnotationIndex::new();
        self.collect_into(&mut index);
        index
    }

    /// Decode a `@WebServlet` annotation.
    fn decode_web_servlet(&self, ann: &DecodedAnnotation) -> WebServletInfo {
        let mut info = WebServletInfo {
            class_name: self.class_name.clone(),
            ..WebServletInfo::default()
        };

        // `value` and `urlPatterns` are equivalent; both contribute patterns.
        for key in ["value", "urlPatterns"] {
            if let Some(ev) = ann.get(key) {
                info.url_patterns.extend(string_list(ev));
            }
        }
        if let Some(ev) = ann.get("name") {
            if let Some(s) = first_string(ev) {
                info.servlet_name = s;
            }
        }
        if info.servlet_name.is_empty() {
            info.servlet_name = self.class_name.clone();
        }
        if let Some(ev) = ann.get("loadOnStartup") {
            info.load_on_startup = first_int(ev).map(|n| n as i32);
        }
        if let Some(ev) = ann.get("asyncSupported") {
            info.async_supported = first_bool(ev).unwrap_or(false);
        }
        if let Some(ev) = ann.get("initParams") {
            info.init_params = self.decode_init_params(ev);
        }
        info
    }

    /// Decode a `@WebFilter` annotation.
    fn decode_web_filter(&self, ann: &DecodedAnnotation) -> WebFilterInfo {
        let mut info = WebFilterInfo {
            class_name: self.class_name.clone(),
            ..WebFilterInfo::default()
        };

        for key in ["value", "urlPatterns"] {
            if let Some(ev) = ann.get(key) {
                info.url_patterns.extend(string_list(ev));
            }
        }
        if let Some(ev) = ann.get("servletNames") {
            info.servlet_names.extend(string_list(ev));
        }
        if let Some(ev) = ann.get("filterName") {
            if let Some(s) = first_string(ev) {
                info.filter_name = s;
            }
        }
        if info.filter_name.is_empty() {
            info.filter_name = self.class_name.clone();
        }
        if let Some(ev) = ann.get("asyncSupported") {
            info.async_supported = first_bool(ev).unwrap_or(false);
        }
        if let Some(ev) = ann.get("initParams") {
            info.init_params = self.decode_init_params(ev);
        }
        info
    }

    /// The dotted, fully-qualified type names of every class-level
    /// annotation on this class.
    ///
    /// Annotation descriptors in a `.class` file are JVM field descriptors of
    /// the form `Lcom/example/MyAnnotation;`; this strips the leading `L`
    /// and trailing `;` and converts the slashes to dots, yielding the same
    /// dotted FQCN form used by [`ClassFile::class_name`]. Descriptors that
    /// do not match the expected shape are skipped.
    pub fn annotation_type_names(&self) -> Vec<String> {
        self.annotations
            .iter()
            .filter_map(|a| descriptor_to_fqcn(&a.descriptor))
            .collect()
    }

    /// Project this class into the compact [`ClassMeta`] used by
    /// [`crate::scanner::ClassgraphIndex`] for `@HandlesTypes`-style
    /// supertype / interface / annotation lookups.
    ///
    /// All names are in the dotted FQCN form (e.g. `com.example.Foo`,
    /// `java.lang.Object`), matching what `java.lang.Class.getName()`
    /// returns at run time.
    pub fn meta(&self) -> ClassMeta {
        ClassMeta {
            name: self.class_name.clone(),
            super_name: self.super_name.clone(),
            interfaces: self.interface_names.clone(),
            annotations: self.annotation_type_names(),
        }
    }

    /// Decode an `initParams` element — an array of `@WebInitParam` nested
    /// annotations — into `(name, value)` pairs.
    fn decode_init_params(&self, ev: &ElementValue) -> Vec<InitParam> {
        let mut params = Vec::new();
        let items: Vec<&ElementValue> = match ev {
            ElementValue::Array(items) => items.iter().collect(),
            single => vec![single],
        };
        for item in items {
            if let ElementValue::Annotation(nested) = item {
                if !is_descriptor(&nested.descriptor, WEBINITPARAM_DESCRIPTORS) {
                    continue;
                }
                let name = nested.get("name").and_then(first_string);
                let value = nested.get("value").and_then(first_string);
                if let (Some(name), Some(value)) = (name, value) {
                    params.push((name, value));
                }
            }
        }
        params
    }
}

/// Whether `descriptor` matches any descriptor in `set`.
fn is_descriptor(descriptor: &str, set: &[&str]) -> bool {
    set.contains(&descriptor)
}

/// Convert a JVM class field descriptor (`Lcom/example/Foo;`) to the dotted
/// FQCN form (`com.example.Foo`). Returns `None` for descriptors that do not
/// match the expected `L…;` shape (primitives, arrays, malformed input).
fn descriptor_to_fqcn(descriptor: &str) -> Option<String> {
    let bytes = descriptor.as_bytes();
    if bytes.len() < 3 || bytes[0] != b'L' || bytes[bytes.len() - 1] != b';' {
        return None;
    }
    let internal = &descriptor[1..descriptor.len() - 1];
    Some(internal.replace('/', "."))
}

/// Extract every string from a (possibly array, possibly scalar)
/// element-value.
fn string_list(ev: &ElementValue) -> Vec<String> {
    match ev {
        ElementValue::Array(items) => items.iter().filter_map(first_string).collect(),
        scalar => first_string(scalar).into_iter().collect(),
    }
}

/// Extract the first string from a scalar (or first element of an array)
/// element-value.
fn first_string(ev: &ElementValue) -> Option<String> {
    match ev {
        ElementValue::Const { string, .. } => string.clone(),
        ElementValue::Array(items) => items.first().and_then(first_string),
        _ => None,
    }
}

/// Extract the first integer from a scalar (or first element of an array)
/// element-value.
fn first_int(ev: &ElementValue) -> Option<i64> {
    match ev {
        ElementValue::Const { int, .. } => *int,
        ElementValue::Array(items) => items.first().and_then(first_int),
        _ => None,
    }
}

/// Interpret an integer-typed element-value as a Java `boolean` (`0` is
/// `false`, anything else `true`).
fn first_bool(ev: &ElementValue) -> Option<bool> {
    first_int(ev).map(|n| n != 0)
}

/// Parse the `constant_pool_count` and the constant pool itself.
///
/// The pool is one-indexed; index 0 is reserved, so a placeholder is pushed at
/// slot 0. `Long` and `Double` entries occupy two slots (JVMS §4.4.5).
fn parse_constant_pool(c: &mut Cursor<'_>) -> Result<Vec<Constant>> {
    let count = c.u16()?;
    if count == 0 {
        return Err(malformed("constant_pool_count is zero"));
    }

    let mut pool = Vec::with_capacity(count as usize);
    pool.push(Constant::Unusable); // slot 0 is never valid

    let mut index = 1u16;
    while index < count {
        let tag = c.u8()?;
        let constant = match tag {
            1 => {
                // CONSTANT_Utf8
                let len = c.u16()? as usize;
                let raw = c.take(len)?;
                Constant::Utf8(decode_modified_utf8(raw)?)
            }
            3 => Constant::Integer(c.i32()?),   // CONSTANT_Integer
            4 => Constant::Float(c.f32()?),     // CONSTANT_Float
            5 => Constant::Long(c.i64()?),      // CONSTANT_Long
            6 => Constant::Double(c.f64()?),    // CONSTANT_Double
            7 => Constant::Class(c.u16()?),     // CONSTANT_Class
            8 => Constant::StringRef(c.u16()?), // CONSTANT_String
            9..=11 => {
                // Fieldref / Methodref / InterfaceMethodref: two u16s
                c.skip(4)?;
                Constant::Other
            }
            12 => {
                // CONSTANT_NameAndType
                let name = c.u16()?;
                let desc = c.u16()?;
                Constant::NameAndType(name, desc)
            }
            15 => {
                // CONSTANT_MethodHandle: u8 + u16
                c.skip(3)?;
                Constant::Other
            }
            16 => {
                // CONSTANT_MethodType: u16
                c.skip(2)?;
                Constant::Other
            }
            17 | 18 => {
                // CONSTANT_Dynamic / CONSTANT_InvokeDynamic: u16 + u16
                c.skip(4)?;
                Constant::Other
            }
            19 | 20 => {
                // CONSTANT_Module / CONSTANT_Package: u16
                c.skip(2)?;
                Constant::Other
            }
            other => {
                return Err(malformed(format!("unknown constant pool tag {other}")));
            }
        };

        let wide = matches!(constant, Constant::Long(_) | Constant::Double(_));
        pool.push(constant);
        index += 1;
        if wide {
            // Long/Double consume the following index too.
            pool.push(Constant::Unusable);
            index += 1;
        }
    }

    Ok(pool)
}

/// Decode a JVM "modified UTF-8" byte string (JVMS §4.4.7).
///
/// The two ways modified UTF-8 differs from standard UTF-8 are handled: the
/// NUL character is encoded as the two bytes `0xC0 0x80`, and characters
/// outside the BMP are encoded as a six-byte surrogate pair. ASCII — by far
/// the common case for the strings we care about — is the fast path.
fn decode_modified_utf8(bytes: &[u8]) -> Result<String> {
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        if b0 == 0 {
            return Err(malformed("invalid 0x00 byte in modified-UTF8 string"));
        }
        if b0 < 0x80 {
            out.push(b0 as char);
            i += 1;
            continue;
        }
        // Multi-byte sequence.
        if b0 & 0xE0 == 0xC0 {
            // Two-byte form: 110x_xxxx 10xx_xxxx.
            let b1 = *bytes
                .get(i + 1)
                .ok_or_else(|| malformed("truncated 2-byte modified-UTF8 sequence"))?;
            let code = (((b0 & 0x1F) as u32) << 6) | ((b1 & 0x3F) as u32);
            push_code_point(&mut out, code)?;
            i += 2;
        } else if b0 & 0xF0 == 0xE0 {
            // Three-byte form, or the first half of a six-byte surrogate pair.
            let b1 = *bytes
                .get(i + 1)
                .ok_or_else(|| malformed("truncated 3-byte modified-UTF8 sequence"))?;
            let b2 = *bytes
                .get(i + 2)
                .ok_or_else(|| malformed("truncated 3-byte modified-UTF8 sequence"))?;
            let first =
                (((b0 & 0x0F) as u32) << 12) | (((b1 & 0x3F) as u32) << 6) | ((b2 & 0x3F) as u32);
            // A six-byte supplementary sequence: high surrogate followed by a
            // second three-byte group encoding the low surrogate.
            if (0xD800..=0xDBFF).contains(&first) && i + 5 < bytes.len() {
                let c3 = bytes[i + 3];
                let c4 = bytes[i + 4];
                let c5 = bytes[i + 5];
                if c3 & 0xF0 == 0xE0 {
                    let second = (((c3 & 0x0F) as u32) << 12)
                        | (((c4 & 0x3F) as u32) << 6)
                        | ((c5 & 0x3F) as u32);
                    if (0xDC00..=0xDFFF).contains(&second) {
                        let code = 0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00);
                        push_code_point(&mut out, code)?;
                        i += 6;
                        continue;
                    }
                }
            }
            push_code_point(&mut out, first)?;
            i += 3;
        } else {
            return Err(malformed(format!(
                "invalid modified-UTF8 lead byte 0x{b0:02X}"
            )));
        }
    }
    Ok(out)
}

/// Append a Unicode scalar value to `out`, rejecting unpaired surrogates.
fn push_code_point(out: &mut String, code: u32) -> Result<()> {
    match char::from_u32(code) {
        Some(ch) => {
            out.push(ch);
            Ok(())
        }
        None => Err(malformed(format!(
            "modified-UTF8 string contains invalid code point U+{code:04X}"
        ))),
    }
}

/// Resolve a `CONSTANT_Class` index to a dotted class name.
///
/// Internal class names use `/` as the package separator (`com/example/Foo`);
/// this converts them to the dotted form callers expect.
fn resolve_class_name(pool: &[Constant], class_index: u16) -> Result<String> {
    let name_index = match pool.get(class_index as usize) {
        Some(Constant::Class(idx)) => *idx,
        _ => {
            return Err(malformed(format!(
                "this_class index {class_index} does not point to a Class constant"
            )))
        }
    };
    match pool.get(name_index as usize) {
        Some(Constant::Utf8(internal)) => Ok(internal.replace('/', ".")),
        _ => Err(malformed(format!(
            "Class constant name index {name_index} does not point to a Utf8 constant"
        ))),
    }
}

/// Skip a field table or a method table: a `u16` count followed by that many
/// members, each of which is `access_flags`, `name_index`, `descriptor_index`
/// and an attribute list.
fn skip_member_table(c: &mut Cursor<'_>, pool: &[Constant]) -> Result<()> {
    let count = c.u16()?;
    for _ in 0..count {
        c.skip(6)?; // access_flags + name_index + descriptor_index
        skip_attributes(c, pool)?;
    }
    Ok(())
}

/// Skip an entire `attributes` table: a `u16` count followed by that many
/// `(name_index: u16, length: u32, info: [length])` records.
fn skip_attributes(c: &mut Cursor<'_>, _pool: &[Constant]) -> Result<()> {
    let count = c.u16()?;
    for _ in 0..count {
        let _name_index = c.u16()?;
        let length = c.u32()? as usize;
        c.skip(length)?;
    }
    Ok(())
}

/// Walk the class-level attributes, decoding only `RuntimeVisibleAnnotations`
/// and skipping everything else by length.
fn parse_class_attributes(c: &mut Cursor<'_>, pool: &[Constant]) -> Result<Vec<DecodedAnnotation>> {
    let count = c.u16()?;
    let mut annotations = Vec::new();

    for _ in 0..count {
        let name_index = c.u16()?;
        let length = c.u32()? as usize;
        let body = c.take(length)?;

        let name = match pool.get(name_index as usize) {
            Some(Constant::Utf8(s)) => s.as_str(),
            _ => {
                // An attribute whose name we cannot resolve is skipped, not
                // fatal — the body has already been consumed by `take`.
                continue;
            }
        };

        if name == "RuntimeVisibleAnnotations" {
            let mut inner = Cursor::new(body);
            annotations.extend(parse_annotations(&mut inner, pool)?);
        }
    }

    Ok(annotations)
}

/// Parse a `RuntimeVisibleAnnotations`-style body: a `u16` count followed by
/// that many `annotation` structures (JVMS §4.7.16).
fn parse_annotations(c: &mut Cursor<'_>, pool: &[Constant]) -> Result<Vec<DecodedAnnotation>> {
    let num = c.u16()?;
    let mut out = Vec::with_capacity(num as usize);
    for _ in 0..num {
        out.push(parse_one_annotation(c, pool)?);
    }
    Ok(out)
}

/// Parse a single `annotation` structure: a type-descriptor index followed by
/// `num_element_value_pairs` `(element_name_index, element_value)` pairs.
fn parse_one_annotation(c: &mut Cursor<'_>, pool: &[Constant]) -> Result<DecodedAnnotation> {
    let type_index = c.u16()?;
    let descriptor = match pool.get(type_index as usize) {
        Some(Constant::Utf8(s)) => s.clone(),
        _ => return Err(malformed("annotation type index is not a Utf8 constant")),
    };

    let num_pairs = c.u16()?;
    let mut elements = Vec::with_capacity(num_pairs as usize);
    for _ in 0..num_pairs {
        let name_index = c.u16()?;
        let name = match pool.get(name_index as usize) {
            Some(Constant::Utf8(s)) => s.clone(),
            _ => return Err(malformed("annotation element name is not a Utf8 constant")),
        };
        let value = parse_element_value(c, pool)?;
        elements.push((name, value));
    }

    Ok(DecodedAnnotation {
        descriptor,
        elements,
    })
}

/// Parse one `element_value` structure (JVMS §4.7.16.1).
///
/// The leading one-byte tag selects the shape of the value that follows.
fn parse_element_value(c: &mut Cursor<'_>, pool: &[Constant]) -> Result<ElementValue> {
    let tag = c.u8()?;
    match tag {
        // Primitive constants and String: a single const_value_index.
        b'B' | b'C' | b'I' | b'S' | b'Z' => {
            let idx = c.u16()?;
            match pool.get(idx as usize) {
                Some(Constant::Integer(n)) => Ok(ElementValue::Const {
                    string: Some(n.to_string()),
                    int: Some(*n as i64),
                }),
                _ => Ok(ElementValue::Const {
                    string: None,
                    int: None,
                }),
            }
        }
        b'D' => {
            let idx = c.u16()?;
            match pool.get(idx as usize) {
                Some(Constant::Double(d)) => Ok(ElementValue::Const {
                    string: Some(d.to_string()),
                    int: None,
                }),
                _ => Ok(ElementValue::Const {
                    string: None,
                    int: None,
                }),
            }
        }
        b'F' => {
            let idx = c.u16()?;
            match pool.get(idx as usize) {
                Some(Constant::Float(f)) => Ok(ElementValue::Const {
                    string: Some(f.to_string()),
                    int: None,
                }),
                _ => Ok(ElementValue::Const {
                    string: None,
                    int: None,
                }),
            }
        }
        b'J' => {
            let idx = c.u16()?;
            match pool.get(idx as usize) {
                Some(Constant::Long(n)) => Ok(ElementValue::Const {
                    string: Some(n.to_string()),
                    int: Some(*n),
                }),
                _ => Ok(ElementValue::Const {
                    string: None,
                    int: None,
                }),
            }
        }
        b's' => {
            // String: const_value_index points at a Utf8 constant.
            let idx = c.u16()?;
            match pool.get(idx as usize) {
                Some(Constant::Utf8(s)) => Ok(ElementValue::Const {
                    string: Some(s.clone()),
                    int: None,
                }),
                _ => Ok(ElementValue::Const {
                    string: None,
                    int: None,
                }),
            }
        }
        b'e' => {
            // Enum constant: type_name_index + const_name_index.
            c.skip(4)?;
            Ok(ElementValue::Other)
        }
        b'c' => {
            // Class literal: class_info_index.
            c.skip(2)?;
            Ok(ElementValue::Other)
        }
        b'@' => {
            // Nested annotation.
            let nested = parse_one_annotation(c, pool)?;
            Ok(ElementValue::Annotation(nested))
        }
        b'[' => {
            // Array of element values.
            let num = c.u16()?;
            let mut items = Vec::with_capacity(num as usize);
            for _ in 0..num {
                items.push(parse_element_value(c, pool)?);
            }
            Ok(ElementValue::Array(items))
        }
        other => Err(malformed(format!(
            "unknown element_value tag 0x{other:02X} ('{}')",
            other as char
        ))),
    }
}

// ---------------------------------------------------------------------------
// Test-only class-file builder
// ---------------------------------------------------------------------------

/// A tiny, dependency-free Java `.class` file builder used by the unit tests so
/// the parser can be exercised even when no JDK / `javac` is available.
///
/// It is deliberately minimal: it can emit a class with a chosen name and a
/// single class-level annotation whose elements are strings, integers, an
/// array of strings, or an array of nested `@WebInitParam` annotations — which
/// is exactly the surface the Servlet annotations exercise.
#[cfg(test)]
pub(crate) mod test_builder {
    use std::collections::BTreeMap;

    /// An element-value the builder knows how to emit.
    pub(crate) enum Val {
        /// A `String` element.
        Str(&'static str),
        /// An `int` element.
        Int(i32),
        /// A `boolean` element (emitted as an `int` with tag `Z`).
        Bool(bool),
        /// An array of `String` elements.
        StrArray(Vec<&'static str>),
        /// An array of nested `@WebInitParam` annotations, each `(name, value)`.
        InitParams(Vec<(&'static str, &'static str)>),
    }

    /// Incrementally interned constant pool.
    #[derive(Default)]
    struct Pool {
        entries: Vec<u8>,
        count: u16,
        utf8: BTreeMap<String, u16>,
    }

    impl Pool {
        fn new() -> Pool {
            Pool {
                entries: Vec::new(),
                count: 1, // pool is one-indexed
                utf8: BTreeMap::new(),
            }
        }

        fn utf8(&mut self, s: &str) -> u16 {
            if let Some(&idx) = self.utf8.get(s) {
                return idx;
            }
            self.entries.push(1); // CONSTANT_Utf8
            let bytes = s.as_bytes();
            self.entries
                .extend_from_slice(&(bytes.len() as u16).to_be_bytes());
            self.entries.extend_from_slice(bytes);
            let idx = self.count;
            self.count += 1;
            self.utf8.insert(s.to_string(), idx);
            idx
        }

        fn class(&mut self, internal_name: &str) -> u16 {
            let name = self.utf8(internal_name);
            self.entries.push(7); // CONSTANT_Class
            self.entries.extend_from_slice(&name.to_be_bytes());
            let idx = self.count;
            self.count += 1;
            idx
        }

        fn integer(&mut self, n: i32) -> u16 {
            self.entries.push(3); // CONSTANT_Integer
            self.entries.extend_from_slice(&n.to_be_bytes());
            let idx = self.count;
            self.count += 1;
            idx
        }
    }

    /// Encode one `element_value` into `out`, interning constants in `pool`.
    fn encode_value(pool: &mut Pool, out: &mut Vec<u8>, val: &Val) {
        match val {
            Val::Str(s) => {
                let idx = pool.utf8(s);
                out.push(b's');
                out.extend_from_slice(&idx.to_be_bytes());
            }
            Val::Int(n) => {
                let idx = pool.integer(*n);
                out.push(b'I');
                out.extend_from_slice(&idx.to_be_bytes());
            }
            Val::Bool(b) => {
                let idx = pool.integer(i32::from(*b));
                out.push(b'Z');
                out.extend_from_slice(&idx.to_be_bytes());
            }
            Val::StrArray(items) => {
                out.push(b'[');
                out.extend_from_slice(&(items.len() as u16).to_be_bytes());
                for item in items {
                    let idx = pool.utf8(item);
                    out.push(b's');
                    out.extend_from_slice(&idx.to_be_bytes());
                }
            }
            Val::InitParams(pairs) => {
                out.push(b'[');
                out.extend_from_slice(&(pairs.len() as u16).to_be_bytes());
                for (name, value) in pairs {
                    out.push(b'@'); // nested annotation
                    let desc = pool.utf8("Ljakarta/servlet/annotation/WebInitParam;");
                    out.extend_from_slice(&desc.to_be_bytes());
                    out.extend_from_slice(&2u16.to_be_bytes()); // two pairs
                    let name_key = pool.utf8("name");
                    out.extend_from_slice(&name_key.to_be_bytes());
                    encode_value(pool, out, &Val::Str(name));
                    let value_key = pool.utf8("value");
                    out.extend_from_slice(&value_key.to_be_bytes());
                    encode_value(pool, out, &Val::Str(value));
                }
            }
        }
    }

    /// Build a minimal `.class` file: a class named `internal_name` carrying a
    /// single class-level annotation of type `annotation_descriptor` with the
    /// given `(element-name, value)` pairs.
    pub(crate) fn build_class(
        internal_name: &str,
        annotation_descriptor: &str,
        elements: &[(&'static str, Val)],
    ) -> Vec<u8> {
        build_class_with_hierarchy(
            internal_name,
            "java/lang/Object",
            &[],
            &[(annotation_descriptor, elements)],
        )
    }

    /// Build a `.class` file with an explicit superclass and interface list
    /// plus zero or more class-level annotations.
    ///
    /// Used by the `ClassgraphIndex` tests, which need to assert that
    /// `extends` / `implements` edges are followed transitively. All names
    /// passed in are JVM **internal** form (slash-separated); the parser
    /// converts them to dotted FQCNs on the way out.
    pub(crate) fn build_class_with_hierarchy(
        internal_name: &str,
        super_internal: &str,
        interface_internals: &[&str],
        annotations: &[(&str, &[(&'static str, Val)])],
    ) -> Vec<u8> {
        let mut pool = Pool::new();

        // Constants referenced structurally.
        let this_class = pool.class(internal_name);
        let super_class = pool.class(super_internal);
        let interface_indices: Vec<u16> =
            interface_internals.iter().map(|n| pool.class(n)).collect();

        // Pre-intern the RuntimeVisibleAnnotations attribute name only when
        // we actually have annotations to emit; otherwise the file should
        // not carry an empty RVA attribute at all (the parser tolerates one,
        // but it's cleaner to omit).
        let mut rva_body = Vec::new();
        if !annotations.is_empty() {
            rva_body.extend_from_slice(&(annotations.len() as u16).to_be_bytes());
            for (descriptor, elements) in annotations {
                let ann_desc = pool.utf8(descriptor);
                rva_body.extend_from_slice(&ann_desc.to_be_bytes());
                rva_body.extend_from_slice(&(elements.len() as u16).to_be_bytes());
                for (name, val) in *elements {
                    let name_idx = pool.utf8(name);
                    rva_body.extend_from_slice(&name_idx.to_be_bytes());
                    encode_value(&mut pool, &mut rva_body, val);
                }
            }
        }
        let rva_name = if !rva_body.is_empty() {
            Some(pool.utf8("RuntimeVisibleAnnotations"))
        } else {
            None
        };

        // Assemble the file.
        let mut file = Vec::new();
        file.extend_from_slice(&0xCAFE_BABEu32.to_be_bytes());
        file.extend_from_slice(&0u16.to_be_bytes()); // minor
        file.extend_from_slice(&52u16.to_be_bytes()); // major (Java 8)
        file.extend_from_slice(&pool.count.to_be_bytes()); // constant_pool_count
        file.extend_from_slice(&pool.entries);
        file.extend_from_slice(&0x0021u16.to_be_bytes()); // access_flags: public super
        file.extend_from_slice(&this_class.to_be_bytes());
        file.extend_from_slice(&super_class.to_be_bytes());
        file.extend_from_slice(&(interface_indices.len() as u16).to_be_bytes());
        for idx in &interface_indices {
            file.extend_from_slice(&idx.to_be_bytes());
        }
        file.extend_from_slice(&0u16.to_be_bytes()); // fields_count
        file.extend_from_slice(&0u16.to_be_bytes()); // methods_count
        let attribute_count: u16 = if rva_name.is_some() { 1 } else { 0 };
        file.extend_from_slice(&attribute_count.to_be_bytes());
        if let Some(rva) = rva_name {
            file.extend_from_slice(&rva.to_be_bytes());
            file.extend_from_slice(&(rva_body.len() as u32).to_be_bytes());
            file.extend_from_slice(&rva_body);
        }

        file
    }
}

#[cfg(test)]
mod tests {
    use super::test_builder::{build_class, Val};
    use super::*;

    #[test]
    fn empty_index_reports_empty() {
        let idx = AnnotationIndex::new();
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn merge_appends_all_categories() {
        let mut a = AnnotationIndex {
            web_servlets: vec![WebServletInfo {
                class_name: "com.example.A".to_string(),
                servlet_name: "A".to_string(),
                ..WebServletInfo::default()
            }],
            ..AnnotationIndex::default()
        };
        let b = AnnotationIndex {
            web_filters: vec![WebFilterInfo {
                class_name: "com.example.F".to_string(),
                filter_name: "F".to_string(),
                ..WebFilterInfo::default()
            }],
            web_listeners: vec![WebListenerInfo {
                class_name: "com.example.L".to_string(),
            }],
            ..AnnotationIndex::default()
        };
        a.merge(b);
        assert_eq!(a.len(), 3);
        assert_eq!(a.web_servlets.len(), 1);
        assert_eq!(a.web_filters.len(), 1);
        assert_eq!(a.web_listeners.len(), 1);
    }

    #[test]
    fn rejects_bad_magic() {
        let err = ClassFile::parse(&[0x00, 0x01, 0x02, 0x03, 0x04, 0x05]).unwrap_err();
        assert!(matches!(err, Error::Deployment(_)));
    }

    #[test]
    fn rejects_truncated_file() {
        let err = ClassFile::parse(&[0xCA, 0xFE, 0xBA, 0xBE]).unwrap_err();
        assert!(matches!(err, Error::Deployment(_)));
    }

    #[test]
    fn parses_constant_pool_and_class_name() {
        // A class with no annotations exercises pure constant-pool + this_class
        // resolution.
        let bytes = build_class("com/example/Plain", "Lcom/example/Marker;", &[]);
        let cf = ClassFile::parse(&bytes).expect("parse plain class");
        assert_eq!(cf.class_name, "com.example.Plain");
        assert_eq!(cf.major_version, 52);
        // The constant pool must contain the interned UTF8 names.
        assert!(cf
            .constant_pool
            .iter()
            .any(|c| matches!(c, Constant::Utf8(s) if s == "com/example/Plain")));
        // A non-Servlet annotation must not register as a servlet annotation.
        assert!(!cf.has_servlet_annotations());
    }

    #[test]
    fn extracts_webservlet_annotation() {
        let bytes = build_class(
            "com/example/HelloServlet",
            "Ljakarta/servlet/annotation/WebServlet;",
            &[
                ("name", Val::Str("hello")),
                ("urlPatterns", Val::StrArray(vec!["/hello", "/hi"])),
                ("loadOnStartup", Val::Int(3)),
                ("asyncSupported", Val::Bool(true)),
                (
                    "initParams",
                    Val::InitParams(vec![("debug", "true"), ("mode", "fast")]),
                ),
            ],
        );
        let cf = ClassFile::parse(&bytes).expect("parse annotated class");
        assert!(cf.has_servlet_annotations());

        let idx = cf.servlet_annotations();
        assert_eq!(idx.web_servlets.len(), 1);
        let s = &idx.web_servlets[0];
        assert_eq!(s.class_name, "com.example.HelloServlet");
        assert_eq!(s.servlet_name, "hello");
        assert_eq!(s.url_patterns, vec!["/hello", "/hi"]);
        assert_eq!(s.load_on_startup, Some(3));
        assert!(s.async_supported);
        assert_eq!(
            s.init_params,
            vec![
                ("debug".to_string(), "true".to_string()),
                ("mode".to_string(), "fast".to_string()),
            ]
        );
    }

    #[test]
    fn webservlet_value_element_supplies_patterns() {
        // The single-member `value` form: @WebServlet("/only").
        let bytes = build_class(
            "com/example/OnlyServlet",
            "Ljakarta/servlet/annotation/WebServlet;",
            &[("value", Val::StrArray(vec!["/only"]))],
        );
        let cf = ClassFile::parse(&bytes).expect("parse");
        let idx = cf.servlet_annotations();
        assert_eq!(idx.web_servlets.len(), 1);
        assert_eq!(idx.web_servlets[0].url_patterns, vec!["/only"]);
        // No explicit name: defaults to the class name.
        assert_eq!(idx.web_servlets[0].servlet_name, "com.example.OnlyServlet");
    }

    #[test]
    fn extracts_webfilter_annotation() {
        let bytes = build_class(
            "com/example/AuthFilter",
            "Ljakarta/servlet/annotation/WebFilter;",
            &[
                ("filterName", Val::Str("auth")),
                ("urlPatterns", Val::StrArray(vec!["/*"])),
                ("asyncSupported", Val::Bool(false)),
            ],
        );
        let cf = ClassFile::parse(&bytes).expect("parse");
        let idx = cf.servlet_annotations();
        assert_eq!(idx.web_filters.len(), 1);
        let f = &idx.web_filters[0];
        assert_eq!(f.class_name, "com.example.AuthFilter");
        assert_eq!(f.filter_name, "auth");
        assert_eq!(f.url_patterns, vec!["/*"]);
        assert!(!f.async_supported);
    }

    #[test]
    fn extracts_weblistener_annotation() {
        let bytes = build_class(
            "com/example/AppListener",
            "Ljakarta/servlet/annotation/WebListener;",
            &[],
        );
        let cf = ClassFile::parse(&bytes).expect("parse");
        let idx = cf.servlet_annotations();
        assert_eq!(idx.web_listeners.len(), 1);
        assert_eq!(idx.web_listeners[0].class_name, "com.example.AppListener");
    }

    #[test]
    fn accepts_legacy_javax_descriptors() {
        let bytes = build_class(
            "com/example/LegacyServlet",
            "Ljavax/servlet/annotation/WebServlet;",
            &[("value", Val::StrArray(vec!["/legacy"]))],
        );
        let cf = ClassFile::parse(&bytes).expect("parse");
        let idx = cf.servlet_annotations();
        assert_eq!(idx.web_servlets.len(), 1);
        assert_eq!(idx.web_servlets[0].url_patterns, vec!["/legacy"]);
    }

    #[test]
    fn modified_utf8_decodes_ascii_and_supplementary() {
        // Plain ASCII.
        assert_eq!(decode_modified_utf8(b"hello").unwrap(), "hello");
        // Two-byte form of U+00E9 (é): 0xC3 0xA9.
        assert_eq!(decode_modified_utf8(&[0xC3, 0xA9]).unwrap(), "é");
        // Six-byte supplementary form of U+1F600 (😀).
        let emoji = [0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80];
        assert_eq!(decode_modified_utf8(&emoji).unwrap(), "😀");
        // A raw 0x00 byte is illegal in modified UTF-8.
        assert!(decode_modified_utf8(&[0x00]).is_err());
    }
}
