//! Parsing of inbound `Cookie:` headers and construction of `Set-Cookie`
//! values for the session-tracking cookie.

use std::fmt;

use crate::SESSION_COOKIE_NAME;

/// The `SameSite` attribute of a `Set-Cookie` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    /// `SameSite=Strict` — the cookie is withheld on all cross-site requests.
    Strict,
    /// `SameSite=Lax` — sent on top-level cross-site navigations only.
    Lax,
    /// `SameSite=None` — always sent; requires `Secure` to be honoured by
    /// modern browsers.
    None,
}

impl fmt::Display for SameSite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SameSite::Strict => "Strict",
            SameSite::Lax => "Lax",
            SameSite::None => "None",
        })
    }
}

/// Builds and parses the `JSESSIONID` cookie, mirroring Tomcat's
/// `Rfc6265CookieProcessor`.
///
/// A processor is configured once (path, `Secure`, `SameSite`) and then reused
/// to:
///
/// * extract the inbound session id from a request's `Cookie:` header
///   ([`CookieProcessor::extract_session_id`]), and
/// * build the `Set-Cookie` response header value for a session id
///   ([`CookieProcessor::build_set_cookie`]).
#[derive(Debug, Clone)]
pub struct CookieProcessor {
    /// The `Path` attribute. Defaults to `/`.
    path: String,
    /// Whether to emit the `Secure` attribute.
    secure: bool,
    /// The optional `SameSite` attribute.
    same_site: Option<SameSite>,
}

impl Default for CookieProcessor {
    fn default() -> Self {
        Self {
            path: "/".to_string(),
            secure: false,
            same_site: None,
        }
    }
}

impl CookieProcessor {
    /// Create a processor with Tomcat-like defaults: `Path=/`, no `Secure`,
    /// no `SameSite`. (`HttpOnly` is always emitted.)
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the `Path` attribute used in generated `Set-Cookie` values.
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    /// Enable or disable the `Secure` attribute.
    pub fn with_secure(mut self, secure: bool) -> Self {
        self.secure = secure;
        self
    }

    /// Set (or clear, with `None`) the `SameSite` attribute.
    pub fn with_same_site(mut self, same_site: Option<SameSite>) -> Self {
        self.same_site = same_site;
        self
    }

    /// Parse a `Cookie:` header value into its `(name, value)` pairs.
    ///
    /// The input is the raw header value, e.g.
    /// `JSESSIONID=ABC123; theme=dark`. Cookie pairs are separated by `;`,
    /// surrounding whitespace is trimmed, and a value may itself be empty or
    /// double-quoted (the quotes are stripped). Entries without an `=` are
    /// skipped, matching lenient browser behaviour.
    pub fn parse_cookie_header(header: &str) -> Vec<(String, String)> {
        header
            .split(';')
            .filter_map(|pair| {
                let pair = pair.trim();
                if pair.is_empty() {
                    return None;
                }
                let (name, value) = pair.split_once('=')?;
                let name = name.trim();
                if name.is_empty() {
                    return None;
                }
                let value = value.trim();
                // Strip optional surrounding double quotes from the value.
                let value = value
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .unwrap_or(value);
                Some((name.to_string(), value.to_string()))
            })
            .collect()
    }

    /// Extract the `JSESSIONID` value from a `Cookie:` header, if present.
    ///
    /// If the header carries the cookie more than once the first occurrence
    /// wins.
    pub fn extract_session_id(header: &str) -> Option<String> {
        Self::parse_cookie_header(header)
            .into_iter()
            .find(|(name, _)| name == SESSION_COOKIE_NAME)
            .map(|(_, value)| value)
    }

    /// Build the `Set-Cookie` header **value** binding `JSESSIONID` to
    /// `session_id`.
    ///
    /// The result always carries `Path` and `HttpOnly`, and additionally
    /// `Secure` and/or `SameSite=...` when the processor is configured for
    /// them. Example output:
    ///
    /// ```text
    /// JSESSIONID=ABC123; Path=/; HttpOnly; Secure; SameSite=Lax
    /// ```
    pub fn build_set_cookie(&self, session_id: &str) -> String {
        let mut out = format!("{SESSION_COOKIE_NAME}={session_id}");
        out.push_str("; Path=");
        out.push_str(&self.path);
        out.push_str("; HttpOnly");
        if self.secure {
            out.push_str("; Secure");
        }
        if let Some(same_site) = self.same_site {
            out.push_str("; SameSite=");
            out.push_str(&same_site.to_string());
        }
        out
    }

    /// Build a `Set-Cookie` value that **expires** the session cookie,
    /// instructing the browser to drop it (used on session invalidation).
    pub fn build_expiry_cookie(&self) -> String {
        format!(
            "{SESSION_COOKIE_NAME}=; Path={}; HttpOnly; Max-Age=0",
            self.path
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_cookie_pairs() {
        // Names are taken verbatim; values have surrounding quotes stripped.
        let pairs = CookieProcessor::parse_cookie_header(
            "  JSESSIONID=ABC123 ; theme=dark; empty=; quoted=\"v\" ",
        );
        assert_eq!(
            pairs,
            vec![
                ("JSESSIONID".to_string(), "ABC123".to_string()),
                ("theme".to_string(), "dark".to_string()),
                ("empty".to_string(), "".to_string()),
                ("quoted".to_string(), "v".to_string()),
            ]
        );
    }

    #[test]
    fn skips_malformed_entries() {
        let pairs = CookieProcessor::parse_cookie_header("no-equals; =novalue; a=b");
        assert_eq!(pairs, vec![("a".to_string(), "b".to_string())]);
    }

    #[test]
    fn extracts_jsessionid() {
        assert_eq!(
            CookieProcessor::extract_session_id("theme=dark; JSESSIONID=DEADBEEF; x=y"),
            Some("DEADBEEF".to_string())
        );
        assert_eq!(CookieProcessor::extract_session_id("theme=dark; x=y"), None);
    }

    #[test]
    fn builds_minimal_set_cookie() {
        let proc = CookieProcessor::new();
        assert_eq!(
            proc.build_set_cookie("ABC123"),
            "JSESSIONID=ABC123; Path=/; HttpOnly"
        );
    }

    #[test]
    fn builds_full_set_cookie() {
        let proc = CookieProcessor::new()
            .with_path("/app")
            .with_secure(true)
            .with_same_site(Some(SameSite::Lax));
        assert_eq!(
            proc.build_set_cookie("ABC123"),
            "JSESSIONID=ABC123; Path=/app; HttpOnly; Secure; SameSite=Lax"
        );
    }

    #[test]
    fn round_trips_through_parse() {
        let proc = CookieProcessor::new();
        let header = proc.build_set_cookie("ROUNDTRIP01");
        // The leading `name=value` segment is a valid Cookie-header entry.
        let cookie_part = header.split(';').next().unwrap();
        assert_eq!(
            CookieProcessor::extract_session_id(cookie_part),
            Some("ROUNDTRIP01".to_string())
        );
    }

    #[test]
    fn expiry_cookie_has_max_age_zero() {
        let proc = CookieProcessor::new().with_path("/app");
        assert_eq!(
            proc.build_expiry_cookie(),
            "JSESSIONID=; Path=/app; HttpOnly; Max-Age=0"
        );
    }
}
