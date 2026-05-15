//! Golden-file helpers for the compatibility harness.
//!
//! "Goldens" are tiny text expectations checked in under `golden/`. Each one
//! pins the small, stable surface of a Tomcat-RS response — status, the
//! handful of headers we treat as observable, and a substring fingerprint of
//! the body — for one [`CompatScenario`](crate::CompatScenario).
//!
//! The on-disk format is deliberately minimal so divergences read cleanly in
//! `git diff`:
//!
//! ```text
//! status: 200
//! header.content-type: text/html; charset=utf-8
//! header.server: Tomcat-RS/0.1.0
//! body.contains: it works
//! body.len_ge: 100
//! ```
//!
//! Lines starting with `#` and blank lines are ignored.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::CompatResult;

/// A parsed golden expectation.
#[derive(Debug, Default, Clone)]
pub struct Golden {
    /// Expected HTTP status code, if any.
    pub status: Option<u16>,
    /// Header-name → expected value. Names are lower-cased.
    pub headers_eq: BTreeMap<String, String>,
    /// Header-name that must be present (any value).
    pub headers_present: Vec<String>,
    /// Substrings the body must contain.
    pub body_contains: Vec<String>,
    /// Minimum acceptable body length, in bytes.
    pub body_len_ge: Option<usize>,
}

impl Golden {
    /// Parse a golden from its on-disk textual representation.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut g = Golden::default();
        for (lineno, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line
                .split_once(':')
                .ok_or_else(|| format!("golden line {}: missing ':' in {raw:?}", lineno + 1))?;
            let key = key.trim();
            let value = value.trim();
            match key {
                "status" => {
                    g.status = Some(value.parse().map_err(|e| {
                        format!("golden line {}: bad status {value:?}: {e}", lineno + 1)
                    })?);
                }
                "body.contains" => g.body_contains.push(value.to_string()),
                "body.len_ge" => {
                    g.body_len_ge = Some(value.parse().map_err(|e| {
                        format!("golden line {}: bad body.len_ge {value:?}: {e}", lineno + 1)
                    })?);
                }
                k if k.starts_with("header.") => {
                    let name = k.trim_start_matches("header.").to_ascii_lowercase();
                    g.headers_eq.insert(name, value.to_string());
                }
                k if k.starts_with("present.") => {
                    let name = k.trim_start_matches("present.").to_ascii_lowercase();
                    g.headers_present.push(name);
                }
                other => return Err(format!("golden line {}: unknown key {other:?}", lineno + 1)),
            }
        }
        Ok(g)
    }

    /// Verify a [`CompatResult`] against this golden. Returns a vector of
    /// human-readable diff messages; an empty vector means the result matches.
    pub fn diff(&self, actual: &CompatResult) -> Vec<String> {
        let mut diffs = Vec::new();
        if let Some(expected) = self.status {
            if actual.status != expected {
                diffs.push(format!(
                    "status: expected {expected}, got {}",
                    actual.status
                ));
            }
        }
        for (name, expected) in &self.headers_eq {
            match actual.header(name) {
                Some(got) if got == expected => {}
                Some(got) => {
                    diffs.push(format!("header {name}: expected {expected:?}, got {got:?}"))
                }
                None => diffs.push(format!("header {name}: missing, expected {expected:?}")),
            }
        }
        for name in &self.headers_present {
            if actual.header(name).is_none() {
                diffs.push(format!("header {name}: required to be present"));
            }
        }
        let body_text = String::from_utf8_lossy(&actual.body);
        for needle in &self.body_contains {
            if !body_text.contains(needle) {
                diffs.push(format!("body: did not contain {needle:?}"));
            }
        }
        if let Some(min) = self.body_len_ge {
            if actual.body.len() < min {
                diffs.push(format!(
                    "body.len: expected >= {min}, got {}",
                    actual.body.len()
                ));
            }
        }
        diffs
    }
}

/// Path to the `golden/` directory next to this crate's `Cargo.toml`.
pub fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("golden")
}

/// Load and parse the golden for `name`. Panics with a helpful message if the
/// file is missing or malformed.
pub fn load(name: &str) -> Golden {
    let path = golden_dir().join(format!("{name}.golden"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read golden {}: {e}", path.display()));
    Golden::parse(&text)
        .unwrap_or_else(|e| panic!("failed to parse golden {}: {e}", path.display()))
}

/// Assert `actual` matches the golden named `name`. Pretty-prints every diff
/// it finds before panicking.
pub fn assert_matches(name: &str, actual: &CompatResult) {
    let golden = load(name);
    let diffs = golden.diff(actual);
    if !diffs.is_empty() {
        let body_preview = String::from_utf8_lossy(&actual.body);
        let preview: String = body_preview.chars().take(400).collect();
        panic!(
            "golden {name} mismatch:\n  - {}\n\nactual status: {}\nactual headers: {:?}\nactual body (first 400 chars): {preview}\nnotes: {}",
            diffs.join("\n  - "),
            actual.status,
            actual.headers,
            actual.notes,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn parses_a_golden_with_every_directive() {
        let text = "\
# comment
status: 200
header.Content-Type: text/html; charset=utf-8
present.Server:
body.contains: it works
body.len_ge: 5
";
        let g = Golden::parse(text).unwrap();
        assert_eq!(g.status, Some(200));
        assert_eq!(
            g.headers_eq.get("content-type").map(String::as_str),
            Some("text/html; charset=utf-8")
        );
        assert!(g.headers_present.contains(&"server".to_string()));
        assert_eq!(g.body_contains, vec!["it works".to_string()]);
        assert_eq!(g.body_len_ge, Some(5));
    }

    #[test]
    fn diff_reports_each_mismatch() {
        let g = Golden::parse(
            "\
status: 200
header.content-type: text/plain
present.x-required:
body.contains: hello
body.len_ge: 100
",
        )
        .unwrap();
        let actual = CompatResult::from_parts(
            404,
            vec![("Content-Type".into(), "text/html".into())],
            Bytes::from_static(b"hi"),
            "",
        );
        let diffs = g.diff(&actual);
        assert!(diffs.iter().any(|d| d.contains("status")), "{diffs:?}");
        assert!(
            diffs.iter().any(|d| d.contains("content-type")),
            "{diffs:?}"
        );
        assert!(diffs.iter().any(|d| d.contains("x-required")), "{diffs:?}");
        assert!(diffs.iter().any(|d| d.contains("hello")), "{diffs:?}");
        assert!(diffs.iter().any(|d| d.contains("body.len")), "{diffs:?}");
    }

    #[test]
    fn matching_response_has_empty_diff() {
        let g = Golden::parse(
            "\
status: 200
header.content-type: text/plain
present.server:
body.contains: ok
body.len_ge: 2
",
        )
        .unwrap();
        let actual = CompatResult::from_parts(
            200,
            vec![
                ("Content-Type".into(), "text/plain".into()),
                ("Server".into(), "Tomcat-RS/0.1.0".into()),
            ],
            Bytes::from_static(b"ok"),
            "",
        );
        assert!(g.diff(&actual).is_empty());
    }
}
