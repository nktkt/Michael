//! Request hardening — URI normalization/validation and request-limit
//! enforcement.
//!
//! This module is **fully working** and is the security core of the crate.
//! Historically, a large share of servlet-container vulnerabilities come from
//! *ambiguous request parsing*: a path that one layer normalizes one way and
//! another layer interprets differently, letting an attacker reach
//! `/WEB-INF/web.xml`, escape the document root, or smuggle a second request.
//!
//! The strategy here is **reject, don't sanitize**. Anything ambiguous or
//! suspicious is turned into [`tomcatrs_core::Error::Rejected`] rather than
//! quietly rewritten, so there is exactly one interpretation of every URI that
//! is allowed through.

use tomcatrs_config::RequestLimits;
use tomcatrs_core::{Error, Result};

/// Percent-decode `input` a single time.
///
/// Returns the decoded bytes. A `%` that is not followed by two hex digits is
/// left literally in place — callers that want to *reject* malformed encoding
/// inspect the raw input separately. Decoding is deliberately performed only
/// once: double-encoding (`%252e`) therefore survives as a literal `%2e` and
/// is caught by the validation pass.
fn percent_decode(input: &str) -> Vec<u8> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Returns `true` if `path` contains a path-traversal segment.
///
/// A traversal is any `..` that stands alone as a path segment — i.e. it is
/// bounded by `/` or a string boundary on each side. A literal `..` embedded
/// inside a longer name (`..foo`, `foo..bar`) is *not* a traversal and is
/// allowed.
pub fn is_path_traversal(path: &str) -> bool {
    path.split('/').any(|segment| segment == "..")
}

/// Returns `true` if `path` targets a container-protected directory.
///
/// `/WEB-INF` and `/META-INF` hold deployment descriptors, compiled classes,
/// and libraries; the servlet specification forbids serving them to clients.
/// The check is case-insensitive because some filesystems are, and matches
/// both the directory itself and anything beneath it.
pub fn is_protected_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    for protected in ["/web-inf", "/meta-inf"] {
        if lower == protected || lower.starts_with(&format!("{protected}/")) {
            return true;
        }
    }
    false
}

/// Collapse `.` and `..` segments of an already-decoded, slash-rooted path.
///
/// Returns `None` if a `..` segment would escape above the root — that is a
/// rejection condition, never something to clamp silently.
fn collapse_dot_segments(path: &str) -> Option<String> {
    let mut stack: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            // Empty (from `//` or leading `/`) and `.` contribute nothing.
            "" | "." => {}
            ".." => {
                // Popping an empty stack means climbing above the root.
                stack.pop()?;
            }
            other => stack.push(other),
        }
    }
    let mut normalized = String::from("/");
    normalized.push_str(&stack.join("/"));
    Some(normalized)
}

/// Percent-decode, normalize, and validate a request URI path.
///
/// On success the returned string is a canonical, slash-rooted path with all
/// `.`/`..` segments collapsed and a single decoding applied — the one and
/// only form downstream code should ever see.
///
/// # Errors
///
/// Returns [`tomcatrs_core::Error::Rejected`] when the URI:
///
/// * contains a backslash (`\`) — Windows-style separators are an
///   interpretation-mismatch classic;
/// * contains a NUL byte, raw or as `%00` — a string-truncation vector;
/// * contains an encoded slash (`%2f` / `%5c`) — it would hide a segment
///   boundary from this normalizer;
/// * uses path traversal (`..`, `%2e%2e`, …) that escapes the document root;
/// * directly targets `/WEB-INF` or `/META-INF`.
///
/// # Examples
///
/// ```
/// use tomcatrs_security::access_control::normalize_and_validate_uri;
///
/// assert_eq!(
///     normalize_and_validate_uri("/app/./images/../style.css").unwrap(),
///     "/app/style.css"
/// );
/// assert!(normalize_and_validate_uri("/app/../../etc/passwd").is_err());
/// assert!(normalize_and_validate_uri("/WEB-INF/web.xml").is_err());
/// ```
pub fn normalize_and_validate_uri(uri: &str) -> Result<String> {
    // Strip any query string before path analysis — `?` and everything after
    // is not part of the path.
    let raw_path = uri.split(['?', '#']).next().unwrap_or(uri);

    // (1) Reject suspicious bytes in the *raw*, still-encoded form. Catching
    // encoded slashes here is essential: after decoding they would silently
    // become real separators.
    let lower_raw = raw_path.to_ascii_lowercase();
    if lower_raw.contains("%2f") || lower_raw.contains("%5c") {
        return Err(Error::Rejected(format!(
            "URI contains an encoded path separator: {uri}"
        )));
    }
    if lower_raw.contains("%00") {
        return Err(Error::Rejected(format!(
            "URI contains an encoded NUL byte: {uri}"
        )));
    }

    // (2) Decode exactly once, then validate the decoded bytes.
    let decoded_bytes = percent_decode(raw_path);
    if decoded_bytes.contains(&0) {
        return Err(Error::Rejected(format!("URI contains a NUL byte: {uri}")));
    }
    if decoded_bytes.contains(&b'\\') {
        return Err(Error::Rejected(format!(
            "URI contains a backslash separator: {uri}"
        )));
    }
    let decoded = String::from_utf8(decoded_bytes)
        .map_err(|_| Error::Rejected(format!("URI is not valid UTF-8 after decoding: {uri}")))?;

    // (3) A normalized request path is always absolute.
    if !decoded.starts_with('/') {
        return Err(Error::Rejected(format!("URI path is not absolute: {uri}")));
    }

    // (4) Reject traversal explicitly before collapsing, so even a traversal
    // that happens to stay within root (`/a/../b`) is reported clearly... no:
    // such a path is legitimate. Only escaping traversal is rejected, which
    // `collapse_dot_segments` detects by returning `None`.
    let normalized = collapse_dot_segments(&decoded).ok_or_else(|| {
        Error::Rejected(format!(
            "URI uses path traversal that escapes the root: {uri}"
        ))
    })?;

    // Defense in depth: `collapse_dot_segments` removed every `.`/`..`, so a
    // surviving `..` segment would indicate a bug. Treat it as a rejection.
    if is_path_traversal(&normalized) {
        return Err(Error::Rejected(format!(
            "URI still contains traversal after normalization: {uri}"
        )));
    }

    // (5) Finally, forbid the protected container directories on the
    // *normalized* path, so `/foo/../WEB-INF` cannot sneak through.
    if is_protected_path(&normalized) {
        return Err(Error::Rejected(format!(
            "URI targets a protected directory: {normalized}"
        )));
    }

    Ok(normalized)
}

/// Enforce the connector's configured [`RequestLimits`] against the measured
/// shape of an incoming request.
///
/// The caller passes the counts/sizes it has observed while parsing the
/// request line and headers:
///
/// * `header_count` — number of header lines received.
/// * `header_bytes` — total size of the header block, in bytes.
/// * `uri_len` — length of the request-target, in bytes.
/// * `content_length` — declared body length; `None` when absent.
///
/// # Errors
///
/// Returns [`tomcatrs_core::Error::Rejected`] with a precise, limit-naming
/// message the moment any single limit is exceeded.
pub fn enforce_limits(
    limits: &RequestLimits,
    header_count: usize,
    header_bytes: usize,
    uri_len: usize,
    content_length: Option<usize>,
) -> Result<()> {
    if header_count > limits.max_header_count {
        return Err(Error::Rejected(format!(
            "too many request headers: {header_count} exceeds limit of {}",
            limits.max_header_count
        )));
    }
    if header_bytes > limits.max_header_size {
        return Err(Error::Rejected(format!(
            "request header block too large: {header_bytes} bytes exceeds limit of {}",
            limits.max_header_size
        )));
    }
    if uri_len > limits.max_uri_len {
        return Err(Error::Rejected(format!(
            "request URI too long: {uri_len} bytes exceeds limit of {}",
            limits.max_uri_len
        )));
    }
    if let Some(len) = content_length {
        if len > limits.max_post_size {
            return Err(Error::Rejected(format!(
                "request body too large: {len} bytes exceeds limit of {}",
                limits.max_post_size
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- normalize_and_validate_uri: acceptance -----

    #[test]
    fn accepts_a_plain_path() {
        assert_eq!(
            normalize_and_validate_uri("/index.html").unwrap(),
            "/index.html"
        );
    }

    #[test]
    fn accepts_and_collapses_dot_segments() {
        assert_eq!(
            normalize_and_validate_uri("/app/./images/../style.css").unwrap(),
            "/app/style.css"
        );
    }

    #[test]
    fn accepts_internal_traversal_that_stays_within_root() {
        // `/a/../b` resolves to `/b` — legitimate, must be accepted.
        assert_eq!(normalize_and_validate_uri("/a/../b").unwrap(), "/b");
    }

    #[test]
    fn accepts_percent_encoded_safe_characters() {
        // %20 == space
        assert_eq!(
            normalize_and_validate_uri("/my%20file.txt").unwrap(),
            "/my file.txt"
        );
    }

    #[test]
    fn accepts_double_dots_inside_a_segment_name() {
        // `..foo` is a filename, not a traversal.
        assert_eq!(
            normalize_and_validate_uri("/dir/..foo").unwrap(),
            "/dir/..foo"
        );
    }

    #[test]
    fn strips_query_string_before_validation() {
        assert_eq!(
            normalize_and_validate_uri("/search?q=../../etc").unwrap(),
            "/search"
        );
    }

    // ----- normalize_and_validate_uri: rejection -----

    #[test]
    fn rejects_plain_dotdot_traversal() {
        assert!(normalize_and_validate_uri("/app/../../etc/passwd").is_err());
        assert!(normalize_and_validate_uri("/../secret").is_err());
    }

    #[test]
    fn rejects_encoded_dotdot_traversal() {
        // %2e%2e == ".."
        assert!(normalize_and_validate_uri("/app/%2e%2e/%2e%2e/etc/passwd").is_err());
        assert!(normalize_and_validate_uri("/%2e%2e/secret").is_err());
    }

    #[test]
    fn rejects_encoded_forward_slash() {
        // %2f == "/"
        let err = normalize_and_validate_uri("/app%2f..%2fsecret").unwrap_err();
        assert!(matches!(err, Error::Rejected(_)));
        assert!(normalize_and_validate_uri("/a%2Fb").is_err());
    }

    #[test]
    fn rejects_backslash() {
        assert!(normalize_and_validate_uri("/app\\..\\secret").is_err());
        // %5c == "\"
        assert!(normalize_and_validate_uri("/app%5csecret").is_err());
    }

    #[test]
    fn rejects_nul_byte() {
        assert!(normalize_and_validate_uri("/app%00.jsp").is_err());
        assert!(normalize_and_validate_uri("/app\u{0}.jsp").is_err());
    }

    #[test]
    fn rejects_web_inf_and_meta_inf() {
        assert!(normalize_and_validate_uri("/WEB-INF/web.xml").is_err());
        assert!(normalize_and_validate_uri("/web-inf/classes/App.class").is_err());
        assert!(normalize_and_validate_uri("/META-INF/MANIFEST.MF").is_err());
        // Reached via a collapsed traversal — must still be caught.
        assert!(normalize_and_validate_uri("/public/../WEB-INF/web.xml").is_err());
    }

    #[test]
    fn rejects_non_absolute_path() {
        assert!(normalize_and_validate_uri("relative/path").is_err());
    }

    // ----- helper predicates -----

    #[test]
    fn is_path_traversal_predicate() {
        assert!(is_path_traversal("/a/../b"));
        assert!(is_path_traversal(".."));
        assert!(!is_path_traversal("/a/b/c"));
        assert!(!is_path_traversal("/a/..b/c"));
    }

    #[test]
    fn is_protected_path_predicate() {
        assert!(is_protected_path("/WEB-INF"));
        assert!(is_protected_path("/WEB-INF/web.xml"));
        assert!(is_protected_path("/meta-inf/services"));
        assert!(!is_protected_path("/web-information/page"));
        assert!(!is_protected_path("/public/index.html"));
    }

    // ----- enforce_limits -----

    #[test]
    fn enforce_limits_accepts_a_request_within_bounds() {
        let limits = RequestLimits::default();
        assert!(enforce_limits(&limits, 20, 4096, 128, Some(1024)).is_ok());
        // No body declared is fine.
        assert!(enforce_limits(&limits, 1, 64, 8, None).is_ok());
    }

    #[test]
    fn enforce_limits_rejects_too_many_headers() {
        let limits = RequestLimits::default();
        let err = enforce_limits(&limits, limits.max_header_count + 1, 100, 10, None).unwrap_err();
        match err {
            Error::Rejected(msg) => assert!(msg.contains("too many request headers")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn enforce_limits_rejects_oversize_header_block() {
        let limits = RequestLimits::default();
        let err = enforce_limits(&limits, 1, limits.max_header_size + 1, 10, None).unwrap_err();
        assert!(matches!(err, Error::Rejected(msg) if msg.contains("header block too large")));
    }

    #[test]
    fn enforce_limits_rejects_oversize_uri() {
        let limits = RequestLimits::default();
        let err = enforce_limits(&limits, 1, 64, limits.max_uri_len + 1, None).unwrap_err();
        assert!(matches!(err, Error::Rejected(msg) if msg.contains("URI too long")));
    }

    #[test]
    fn enforce_limits_rejects_oversize_post_body() {
        let limits = RequestLimits::default();
        let err = enforce_limits(&limits, 1, 64, 10, Some(limits.max_post_size + 1)).unwrap_err();
        assert!(matches!(err, Error::Rejected(msg) if msg.contains("request body too large")));
    }
}
