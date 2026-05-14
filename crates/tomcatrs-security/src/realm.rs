//! Authentication realms.
//!
//! A [`Realm`] is the abstraction Tomcat uses to decouple authenticators
//! (`BASIC`, `DIGEST`, `FORM`) from the underlying credential store. An
//! authenticator extracts a username/credential pair from the request and asks
//! the realm to turn it into an authenticated [`Principal`]; the realm is also
//! consulted for role checks during authorization.

use std::collections::HashMap;

use parking_lot::RwLock;
use sha2::{Digest, Sha256};

/// An authenticated user identity plus the roles it has been granted.
///
/// This is the security-context equivalent of `java.security.Principal`
/// combined with the container's role model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// The authenticated user name.
    pub name: String,
    /// The set of roles granted to this principal.
    pub roles: Vec<String>,
}

impl Principal {
    /// Construct a principal from a name and a role list.
    pub fn new(name: impl Into<String>, roles: Vec<String>) -> Self {
        Principal {
            name: name.into(),
            roles,
        }
    }

    /// Returns `true` if this principal holds `role`.
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
    }
}

/// The authentication and authorization contract every credential store
/// implements.
///
/// Implementations must be cheap to share across tasks (`Send + Sync`); the
/// authenticators hold realms behind shared references.
#[async_trait::async_trait]
pub trait Realm: Send + Sync {
    /// Verify `credential` for `username` and, on success, return the
    /// corresponding [`Principal`].
    ///
    /// Returns `Ok(None)` when the credentials are syntactically valid but do
    /// not match a known user — callers must treat that as an authentication
    /// failure, not an error. `Err(..)` is reserved for genuine backend
    /// failures (a database being unreachable, for example).
    async fn authenticate(
        &self,
        username: &str,
        credential: &str,
    ) -> tomcatrs_core::Result<Option<Principal>>;

    /// Returns `true` if `principal` is a member of `role`.
    async fn has_role(&self, principal: &Principal, role: &str) -> bool;
}

/// Hash a plaintext credential with SHA-256 and return the lowercase hex
/// digest.
///
/// This is deliberately a module-level helper so the hashing scheme used at
/// `add_user` time and at `authenticate` time can never drift apart.
fn hash_credential(credential: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(credential.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{:02x}", byte);
    }
    hex
}

/// A single stored account: the SHA-256 hash of the password plus the user's
/// roles.
#[derive(Debug, Clone)]
struct StoredUser {
    credential_hash: String,
    roles: Vec<String>,
}

/// A fully-working in-memory [`Realm`].
///
/// Passwords are **never** stored in plaintext: each is hashed with SHA-256 at
/// insertion time, and authentication compares hashes. This is appropriate for
/// tests, embedded deployments, and the `MemoryRealm`-style configuration
/// Tomcat ships in `conf/tomcat-users.xml`.
///
/// The store is guarded by an `RwLock` so the realm can be shared (`Arc`) and
/// mutated after construction.
///
/// # Examples
///
/// ```
/// use tomcatrs_security::realm::InMemoryRealm;
///
/// let realm = InMemoryRealm::new()
///     .with_user("admin", "s3cret", vec!["manager".into(), "admin".into()]);
/// ```
#[derive(Debug, Default)]
pub struct InMemoryRealm {
    users: RwLock<HashMap<String, StoredUser>>,
}

impl InMemoryRealm {
    /// Create an empty realm.
    pub fn new() -> Self {
        InMemoryRealm {
            users: RwLock::new(HashMap::new()),
        }
    }

    /// Add (or replace) a user, hashing `password` before it is stored.
    ///
    /// Takes `&self` so callers can populate a realm already wrapped in an
    /// `Arc`.
    pub fn add_user(&self, username: impl Into<String>, password: &str, roles: Vec<String>) {
        let username = username.into();
        let stored = StoredUser {
            credential_hash: hash_credential(password),
            roles,
        };
        self.users.write().insert(username, stored);
    }

    /// Builder-style variant of [`add_user`](Self::add_user).
    ///
    /// Consumes and returns `self` so users can be chained at construction
    /// time.
    pub fn with_user(
        self,
        username: impl Into<String>,
        password: &str,
        roles: Vec<String>,
    ) -> Self {
        self.add_user(username, password, roles);
        self
    }

    /// Returns the number of registered users.
    pub fn user_count(&self) -> usize {
        self.users.read().len()
    }
}

#[async_trait::async_trait]
impl Realm for InMemoryRealm {
    async fn authenticate(
        &self,
        username: &str,
        credential: &str,
    ) -> tomcatrs_core::Result<Option<Principal>> {
        let presented = hash_credential(credential);
        let users = self.users.read();
        match users.get(username) {
            // Compare the freshly-computed hash against the stored hash. Both
            // are fixed-length lowercase hex strings of equal length, so a
            // direct comparison does not leak the password length.
            Some(user) if user.credential_hash == presented => Ok(Some(Principal {
                name: username.to_string(),
                roles: user.roles.clone(),
            })),
            _ => Ok(None),
        }
    }

    async fn has_role(&self, principal: &Principal, role: &str) -> bool {
        // The principal already carries its granted roles; the realm simply
        // confirms membership. A database-backed realm would re-query here.
        principal.has_role(role)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_are_not_stored_in_plaintext() {
        let realm = InMemoryRealm::new();
        realm.add_user("alice", "hunter2", vec!["user".into()]);
        let guard = realm.users.read();
        let stored = guard.get("alice").unwrap();
        assert_ne!(stored.credential_hash, "hunter2");
        assert_eq!(stored.credential_hash.len(), 64); // SHA-256 hex
    }

    #[tokio::test]
    async fn authenticate_accepts_good_password() {
        let realm = InMemoryRealm::new().with_user("bob", "correct horse", vec!["staff".into()]);
        let principal = realm
            .authenticate("bob", "correct horse")
            .await
            .unwrap()
            .expect("good credentials should authenticate");
        assert_eq!(principal.name, "bob");
        assert!(principal.has_role("staff"));
    }

    #[tokio::test]
    async fn authenticate_rejects_bad_password() {
        let realm = InMemoryRealm::new().with_user("bob", "correct horse", vec!["staff".into()]);
        assert!(realm.authenticate("bob", "wrong").await.unwrap().is_none());
        assert!(realm
            .authenticate("nobody", "whatever")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn has_role_reflects_principal_membership() {
        let realm = InMemoryRealm::new();
        let p = Principal::new("carol", vec!["admin".into()]);
        assert!(realm.has_role(&p, "admin").await);
        assert!(!realm.has_role(&p, "guest").await);
    }
}
