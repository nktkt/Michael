//! Parser for `conf/catalina.properties`-style files.
//!
//! Tomcat's `catalina.properties` is an ordinary Java properties file: one
//! `key=value` pair per line, `#` or `!` comment lines, blank lines ignored,
//! and trailing-backslash line continuations. This module parses that format
//! into a [`CatalinaProperties`] map. It deliberately keeps the supported
//! syntax small but covers what real Tomcat distributions ship.

use std::collections::HashMap;

use tomcatrs_core::Result;

/// An in-memory view of a parsed properties file.
#[derive(Debug, Clone, Default)]
pub struct CatalinaProperties {
    /// The parsed key/value entries, in no particular order.
    entries: HashMap<String, String>,
}

impl CatalinaProperties {
    /// Create an empty property set.
    pub fn new() -> CatalinaProperties {
        CatalinaProperties {
            entries: HashMap::new(),
        }
    }

    /// Parse a properties document held in memory.
    ///
    /// Comment lines (starting with `#` or `!` after optional whitespace) and
    /// blank lines are ignored. A line ending in an odd number of backslashes
    /// continues onto the next line. Keys and values are separated by the
    /// first unescaped `=` or `:`; surrounding whitespace is trimmed.
    ///
    /// This never fails — malformed lines without a separator are treated as a
    /// key with an empty value — so it returns `Result` only for API symmetry
    /// with the other parsers in this crate.
    pub fn from_str(text: &str) -> Result<CatalinaProperties> {
        let mut entries = HashMap::new();
        let mut logical = String::new();

        for raw in text.lines() {
            let line = raw;
            // Accumulate continuation lines before interpreting anything.
            if !logical.is_empty() {
                logical.push_str(line.trim_start());
            } else {
                let trimmed = line.trim_start();
                if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
                    continue;
                }
                logical.push_str(trimmed);
            }

            // A trailing odd run of backslashes means "continued".
            let trailing_backslashes = logical.chars().rev().take_while(|&c| c == '\\').count();
            if trailing_backslashes % 2 == 1 {
                logical.pop(); // drop the continuation backslash
                continue;
            }

            if let Some((key, value)) = split_entry(&logical) {
                entries.insert(key, value);
            }
            logical.clear();
        }

        // A file ending mid-continuation: still record what we have.
        if !logical.is_empty() {
            if let Some((key, value)) = split_entry(&logical) {
                entries.insert(key, value);
            }
        }

        Ok(CatalinaProperties { entries })
    }

    /// Parse a properties file from disk.
    ///
    /// # Errors
    ///
    /// Returns [`tomcatrs_core::Error::Io`] if the file cannot be read.
    pub fn from_file(path: impl AsRef<std::path::Path>) -> Result<CatalinaProperties> {
        let text = std::fs::read_to_string(path.as_ref())?;
        Self::from_str(&text)
    }

    /// Look up a property by key, returning `None` if it is not set.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }

    /// Number of distinct keys parsed.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the property set is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate over every `(key, value)` pair.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

/// Split a logical line into a trimmed `(key, value)` pair.
///
/// The separator is the first unescaped `=` or `:`. A line with no separator
/// becomes a key with an empty value. Returns `None` for an empty key.
fn split_entry(line: &str) -> Option<(String, String)> {
    let chars: Vec<char> = line.chars().collect();
    let mut sep: Option<usize> = None;
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                i += 2; // skip the escaped character
                continue;
            }
            '=' | ':' => {
                sep = Some(i);
                break;
            }
            _ => {}
        }
        i += 1;
    }

    let (key_raw, value_raw) = match sep {
        Some(idx) => (
            chars[..idx].iter().collect::<String>(),
            chars[idx + 1..].iter().collect::<String>(),
        ),
        None => (line.to_string(), String::new()),
    };

    let key = unescape(key_raw.trim());
    if key.is_empty() {
        return None;
    }
    let value = unescape(value_raw.trim());
    Some((key, value))
}

/// Resolve the common Java-properties backslash escapes (`\\`, `\=`, `\:`,
/// `\t`, `\n`, `\r`, `\ `). Unknown escapes keep the escaped character.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('f') => out.push('\u{000C}'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_properties() {
        let text = "\
# Catalina sample
common.loader=\"${catalina.base}/lib\"
server.info=Tomcat-RS

! bang comment
package.access = sun.,org.apache.catalina.
";
        let props = CatalinaProperties::from_str(text).unwrap();
        assert_eq!(props.len(), 3);
        assert_eq!(props.get("common.loader"), Some("\"${catalina.base}/lib\""));
        assert_eq!(props.get("server.info"), Some("Tomcat-RS"));
        assert_eq!(
            props.get("package.access"),
            Some("sun.,org.apache.catalina.")
        );
        assert_eq!(props.get("missing"), None);
    }

    #[test]
    fn handles_line_continuations() {
        let text = "tomcat.util.scan.StandardJarScanFilter.jarsToSkip=\\\nbootstrap.jar,\\\ncommons-daemon.jar";
        let props = CatalinaProperties::from_str(text).unwrap();
        assert_eq!(
            props.get("tomcat.util.scan.StandardJarScanFilter.jarsToSkip"),
            Some("bootstrap.jar,commons-daemon.jar")
        );
    }

    #[test]
    fn colon_separator_and_escapes() {
        let props = CatalinaProperties::from_str("path\\:name : a\\=b").unwrap();
        assert_eq!(props.get("path:name"), Some("a=b"));
    }

    #[test]
    fn empty_input_is_empty() {
        let props = CatalinaProperties::from_str("\n# only a comment\n\n").unwrap();
        assert!(props.is_empty());
    }
}
