//! `FORM` authentication — the Servlet-spec `j_security_check` flow.
//!
//! This module implements the Servlet specification's FORM-based login
//! workflow. A complete `FORM` authenticator must:
//!
//! 1. Intercept requests to a protected resource from an unauthenticated user.
//! 2. Save the original request (URL, method, query string) so it can be
//!    replayed after a successful login.
//! 3. Forward the user to the configured login page.
//! 4. Wait for the user-agent to `POST` `j_username` / `j_password` to the
//!    well-known [`J_SECURITY_CHECK`] action path.
//! 5. Verify the submitted credentials against the [`Realm`] and, on success,
//!    redirect the user-agent back to the originally-requested URL; on failure
//!    forward to the error page.
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

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

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

/// The minimum information needed to replay an unauthenticated request after
/// the user has successfully logged in.
///
/// The Servlet spec requires that after FORM login the container restore the
/// user's *original* request, not merely redirect them to a default landing
/// page. That requires remembering at least the HTTP method, the path, and any
/// query string of the request that triggered the login challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedRequest {
    /// The HTTP method of the originally-requested resource (e.g. `GET`).
    pub method: String,
    /// The path portion of the originally-requested URI.
    pub path: String,
    /// The query string (without the leading `?`), if any.
    pub query: Option<String>,
}

/// An in-memory store of [`SavedRequest`]s keyed by an opaque token.
///
/// In a complete container this token would be the session id; the store is
/// agnostic to its provenance. The store uses [`parking_lot::Mutex`] so it can
/// be cheaply shared (`Arc`) between request-handling tasks without an `async`
/// lock.
#[derive(Debug, Default)]
pub struct SavedRequestStore {
    entries: Mutex<HashMap<String, SavedRequest>>,
}

impl SavedRequestStore {
    /// Construct an empty store.
    pub fn new() -> Self {
        SavedRequestStore {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Save `request` under `token`. An existing entry for the same token is
    /// replaced.
    pub fn save(&self, token: impl Into<String>, request: SavedRequest) {
        self.entries.lock().insert(token.into(), request);
    }

    /// Remove and return the saved request for `token`, if any.
    ///
    /// Saved requests are consumed on replay: a token is one-shot.
    pub fn take(&self, token: &str) -> Option<SavedRequest> {
        self.entries.lock().remove(token)
    }

    /// Returns the number of currently-saved requests (primarily for tests).
    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// Returns `true` if no requests are currently saved.
    pub fn is_empty(&self) -> bool {
        self.entries.lock().is_empty()
    }
}

/// The outcome of running the FORM authenticator state machine against one
/// request.
///
/// The connector layer translates these variants into HTTP responses:
///
/// * [`ShowLoginPage`](FormAuthOutcome::ShowLoginPage) — forward to the
///   configured login page.
/// * [`Authenticated`](FormAuthOutcome::Authenticated) — the user has just
///   completed `j_security_check`; respond with a redirect to `redirect_to`
///   (the previously-saved URL, or `/` if no request was saved).
/// * [`ShowErrorPage`](FormAuthOutcome::ShowErrorPage) — `j_security_check`
///   credentials were rejected; forward to the configured error page.
/// * [`Continue`](FormAuthOutcome::Continue) — the user is already
///   authenticated; let the request through to the servlet pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormAuthOutcome {
    /// Forward the user-agent to the login page; the originating request has
    /// been saved under the supplied token.
    ShowLoginPage,
    /// Credentials accepted. The caller should redirect the user-agent to
    /// `redirect_to`.
    Authenticated {
        /// The newly-authenticated principal.
        principal: Principal,
        /// The URL to send the user-agent to next: the previously-saved
        /// request, reconstructed as a path-plus-query string, or `/` when no
        /// request had been saved.
        redirect_to: String,
    },
    /// Credentials rejected. Forward the user-agent to the error page.
    ShowErrorPage,
    /// The request is not part of a login flow; pass it through to the next
    /// layer.
    Continue,
}

/// A `FORM` authenticator bound to a login page, an error page, and a saved-
/// request store.
///
/// The authenticator itself is stateless beyond its configuration; per-user
/// state lives in the [`SavedRequestStore`].
pub struct FormAuthenticator {
    realm: Arc<dyn Realm>,
    login_page: String,
    error_page: String,
    store: Arc<SavedRequestStore>,
}

impl std::fmt::Debug for FormAuthenticator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FormAuthenticator")
            .field("login_page", &self.login_page)
            .field("error_page", &self.error_page)
            .field("saved_requests", &self.store.len())
            .finish()
    }
}

impl FormAuthenticator {
    /// Create an authenticator pointing at the application's login and error
    /// pages (as declared in `web.xml`'s `<form-login-config>`).
    pub fn new(
        realm: Arc<dyn Realm>,
        login_page: impl Into<String>,
        error_page: impl Into<String>,
        store: Arc<SavedRequestStore>,
    ) -> Self {
        FormAuthenticator {
            realm,
            login_page: login_page.into(),
            error_page: error_page.into(),
            store,
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

    /// Borrow the shared saved-request store.
    pub fn store(&self) -> &Arc<SavedRequestStore> {
        &self.store
    }

    /// Returns `true` if `path` is the `j_security_check` action endpoint.
    pub fn is_security_check(path: &str) -> bool {
        path == J_SECURITY_CHECK
    }

    /// Verify a `j_security_check` submission against the realm.
    ///
    /// Exposed separately so callers that already have a [`FormCredentials`]
    /// in hand can authenticate without driving the full state machine.
    pub async fn authenticate(
        &self,
        credentials: &FormCredentials,
    ) -> tomcatrs_core::Result<Option<Principal>> {
        self.realm
            .authenticate(&credentials.username, &credentials.password)
            .await
    }

    /// Drive one step of the FORM authentication state machine for a single
    /// inbound request.
    ///
    /// * `path` — the request path.
    /// * `method` — the request method (`GET`, `POST`, …).
    /// * `body` — the request body, when the connector has buffered one
    ///   (required for `j_security_check`).
    /// * `saved_token` — an opaque per-user token (typically the session id)
    ///   under which the original request should be saved, and from which it
    ///   should be replayed once authentication succeeds.
    ///
    /// The caller is responsible for tracking whether the user is already
    /// authenticated and bypassing this method entirely in that case; this
    /// authenticator never returns [`FormAuthOutcome::Continue`] unless the
    /// caller signals an existing authenticated session by *not* providing a
    /// `saved_token` for an unprotected request. Concretely:
    ///
    /// * `POST` to `/j_security_check` → parse the body and authenticate. On
    ///   success replay the saved request (if any) and return
    ///   [`FormAuthOutcome::Authenticated`]; on failure return
    ///   [`FormAuthOutcome::ShowErrorPage`].
    /// * Any other request, given a `saved_token` → save the request and
    ///   return [`FormAuthOutcome::ShowLoginPage`].
    /// * Any other request without a `saved_token` → return
    ///   [`FormAuthOutcome::Continue`] (the authenticator has nothing to do).
    pub async fn process(
        &self,
        path: &str,
        method: &str,
        body: Option<&str>,
        saved_token: Option<&str>,
    ) -> tomcatrs_core::Result<FormAuthOutcome> {
        if Self::is_security_check(path) && method.eq_ignore_ascii_case("POST") {
            // Parse credentials out of the urlencoded body. A missing body or
            // missing field is an authentication failure, not a server error.
            let Some(body) = body else {
                return Ok(FormAuthOutcome::ShowErrorPage);
            };
            let Some(creds) = FormCredentials::from_form_body(body) else {
                return Ok(FormAuthOutcome::ShowErrorPage);
            };
            match self.authenticate(&creds).await? {
                Some(principal) => {
                    // On success replay the saved request, if any.
                    let redirect_to = saved_token
                        .and_then(|t| self.store.take(t))
                        .map(|req| match req.query {
                            Some(q) if !q.is_empty() => format!("{}?{}", req.path, q),
                            _ => req.path,
                        })
                        .unwrap_or_else(|| "/".to_string());
                    Ok(FormAuthOutcome::Authenticated {
                        principal,
                        redirect_to,
                    })
                }
                None => Ok(FormAuthOutcome::ShowErrorPage),
            }
        } else if let Some(token) = saved_token {
            // Unauthenticated request to a protected resource: stash it so the
            // user-agent can be redirected back here post-login, then ask the
            // caller to render the login page.
            let (path_only, query) = match path.split_once('?') {
                Some((p, q)) => (p.to_string(), Some(q.to_string())),
                None => (path.to_string(), None),
            };
            self.store.save(
                token,
                SavedRequest {
                    method: method.to_string(),
                    path: path_only,
                    query,
                },
            );
            Ok(FormAuthOutcome::ShowLoginPage)
        } else {
            Ok(FormAuthOutcome::Continue)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::realm::InMemoryRealm;

    fn make_authenticator() -> (FormAuthenticator, Arc<SavedRequestStore>) {
        let realm: Arc<dyn Realm> =
            Arc::new(InMemoryRealm::new().with_user("alice", "secret", vec!["user".into()]));
        let store = Arc::new(SavedRequestStore::new());
        let auth = FormAuthenticator::new(realm, "/login.html", "/error.html", Arc::clone(&store));
        (auth, store)
    }

    #[test]
    fn extracts_credentials_from_form_body() {
        let creds =
            FormCredentials::from_form_body("j_username=alice&j_password=p%40ss+word").unwrap();
        assert_eq!(creds.username, "alice");
        assert_eq!(creds.password, "p@ss word");
    }

    #[test]
    fn extracts_credentials_with_percent_encoding_only() {
        // Exact body specified in the task brief.
        let creds = FormCredentials::from_form_body("j_username=alice&j_password=p%40ss").unwrap();
        assert_eq!(creds.username, "alice");
        assert_eq!(creds.password, "p@ss");
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

    #[test]
    fn saved_request_store_round_trip() {
        let store = SavedRequestStore::new();
        assert!(store.is_empty());
        let req = SavedRequest {
            method: "GET".into(),
            path: "/protected".into(),
            query: Some("a=1".into()),
        };
        store.save("tok-1", req.clone());
        assert_eq!(store.len(), 1);
        let taken = store.take("tok-1").unwrap();
        assert_eq!(taken, req);
        // Token is consumed on take.
        assert!(store.take("tok-1").is_none());
        assert!(store.is_empty());
    }

    #[tokio::test]
    async fn authenticate_delegates_to_realm() {
        let (auth, _store) = make_authenticator();
        let creds = FormCredentials {
            username: "alice".into(),
            password: "secret".into(),
        };
        let principal = auth.authenticate(&creds).await.unwrap().unwrap();
        assert_eq!(principal.name, "alice");
    }

    #[tokio::test]
    async fn unauthenticated_get_saves_request_and_shows_login() {
        let (auth, store) = make_authenticator();
        let outcome = auth
            .process("/protected?x=1", "GET", None, Some("session-A"))
            .await
            .unwrap();
        assert_eq!(outcome, FormAuthOutcome::ShowLoginPage);
        let saved = store.take("session-A").unwrap();
        assert_eq!(saved.method, "GET");
        assert_eq!(saved.path, "/protected");
        assert_eq!(saved.query.as_deref(), Some("x=1"));
    }

    #[tokio::test]
    async fn j_security_check_with_valid_credentials_returns_authenticated() {
        let (auth, store) = make_authenticator();
        // Pre-populate the store as if the user had been bounced here.
        store.save(
            "session-A",
            SavedRequest {
                method: "GET".into(),
                path: "/protected".into(),
                query: Some("x=1".into()),
            },
        );
        let outcome = auth
            .process(
                "/j_security_check",
                "POST",
                Some("j_username=alice&j_password=secret"),
                Some("session-A"),
            )
            .await
            .unwrap();
        match outcome {
            FormAuthOutcome::Authenticated {
                principal,
                redirect_to,
            } => {
                assert_eq!(principal.name, "alice");
                assert_eq!(redirect_to, "/protected?x=1");
            }
            other => panic!("expected Authenticated, got {other:?}"),
        }
        // Saved request is consumed on successful replay.
        assert!(store.take("session-A").is_none());
    }

    #[tokio::test]
    async fn j_security_check_without_saved_request_redirects_to_root() {
        let (auth, _store) = make_authenticator();
        let outcome = auth
            .process(
                "/j_security_check",
                "POST",
                Some("j_username=alice&j_password=secret"),
                None,
            )
            .await
            .unwrap();
        match outcome {
            FormAuthOutcome::Authenticated { redirect_to, .. } => {
                assert_eq!(redirect_to, "/");
            }
            other => panic!("expected Authenticated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn j_security_check_with_invalid_credentials_returns_error_page() {
        let (auth, _store) = make_authenticator();
        let outcome = auth
            .process(
                "/j_security_check",
                "POST",
                Some("j_username=alice&j_password=wrong"),
                Some("session-A"),
            )
            .await
            .unwrap();
        assert_eq!(outcome, FormAuthOutcome::ShowErrorPage);
    }

    #[tokio::test]
    async fn j_security_check_with_missing_body_returns_error_page() {
        let (auth, _store) = make_authenticator();
        let outcome = auth
            .process("/j_security_check", "POST", None, None)
            .await
            .unwrap();
        assert_eq!(outcome, FormAuthOutcome::ShowErrorPage);
    }

    #[tokio::test]
    async fn unrelated_request_without_token_passes_through() {
        let (auth, _store) = make_authenticator();
        let outcome = auth.process("/anything", "GET", None, None).await.unwrap();
        assert_eq!(outcome, FormAuthOutcome::Continue);
    }
}
