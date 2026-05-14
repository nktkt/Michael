//! Annotation model and index for Servlet-spec class annotations.
//!
//! The Servlet specification lets applications declare components with
//! annotations instead of `web.xml`: `@WebServlet`, `@WebFilter`,
//! `@WebListener`, `@ServletSecurity`, and so on. Discovering them requires
//! reading Java class files (or `.jar` entries) on the application classpath.
//!
//! In the Tomcat-RS architecture, classpath bytecode is owned by the JVM side
//! of the bridge — the Rust runtime does not parse Java bytecode itself.
//! Therefore this module provides the **data model** ([`AnnotationIndex`] and
//! the per-annotation info structs) that the JVM bridge will populate in a
//! later version. The [`ClassScanner`](crate::scanner::ClassScanner) in the
//! sibling [`scanner`](crate::scanner) module is the entry point that performs
//! (or, for `v0.1.0`, defers) the scan.
//!
//! Everything here is infallible and panic-free; the `v0.1.0` scan simply
//! yields an empty index.

/// Metadata extracted from one `@WebServlet` annotation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WebServletInfo {
    /// Fully-qualified name of the annotated servlet class.
    pub class_name: String,
    /// The servlet name (`name` attribute, defaulting to the class name).
    pub servlet_name: String,
    /// URL patterns the servlet is mapped to (`value` / `urlPatterns`).
    pub url_patterns: Vec<String>,
    /// `loadOnStartup` order, if specified and non-negative.
    pub load_on_startup: Option<i32>,
}

/// Metadata extracted from one `@WebFilter` annotation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WebFilterInfo {
    /// Fully-qualified name of the annotated filter class.
    pub class_name: String,
    /// The filter name (`filterName`, defaulting to the class name).
    pub filter_name: String,
    /// URL patterns the filter intercepts.
    pub url_patterns: Vec<String>,
}

/// The aggregate result of scanning a web application's classpath for
/// Servlet-spec annotations.
///
/// Populated by the JVM bridge in a later version; in `v0.1.0` it is always
/// empty (see [`ClassScanner`](crate::scanner::ClassScanner)).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnnotationIndex {
    /// Every discovered `@WebServlet`.
    pub web_servlets: Vec<WebServletInfo>,
    /// Every discovered `@WebFilter`.
    pub web_filters: Vec<WebFilterInfo>,
    /// Fully-qualified class names carrying `@WebListener`.
    pub web_listeners: Vec<String>,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_index_reports_empty() {
        let idx = AnnotationIndex::new();
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn populated_index_counts_components() {
        let idx = AnnotationIndex {
            web_servlets: vec![WebServletInfo {
                class_name: "com.example.S".to_string(),
                servlet_name: "S".to_string(),
                url_patterns: vec!["/s".to_string()],
                load_on_startup: Some(1),
            }],
            web_filters: vec![WebFilterInfo::default()],
            web_listeners: vec!["com.example.L".to_string()],
        };
        assert!(!idx.is_empty());
        assert_eq!(idx.len(), 3);
    }
}
