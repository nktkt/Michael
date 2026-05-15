//! Additional [`Realm`] backends: file-backed users, realm chaining, and
//! lock-out throttling.
//!
//! These complement the in-memory [`InMemoryRealm`](crate::realm::InMemoryRealm)
//! with the realm shapes Tomcat ships out of the box:
//!
//! * [`FileRealm`] — the classic `MemoryRealm` / `UserDatabaseRealm`
//!   `tomcat-users.xml` format.
//! * [`CombinedRealm`] — Tomcat's `CombinedRealm`: try a chain of realms in
//!   order, first success wins.
//! * [`LockOutRealm`] — Tomcat's `LockOutRealm`: after too many consecutive
//!   failures for a given username, refuse further attempts for a while.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use quick_xml::events::Event;
use quick_xml::Reader;
use tomcatrs_core::{Error, Result};

use crate::realm::{Principal, Realm};

/// Local name of an element/attribute as a UTF-8 `String`.
fn local_name(name: quick_xml::name::QName<'_>) -> String {
    String::from_utf8_lossy(name.local_name().as_ref()).into_owned()
}

/// A realm backed by a Tomcat-style `tomcat-users.xml` file.
///
/// The file format is the one Tomcat ships in `conf/tomcat-users.xml`:
///
/// ```xml
/// <tomcat-users>
///   <role rolename="manager"/>
///   <role rolename="admin"/>
///   <user username="admin" password="s3cret" roles="manager,admin"/>
/// </tomcat-users>
/// ```
///
/// # Password storage
///
/// **v1 stores passwords as plaintext, exactly as they appear in the XML.**
/// This matches Tomcat's default for `MemoryRealm` and is appropriate for
/// development and small deployments where the file is on a trusted host with
/// strict permissions. Hashed-password support (`<user password="{SHA}…"/>`
/// style) is a future addition.
#[derive(Debug, Default)]
pub struct FileRealm {
    /// Known role names declared via `<role rolename="…"/>`. Stored only so
    /// that `roles="…"` attributes can be sanity-checked against them — a
    /// missing declaration is logged but not an error, mirroring Tomcat's
    /// tolerant behaviour.
    declared_roles: Vec<String>,
    users: HashMap<String, FileUser>,
}

#[derive(Debug, Clone)]
struct FileUser {
    password: String,
    roles: Vec<String>,
}

impl FileRealm {
    /// Construct an empty realm (no users, no roles). Primarily useful as a
    /// starting point for tests.
    pub fn new() -> Self {
        FileRealm::default()
    }

    /// Parse a `tomcat-users.xml` document from a string.
    ///
    /// Unknown elements and unknown attributes are ignored, matching the
    /// tolerant parsing posture of the rest of `tomcatrs-config`.
    pub fn from_xml_str(xml: &str) -> Result<Self> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);

        let mut realm = FileRealm::new();
        let mut buf = Vec::new();
        loop {
            let event = reader
                .read_event_into(&mut buf)
                .map_err(|e| Error::config(format!("tomcat-users.xml parse error: {e}")))?;
            match event {
                Event::Eof => break,
                Event::Empty(start) | Event::Start(start) => {
                    let name = local_name(start.name());
                    match name.as_str() {
                        "role" => {
                            for attr in start.attributes() {
                                let attr = attr.map_err(|e| {
                                    Error::config(format!(
                                        "malformed attribute in tomcat-users.xml: {e}"
                                    ))
                                })?;
                                if local_name(attr.key.into()) == "rolename" {
                                    let v = attr.unescape_value().map_err(|e| {
                                        Error::config(format!(
                                            "invalid attribute value in tomcat-users.xml: {e}"
                                        ))
                                    })?;
                                    realm.declared_roles.push(v.into_owned());
                                }
                            }
                        }
                        "user" => {
                            let mut username: Option<String> = None;
                            let mut password: Option<String> = None;
                            let mut roles_attr: Option<String> = None;
                            for attr in start.attributes() {
                                let attr = attr.map_err(|e| {
                                    Error::config(format!(
                                        "malformed attribute in tomcat-users.xml: {e}"
                                    ))
                                })?;
                                let key = local_name(attr.key.into());
                                let value = attr
                                    .unescape_value()
                                    .map_err(|e| {
                                        Error::config(format!(
                                            "invalid attribute value in tomcat-users.xml: {e}"
                                        ))
                                    })?
                                    .into_owned();
                                match key.as_str() {
                                    "username" => username = Some(value),
                                    "password" => password = Some(value),
                                    "roles" => roles_attr = Some(value),
                                    _ => {}
                                }
                            }
                            let username = username.ok_or_else(|| {
                                Error::config("<user> missing 'username' attribute")
                            })?;
                            let password = password.unwrap_or_default();
                            let roles = roles_attr
                                .map(|s| {
                                    s.split(',')
                                        .map(|r| r.trim().to_string())
                                        .filter(|r| !r.is_empty())
                                        .collect::<Vec<_>>()
                                })
                                .unwrap_or_default();
                            realm.users.insert(username, FileUser { password, roles });
                        }
                        _ => {
                            // Tolerate <tomcat-users>, <group>, comments, etc.
                        }
                    }
                }
                _ => {}
            }
            buf.clear();
        }
        Ok(realm)
    }

    /// Parse a `tomcat-users.xml` file from disk.
    pub fn from_xml_file(path: impl AsRef<Path>) -> Result<Self> {
        let contents = std::fs::read_to_string(path.as_ref())?;
        Self::from_xml_str(&contents)
    }

    /// Returns the number of users loaded into the realm.
    pub fn user_count(&self) -> usize {
        self.users.len()
    }

    /// Returns the list of declared role names (the union of every
    /// `<role rolename="…"/>` element seen during parsing).
    pub fn declared_roles(&self) -> &[String] {
        &self.declared_roles
    }
}

#[async_trait::async_trait]
impl Realm for FileRealm {
    async fn authenticate(&self, username: &str, credential: &str) -> Result<Option<Principal>> {
        match self.users.get(username) {
            Some(user) if user.password == credential => Ok(Some(Principal {
                name: username.to_string(),
                roles: user.roles.clone(),
            })),
            _ => Ok(None),
        }
    }

    async fn has_role(&self, principal: &Principal, role: &str) -> bool {
        principal.has_role(role)
    }
}

/// A [`Realm`] that delegates to a list of inner realms in order, returning
/// the first successful authentication.
///
/// Mirrors Tomcat's `org.apache.catalina.realm.CombinedRealm`. A backend
/// returning `Ok(None)` is treated as "not my user" and the next backend is
/// tried; a backend returning `Err(..)` short-circuits the chain (a genuine
/// backend failure is propagated rather than masked by the next realm
/// happening to succeed).
pub struct CombinedRealm {
    realms: Vec<Arc<dyn Realm>>,
}

impl std::fmt::Debug for CombinedRealm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CombinedRealm")
            .field("realm_count", &self.realms.len())
            .finish()
    }
}

impl CombinedRealm {
    /// Construct a [`CombinedRealm`] from a list of inner realms. The order is
    /// preserved and is the order in which backends will be tried.
    pub fn new(realms: Vec<Arc<dyn Realm>>) -> Self {
        CombinedRealm { realms }
    }

    /// Append a realm to the end of the chain.
    pub fn push(&mut self, realm: Arc<dyn Realm>) {
        self.realms.push(realm);
    }

    /// Returns the number of inner realms.
    pub fn len(&self) -> usize {
        self.realms.len()
    }

    /// Returns `true` if the chain is empty.
    pub fn is_empty(&self) -> bool {
        self.realms.is_empty()
    }
}

#[async_trait::async_trait]
impl Realm for CombinedRealm {
    async fn authenticate(&self, username: &str, credential: &str) -> Result<Option<Principal>> {
        for realm in &self.realms {
            if let Some(principal) = realm.authenticate(username, credential).await? {
                return Ok(Some(principal));
            }
        }
        Ok(None)
    }

    async fn has_role(&self, principal: &Principal, role: &str) -> bool {
        for realm in &self.realms {
            if realm.has_role(principal, role).await {
                return true;
            }
        }
        false
    }

    async fn digest_ha1(&self, username: &str, realm_name: &str) -> Option<String> {
        for realm in &self.realms {
            if let Some(ha1) = realm.digest_ha1(username, realm_name).await {
                return Some(ha1);
            }
        }
        None
    }
}

#[derive(Debug, Clone, Copy)]
struct FailureRecord {
    count: u32,
    locked_at: Option<Instant>,
}

/// A [`Realm`] decorator that rate-limits failed authentications per username.
///
/// Mirrors Tomcat's `org.apache.catalina.realm.LockOutRealm`. After
/// [`failure_count`](LockOutRealm::failure_count) consecutive failures for the
/// same username the user is locked out for [`lockout_time`](LockOutRealm::lockout_time);
/// any authentication attempt during that window is refused without consulting
/// the inner realm. After the window elapses the next attempt is forwarded
/// again, and successful authentication clears the counter.
pub struct LockOutRealm {
    inner: Arc<dyn Realm>,
    failure_count: u32,
    lockout_time: Duration,
    failures: Mutex<HashMap<String, FailureRecord>>,
}

impl std::fmt::Debug for LockOutRealm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockOutRealm")
            .field("failure_count", &self.failure_count)
            .field("lockout_time", &self.lockout_time)
            .finish()
    }
}

impl LockOutRealm {
    /// Wrap `inner`, locking out a username after `failure_count` consecutive
    /// failures for `lockout_time`.
    pub fn new(inner: Arc<dyn Realm>, failure_count: u32, lockout_time: Duration) -> Self {
        LockOutRealm {
            inner,
            failure_count,
            lockout_time,
            failures: Mutex::new(HashMap::new()),
        }
    }

    /// Returns the configured failure threshold.
    pub fn failure_count(&self) -> u32 {
        self.failure_count
    }

    /// Returns the configured lockout window.
    pub fn lockout_time(&self) -> Duration {
        self.lockout_time
    }

    /// Returns `true` if the given username is currently locked out.
    pub fn is_locked(&self, username: &str) -> bool {
        let guard = self.failures.lock();
        match guard.get(username) {
            Some(record) => {
                if let Some(at) = record.locked_at {
                    at.elapsed() < self.lockout_time
                } else {
                    false
                }
            }
            None => false,
        }
    }
}

#[async_trait::async_trait]
impl Realm for LockOutRealm {
    async fn authenticate(&self, username: &str, credential: &str) -> Result<Option<Principal>> {
        // Fast path: refuse without consulting the inner realm while the
        // lockout window is still open.
        {
            let mut guard = self.failures.lock();
            if let Some(record) = guard.get_mut(username) {
                if let Some(at) = record.locked_at {
                    if at.elapsed() < self.lockout_time {
                        return Ok(None);
                    }
                    // Window elapsed: clear the lock and let this attempt go
                    // through.
                    record.locked_at = None;
                    record.count = 0;
                }
            }
        }

        let outcome = self.inner.authenticate(username, credential).await?;
        let mut guard = self.failures.lock();
        match &outcome {
            Some(_) => {
                // Successful authentication: forget any prior failures.
                guard.remove(username);
            }
            None => {
                let entry = guard.entry(username.to_string()).or_insert(FailureRecord {
                    count: 0,
                    locked_at: None,
                });
                entry.count = entry.count.saturating_add(1);
                if entry.count >= self.failure_count {
                    entry.locked_at = Some(Instant::now());
                }
            }
        }
        Ok(outcome)
    }

    async fn has_role(&self, principal: &Principal, role: &str) -> bool {
        self.inner.has_role(principal, role).await
    }

    async fn digest_ha1(&self, username: &str, realm_name: &str) -> Option<String> {
        self.inner.digest_ha1(username, realm_name).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::realm::InMemoryRealm;

    const SAMPLE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<tomcat-users>
    <role rolename="manager"/>
    <role rolename="admin"/>
    <user username="admin" password="s3cret" roles="manager,admin"/>
    <user username="alice" password="hunter2" roles="manager"/>
</tomcat-users>"#;

    #[tokio::test]
    async fn file_realm_parses_users_and_roles() {
        let realm = FileRealm::from_xml_str(SAMPLE_XML).unwrap();
        assert_eq!(realm.user_count(), 2);
        assert_eq!(realm.declared_roles(), &["manager", "admin"]);

        let admin = realm
            .authenticate("admin", "s3cret")
            .await
            .unwrap()
            .expect("good admin credentials");
        assert_eq!(admin.name, "admin");
        assert!(admin.has_role("manager"));
        assert!(admin.has_role("admin"));

        let alice = realm
            .authenticate("alice", "hunter2")
            .await
            .unwrap()
            .expect("good alice credentials");
        assert!(alice.has_role("manager"));
        assert!(!alice.has_role("admin"));
    }

    #[tokio::test]
    async fn file_realm_rejects_bad_password_and_unknown_user() {
        let realm = FileRealm::from_xml_str(SAMPLE_XML).unwrap();
        assert!(realm
            .authenticate("admin", "wrong")
            .await
            .unwrap()
            .is_none());
        assert!(realm
            .authenticate("nobody", "whatever")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn file_realm_handles_empty_roles_attribute() {
        let xml = r#"<tomcat-users>
            <user username="noroles" password="pw" roles=""/>
        </tomcat-users>"#;
        let realm = FileRealm::from_xml_str(xml).unwrap();
        let p = realm.authenticate("noroles", "pw").await.unwrap().unwrap();
        assert!(p.roles.is_empty());
    }

    #[tokio::test]
    async fn file_realm_rejects_user_without_username() {
        let xml = r#"<tomcat-users>
            <user password="pw" roles="a"/>
        </tomcat-users>"#;
        assert!(FileRealm::from_xml_str(xml).is_err());
    }

    #[tokio::test]
    async fn combined_realm_first_success_wins() {
        let r1: Arc<dyn Realm> =
            Arc::new(InMemoryRealm::new().with_user("alice", "pw-one", vec!["one".into()]));
        let r2: Arc<dyn Realm> =
            Arc::new(InMemoryRealm::new().with_user("alice", "pw-two", vec!["two".into()]));
        // Two distinct realms both know "alice"; the first one with matching
        // credentials wins.
        let combined = CombinedRealm::new(vec![Arc::clone(&r1), Arc::clone(&r2)]);
        let p = combined
            .authenticate("alice", "pw-two")
            .await
            .unwrap()
            .expect("second realm should authenticate alice");
        assert!(p.has_role("two"));
        assert!(!p.has_role("one"));

        // Swapping the order makes the *first* realm answer for the same
        // password.
        let combined = CombinedRealm::new(vec![Arc::clone(&r2), Arc::clone(&r1)]);
        let p = combined
            .authenticate("alice", "pw-two")
            .await
            .unwrap()
            .unwrap();
        assert!(p.has_role("two"));
    }

    #[tokio::test]
    async fn combined_realm_returns_none_when_no_backend_matches() {
        let r1: Arc<dyn Realm> = Arc::new(InMemoryRealm::new().with_user("alice", "pw", vec![]));
        let r2: Arc<dyn Realm> = Arc::new(InMemoryRealm::new().with_user("bob", "pw", vec![]));
        let combined = CombinedRealm::new(vec![r1, r2]);
        assert!(combined
            .authenticate("nobody", "anything")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn lockout_realm_locks_after_three_failures() {
        let inner: Arc<dyn Realm> =
            Arc::new(InMemoryRealm::new().with_user("alice", "secret", vec!["user".into()]));
        let lockout = LockOutRealm::new(Arc::clone(&inner), 3, Duration::from_millis(50));

        // Three bad attempts in a row trip the lock.
        for _ in 0..3 {
            assert!(lockout
                .authenticate("alice", "wrong")
                .await
                .unwrap()
                .is_none());
        }
        assert!(lockout.is_locked("alice"));

        // Even the *correct* password is refused while the lock is held.
        assert!(lockout
            .authenticate("alice", "secret")
            .await
            .unwrap()
            .is_none());

        // After the lockout window elapses the realm forwards again.
        std::thread::sleep(Duration::from_millis(80));
        assert!(!lockout.is_locked("alice"));
        let p = lockout
            .authenticate("alice", "secret")
            .await
            .unwrap()
            .expect("post-window correct password should succeed");
        assert_eq!(p.name, "alice");

        // Successful auth resets the counter.
        assert!(!lockout.is_locked("alice"));
    }

    #[tokio::test]
    async fn lockout_realm_does_not_lock_on_success() {
        let inner: Arc<dyn Realm> =
            Arc::new(InMemoryRealm::new().with_user("alice", "secret", vec![]));
        let lockout = LockOutRealm::new(inner, 3, Duration::from_millis(50));

        // Two failures, then a success — no lock should be in force.
        assert!(lockout
            .authenticate("alice", "wrong")
            .await
            .unwrap()
            .is_none());
        assert!(lockout
            .authenticate("alice", "wrong")
            .await
            .unwrap()
            .is_none());
        assert!(lockout
            .authenticate("alice", "secret")
            .await
            .unwrap()
            .is_some());

        // A further failure should *not* immediately lock out (counter reset).
        assert!(lockout
            .authenticate("alice", "wrong")
            .await
            .unwrap()
            .is_none());
        assert!(!lockout.is_locked("alice"));
    }
}
