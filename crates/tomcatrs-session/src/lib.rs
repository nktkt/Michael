//! `tomcatrs-session` — HTTP session management for the **Tomcat-RS
//! Compatibility Runtime**.
//!
//! This crate mirrors the responsibilities of Apache Tomcat's
//! `org.apache.catalina.session` package:
//!
//! * [`SessionData`] — the in-memory representation of an HTTP session.
//! * [`SessionStore`] — a pluggable async persistence trait, with three
//!   implementations:
//!   * [`MemorySessionStore`] — a lock-free in-process map (the default).
//!   * [`FileSessionStore`] — one JSON file per session on disk.
//!   * [`RedisSessionStore`] — a Redis-backed store, gated behind the
//!     non-default `redis` cargo feature.
//! * [`SessionManager`] — the façade webapps use to create, look up, persist,
//!   and expire sessions.
//! * [`CookieProcessor`] — parses inbound `Cookie:` headers and builds
//!   `Set-Cookie` values for the `JSESSIONID` cookie.
//!
//! # Attribute model
//!
//! In `v0.1.0` session attributes are restricted to `String` values
//! ([`SessionData::attributes`]). This keeps every store trivially
//! serialisable; a later release will introduce a typed attribute value.
//!
//! # Example
//!
//! ```
//! # use std::sync::Arc;
//! # use tomcatrs_session::{SessionManager, MemorySessionStore};
//! # tokio_test_helper(async {
//! let manager = SessionManager::new(Arc::new(MemorySessionStore::new()));
//! let session = manager.create().await.unwrap();
//! let found = manager.find(&session.id).await.unwrap();
//! assert_eq!(found.unwrap().id, session.id);
//! # });
//! # fn tokio_test_helper<F: std::future::Future>(f: F) {
//! #     tokio::runtime::Runtime::new().unwrap().block_on(f);
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod cookie;
mod manager;
mod store_file;
mod store_memory;
mod store_redis;

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

pub use cookie::{CookieProcessor, SameSite};
pub use manager::SessionManager;
pub use store_file::FileSessionStore;
pub use store_memory::MemorySessionStore;
pub use store_redis::RedisSessionStore;

/// The default `max-inactive-interval` for a freshly created session: 30
/// minutes, matching Tomcat's `web.xml` default.
pub const DEFAULT_MAX_INACTIVE_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// The standard name of the Servlet session-tracking cookie.
pub const SESSION_COOKIE_NAME: &str = "JSESSIONID";

/// The in-memory representation of a single HTTP session.
///
/// `SessionData` is a plain data record: it carries no behaviour beyond
/// expiry bookkeeping. Persistence is the responsibility of a
/// [`SessionStore`], and lifecycle orchestration the responsibility of a
/// [`SessionManager`].
///
/// Timestamps and durations are (de)serialised as integers — `created` and
/// `last_accessed` as Unix-epoch milliseconds, `max_inactive_interval` as
/// whole seconds — so that every store backend produces a stable, portable
/// on-the-wire form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionData {
    /// The opaque session identifier (the `JSESSIONID` value).
    pub id: String,

    /// When this session was first created.
    #[serde(with = "epoch_millis")]
    pub created: SystemTime,

    /// When this session was last accessed by a request.
    #[serde(with = "epoch_millis")]
    pub last_accessed: SystemTime,

    /// The maximum time the session may remain idle before it is considered
    /// expired. Defaults to [`DEFAULT_MAX_INACTIVE_INTERVAL`].
    #[serde(with = "duration_secs")]
    pub max_inactive_interval: Duration,

    /// Application-set session attributes. In `v0.1.0` values are `String`s.
    pub attributes: HashMap<String, String>,
}

impl SessionData {
    /// Create a new, empty session with the given id.
    ///
    /// `created` and `last_accessed` are stamped with the current time and
    /// `max_inactive_interval` is set to [`DEFAULT_MAX_INACTIVE_INTERVAL`].
    pub fn new(id: String) -> Self {
        let now = SystemTime::now();
        Self {
            id,
            created: now,
            last_accessed: now,
            max_inactive_interval: DEFAULT_MAX_INACTIVE_INTERVAL,
            attributes: HashMap::new(),
        }
    }

    /// Return `true` if the session has been idle longer than its
    /// `max_inactive_interval` as measured against `now`.
    ///
    /// A non-positive `max_inactive_interval` (zero) means the session never
    /// expires, mirroring the Servlet spec's interpretation of
    /// `setMaxInactiveInterval(0)` being treated as "use default"; here we
    /// simply treat a zero interval as "immortal" so callers can opt out.
    pub fn is_expired(&self, now: SystemTime) -> bool {
        if self.max_inactive_interval.is_zero() {
            return false;
        }
        match now.duration_since(self.last_accessed) {
            Ok(idle) => idle > self.max_inactive_interval,
            // `last_accessed` is in the future relative to `now`: not expired.
            Err(_) => false,
        }
    }

    /// Update `last_accessed` to the current time, resetting the idle clock.
    pub fn touch(&mut self) {
        self.last_accessed = SystemTime::now();
    }
}

/// An async, object-safe persistence backend for [`SessionData`].
///
/// Implementations must be cheap to share across tasks (`Send + Sync`) since a
/// single store is typically wrapped in an [`std::sync::Arc`] and handed to a
/// [`SessionManager`].
#[async_trait::async_trait]
pub trait SessionStore: Send + Sync + std::any::Any {
    /// Load the session with the given id, or `Ok(None)` if it is not present.
    async fn load(&self, id: &str) -> tomcatrs_core::Result<Option<SessionData>>;

    /// Persist `session`, creating it or overwriting any existing entry with
    /// the same id.
    async fn save(&self, session: SessionData) -> tomcatrs_core::Result<()>;

    /// Remove the session with the given id. Deleting a missing session is not
    /// an error.
    async fn delete(&self, id: &str) -> tomcatrs_core::Result<()>;

    /// Upcast to [`std::any::Any`] so a [`SessionManager`] can recognise a
    /// concrete backend (notably [`MemorySessionStore`]) behind the trait
    /// object and take a faster path. Every implementation is simply
    /// `fn as_any(&self) -> &dyn Any { self }`.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// `serde` adaptor: (de)serialise a [`SystemTime`] as Unix-epoch milliseconds.
mod epoch_millis {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        let millis = t
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        s.serialize_u64(millis)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SystemTime, D::Error> {
        let millis = u64::deserialize(d)?;
        Ok(UNIX_EPOCH + Duration::from_millis(millis))
    }
}

/// `serde` adaptor: (de)serialise a [`Duration`] as whole seconds.
mod duration_secs {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_secs(u64::deserialize(d)?))
    }
}

/// Generate a cryptographically-strong session identifier.
///
/// The result is 32 uppercase hexadecimal characters (128 bits of entropy),
/// which matches the shape of a Tomcat `JSESSIONID`. Randomness is drawn from
/// the operating system CSPRNG via [`rand::rngs::OsRng`].
pub fn generate_session_id() -> String {
    use rand::RngCore;

    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);

    let mut id = String::with_capacity(32);
    for b in bytes {
        // `write!` to a String is infallible.
        use std::fmt::Write;
        let _ = write!(id, "{:02X}", b);
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_look_like_jsessionid() {
        let id = generate_session_id();
        assert_eq!(id.len(), 32);
        assert!(id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_lowercase()));
        // Overwhelmingly likely to be unique.
        assert_ne!(id, generate_session_id());
    }

    #[test]
    fn new_session_has_defaults() {
        let s = SessionData::new("ABC".to_string());
        assert_eq!(s.id, "ABC");
        assert_eq!(s.max_inactive_interval, DEFAULT_MAX_INACTIVE_INTERVAL);
        assert!(s.attributes.is_empty());
        assert_eq!(s.created, s.last_accessed);
    }

    #[test]
    fn expiry_is_relative_to_last_accessed() {
        let mut s = SessionData::new("X".to_string());
        s.max_inactive_interval = Duration::from_secs(60);
        let now = SystemTime::now();
        // Just accessed: not expired.
        assert!(!s.is_expired(now));
        // Backdate last access by two minutes: expired.
        s.last_accessed = now - Duration::from_secs(120);
        assert!(s.is_expired(now));
    }

    #[test]
    fn zero_interval_never_expires() {
        let mut s = SessionData::new("X".to_string());
        s.max_inactive_interval = Duration::ZERO;
        s.last_accessed = SystemTime::now() - Duration::from_secs(86_400);
        assert!(!s.is_expired(SystemTime::now()));
    }

    #[test]
    fn touch_advances_last_accessed() {
        let mut s = SessionData::new("X".to_string());
        s.last_accessed = SystemTime::now() - Duration::from_secs(120);
        let before = s.last_accessed;
        s.touch();
        assert!(s.last_accessed > before);
    }

    #[test]
    fn session_data_json_round_trips() {
        let mut s = SessionData::new("DEADBEEF".to_string());
        s.attributes.insert("user".to_string(), "alice".to_string());
        s.max_inactive_interval = Duration::from_secs(900);
        let json = serde_json::to_string(&s).unwrap();
        let back: SessionData = serde_json::from_str(&json).unwrap();
        assert_eq!(s.id, back.id);
        assert_eq!(s.attributes, back.attributes);
        assert_eq!(s.max_inactive_interval, back.max_inactive_interval);
        // Millisecond precision is preserved within the epoch-millis adaptor.
        assert_eq!(
            s.created
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            back.created
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        );
    }
}
