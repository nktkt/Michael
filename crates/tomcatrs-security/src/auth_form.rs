//! `FORM` authentication — the Servlet-spec `j_security_check` flow (scaffold).
//!
//! The wire types and the request-shape parsing are **real**. What is deferred
//! is the session integration: a complete `FORM` authenticator must save the
//! originally-requested URI, redirect unauthenticated users to the login page,
//! intercept the `j_security_check` POST, and on success replay the saved
//! request. That state machine belongs with the session manager
//! (`tomcatrs-session`) and is therefore out of scope for v0.1.0.
//!
//! # The `j_security_check` contract
//!
//! A login form must `POST` to the well-known action [`J_SECURITY_CHECK`] with
//! two `application/x-www-form-urlencoded` parameters:
//!
//! * [`J_USERNAME`] — the user-supplied identity.
//! * [`J_PASSWORD`] — the user-supplied secret.
//!
//! [`FormCredentials::from_form_body`] extracts exactly those two parameters
//! from a urlencoded body.

use crate::realm::{Principal, Realm};

/// The well-known action path a login form must `POST` to.
pub const J_SECURITY_CHECK: &str = "/j_security_check";

/// The well-known username parameter name.
pub const J_USERNAME: &str = "j_username";

/// The well-known password parameter name.
pub const J_PASSWORD: &str = "j_password";

/// Credentials extracted from a `j_security_check` submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormCredentials {
    /// The submitted `j_username` value.
    pub username: String,
    /// The submitted `j_password` value.
    pub password: String,
}

/// Decode one `application/x-www-form-urlencoded` token (`+` → space,
/// `%XX` → byte).
fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl FormCredentials {
    /// Extract [`J_USERNAME`]/[`J_PASSWORD`] from a urlencoded request body.
    ///
    /// Returns `None` unless *both* parameters are present. Other parameters in
    /// the body are ignored.
    pub fn from_form_body(body: &str) -> Option<FormCredentials> {
        let mut username = None;
        let mut password = None;
        for pair in body.split('&') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            match url_decode(key).as_str() {
                J_USERNAME => username = Some(url_decode(value)),
                J_PASSWORD => password = Some(url_decode(value)),
                _ => {}
            }
        }
        Some(FormCredentials {
            username: username?,
            password: password?,
        })
    }
}

/// A `FORM` authenticator bound to a login and an error page.
///
/// The page paths are retained for the (future) redirect state machine; in
/// v0.1.0 only [`is_security_check`] and credential verification via
/// [`authenticate`] are wired up.
///
/// [`is_security_check`]: FormAuthenticator::is_security_check
/// [`authenticate`]: FormAuthenticator::authenticate
#[derive(Debug, Clone)]
pub struct FormAuthenticator {
    login_page: String,
    error_page: String,
}

impl FormAuthenticator {
    /// Create an authenticator pointing at the application's login and error
    /// pages (as declared in `web.xml`'s `<form-login-config>`).
    pub fn new(login_page: impl Into<String>, error_page: impl Into<String>) -> Self {
        FormAuthenticator {
            login_page: login_page.into(),
            error_page: error_page.into(),
        }
    }

    /// The configured login-form page path.
    pub fn login_page(&self) -> &str {
        &self.login_page
    }

    /// The configured authentication-error page path.
    pub fn error_page(&self) -> &str {
        &self.error_page
    }

    /// Returns `true` if `path` is the `j_security_check` action endpoint.
    pub fn is_security_check(path: &str) -> bool {
        path == J_SECURITY_CHECK
    }

    /// Verify a `j_security_check` submission against the realm.
    ///
    /// This part *is* wired up: given the parsed [`FormCredentials`], it
    /// delegates straight to the realm. The surrounding redirect/replay
    /// choreography is what remains unimplemented.
    pub async fn authenticate(
        &self,
        realm: &dyn Realm,
        credentials: &FormCredentials,
    ) -> tomcatrs_core::Result<Option<Principal>> {
        realm
            .authenticate(&credentials.username, &credentials.password)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::realm::InMemoryRealm;

    #[test]
    fn extracts_credentials_from_form_body() {
        let creds =
            FormCredentials::from_form_body("j_username=alice&j_password=p%40ss+word").unwrap();
        assert_eq!(creds.username, "alice");
        assert_eq!(creds.password, "p@ss word");
    }

    #[test]
    fn missing_field_yields_none() {
        assert!(FormCredentials::from_form_body("j_username=alice").is_none());
    }

    #[test]
    fn recognises_the_security_check_path() {
        assert!(FormAuthenticator::is_security_check("/j_security_check"));
        assert!(!FormAuthenticator::is_security_check("/login"));
    }

    #[tokio::test]
    async fn authenticate_delegates_to_realm() {
        let realm = InMemoryRealm::new().with_user("alice", "secret", vec!["user".into()]);
        let auth = FormAuthenticator::new("/login.html", "/error.html");
        let creds = FormCredentials {
            username: "alice".into(),
            password: "secret".into(),
        };
        let principal = auth.authenticate(&realm, &creds).await.unwrap().unwrap();
        assert_eq!(principal.name, "alice");
    }
}
