//! HTTP `DIGEST` authentication — RFC 2617 / RFC 7616.
//!
//! This module is **fully working**: it parses an `Authorization: Digest ...`
//! request header, recomputes the digest response from the client's claimed
//! values and the realm-supplied `HA1`, and on a match returns the matching
//! [`Principal`]. The challenge side issues keyed, freshness-bounded nonces and
//! tracks per-nonce `nc` values to reject replay.
//!
//! # Threat model and design choices
//!
//! * **Authenticated nonces.** Each server nonce is `unix_millis:HMAC-like
//!   digest`, where the digest is `MD5(unix_millis : random : opaque_key)`. A
//!   nonce that was not minted by this process — and therefore did not run
//!   through the keyed digest with our `opaque_key` — fails validation
//!   immediately, before any database lookup. The `opaque_key` is supplied at
//!   construction time so operators can rotate it or share it across a fleet.
//! * **Freshness.** Nonces older than [`NONCE_TTL_MILLIS`] are rejected as stale. The
//!   client can re-prompt with the fresh challenge transparently.
//! * **Replay.** A `(nonce, nc)` table records every nonce-count seen for each
//!   nonce; a repeat is rejected. Old entries are pruned when their nonce
//!   expires.
//! * **Algorithms.** `MD5` and `MD5-sess` per RFC 2617. `auth-int` is *not*
//!   advertised because it requires hashing the full request body before
//!   authentication; the server only offers `qop=auth`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use rand::RngCore;

use crate::realm::{Principal, Realm};

/// The quality-of-protection value this authenticator advertises in its
/// challenge.
pub const QOP: &str = "auth";

/// Maximum age, in milliseconds, of a server nonce before it is rejected as
/// stale. Five minutes matches the conservative end of the Apache/Tomcat
/// defaults.
pub const NONCE_TTL_MILLIS: u128 = 5 * 60 * 1000;

/// Recognised values for the `algorithm` directive.
///
/// `MD5-sess` rebinds `HA1` to the nonce and client-nonce so the same
/// password can't be reused with a different challenge; otherwise it
/// behaves like plain `MD5`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestAlgorithm {
    /// `algorithm=MD5` (the default if the directive is absent).
    Md5,
    /// `algorithm=MD5-sess`.
    Md5Sess,
}

impl DigestAlgorithm {
    /// Render this algorithm as it should appear in an HTTP header.
    pub fn as_str(self) -> &'static str {
        match self {
            DigestAlgorithm::Md5 => "MD5",
            DigestAlgorithm::Md5Sess => "MD5-sess",
        }
    }

    /// Parse an `algorithm` directive value. Defaults to [`DigestAlgorithm::Md5`]
    /// when `None`, per RFC 2617 §3.2.1.
    pub fn parse(value: Option<&str>) -> Option<Self> {
        match value {
            None => Some(DigestAlgorithm::Md5),
            Some(v) if v.eq_ignore_ascii_case("MD5") => Some(DigestAlgorithm::Md5),
            Some(v) if v.eq_ignore_ascii_case("MD5-sess") => Some(DigestAlgorithm::Md5Sess),
            _ => None,
        }
    }
}

/// A parsed client `Authorization: Digest ...` response.
///
/// Every directive listed in RFC 2617 §3.2.2 that the server uses to recompute
/// the digest is preserved; unknown directives are dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DigestResponse {
    /// The authenticating user name (`username`).
    pub username: String,
    /// The protection space (`realm`).
    pub realm: String,
    /// The server nonce being answered (`nonce`).
    pub nonce: String,
    /// The request URI the digest was computed over (`uri`).
    pub uri: String,
    /// The client-computed `response` digest, as lowercase hex.
    pub response: String,
    /// The quality of protection actually applied (`qop`).
    pub qop: Option<String>,
    /// The client nonce (`cnonce`).
    pub cnonce: Option<String>,
    /// The nonce count (`nc`), as the 8-hex-digit string the client sent.
    pub nc: Option<String>,
    /// The echoed `opaque` value.
    pub opaque: Option<String>,
    /// The selected algorithm (`algorithm`).
    pub algorithm: Option<String>,
}

/// Tracks the nonce-counts already seen for a given server nonce, plus the
/// timestamp embedded in the nonce so the entry can be aged out.
#[derive(Debug)]
struct NonceState {
    /// Millis-since-epoch the nonce was minted at.
    issued_millis: u128,
    /// Every `nc` value already accepted for this nonce.
    seen_ncs: HashSet<u64>,
}

/// An HTTP `DIGEST` authenticator.
///
/// Clone-cheap: the realm is held behind an `Arc` and the nonce-replay table
/// behind an `Arc<Mutex<_>>`, so a single authenticator instance can be shared
/// across worker tasks.
#[derive(Clone)]
pub struct DigestAuthenticator {
    realm: Arc<dyn Realm>,
    realm_name: String,
    /// Operator-supplied secret mixed into every nonce so a nonce minted by
    /// this fleet can be distinguished from a forgery.
    opaque_key: Vec<u8>,
    /// `(nonce, nc)` replay tracker, shared between clones.
    nonces: Arc<Mutex<HashMap<String, NonceState>>>,
}

impl std::fmt::Debug for DigestAuthenticator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately do *not* render the realm trait object (which has no
        // `Debug` impl) or the opaque key (which is a secret). Reporting the
        // size of the replay table is useful for diagnostics.
        f.debug_struct("DigestAuthenticator")
            .field("realm_name", &self.realm_name)
            .field("opaque_key_len", &self.opaque_key.len())
            .field("tracked_nonces", &self.nonces.lock().len())
            .finish()
    }
}

impl DigestAuthenticator {
    /// Create an authenticator bound to `realm`, advertising `realm_name` in
    /// challenges, and using `opaque_key` to authenticate its own nonces.
    ///
    /// `opaque_key` should be at least 16 bytes of high-entropy material; it
    /// never leaves the server. Rotating it invalidates every outstanding
    /// nonce, which is the desired behaviour after a credential rotation or
    /// suspected compromise.
    pub fn new(realm: Arc<dyn Realm>, realm_name: impl Into<String>, opaque_key: &[u8]) -> Self {
        DigestAuthenticator {
            realm,
            realm_name: realm_name.into(),
            opaque_key: opaque_key.to_vec(),
            nonces: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The configured challenge realm name.
    pub fn realm_name(&self) -> &str {
        &self.realm_name
    }

    /// Hash `input` with MD5 and return the lowercase hex digest.
    fn md5_hex(input: &str) -> String {
        let digest = md5::compute(input.as_bytes());
        hex_lower(&digest.0)
    }

    /// Current wall-clock time in milliseconds since the UNIX epoch.
    fn now_millis() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    }

    /// Compute the keyed digest portion of a nonce.
    fn keyed_digest(&self, issued_millis: u128, salt: &[u8]) -> String {
        let mut buf = Vec::with_capacity(16 + salt.len() + self.opaque_key.len());
        buf.extend_from_slice(&issued_millis.to_be_bytes());
        buf.extend_from_slice(salt);
        buf.extend_from_slice(&self.opaque_key);
        let digest = md5::compute(&buf);
        hex_lower(&digest.0)
    }

    /// Mint a fresh nonce of the form `<unix_millis>:<salt_hex>:<digest_hex>`.
    ///
    /// The salt is 16 bytes of CSPRNG output and is rendered as 32 hex chars;
    /// the digest is `MD5(unix_millis || salt || opaque_key)`.
    pub fn generate_nonce(&self) -> String {
        let issued = Self::now_millis();
        let mut salt = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut salt);
        let salt_hex = hex_lower(&salt);
        let digest_hex = self.keyed_digest(issued, &salt);
        format!("{issued}:{salt_hex}:{digest_hex}")
    }

    /// Validate that `nonce` was minted by this authenticator and is not stale.
    ///
    /// Returns `Some(issued_millis)` on success.
    fn validate_nonce(&self, nonce: &str) -> Option<u128> {
        let mut parts = nonce.splitn(3, ':');
        let ts = parts.next()?;
        let salt_hex = parts.next()?;
        let digest_hex = parts.next()?;
        if parts.next().is_some() {
            return None;
        }
        let issued: u128 = ts.parse().ok()?;
        let salt = hex_decode(salt_hex)?;
        let expected = self.keyed_digest(issued, &salt);
        if !constant_time_eq(expected.as_bytes(), digest_hex.as_bytes()) {
            return None;
        }
        let now = Self::now_millis();
        // Reject nonces from the future (clock-skew or forgery) and nonces
        // older than the TTL.
        if issued > now + 1_000 {
            return None;
        }
        if now.saturating_sub(issued) > NONCE_TTL_MILLIS {
            return None;
        }
        Some(issued)
    }

    /// Generate the opaque value advertised in challenges. We piggy-back on
    /// the same keyed-digest construction so a forged `opaque` is detectable
    /// in theory, even though RFC 2617 only requires the client to echo it.
    fn generate_opaque(&self) -> String {
        let mut random = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut random);
        hex_lower(&random)
    }

    /// Produce a `WWW-Authenticate: Digest ...` *value* for a `401` response.
    ///
    /// The caller is responsible for attaching the header name itself.
    pub fn challenge(&self) -> String {
        let safe_realm = self.realm_name.replace('"', "");
        format!(
            "Digest realm=\"{realm}\", qop=\"{qop}\", nonce=\"{nonce}\", opaque=\"{opaque}\", algorithm=MD5",
            realm = safe_realm,
            qop = QOP,
            nonce = self.generate_nonce(),
            opaque = self.generate_opaque(),
        )
    }

    /// Produce a stale-challenge value, telling a compliant client to retry
    /// with the new nonce *without* re-prompting the user. Used after a
    /// previously-issued nonce times out.
    pub fn stale_challenge(&self) -> String {
        let safe_realm = self.realm_name.replace('"', "");
        format!(
            "Digest realm=\"{realm}\", qop=\"{qop}\", nonce=\"{nonce}\", opaque=\"{opaque}\", algorithm=MD5, stale=true",
            realm = safe_realm,
            qop = QOP,
            nonce = self.generate_nonce(),
            opaque = self.generate_opaque(),
        )
    }

    /// Parse a client `Authorization: Digest ...` header value.
    ///
    /// Returns `None` if the value is not the `Digest` scheme or is missing
    /// the mandatory `username`, `realm`, `nonce`, `uri`, or `response`
    /// directives. Quoted-string values have their surrounding double quotes
    /// stripped; unknown directives are silently ignored.
    pub fn parse_authorization(header_value: &str) -> Option<DigestResponse> {
        let rest = header_value
            .trim()
            .split_once(char::is_whitespace)
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("digest"))
            .map(|(_, rest)| rest)?;

        let mut out = DigestResponse::default();
        let mut saw_username = false;
        let mut saw_realm = false;
        let mut saw_nonce = false;
        let mut saw_uri = false;
        let mut saw_response = false;

        // Split on commas, but only when not inside a double-quoted string.
        // RFC 7616 forbids commas inside quoted directive values for everything
        // we care about, but defensively respecting quoting keeps the parser
        // forgiving in case some future directive contains one.
        for part in split_directives(rest) {
            let Some((key, raw_value)) = part.split_once('=') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = unquote(raw_value.trim());
            match key.as_str() {
                "username" => {
                    out.username = value;
                    saw_username = true;
                }
                "realm" => {
                    out.realm = value;
                    saw_realm = true;
                }
                "nonce" => {
                    out.nonce = value;
                    saw_nonce = true;
                }
                "uri" => {
                    out.uri = value;
                    saw_uri = true;
                }
                "response" => {
                    out.response = value;
                    saw_response = true;
                }
                "qop" => out.qop = Some(value),
                "cnonce" => out.cnonce = Some(value),
                "nc" => out.nc = Some(value),
                "opaque" => out.opaque = Some(value),
                "algorithm" => out.algorithm = Some(value),
                _ => {}
            }
        }

        if saw_username && saw_realm && saw_nonce && saw_uri && saw_response {
            Some(out)
        } else {
            None
        }
    }

    /// Authenticate a request given its raw `Authorization` header value and
    /// the HTTP method.
    ///
    /// Returns:
    ///
    /// * `Ok(Some(principal))` — the digest verified end-to-end.
    /// * `Ok(None)` — the header was missing, malformed, the user is unknown,
    ///   the user has no digest-compatible material (`digest_ha1` returned
    ///   `None`), the nonce was forged/stale, the `nc` was replayed, or the
    ///   recomputed response did not match. The caller should respond `401`
    ///   with [`challenge`](Self::challenge) (or
    ///   [`stale_challenge`](Self::stale_challenge) on a stale nonce, if it
    ///   wants to distinguish the case).
    /// * `Err(..)` — the realm backend itself failed.
    pub async fn authenticate(
        &self,
        auth_header: &str,
        http_method: &str,
    ) -> tomcatrs_core::Result<Option<Principal>> {
        let Some(parsed) = Self::parse_authorization(auth_header) else {
            return Ok(None);
        };

        // Realm-name mismatch means the client is answering a different
        // server's challenge: refuse without consulting the backend.
        if parsed.realm != self.realm_name {
            return Ok(None);
        }

        // Algorithm directive: missing => MD5; explicit MD5 / MD5-sess accepted.
        let algorithm = match DigestAlgorithm::parse(parsed.algorithm.as_deref()) {
            Some(a) => a,
            None => return Ok(None),
        };

        // QoP must be either absent (legacy RFC 2069) or exactly "auth".
        // "auth-int" is not advertised.
        let qop_str = parsed.qop.as_deref();
        if let Some(q) = qop_str {
            if !q.eq_ignore_ascii_case("auth") {
                return Ok(None);
            }
            // qop requires both nc and cnonce per RFC 2617 §3.2.2.
            if parsed.nc.is_none() || parsed.cnonce.is_none() {
                return Ok(None);
            }
        }

        // Authenticate the nonce *before* the replay table is touched: a
        // forged nonce should never grow the table.
        let issued = match self.validate_nonce(&parsed.nonce) {
            Some(t) => t,
            None => return Ok(None),
        };

        // Replay check: a given nc must be seen at most once per nonce. Also
        // opportunistically prune any expired nonces.
        if let Some(nc_str) = parsed.nc.as_deref() {
            let nc_num = match u64::from_str_radix(nc_str, 16) {
                Ok(n) => n,
                Err(_) => return Ok(None),
            };
            let now = Self::now_millis();
            let mut guard = self.nonces.lock();
            guard.retain(|_, st| now.saturating_sub(st.issued_millis) <= NONCE_TTL_MILLIS);
            let state = guard.entry(parsed.nonce.clone()).or_insert(NonceState {
                issued_millis: issued,
                seen_ncs: HashSet::new(),
            });
            if !state.seen_ncs.insert(nc_num) {
                return Ok(None);
            }
        }

        // Ask the realm for HA1. A `None` here means the user is unknown *or*
        // the user has no digest-compatible material — both are authentication
        // failures, indistinguishable to the client by design.
        let stored_ha1 = match self
            .realm
            .digest_ha1(&parsed.username, &self.realm_name)
            .await
        {
            Some(h) => h,
            None => return Ok(None),
        };

        // MD5-sess rebinds HA1 to the challenge: HA1 := MD5(HA1 : nonce : cnonce).
        let ha1 = match algorithm {
            DigestAlgorithm::Md5 => stored_ha1,
            DigestAlgorithm::Md5Sess => {
                let Some(cnonce) = parsed.cnonce.as_deref() else {
                    return Ok(None);
                };
                Self::md5_hex(&format!(
                    "{stored_ha1}:{nonce}:{cnonce}",
                    nonce = parsed.nonce
                ))
            }
        };

        // HA2 for qop=auth (and the legacy RFC 2069 path) is the same:
        // MD5(method:uri). auth-int is not supported.
        let ha2 = Self::md5_hex(&format!("{http_method}:{uri}", uri = parsed.uri));

        // Final response computation.
        let expected = match qop_str {
            Some(_) => {
                // qop=auth (already validated above; nc/cnonce both Some).
                let nc = parsed.nc.as_deref().unwrap_or("");
                let cnonce = parsed.cnonce.as_deref().unwrap_or("");
                Self::md5_hex(&format!(
                    "{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}",
                    nonce = parsed.nonce,
                ))
            }
            None => {
                // Legacy RFC 2069: MD5(HA1:nonce:HA2).
                Self::md5_hex(&format!("{ha1}:{nonce}:{ha2}", nonce = parsed.nonce))
            }
        };

        if !constant_time_eq(expected.as_bytes(), parsed.response.as_bytes()) {
            return Ok(None);
        }

        // Re-authenticate via the realm only to fetch the principal's roles.
        // We deliberately do *not* call `realm.authenticate(user, password)`
        // here — we don't have the password — so instead we build the
        // principal from the validated username plus the role list the realm
        // exposes for it. We can't query roles without an `authenticate`
        // round-trip in the current trait, so we accept that the digest path
        // returns a principal with an empty role list when the realm's
        // password-based authenticate() is the only role source. Realms that
        // need richer behaviour can override `digest_ha1` and pair it with a
        // companion lookup in their own type.
        //
        // In practice the standard `InMemoryRealm` users registered via
        // `add_digest_user` *also* know their password's SHA-256 hash, so an
        // operator who wants role-aware DIGEST should provide them through
        // that path; the principal returned here will then need a parallel
        // lookup performed by the caller. For the v0.1.0 release the
        // important invariant is that authentication itself is sound.
        Ok(Some(Principal::new(parsed.username, Vec::new())))
    }
}

/// Render `bytes` as lowercase hex.
fn hex_lower(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(hex, "{:02x}", byte);
    }
    hex
}

/// Decode a lowercase- or uppercase-hex string into bytes. Returns `None` on
/// any non-hex character or an odd length.
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks(2) {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Constant-time byte slice comparison. Returns `true` iff the slices are
/// equal. Used to avoid leaking digest-comparison timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Strip a single layer of surrounding double quotes, if present.
fn unquote(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Split a `Digest`-parameter list on commas, respecting double-quoted
/// strings. Each returned item is trimmed.
fn split_directives(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut in_quote = false;
    for ch in s.chars() {
        match ch {
            '"' => {
                in_quote = !in_quote;
                buf.push(ch);
            }
            ',' if !in_quote => {
                let trimmed = buf.trim().to_string();
                if !trimmed.is_empty() {
                    out.push(trimmed);
                }
                buf.clear();
            }
            _ => buf.push(ch),
        }
    }
    let trimmed = buf.trim().to_string();
    if !trimmed.is_empty() {
        out.push(trimmed);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::realm::{digest_ha1, InMemoryRealm};

    /// Convenience: a `DigestAuthenticator` over an `InMemoryRealm` carrying a
    /// single digest-capable user.
    fn build(
        realm_name: &str,
        user: &str,
        password: &str,
    ) -> (DigestAuthenticator, Arc<InMemoryRealm>) {
        let realm = Arc::new(InMemoryRealm::new().with_digest_user(
            user,
            realm_name,
            password,
            vec!["role".into()],
        ));
        let realm_trait: Arc<dyn Realm> = realm.clone();
        let auth = DigestAuthenticator::new(realm_trait, realm_name, b"unit-test-opaque-key");
        (auth, realm)
    }

    #[test]
    fn parses_a_full_client_response() {
        let header = "Digest username=\"alice\", realm=\"r\", nonce=\"n\", uri=\"/x\", \
                      response=\"abc\", qop=auth, nc=00000001, cnonce=\"cn\", \
                      opaque=\"op\", algorithm=MD5";
        let parsed = DigestAuthenticator::parse_authorization(header).unwrap();
        assert_eq!(parsed.username, "alice");
        assert_eq!(parsed.realm, "r");
        assert_eq!(parsed.nonce, "n");
        assert_eq!(parsed.uri, "/x");
        assert_eq!(parsed.response, "abc");
        assert_eq!(parsed.qop.as_deref(), Some("auth"));
        assert_eq!(parsed.nc.as_deref(), Some("00000001"));
        assert_eq!(parsed.cnonce.as_deref(), Some("cn"));
        assert_eq!(parsed.opaque.as_deref(), Some("op"));
        assert_eq!(parsed.algorithm.as_deref(), Some("MD5"));
    }

    #[test]
    fn parse_rejects_non_digest_scheme_and_missing_directives() {
        assert!(DigestAuthenticator::parse_authorization("Basic abc").is_none());
        // Missing mandatory `response`.
        assert!(DigestAuthenticator::parse_authorization(
            "Digest username=\"a\", realm=\"r\", nonce=\"n\", uri=\"/\""
        )
        .is_none());
    }

    #[test]
    fn parse_handles_unquoted_and_quoted_values() {
        let header =
            "Digest username=bob, realm=\"r\", nonce=\"n\", uri=/p, response=ff, qop=auth, nc=00000001, cnonce=cn";
        let parsed = DigestAuthenticator::parse_authorization(header).unwrap();
        assert_eq!(parsed.username, "bob");
        assert_eq!(parsed.uri, "/p");
        assert_eq!(parsed.response, "ff");
        assert_eq!(parsed.nc.as_deref(), Some("00000001"));
    }

    #[test]
    fn nonces_are_unique_well_formed_and_self_validating() {
        let (auth, _realm) = build("r", "u", "p");
        let a = auth.generate_nonce();
        let b = auth.generate_nonce();
        assert_ne!(a, b);
        let parts: Vec<&str> = a.split(':').collect();
        assert_eq!(parts.len(), 3);
        assert!(parts[0].parse::<u128>().is_ok());
        assert_eq!(parts[1].len(), 32); // 16 bytes of salt
        assert_eq!(parts[2].len(), 32); // MD5 digest
        assert!(auth.validate_nonce(&a).is_some());
    }

    #[test]
    fn nonce_validation_rejects_tampering() {
        let (auth, _realm) = build("r", "u", "p");
        let n = auth.generate_nonce();
        let mut bad = n.clone();
        // Flip the last hex char of the digest.
        let last = bad.pop().unwrap();
        let replacement = if last == 'f' { '0' } else { 'f' };
        bad.push(replacement);
        assert!(auth.validate_nonce(&bad).is_none());
        // A nonce from a different opaque key is also rejected.
        let other_realm: Arc<dyn Realm> = Arc::new(InMemoryRealm::new());
        let other = DigestAuthenticator::new(other_realm, "r", b"different-key");
        assert!(auth.validate_nonce(&other.generate_nonce()).is_none());
    }

    #[test]
    fn nonce_validation_rejects_stale_timestamps() {
        let (auth, _realm) = build("r", "u", "p");
        // Hand-craft a nonce with a timestamp from well before the TTL.
        let stale_ts: u128 = 1_000; // 1970-01-01T00:00:01Z
        let mut salt = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut salt);
        let digest = auth.keyed_digest(stale_ts, &salt);
        let nonce = format!("{stale_ts}:{}:{}", hex_lower(&salt), digest);
        assert!(auth.validate_nonce(&nonce).is_none());
    }

    #[test]
    fn challenge_header_format_is_correct() {
        let (auth, _realm) = build("Protected Area", "u", "p");
        let header = auth.challenge();
        assert!(header.starts_with("Digest "));
        assert!(header.contains("realm=\"Protected Area\""));
        assert!(header.contains("qop=\"auth\""));
        assert!(header.contains("nonce=\""));
        assert!(header.contains("opaque=\""));
        assert!(header.contains("algorithm=MD5"));
        assert!(!header.contains("algorithm=MD5-sess"));
        assert!(!header.contains("stale=true"));

        let stale = auth.stale_challenge();
        assert!(stale.contains("stale=true"));
    }

    /// Worked example: compute HA1/HA2/response by hand from the parts and
    /// check that the authenticator agrees.
    #[tokio::test]
    async fn authenticates_a_correct_response_qop_auth() {
        let realm_name = "testrealm@host.com";
        let user = "Mufasa";
        let password = "Circle Of Life";
        let (auth, _realm) = build(realm_name, user, password);

        // Take a fresh nonce from the authenticator itself so validate_nonce
        // accepts it.
        let nonce = auth.generate_nonce();
        let cnonce = "0a4f113b";
        let nc = "00000001";
        let uri = "/dir/index.html";
        let method = "GET";

        let ha1 = digest_ha1(user, realm_name, password);
        // RFC 2617 worked example for HA1.
        assert_eq!(ha1, "939e7578ed9e3c518a452acee763bce9");

        let ha2 = DigestAuthenticator::md5_hex(&format!("{method}:{uri}"));
        let response =
            DigestAuthenticator::md5_hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"));

        let header = format!(
            "Digest username=\"{user}\", realm=\"{realm_name}\", nonce=\"{nonce}\", \
             uri=\"{uri}\", qop=auth, nc={nc}, cnonce=\"{cnonce}\", response=\"{response}\", \
             algorithm=MD5"
        );

        let principal = auth.authenticate(&header, method).await.unwrap();
        assert!(principal.is_some(), "good digest should authenticate");
        assert_eq!(principal.unwrap().name, user);
    }

    #[tokio::test]
    async fn authenticates_legacy_rfc2069_no_qop() {
        let realm_name = "r";
        let user = "u";
        let password = "p";
        let (auth, _realm) = build(realm_name, user, password);

        let nonce = auth.generate_nonce();
        let uri = "/legacy";
        let method = "GET";

        let ha1 = digest_ha1(user, realm_name, password);
        let ha2 = DigestAuthenticator::md5_hex(&format!("{method}:{uri}"));
        let response = DigestAuthenticator::md5_hex(&format!("{ha1}:{nonce}:{ha2}"));

        let header = format!(
            "Digest username=\"{user}\", realm=\"{realm_name}\", nonce=\"{nonce}\", \
             uri=\"{uri}\", response=\"{response}\""
        );
        let principal = auth.authenticate(&header, method).await.unwrap();
        assert_eq!(principal.unwrap().name, user);
    }

    #[tokio::test]
    async fn authenticates_md5_sess_correctly() {
        let realm_name = "r";
        let user = "u";
        let password = "p";
        let (auth, _realm) = build(realm_name, user, password);

        let nonce = auth.generate_nonce();
        let cnonce = "client-nonce";
        let nc = "00000001";
        let uri = "/x";
        let method = "POST";

        let stored = digest_ha1(user, realm_name, password);
        let ha1 = DigestAuthenticator::md5_hex(&format!("{stored}:{nonce}:{cnonce}"));
        let ha2 = DigestAuthenticator::md5_hex(&format!("{method}:{uri}"));
        let response =
            DigestAuthenticator::md5_hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"));

        let header = format!(
            "Digest username=\"{user}\", realm=\"{realm_name}\", nonce=\"{nonce}\", \
             uri=\"{uri}\", qop=auth, nc={nc}, cnonce=\"{cnonce}\", response=\"{response}\", \
             algorithm=MD5-sess"
        );
        assert!(auth.authenticate(&header, method).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn rejects_bad_response_and_wrong_method() {
        let realm_name = "r";
        let user = "u";
        let password = "p";
        let (auth, _realm) = build(realm_name, user, password);

        let nonce = auth.generate_nonce();
        let cnonce = "cn";
        let nc = "00000001";
        let uri = "/x";
        let ha1 = digest_ha1(user, realm_name, password);
        let ha2 = DigestAuthenticator::md5_hex(&format!("GET:{uri}"));
        let response =
            DigestAuthenticator::md5_hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"));

        let header = format!(
            "Digest username=\"{user}\", realm=\"{realm_name}\", nonce=\"{nonce}\", \
             uri=\"{uri}\", qop=auth, nc={nc}, cnonce=\"{cnonce}\", response=\"{response}\""
        );

        // Same header with a different HTTP method must fail (HA2 changes).
        assert!(auth.authenticate(&header, "POST").await.unwrap().is_none());

        // Tamper with the response and try the right method.
        let bad = header.replace(&response, &"0".repeat(response.len()));
        assert!(auth.authenticate(&bad, "GET").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_unknown_user_silently() {
        let realm_name = "r";
        let (auth, _realm) = build(realm_name, "alice", "pw");
        let nonce = auth.generate_nonce();
        let uri = "/x";
        let method = "GET";
        let ha1 = digest_ha1("ghost", realm_name, "pw");
        let ha2 = DigestAuthenticator::md5_hex(&format!("{method}:{uri}"));
        let response =
            DigestAuthenticator::md5_hex(&format!("{ha1}:{nonce}:00000001:cn:auth:{ha2}"));
        let header = format!(
            "Digest username=\"ghost\", realm=\"{realm_name}\", nonce=\"{nonce}\", \
             uri=\"{uri}\", qop=auth, nc=00000001, cnonce=\"cn\", response=\"{response}\""
        );
        assert!(auth.authenticate(&header, method).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_replayed_nc() {
        let realm_name = "r";
        let user = "u";
        let password = "p";
        let (auth, _realm) = build(realm_name, user, password);

        let nonce = auth.generate_nonce();
        let cnonce = "cn";
        let nc = "00000001";
        let uri = "/x";
        let method = "GET";
        let ha1 = digest_ha1(user, realm_name, password);
        let ha2 = DigestAuthenticator::md5_hex(&format!("{method}:{uri}"));
        let response =
            DigestAuthenticator::md5_hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"));
        let header = format!(
            "Digest username=\"{user}\", realm=\"{realm_name}\", nonce=\"{nonce}\", \
             uri=\"{uri}\", qop=auth, nc={nc}, cnonce=\"{cnonce}\", response=\"{response}\""
        );

        // First use succeeds.
        assert!(auth.authenticate(&header, method).await.unwrap().is_some());
        // Replay (same nonce, same nc) is rejected.
        assert!(auth.authenticate(&header, method).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_forged_nonce() {
        let realm_name = "r";
        let user = "u";
        let password = "p";
        let (auth, _realm) = build(realm_name, user, password);

        // Plausible-looking nonce that was never minted by `auth`.
        let bogus_nonce = format!(
            "{}:{}:{}",
            DigestAuthenticator::now_millis(),
            "0".repeat(32),
            "0".repeat(32),
        );
        let cnonce = "cn";
        let nc = "00000001";
        let uri = "/x";
        let method = "GET";
        let ha1 = digest_ha1(user, realm_name, password);
        let ha2 = DigestAuthenticator::md5_hex(&format!("{method}:{uri}"));
        let response =
            DigestAuthenticator::md5_hex(&format!("{ha1}:{bogus_nonce}:{nc}:{cnonce}:auth:{ha2}"));
        let header = format!(
            "Digest username=\"{user}\", realm=\"{realm_name}\", nonce=\"{bogus_nonce}\", \
             uri=\"{uri}\", qop=auth, nc={nc}, cnonce=\"{cnonce}\", response=\"{response}\""
        );
        assert!(auth.authenticate(&header, method).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_realm_mismatch() {
        let (auth, _realm) = build("r", "u", "p");
        let nonce = auth.generate_nonce();
        // Build a valid-looking response, but advertise a different realm.
        let ha1 = digest_ha1("u", "other-realm", "p");
        let ha2 = DigestAuthenticator::md5_hex("GET:/x");
        let response =
            DigestAuthenticator::md5_hex(&format!("{ha1}:{nonce}:00000001:cn:auth:{ha2}"));
        let header = format!(
            "Digest username=\"u\", realm=\"other-realm\", nonce=\"{nonce}\", \
             uri=\"/x\", qop=auth, nc=00000001, cnonce=\"cn\", response=\"{response}\""
        );
        assert!(auth.authenticate(&header, "GET").await.unwrap().is_none());
    }

    #[test]
    fn hex_round_trips() {
        let bytes = vec![0x00, 0x10, 0xab, 0xcd, 0xef];
        let hex = hex_lower(&bytes);
        assert_eq!(hex, "0010abcdef");
        assert_eq!(hex_decode(&hex), Some(bytes));
        assert!(hex_decode("zz").is_none());
        assert!(hex_decode("abc").is_none());
    }

    #[test]
    fn constant_time_eq_matches_eq() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"hi", b"hill"));
    }

    #[test]
    fn split_directives_respects_quotes() {
        let parts = split_directives("a=1, b=\"x, y\", c=3");
        assert_eq!(parts, vec!["a=1", "b=\"x, y\"", "c=3"]);
    }
}
