//! Redis-backed session store, gated behind the non-default `redis` cargo
//! feature.
//!
//! # Feature gating
//!
//! The [`RedisSessionStore`] *type* exists unconditionally so downstream code
//! can name it without caring whether the feature is on. Its behaviour,
//! however, depends on the `redis` cargo feature:
//!
//! * **feature disabled** (the default): the type is an empty stub and every
//!   operation — including [`RedisSessionStore::connect`] — returns
//!   [`tomcatrs_core::Error::Other`] telling the caller to rebuild with
//!   `--features redis`. This keeps default builds free of the `redis`
//!   dependency and its transitive C/async machinery.
//! * **feature enabled** (`--features redis`): the store is fully functional.
//!
//! # Behaviour with the feature enabled
//!
//! * **Connection management.** The store holds a [`redis::Client`] plus a
//!   lazily-initialised [`redis::aio::MultiplexedConnection`]. The multiplexed
//!   connection is `Clone` and pipelines concurrent commands over a single
//!   socket, so it acts as a lightweight built-in pool: every `load`/`save`/
//!   `delete` shares it instead of opening a fresh TCP connection. The
//!   connection is created on first use (behind a [`tokio::sync::Mutex`]) and
//!   re-established automatically if it has dropped.
//! * **Key namespacing.** Sessions are stored under
//!   `tomcatrs:session:<id>` so they never collide with other data in a
//!   shared Redis instance.
//! * **TTL.** On `save`, the Redis key TTL is set to the session's
//!   `max_inactive_interval`, so Redis evicts idle sessions for us. A zero
//!   interval (immortal session) is stored without a TTL.
//! * **Encoding.** Each session is serialised to a JSON string via
//!   `serde_json`, matching the on-the-wire form used by every other backend.
//! * **Error mapping.** Every `redis` / `serde_json` failure is mapped to
//!   [`tomcatrs_core::Error::Other`] with a descriptive, operation-tagged
//!   message.
//!
//! `load_all` is intentionally **not** overridden: scanning keys in a shared
//! Redis instance (`SCAN`/`KEYS`) is an anti-pattern for a session store, so
//! the backend keeps the trait default ("not supported").

#[cfg(not(feature = "redis"))]
use crate::SessionData;
#[cfg(not(feature = "redis"))]
use crate::SessionStore;
#[cfg(feature = "redis")]
use crate::{SessionData, SessionStore};

/// Key prefix under which sessions are stored in Redis.
///
/// The two-segment, colon-delimited shape (`tomcatrs:session:<id>`) follows
/// the conventional Redis namespacing pattern and keeps Tomcat-RS sessions
/// from colliding with unrelated keys in a shared instance.
#[cfg(feature = "redis")]
const KEY_PREFIX: &str = "tomcatrs:session:";

/// A [`SessionStore`](crate::SessionStore) backed by a Redis server.
///
/// Appropriate for multi-node deployments where sessions must be shared across
/// application instances. See the [module docs](self) for the connection,
/// namespacing, TTL, and error-mapping semantics.
///
/// The store is cheap to [`Clone`]: clones share the same client and the same
/// lazily-built multiplexed connection.
#[cfg(feature = "redis")]
#[derive(Clone)]
pub struct RedisSessionStore {
    client: redis::Client,
    /// Lazily-initialised multiplexed connection, shared by all clones.
    ///
    /// `MultiplexedConnection` is itself `Clone` and internally synchronised;
    /// the `Mutex` only guards the *first* creation (and re-creation after a
    /// drop), not steady-state command traffic.
    conn: std::sync::Arc<tokio::sync::Mutex<Option<redis::aio::MultiplexedConnection>>>,
}

/// A [`SessionStore`](crate::SessionStore) backed by a Redis server.
///
/// This is the feature-disabled stub: the `redis` cargo feature is off, so the
/// type carries no state and every operation returns an error directing the
/// caller to enable the feature.
#[cfg(not(feature = "redis"))]
#[derive(Debug, Clone)]
pub struct RedisSessionStore {
    _private: (),
}

#[cfg(feature = "redis")]
impl RedisSessionStore {
    /// Connect to Redis using a standard `redis://` connection URL.
    ///
    /// This only validates and stores the URL; no socket is opened until the
    /// first `load`/`save`/`delete` call lazily establishes the multiplexed
    /// connection.
    pub fn connect(url: &str) -> tomcatrs_core::Result<Self> {
        let client = redis::Client::open(url)
            .map_err(|e| tomcatrs_core::Error::Other(format!("redis connect error: {e}")))?;
        Ok(Self {
            client,
            conn: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        })
    }

    /// The Redis key under which a session id is stored.
    fn key(id: &str) -> String {
        format!("{KEY_PREFIX}{id}")
    }

    /// Return a live multiplexed connection, creating it on first use and
    /// transparently re-creating it if the previous one has dropped.
    async fn connection(&self) -> tomcatrs_core::Result<redis::aio::MultiplexedConnection> {
        let mut guard = self.conn.lock().await;
        if let Some(conn) = guard.as_ref() {
            return Ok(conn.clone());
        }
        let conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| tomcatrs_core::Error::Other(format!("redis connection error: {e}")))?;
        *guard = Some(conn.clone());
        Ok(conn)
    }
}

#[cfg(feature = "redis")]
#[async_trait::async_trait]
impl SessionStore for RedisSessionStore {
    async fn load(&self, id: &str) -> tomcatrs_core::Result<Option<SessionData>> {
        use redis::AsyncCommands;

        let mut conn = self.connection().await?;
        let raw: Option<String> = conn
            .get(Self::key(id))
            .await
            .map_err(|e| tomcatrs_core::Error::Other(format!("redis GET error: {e}")))?;

        match raw {
            None => Ok(None),
            Some(json) => {
                let session = serde_json::from_str(&json).map_err(|e| {
                    tomcatrs_core::Error::Other(format!("corrupt redis session {id}: {e}"))
                })?;
                Ok(Some(session))
            }
        }
    }

    async fn save(&self, session: SessionData) -> tomcatrs_core::Result<()> {
        use redis::AsyncCommands;

        let mut conn = self.connection().await?;
        let json = serde_json::to_string(&session).map_err(|e| {
            tomcatrs_core::Error::Other(format!("failed to serialise session: {e}"))
        })?;
        let key = Self::key(&session.id);

        let ttl = session.max_inactive_interval.as_secs();
        if ttl > 0 {
            // Persist with an expiry so Redis evicts idle sessions for us.
            conn.set_ex::<_, _, ()>(key, json, ttl)
                .await
                .map_err(|e| tomcatrs_core::Error::Other(format!("redis SETEX error: {e}")))?;
        } else {
            // A zero `max_inactive_interval` means "immortal": store with no
            // TTL, and clear any stale TTL a previous save may have set.
            conn.set::<_, _, ()>(&key, json)
                .await
                .map_err(|e| tomcatrs_core::Error::Other(format!("redis SET error: {e}")))?;
            conn.persist::<_, ()>(&key)
                .await
                .map_err(|e| tomcatrs_core::Error::Other(format!("redis PERSIST error: {e}")))?;
        }
        Ok(())
    }

    async fn delete(&self, id: &str) -> tomcatrs_core::Result<()> {
        use redis::AsyncCommands;

        let mut conn = self.connection().await?;
        conn.del::<_, ()>(Self::key(id))
            .await
            .map_err(|e| tomcatrs_core::Error::Other(format!("redis DEL error: {e}")))?;
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(not(feature = "redis"))]
impl RedisSessionStore {
    /// Attempt to connect to Redis.
    ///
    /// The `redis` cargo feature is disabled in this build, so this always
    /// returns [`tomcatrs_core::Error::Other`]. Rebuild with
    /// `--features redis` to enable the backend.
    pub fn connect(_url: &str) -> tomcatrs_core::Result<Self> {
        Err(tomcatrs_core::Error::Other(
            "redis session store requires the `redis` cargo feature".to_string(),
        ))
    }
}

#[cfg(not(feature = "redis"))]
#[async_trait::async_trait]
impl SessionStore for RedisSessionStore {
    async fn load(&self, _id: &str) -> tomcatrs_core::Result<Option<SessionData>> {
        Err(tomcatrs_core::Error::Other(
            "redis session store requires the `redis` cargo feature".to_string(),
        ))
    }

    async fn save(&self, _session: SessionData) -> tomcatrs_core::Result<()> {
        Err(tomcatrs_core::Error::Other(
            "redis session store requires the `redis` cargo feature".to_string(),
        ))
    }

    async fn delete(&self, _id: &str) -> tomcatrs_core::Result<()> {
        Err(tomcatrs_core::Error::Other(
            "redis session store requires the `redis` cargo feature".to_string(),
        ))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(all(test, not(feature = "redis")))]
mod tests {
    use super::*;

    #[test]
    fn connect_fails_without_feature() {
        let err = RedisSessionStore::connect("redis://127.0.0.1/").unwrap_err();
        assert!(err.to_string().contains("redis` cargo feature"));
    }
}

#[cfg(all(test, feature = "redis"))]
mod redis_tests {
    use super::*;

    #[test]
    fn key_is_namespaced() {
        assert_eq!(RedisSessionStore::key("ABC"), "tomcatrs:session:ABC");
    }

    #[test]
    fn connect_accepts_valid_url() {
        // Only the URL is parsed here; no socket is opened.
        assert!(RedisSessionStore::connect("redis://127.0.0.1:6379/").is_ok());
        assert!(RedisSessionStore::connect("not a url").is_err());
    }

    /// Full round-trip against a real Redis at `redis://127.0.0.1/`.
    ///
    /// Ignored by default because it needs a running server; run with
    /// `cargo test -p tomcatrs-session --features redis -- --ignored`.
    #[tokio::test]
    #[ignore = "requires a running redis server"]
    async fn redis_round_trip() {
        let store = RedisSessionStore::connect("redis://127.0.0.1/").unwrap();
        let mut session = SessionData::new("REDISTEST1".to_string());
        session
            .attributes
            .insert("user".to_string(), "alice".to_string());

        store.save(session.clone()).await.unwrap();
        let loaded = store.load("REDISTEST1").await.unwrap().unwrap();
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.attributes, session.attributes);

        store.delete("REDISTEST1").await.unwrap();
        assert!(store.load("REDISTEST1").await.unwrap().is_none());
    }
}
