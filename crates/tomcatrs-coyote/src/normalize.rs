//! Real URI normalization.
//!
//! Tomcat's request processing pipeline normalizes the request target before
//! mapping it to a context and servlet. Getting this wrong is a classic source
//! of path-traversal and request-smuggling vulnerabilities, so this module is
//! deliberately strict.
//!
//! [`normalize_target`] takes a raw request target (the second token of the
//! HTTP/1.1 request line) and produces a [`NormalizedUri`] containing:
//!
//! * `path` — percent-decoded, with `.` and `..` segments collapsed, guaranteed
//!   not to escape the document root.
//! * `query` — the substring after the first `?`, left undecoded (query strings
//!   have their own application-defined encoding).
//!
//! The following are rejected outright (Tomcat's secure defaults):
//!
//! * a path that escapes the root via `..` (`/../etc/passwd`),
//! * a backslash anywhere in the path,
//! * a literal or encoded NUL byte,
//! * an encoded forward slash (`%2f` / `%2F`) — allowing it would let an
//!   attacker smuggle path separators past the mapper.

use std::fmt;

/// The result of successfully normalizing a request target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedUri {
    /// The normalized, percent-decoded path. Always begins with `/`.
    pub path: String,
    /// The raw (still-encoded) query string, without the leading `?`.
    pub query: Option<String>,
}

/// Why a request target was rejected during normalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizeError {
    /// The path tried to escape the document root with `..`.
    PathTraversal,
    /// The path contained a backslash (`\`).
    Backslash,
    /// The path contained a NUL byte (literal or `%00`).
    NulByte,
    /// The path contained an encoded slash (`%2f`).
    EncodedSlash,
    /// A `%` escape was malformed (truncated or non-hex digits).
    BadPercentEncoding,
    /// The decoded path was not valid UTF-8.
    InvalidUtf8,
    /// The request target did not start with `/` and was not the asterisk-form.
    NotAbsolute,
}

impl fmt::Display for NormalizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            NormalizeError::PathTraversal => "path traversal: target escapes the document root",
            NormalizeError::Backslash => "illegal backslash in request path",
            NormalizeError::NulByte => "illegal NUL byte in request path",
            NormalizeError::EncodedSlash => "encoded slash (%2f) is not permitted in the path",
            NormalizeError::BadPercentEncoding => "malformed percent-encoding in request target",
            NormalizeError::InvalidUtf8 => "request path is not valid UTF-8 after decoding",
            NormalizeError::NotAbsolute => "request target must be an absolute path",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for NormalizeError {}

impl From<NormalizeError> for tomcatrs_core::Error {
    fn from(e: NormalizeError) -> Self {
        tomcatrs_core::Error::protocol(e.to_string())
    }
}

/// Decode a single hex digit, or `None` if it is not `0-9a-fA-F`.
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Percent-decode `input`, treating `%2f`/`%2F` specially.
///
/// Returns the decoded bytes. An encoded slash is reported via the
/// `EncodedSlash` error so the caller can reject it before it ever reaches the
/// segment splitter.
fn percent_decode(input: &[u8]) -> Result<Vec<u8>, NormalizeError> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            b'%' => {
                if i + 2 >= input.len() {
                    return Err(NormalizeError::BadPercentEncoding);
                }
                let hi = hex_val(input[i + 1]).ok_or(NormalizeError::BadPercentEncoding)?;
                let lo = hex_val(input[i + 2]).ok_or(NormalizeError::BadPercentEncoding)?;
                let byte = (hi << 4) | lo;
                if byte == b'/' {
                    return Err(NormalizeError::EncodedSlash);
                }
                if byte == 0 {
                    return Err(NormalizeError::NulByte);
                }
                out.push(byte);
                i += 3;
            }
            0 => return Err(NormalizeError::NulByte),
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    Ok(out)
}

/// Collapse `.` and `..` segments in an already-decoded, slash-separated path.
///
/// Rejects any `..` that would pop above the root. The returned path always
/// starts with `/` and never contains `.`/`..` segments or empty segments
/// (consecutive slashes are collapsed).
fn collapse_dot_segments(path: &str) -> Result<String, NormalizeError> {
    let trailing_slash = path.len() > 1 && path.ends_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {
                // Empty segment (from `//` or leading `/`) or `.` — skip.
            }
            ".." => {
                if stack.pop().is_none() {
                    return Err(NormalizeError::PathTraversal);
                }
            }
            other => stack.push(other),
        }
    }
    let mut result = String::with_capacity(path.len());
    result.push('/');
    result.push_str(&stack.join("/"));
    if trailing_slash && result.len() > 1 {
        result.push('/');
    }
    Ok(result)
}

/// Normalize a raw HTTP request target into a [`NormalizedUri`].
///
/// See the [module documentation](self) for the exact rules and rejections.
///
/// # Errors
///
/// Returns a [`NormalizeError`] describing the first violation found.
pub fn normalize_target(target: &str) -> Result<NormalizedUri, NormalizeError> {
    // Split off the query string at the first '?'. The query is not decoded.
    let (raw_path, query) = match target.find('?') {
        Some(idx) => (&target[..idx], Some(target[idx + 1..].to_string())),
        None => (target, None),
    };

    // The asterisk-form ("OPTIONS * HTTP/1.1") is a valid target but has no path
    // to normalize; represent it verbatim.
    if raw_path == "*" {
        return Ok(NormalizedUri {
            path: "*".to_string(),
            query,
        });
    }

    if !raw_path.starts_with('/') {
        return Err(NormalizeError::NotAbsolute);
    }

    // Reject backslashes before decoding: Tomcat treats `\` as an illegal
    // character in the path regardless of where it appears.
    if raw_path.as_bytes().contains(&b'\\') {
        return Err(NormalizeError::Backslash);
    }

    let decoded = percent_decode(raw_path.as_bytes())?;

    // A backslash could also have been smuggled in via percent-encoding.
    if decoded.contains(&b'\\') {
        return Err(NormalizeError::Backslash);
    }

    let decoded_str = String::from_utf8(decoded).map_err(|_| NormalizeError::InvalidUtf8)?;
    let path = collapse_dot_segments(&decoded_str)?;

    Ok(NormalizedUri { path, query })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_path_passes_through() {
        let n = normalize_target("/app/index.html").unwrap();
        assert_eq!(n.path, "/app/index.html");
        assert_eq!(n.query, None);
    }

    #[test]
    fn splits_query_string_undecoded() {
        let n = normalize_target("/search?q=a%20b&n=1").unwrap();
        assert_eq!(n.path, "/search");
        assert_eq!(n.query.as_deref(), Some("q=a%20b&n=1"));
    }

    #[test]
    fn percent_decodes_path() {
        let n = normalize_target("/a%20b/c").unwrap();
        assert_eq!(n.path, "/a b/c");
    }

    #[test]
    fn collapses_single_dot_segments() {
        let n = normalize_target("/a/./b/./c").unwrap();
        assert_eq!(n.path, "/a/b/c");
    }

    #[test]
    fn collapses_double_dot_segments_within_root() {
        let n = normalize_target("/a/b/../c").unwrap();
        assert_eq!(n.path, "/a/c");
    }

    #[test]
    fn collapses_duplicate_slashes() {
        let n = normalize_target("/a//b///c").unwrap();
        assert_eq!(n.path, "/a/b/c");
    }

    #[test]
    fn preserves_trailing_slash() {
        let n = normalize_target("/a/b/").unwrap();
        assert_eq!(n.path, "/a/b/");
    }

    #[test]
    fn rejects_traversal_above_root() {
        assert_eq!(
            normalize_target("/../etc/passwd"),
            Err(NormalizeError::PathTraversal)
        );
        assert_eq!(
            normalize_target("/a/../../b"),
            Err(NormalizeError::PathTraversal)
        );
    }

    #[test]
    fn rejects_traversal_via_encoded_dots() {
        // %2e%2e == ".." — must still be caught after decoding.
        assert_eq!(
            normalize_target("/%2e%2e/secret"),
            Err(NormalizeError::PathTraversal)
        );
    }

    #[test]
    fn rejects_backslash() {
        assert_eq!(normalize_target("/a\\b"), Err(NormalizeError::Backslash));
        assert_eq!(normalize_target("/a%5cb"), Err(NormalizeError::Backslash));
    }

    #[test]
    fn rejects_nul_byte() {
        assert_eq!(normalize_target("/a%00b"), Err(NormalizeError::NulByte));
    }

    #[test]
    fn rejects_encoded_slash() {
        assert_eq!(
            normalize_target("/app/x%2fy"),
            Err(NormalizeError::EncodedSlash)
        );
        assert_eq!(
            normalize_target("/app/x%2Fy"),
            Err(NormalizeError::EncodedSlash)
        );
    }

    #[test]
    fn rejects_bad_percent_encoding() {
        assert_eq!(
            normalize_target("/a%zz"),
            Err(NormalizeError::BadPercentEncoding)
        );
        assert_eq!(
            normalize_target("/a%4"),
            Err(NormalizeError::BadPercentEncoding)
        );
    }

    #[test]
    fn rejects_non_absolute_target() {
        assert_eq!(
            normalize_target("app/index.html"),
            Err(NormalizeError::NotAbsolute)
        );
    }

    #[test]
    fn asterisk_form_is_allowed() {
        let n = normalize_target("*").unwrap();
        assert_eq!(n.path, "*");
    }

    #[test]
    fn root_path_stays_root() {
        let n = normalize_target("/").unwrap();
        assert_eq!(n.path, "/");
    }
}
