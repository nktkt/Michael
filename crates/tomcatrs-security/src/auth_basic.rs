//! HTTP `BASIC` authentication — RFC 7617.
//!
//! This module is **fully working**. It parses an `Authorization: Basic
//! <base64>` header, decodes the `user:password` pair, delegates verification
//! to a [`Realm`], and produces the `WWW-Authenticate` challenge value sent on
//! a `401` response.
//!
//! `BASIC` transmits credentials in a trivially reversible encoding, so it is
//! only safe over TLS. That is a deployment concern, not something this module
//! can enforce, but it is worth restating.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

use crate::realm::{Principal, Realm};

/// A `BASIC` authenticator bound to a realm name.
///
/// The realm *name* here is the human-facing label that appears in the
/// browser's credential prompt (the `realm="..."` parameter of the challenge);
/// it is independent of the [`Realm`] trait object that actually verifies
/// credentials.
#[derive(Debug, Clone)]
pub struct BasicAuthenticator {
    realm_name: String,
}

impl BasicAuthenticator {
    /// Create an authenticator that advertises `realm_name` in its challenge.
    pub fn new(realm_name: impl Into<String>) -> Self {
        BasicAuthenticator {
            realm_name: realm_name.into(),
        }
    }

    /// The configured challenge realm name.
    pub fn realm_name(&self) -> &str {
        &self.realm_name
    }

    /// The value to send in the `WWW-Authenticate` response header on a `401`.
    ///
    /// The returned string is the *value* only, e.g. `Basic realm="Tomcat
    /// Manager"` — the caller is responsible for attaching the header name.
    /// A double-quote in the realm name would break the header grammar, so it
    /// is stripped defensively.
    pub fn challenge(&self) -> String {
        let safe = self.realm_name.replace('"', "");
        format!("Basic realm=\"{safe}\"")
    }

    /// Parse the credentials out of an `Authorization` header value.
    ///
    /// Accepts the full header value (e.g. `Basic dXNlcjpwYXNz`). The `Basic`
    /// scheme token is matched case-insensitively per RFC 7235. Returns the
    /// decoded `(username, password)` pair, or `None` if the value is not a
    /// well-formed `Basic` credential.
    pub fn parse_authorization(header_value: &str) -> Option<(String, String)> {
        let trimmed = header_value.trim();
        let rest = trimmed
            .split_once(char::is_whitespace)
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("basic"))
            .map(|(_, rest)| rest.trim())?;

        let decoded = BASE64.decode(rest).ok()?;
        let text = String::from_utf8(decoded).ok()?;
        // RFC 7617: the user-id must not contain a colon; the password may.
        let (user, pass) = text.split_once(':')?;
        Some((user.to_string(), pass.to_string()))
    }

    /// Authenticate a request given its `Authorization` header value.
    ///
    /// * `Ok(Some(principal))` — credentials parsed and verified.
    /// * `Ok(None)` — the header was missing, malformed, or the credentials
    ///   did not match. The caller should respond `401` with [`challenge`].
    /// * `Err(..)` — the realm backend itself failed.
    ///
    /// [`challenge`]: Self::challenge
    pub async fn authenticate(
        &self,
        realm: &dyn Realm,
        authorization_header: Option<&str>,
    ) -> tomcatrs_core::Result<Option<Principal>> {
        let Some(header) = authorization_header else {
            return Ok(None);
        };
        let Some((user, pass)) = Self::parse_authorization(header) else {
            return Ok(None);
        };
        realm.authenticate(&user, &pass).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::realm::InMemoryRealm;

    #[test]
    fn parses_well_formed_basic_header() {
        // base64("user:pass") == "dXNlcjpwYXNz"
        let parsed = BasicAuthenticator::parse_authorization("Basic dXNlcjpwYXNz");
        assert_eq!(parsed, Some(("user".to_string(), "pass".to_string())));
    }

    #[test]
    fn scheme_token_is_case_insensitive() {
        let parsed = BasicAuthenticator::parse_authorization("bAsIc dXNlcjpwYXNz");
        assert_eq!(parsed, Some(("user".to_string(), "pass".to_string())));
    }

    #[test]
    fn password_may_contain_colons() {
        // base64("user:pa:ss")
        let encoded = BASE64.encode("user:pa:ss");
        let parsed = BasicAuthenticator::parse_authorization(&format!("Basic {encoded}")).unwrap();
        assert_eq!(parsed, ("user".to_string(), "pa:ss".to_string()));
    }

    #[test]
    fn rejects_malformed_headers() {
        assert!(BasicAuthenticator::parse_authorization("Bearer abc").is_none());
        assert!(BasicAuthenticator::parse_authorization("Basic !!!not-base64!!!").is_none());
        // base64("nocolon") — no ':' separator
        let encoded = BASE64.encode("nocolon");
        assert!(BasicAuthenticator::parse_authorization(&format!("Basic {encoded}")).is_none());
    }

    #[test]
    fn challenge_quotes_realm_name() {
        let auth = BasicAuthenticator::new("Tomcat Manager");
        assert_eq!(auth.challenge(), "Basic realm=\"Tomcat Manager\"");
    }

    #[tokio::test]
    async fn authenticate_against_realm_round_trips() {
        let realm = InMemoryRealm::new().with_user("user", "pass", vec!["role".into()]);
        let auth = BasicAuthenticator::new("Test");

        let ok = auth
            .authenticate(&realm, Some("Basic dXNlcjpwYXNz"))
            .await
            .unwrap();
        assert_eq!(ok.unwrap().name, "user");

        let missing = auth.authenticate(&realm, None).await.unwrap();
        assert!(missing.is_none());
    }
}
