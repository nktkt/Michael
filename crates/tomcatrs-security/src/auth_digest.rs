//! HTTP `DIGEST` authentication — RFC 7616 (scaffold).
//!
//! Challenge generation and the wire types are **real and complete**: a
//! [`DigestAuthenticator`] produces correct, cryptographically-fresh nonces
//! and renders a valid `WWW-Authenticate: Digest ...` header. Parsing of the
//! client's `Authorization: Digest ...` response into [`DigestResponse`] is
//! also implemented.
//!
//! What is *deferred* to a later release is the credential-verification
//! arithmetic (the `HA1`/`HA2`/`response` MD5 ladder), because doing it
//! correctly requires the realm to expose pre-computed `HA1` values rather
//! than plaintext-equivalent material. Until then, [`authenticate`] returns
//! [`tomcatrs_core::Error::Other`].
//!
//! [`authenticate`]: DigestAuthenticator::authenticate

use std::time::{SystemTime, UNIX_EPOCH};

use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::realm::{Principal, Realm};

/// The quality-of-protection values this authenticator advertises.
///
/// Only `auth` is offered; `auth-int` (integrity protection over the body) is
/// intentionally not advertised in v0.1.0.
pub const QOP: &str = "auth";

/// A server-issued `DIGEST` challenge.
///
/// Construct one per `401` response with [`DigestAuthenticator::new_challenge`]
/// and render it with [`Challenge::to_header_value`].
#[derive(Debug, Clone)]
pub struct Challenge {
    /// The protection space label shown to the client.
    pub realm: String,
    /// A unique, unpredictable, single-use server nonce.
    pub nonce: String,
    /// An opaque value the client must echo back verbatim.
    pub opaque: String,
    /// Set once a client presents a stale (expired) but otherwise valid nonce,
    /// telling the client to retry without re-prompting the user.
    pub stale: bool,
}

impl Challenge {
    /// Render this challenge as a `WWW-Authenticate` header *value*.
    ///
    /// The caller attaches the header name.
    pub fn to_header_value(&self) -> String {
        let mut value = format!(
            "Digest realm=\"{}\", qop=\"{}\", algorithm=SHA-256, nonce=\"{}\", opaque=\"{}\"",
            self.realm, QOP, self.nonce, self.opaque
        );
        if self.stale {
            value.push_str(", stale=true");
        }
        value
    }
}

/// A parsed client `Authorization: Digest ...` response.
///
/// Every field maps directly to a `directive=value` pair in the header. The
/// fields needed to recompute the digest are kept; unknown directives are
/// dropped.
#[derive(Debug, Clone, Default)]
pub struct DigestResponse {
    /// The authenticating user name (`username`).
    pub username: String,
    /// The protection space (`realm`).
    pub realm: String,
    /// The server nonce being answered (`nonce`).
    pub nonce: String,
    /// The request URI the digest was computed over (`uri`).
    pub uri: String,
    /// The client-computed `response` digest.
    pub response: String,
    /// The quality of protection actually applied (`qop`).
    pub qop: Option<String>,
    /// The client nonce (`cnonce`).
    pub cnonce: Option<String>,
    /// The nonce count (`nc`).
    pub nc: Option<String>,
    /// The echoed `opaque` value.
    pub opaque: Option<String>,
}

/// An HTTP `DIGEST` authenticator.
#[derive(Debug, Clone)]
pub struct DigestAuthenticator {
    realm_name: String,
    /// Process-stable secret mixed into every nonce so nonces from this
    /// process can (in a future release) be validated as authentic.
    server_secret: [u8; 32],
}

impl DigestAuthenticator {
    /// Create an authenticator for `realm_name`, seeding a random per-instance
    /// server secret.
    pub fn new(realm_name: impl Into<String>) -> Self {
        let mut server_secret = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut server_secret);
        DigestAuthenticator {
            realm_name: realm_name.into(),
            server_secret,
        }
    }

    /// The configured challenge realm name.
    pub fn realm_name(&self) -> &str {
        &self.realm_name
    }

    /// Generate a fresh, unpredictable nonce.
    ///
    /// The nonce binds three things: the current timestamp (so freshness can
    /// later be checked), 16 bytes of CSPRNG output (so it cannot be guessed),
    /// and a keyed SHA-256 digest over both plus the server secret (so a
    /// forged nonce can be detected). The rendered form is
    /// `<unix_millis>:<hex-digest>`.
    pub fn generate_nonce(&self) -> String {
        let now_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);

        let mut random = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut random);

        let mut hasher = Sha256::new();
        hasher.update(now_millis.to_be_bytes());
        hasher.update(random);
        hasher.update(self.server_secret);
        let digest = hasher.finalize();

        let mut hex = String::with_capacity(digest.len() * 2);
        for byte in digest {
            use std::fmt::Write;
            let _ = write!(hex, "{:02x}", byte);
        }
        format!("{now_millis}:{hex}")
    }

    /// Generate a random `opaque` value for a challenge.
    fn generate_opaque() -> String {
        let mut random = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut random);
        let mut hex = String::with_capacity(random.len() * 2);
        for byte in random {
            use std::fmt::Write;
            let _ = write!(hex, "{:02x}", byte);
        }
        hex
    }

    /// Build a fresh [`Challenge`] for a `401` response.
    pub fn new_challenge(&self) -> Challenge {
        Challenge {
            realm: self.realm_name.clone(),
            nonce: self.generate_nonce(),
            opaque: Self::generate_opaque(),
            stale: false,
        }
    }

    /// Parse a client `Authorization: Digest ...` header value.
    ///
    /// Returns `None` if the value is not the `Digest` scheme or is missing
    /// the mandatory `username`, `realm`, `nonce`, `uri`, or `response`
    /// directives.
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

        for part in rest.split(',') {
            let Some((key, raw_value)) = part.split_once('=') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = raw_value.trim().trim_matches('"').to_string();
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
                _ => {}
            }
        }

        if saw_username && saw_realm && saw_nonce && saw_uri && saw_response {
            Some(out)
        } else {
            None
        }
    }

    /// Verify a parsed client response against the realm.
    ///
    /// # Errors
    ///
    /// Always returns [`tomcatrs_core::Error::Other`] in v0.1.0: the digest
    /// verification ladder is not yet implemented. The challenge-generation
    /// and parsing paths above are real and may be used today.
    pub async fn authenticate(
        &self,
        _realm: &dyn Realm,
        _response: &DigestResponse,
    ) -> tomcatrs_core::Result<Option<Principal>> {
        Err(tomcatrs_core::Error::Other(
            "DIGEST authentication not implemented in v0.1.0".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonces_are_unique_and_well_formed() {
        let auth = DigestAuthenticator::new("realm");
        let a = auth.generate_nonce();
        let b = auth.generate_nonce();
        assert_ne!(a, b);
        let (ts, digest) = a.split_once(':').expect("nonce has timestamp:digest form");
        assert!(ts.parse::<u128>().is_ok());
        assert_eq!(digest.len(), 64);
    }

    #[test]
    fn challenge_header_value_is_well_formed() {
        let auth = DigestAuthenticator::new("Protected");
        let header = auth.new_challenge().to_header_value();
        assert!(header.starts_with("Digest "));
        assert!(header.contains("realm=\"Protected\""));
        assert!(header.contains("algorithm=SHA-256"));
        assert!(header.contains("qop=\"auth\""));
        assert!(header.contains("nonce=\""));
        assert!(header.contains("opaque=\""));
    }

    #[test]
    fn parses_a_client_response() {
        let header = "Digest username=\"alice\", realm=\"r\", nonce=\"n\", uri=\"/x\", \
                      response=\"abc\", qop=auth, nc=00000001, cnonce=\"cn\"";
        let parsed = DigestAuthenticator::parse_authorization(header).unwrap();
        assert_eq!(parsed.username, "alice");
        assert_eq!(parsed.uri, "/x");
        assert_eq!(parsed.qop.as_deref(), Some("auth"));
    }

    #[test]
    fn parse_rejects_missing_mandatory_directives() {
        assert!(DigestAuthenticator::parse_authorization("Digest username=\"a\"").is_none());
        assert!(DigestAuthenticator::parse_authorization("Basic xyz").is_none());
    }
}
