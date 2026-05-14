//! Cookie parsing/building and HTTP content-negotiation helpers.
//!
//! This module is intentionally low-level and free-standing: it depends only
//! on `std` so that `tomcatrs-coyote` can stay at the bottom of the dependency
//! graph. The higher-level `tomcatrs-session` crate has its own
//! `CookieProcessor` tuned for the `JSESSIONID` cookie; this module instead
//! provides the generic primitives a connector or adapter needs.
//!
//! # Cookies
//!
//! * [`parse_cookie_header`] decodes an inbound `Cookie:` request header into a
//!   list of [`Cookie`] name/value pairs ([RFC 6265] §5.4 parsing, lenient).
//! * [`SetCookie`] builds an outbound `Set-Cookie` response header value via a
//!   small builder API, including the [RFC 6265bis] additions `SameSite` and
//!   `Partitioned`.
//!
//! # Content negotiation
//!
//! * [`parse_accept`] parses an `Accept:` header into quality-ordered
//!   [`MediaRange`]s.
//! * [`parse_accept_encoding`] / [`parse_accept_language`] parse the simpler
//!   token-based `Accept-Encoding:` / `Accept-Language:` headers.
//! * [`negotiate`] picks the best match for a server's available media types.
//!
//! [RFC 6265]: https://www.rfc-editor.org/rfc/rfc6265
//! [RFC 6265bis]: https://datatracker.ietf.org/doc/draft-ietf-httpbis-rfc6265bis/

use std::fmt;

// ===========================================================================
// Request cookies
// ===========================================================================

/// A single name/value pair parsed from an inbound `Cookie:` request header.
///
/// Request cookies carry no attributes — `Path`, `Domain`, `Secure` and the
/// like only ever travel on the *response* `Set-Cookie` header (see
/// [`SetCookie`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cookie {
    /// The cookie name, taken verbatim from the header.
    pub name: String,
    /// The cookie value, with any surrounding double quotes stripped.
    pub value: String,
}

impl Cookie {
    /// Construct a [`Cookie`] from any string-like name and value.
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Cookie {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// Parse a `Cookie:` request header value into its constituent [`Cookie`]s.
///
/// Implements lenient [RFC 6265] §5.4 parsing, matching real browser/server
/// behaviour:
///
/// * pairs are separated by `;`,
/// * surrounding whitespace around each pair, the name, and the value is
///   trimmed,
/// * a value that is wholly wrapped in double quotes has those quotes stripped,
/// * malformed pairs — those with no `=`, or with an empty name — are skipped
///   rather than aborting the whole parse.
///
/// A connection may legally send more than one `Cookie:` header; callers with
/// several header values should concatenate the results of calling this once
/// per header (see [`crate::Request::cookies`]).
///
/// [RFC 6265]: https://www.rfc-editor.org/rfc/rfc6265
pub fn parse_cookie_header(header_value: &str) -> Vec<Cookie> {
    header_value
        .split(';')
        .filter_map(|pair| {
            let pair = pair.trim();
            if pair.is_empty() {
                return None;
            }
            // A pair without '=' is malformed; skip it.
            let (name, value) = pair.split_once('=')?;
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            let value = value.trim();
            // Strip a single layer of surrounding double quotes, if present.
            let value = match (value.strip_prefix('"'), value.strip_suffix('"')) {
                (Some(_), Some(_)) if value.len() >= 2 => &value[1..value.len() - 1],
                _ => value,
            };
            Some(Cookie::new(name, value))
        })
        .collect()
}

// ===========================================================================
// Response cookies (`Set-Cookie`)
// ===========================================================================

/// The `SameSite` attribute of a `Set-Cookie` header ([RFC 6265bis] §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    /// `SameSite=Strict` — withheld on every cross-site request.
    Strict,
    /// `SameSite=Lax` — sent on top-level cross-site navigations only.
    Lax,
    /// `SameSite=None` — always sent; browsers require `Secure` alongside it.
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

/// An outbound cookie, serialized via [`SetCookie::to_header_value`] into a
/// `Set-Cookie` response header value.
///
/// Construct one with [`SetCookie::new`] and chain the builder methods:
///
/// ```
/// use tomcatrs_coyote::cookies::{SetCookie, SameSite};
///
/// let value = SetCookie::new("sid", "abc123")
///     .path("/")
///     .http_only(true)
///     .secure(true)
///     .same_site(SameSite::Lax)
///     .max_age(3600)
///     .to_header_value();
/// assert_eq!(
///     value,
///     "sid=abc123; Path=/; Max-Age=3600; Secure; HttpOnly; SameSite=Lax"
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetCookie {
    /// The cookie name (the `name` of the `name=value` pair).
    pub name: String,
    /// The cookie value (the `value` of the `name=value` pair).
    pub value: String,
    /// The `Path` attribute, if any.
    pub path: Option<String>,
    /// The `Domain` attribute, if any.
    pub domain: Option<String>,
    /// The `Max-Age` attribute in seconds, if any. A value `<= 0` instructs the
    /// client to expire the cookie immediately.
    pub max_age: Option<i64>,
    /// The `Expires` attribute as a preformatted IMF-fixdate string, if any.
    /// Use [`SetCookie::expires_unix`] to set this from a Unix timestamp.
    pub expires: Option<String>,
    /// Whether to emit the `Secure` attribute.
    pub secure: bool,
    /// Whether to emit the `HttpOnly` attribute.
    pub http_only: bool,
    /// The `SameSite` attribute, if any.
    pub same_site: Option<SameSite>,
    /// Whether to emit the `Partitioned` attribute ([CHIPS]).
    ///
    /// [CHIPS]: https://developer.mozilla.org/docs/Web/Privacy/Privacy_sandbox/Partitioned_cookies
    pub partitioned: bool,
}

impl SetCookie {
    /// Begin building a `Set-Cookie` for `name=value`. All attributes start
    /// unset / `false`.
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        SetCookie {
            name: name.into(),
            value: value.into(),
            path: None,
            domain: None,
            max_age: None,
            expires: None,
            secure: false,
            http_only: false,
            same_site: None,
            partitioned: false,
        }
    }

    /// Set the `Path` attribute.
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Set the `Domain` attribute.
    pub fn domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    /// Set the `Max-Age` attribute (in seconds).
    pub fn max_age(mut self, seconds: i64) -> Self {
        self.max_age = Some(seconds);
        self
    }

    /// Set the `Expires` attribute from an already-formatted IMF-fixdate
    /// string (e.g. `Wed, 21 Oct 2015 07:28:00 GMT`).
    pub fn expires(mut self, imf_fixdate: impl Into<String>) -> Self {
        self.expires = Some(imf_fixdate.into());
        self
    }

    /// Set the `Expires` attribute from a Unix timestamp (seconds since the
    /// epoch), formatting it as an [RFC 7231] IMF-fixdate.
    ///
    /// [RFC 7231]: https://www.rfc-editor.org/rfc/rfc7231#section-7.1.1.1
    pub fn expires_unix(mut self, unix_seconds: i64) -> Self {
        self.expires = Some(format_imf_fixdate(unix_seconds));
        self
    }

    /// Enable or disable the `Secure` attribute.
    pub fn secure(mut self, secure: bool) -> Self {
        self.secure = secure;
        self
    }

    /// Enable or disable the `HttpOnly` attribute.
    pub fn http_only(mut self, http_only: bool) -> Self {
        self.http_only = http_only;
        self
    }

    /// Set the `SameSite` attribute.
    pub fn same_site(mut self, same_site: SameSite) -> Self {
        self.same_site = Some(same_site);
        self
    }

    /// Enable or disable the `Partitioned` attribute.
    pub fn partitioned(mut self, partitioned: bool) -> Self {
        self.partitioned = partitioned;
        self
    }

    /// Serialize this cookie into a `Set-Cookie` header **value**.
    ///
    /// Attribute order follows the conventional `Set-Cookie` layout:
    /// `name=value; Domain=…; Path=…; Max-Age=…; Expires=…; Secure; HttpOnly;
    /// SameSite=…; Partitioned`. Only the attributes that are set are emitted.
    ///
    /// ```
    /// use tomcatrs_coyote::cookies::{SetCookie, SameSite};
    ///
    /// let v = SetCookie::new("c", "v").same_site(SameSite::None).secure(true);
    /// assert_eq!(v.to_header_value(), "c=v; Secure; SameSite=None");
    /// ```
    pub fn to_header_value(&self) -> String {
        let mut out = String::with_capacity(self.name.len() + self.value.len() + 32);
        out.push_str(&self.name);
        out.push('=');
        out.push_str(&self.value);

        if let Some(domain) = &self.domain {
            out.push_str("; Domain=");
            out.push_str(domain);
        }
        if let Some(path) = &self.path {
            out.push_str("; Path=");
            out.push_str(path);
        }
        if let Some(max_age) = self.max_age {
            out.push_str("; Max-Age=");
            out.push_str(&max_age.to_string());
        }
        if let Some(expires) = &self.expires {
            out.push_str("; Expires=");
            out.push_str(expires);
        }
        if self.secure {
            out.push_str("; Secure");
        }
        if self.http_only {
            out.push_str("; HttpOnly");
        }
        if let Some(same_site) = self.same_site {
            out.push_str("; SameSite=");
            out.push_str(&same_site.to_string());
        }
        if self.partitioned {
            out.push_str("; Partitioned");
        }
        out
    }
}

impl fmt::Display for SetCookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_header_value())
    }
}

// ===========================================================================
// IMF-fixdate formatting (RFC 7231 §7.1.1.1)
// ===========================================================================

/// Format a Unix timestamp (seconds since 1970-01-01T00:00:00Z) as an
/// [RFC 7231] IMF-fixdate, e.g. `Sun, 06 Nov 1994 08:49:37 GMT`.
///
/// This is a hand-rolled implementation of the civil-from-days algorithm so
/// the crate stays dependency-free. Negative timestamps (dates before the
/// epoch) are supported.
///
/// [RFC 7231]: https://www.rfc-editor.org/rfc/rfc7231#section-7.1.1.1
pub fn format_imf_fixdate(unix_seconds: i64) -> String {
    // Split into whole days and the second-of-day, flooring towards -inf so
    // that pre-epoch timestamps are handled correctly.
    let days = unix_seconds.div_euclid(86_400);
    let secs_of_day = unix_seconds.rem_euclid(86_400);

    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    // Day of week: 1970-01-01 was a Thursday (index 4 with Sunday = 0).
    let dow = (days.rem_euclid(7) + 4) % 7;
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    // Civil date from days since the epoch — Howard Hinnant's algorithm.
    // Shift the epoch to 0000-03-01 so leap days fall at the end of the era.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let year_shifted = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March = 0
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 {
        year_shifted + 1
    } else {
        year_shifted
    };

    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        WEEKDAYS[dow as usize],
        day,
        MONTHS[(month - 1) as usize],
        year,
        hour,
        minute,
        second,
    )
}

// ===========================================================================
// Content negotiation
// ===========================================================================

/// A single entry of an `Accept:` header: a media range plus its quality and
/// any extension parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaRange {
    /// The top-level type, e.g. `text`, or `*` for the wildcard.
    pub type_: String,
    /// The subtype, e.g. `html`, or `*` for the wildcard.
    pub subtype: String,
    /// The `q` quality factor in `[0.0, 1.0]`. Defaults to `1.0` when absent.
    pub quality: f32,
    /// Any `;key=value` parameters other than `q`, in the order they appeared.
    pub params: Vec<(String, String)>,
}

impl MediaRange {
    /// The full `type/subtype` string this range matches against.
    pub fn full_type(&self) -> String {
        format!("{}/{}", self.type_, self.subtype)
    }

    /// A specificity score used to break ties at equal quality: a fully
    /// specified type (`text/html`) outranks a subtype wildcard (`text/*`),
    /// which outranks the full wildcard (`*/*`). More parameters also raise
    /// specificity.
    fn specificity(&self) -> u32 {
        let mut score = 0;
        if self.type_ != "*" {
            score += 2;
        }
        if self.subtype != "*" {
            score += 1;
        }
        score * 4 + self.params.len().min(3) as u32
    }

    /// Does this media range match the concrete `type/subtype` token
    /// `candidate` (honouring `*` wildcards)?
    pub fn matches(&self, candidate: &str) -> bool {
        let (ctype, csub) = match candidate.split_once('/') {
            Some(parts) => parts,
            None => return false,
        };
        (self.type_ == "*" || self.type_.eq_ignore_ascii_case(ctype))
            && (self.subtype == "*" || self.subtype.eq_ignore_ascii_case(csub))
    }
}

/// Parse an `Accept:` header into [`MediaRange`]s, sorted best-first.
///
/// Sorting is by quality descending, then by specificity descending (a
/// concrete `type/subtype` beats `type/*` beats `*/*`). Entries with `q=0` are
/// retained — they explicitly mean "not acceptable" and a caller doing
/// negotiation needs to see them; [`negotiate`] filters them out itself.
///
/// Malformed entries (those without a `/`) are skipped.
pub fn parse_accept(header: &str) -> Vec<MediaRange> {
    let mut ranges: Vec<MediaRange> = header
        .split(',')
        .filter_map(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                return None;
            }
            let mut parts = entry.split(';');
            let media = parts.next()?.trim();
            let (type_, subtype) = media.split_once('/')?;
            let type_ = type_.trim();
            let subtype = subtype.trim();
            if type_.is_empty() || subtype.is_empty() {
                return None;
            }

            let mut quality = 1.0_f32;
            let mut params = Vec::new();
            for param in parts {
                let param = param.trim();
                if param.is_empty() {
                    continue;
                }
                let (key, val) = match param.split_once('=') {
                    Some((k, v)) => (k.trim(), v.trim().trim_matches('"')),
                    None => continue,
                };
                if key.eq_ignore_ascii_case("q") {
                    quality = val.parse::<f32>().unwrap_or(1.0).clamp(0.0, 1.0);
                } else {
                    params.push((key.to_string(), val.to_string()));
                }
            }

            Some(MediaRange {
                type_: type_.to_string(),
                subtype: subtype.to_string(),
                quality,
                params,
            })
        })
        .collect();

    ranges.sort_by(|a, b| {
        b.quality
            .partial_cmp(&a.quality)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.specificity().cmp(&a.specificity()))
    });
    ranges
}

/// One entry of a token-based quality list, e.g. an `Accept-Encoding:` or
/// `Accept-Language:` header item.
#[derive(Debug, Clone, PartialEq)]
pub struct QualityValue {
    /// The token itself, e.g. `gzip`, `en-US`, or `*`. Lower-cased.
    pub value: String,
    /// The `q` quality factor in `[0.0, 1.0]`. Defaults to `1.0` when absent.
    pub quality: f32,
}

/// Parse a comma-separated list of `token;q=weight` items, sorted best-first.
///
/// This is the shared core behind [`parse_accept_encoding`] and
/// [`parse_accept_language`]. Tokens are lower-cased for case-insensitive
/// comparison. Items with `q=0` are kept (they mean "explicitly rejected").
/// Empty items and items whose token is empty are skipped.
pub fn parse_quality_list(header: &str) -> Vec<QualityValue> {
    let mut items: Vec<QualityValue> = header
        .split(',')
        .filter_map(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                return None;
            }
            let mut parts = entry.split(';');
            let token = parts.next()?.trim();
            if token.is_empty() {
                return None;
            }
            let mut quality = 1.0_f32;
            for param in parts {
                let param = param.trim();
                if let Some((key, val)) = param.split_once('=') {
                    if key.trim().eq_ignore_ascii_case("q") {
                        quality = val.trim().parse::<f32>().unwrap_or(1.0).clamp(0.0, 1.0);
                    }
                }
            }
            Some(QualityValue {
                value: token.to_ascii_lowercase(),
                quality,
            })
        })
        .collect();

    items.sort_by(|a, b| {
        b.quality
            .partial_cmp(&a.quality)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    items
}

/// Parse an `Accept-Encoding:` header into a best-first list of
/// [`QualityValue`]s (e.g. `gzip`, `br`, `identity`, `*`).
pub fn parse_accept_encoding(header: &str) -> Vec<QualityValue> {
    parse_quality_list(header)
}

/// Parse an `Accept-Language:` header into a best-first list of
/// [`QualityValue`]s (e.g. `en-us`, `fr`, `*`).
pub fn parse_accept_language(header: &str) -> Vec<QualityValue> {
    parse_quality_list(header)
}

/// Pick the best `available` media type for an `Accept:` header value.
///
/// The `available` slice is the server's list of producible media types in
/// the server's own order of preference (most-preferred first). For each
/// candidate this finds the highest-quality matching [`MediaRange`]; the
/// candidate with the best quality wins, ties broken by the server's ordering.
/// Candidates whose best match has `q=0` are rejected. An empty or absent
/// `Accept` header (which parses to no ranges) means "anything is acceptable",
/// so the server's first choice is returned.
///
/// Returns `None` only when every candidate is explicitly unacceptable.
///
/// ```
/// use tomcatrs_coyote::cookies::negotiate;
///
/// let pick = negotiate(
///     "text/html;q=0.8, application/json;q=0.9",
///     &["text/html", "application/json"],
/// );
/// assert_eq!(pick, Some("application/json"));
/// ```
pub fn negotiate<'a>(accept: &str, available: &[&'a str]) -> Option<&'a str> {
    let ranges = parse_accept(accept);

    // No ranges at all → the client expressed no preference.
    if ranges.is_empty() {
        return available.first().copied();
    }

    let mut best: Option<(&'a str, f32)> = None;
    for (idx, &candidate) in available.iter().enumerate() {
        // The first matching range is the highest quality one, since
        // `parse_accept` already sorted by quality then specificity.
        let quality = ranges
            .iter()
            .find(|r| r.matches(candidate))
            .map(|r| r.quality);
        let quality = match quality {
            Some(q) if q > 0.0 => q,
            _ => continue, // no match, or explicitly q=0
        };
        match best {
            // Strictly greater quality wins; equal quality keeps the earlier
            // (more server-preferred) candidate.
            Some((_, best_q)) if quality > best_q => best = Some((candidate, quality)),
            None => best = Some((candidate, quality)),
            _ => {}
        }
        let _ = idx;
    }
    best.map(|(c, _)| c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multi_cookie_header_with_quotes_and_skips_malformed() {
        let cookies = parse_cookie_header(
            "  JSESSIONID=ABC123 ; theme=dark; quoted=\"a value\"; no-equals; =novalue; empty=",
        );
        assert_eq!(
            cookies,
            vec![
                Cookie::new("JSESSIONID", "ABC123"),
                Cookie::new("theme", "dark"),
                Cookie::new("quoted", "a value"),
                Cookie::new("empty", ""),
            ]
        );
    }

    #[test]
    fn parse_cookie_header_handles_empty_input() {
        assert!(parse_cookie_header("").is_empty());
        assert!(parse_cookie_header("   ;  ; ").is_empty());
    }

    #[test]
    fn set_cookie_minimal() {
        assert_eq!(SetCookie::new("sid", "abc").to_header_value(), "sid=abc");
    }

    #[test]
    fn set_cookie_with_all_attributes() {
        let value = SetCookie::new("sid", "abc123")
            .domain("example.com")
            .path("/app")
            .max_age(3600)
            .expires("Wed, 21 Oct 2015 07:28:00 GMT")
            .secure(true)
            .http_only(true)
            .same_site(SameSite::Lax)
            .partitioned(true)
            .to_header_value();
        assert_eq!(
            value,
            "sid=abc123; Domain=example.com; Path=/app; Max-Age=3600; \
             Expires=Wed, 21 Oct 2015 07:28:00 GMT; Secure; HttpOnly; \
             SameSite=Lax; Partitioned"
        );
    }

    #[test]
    fn set_cookie_same_site_none_secure() {
        let value = SetCookie::new("c", "v")
            .same_site(SameSite::None)
            .secure(true)
            .to_header_value();
        assert_eq!(value, "c=v; Secure; SameSite=None");
    }

    #[test]
    fn imf_fixdate_formats_known_timestamps() {
        // 784111777 == Sun, 06 Nov 1994 08:49:37 GMT (the RFC 7231 example).
        assert_eq!(
            format_imf_fixdate(784_111_777),
            "Sun, 06 Nov 1994 08:49:37 GMT"
        );
        // The epoch itself.
        assert_eq!(format_imf_fixdate(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        // A leap-year date: 2000-02-29.
        assert_eq!(
            format_imf_fixdate(951_782_400),
            "Tue, 29 Feb 2000 00:00:00 GMT"
        );
    }

    #[test]
    fn expires_unix_round_trips_into_header() {
        let v = SetCookie::new("c", "v").expires_unix(0).to_header_value();
        assert_eq!(v, "c=v; Expires=Thu, 01 Jan 1970 00:00:00 GMT");
    }

    #[test]
    fn parse_accept_orders_by_quality_then_specificity() {
        let ranges = parse_accept("text/*;q=0.5, text/html, */*;q=0.1, application/json;q=0.9");
        let ordered: Vec<String> = ranges.iter().map(MediaRange::full_type).collect();
        assert_eq!(
            ordered,
            vec![
                "text/html".to_string(),        // q=1.0, most specific
                "application/json".to_string(), // q=0.9
                "text/*".to_string(),           // q=0.5
                "*/*".to_string(),              // q=0.1
            ]
        );
    }

    #[test]
    fn parse_accept_keeps_extension_params() {
        let ranges = parse_accept("application/json;q=0.8;profile=\"x\"");
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].quality, 0.8);
        assert_eq!(
            ranges[0].params,
            vec![("profile".to_string(), "x".to_string())]
        );
    }

    #[test]
    fn negotiate_picks_best_available() {
        assert_eq!(
            negotiate(
                "text/html;q=0.8, application/json;q=0.9",
                &["text/html", "application/json"]
            ),
            Some("application/json")
        );
        // Server preference breaks an equal-quality tie.
        assert_eq!(
            negotiate("*/*", &["application/json", "text/html"]),
            Some("application/json")
        );
        // Empty Accept → anything goes → server's first choice.
        assert_eq!(
            negotiate("", &["text/plain", "text/html"]),
            Some("text/plain")
        );
    }

    #[test]
    fn negotiate_rejects_explicitly_unacceptable() {
        // text/html is q=0; only text/plain remains acceptable.
        assert_eq!(
            negotiate("text/html;q=0, text/plain", &["text/html", "text/plain"]),
            Some("text/plain")
        );
        // Every candidate explicitly rejected → None.
        assert_eq!(negotiate("text/html;q=0", &["text/html"]), None);
    }

    #[test]
    fn parse_accept_encoding_handles_q_zero() {
        let encodings = parse_accept_encoding("gzip;q=0, br;q=1.0, deflate;q=0.5");
        // Sorted best-first; gzip kept but with quality 0.
        assert_eq!(
            encodings[0],
            QualityValue {
                value: "br".into(),
                quality: 1.0
            }
        );
        assert_eq!(
            encodings[1],
            QualityValue {
                value: "deflate".into(),
                quality: 0.5
            }
        );
        assert_eq!(
            encodings[2],
            QualityValue {
                value: "gzip".into(),
                quality: 0.0
            }
        );
    }

    #[test]
    fn parse_accept_language_lowercases_and_sorts() {
        let langs = parse_accept_language("en-US, fr;q=0.9, de;q=0.8");
        assert_eq!(langs[0].value, "en-us");
        assert_eq!(langs[1].value, "fr");
        assert_eq!(langs[2].value, "de");
    }

    #[test]
    fn parse_quality_list_skips_blank_entries() {
        let items = parse_quality_list(" , gzip , ");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].value, "gzip");
    }
}
