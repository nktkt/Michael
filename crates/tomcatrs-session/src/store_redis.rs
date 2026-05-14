//! Redis-backed session store, gated behind the non-default `redis` cargo
//! feature.
//!
//! When the `redis` feature is **disabled** the [`RedisSessionStore`] type
//! still exists so downstream code can name it unconditionally, but its
//! constructor returns an error. When the feature is **enabled** the store is
//! fully functional and persists each session as a JSON string under the key
//! `session:<id>`, with the Redis key TTL kept in sync with the session's
//! `max_inactive_interval`.

use crate::SessionData;
#[cfg(not(feature = "redis"))]
use crate::SessionStore;

/// Key prefix under which sessions are stored in Redis.
#[cfg(feature = "redis")]
const KEY_PREFIX: &str = "session:";

/// A [`SessionStore`](crate::SessionStore) backed by a Redis server.
///
/// This backend is appropriate for multi-node deployments where sessions must
/// be shared across application instances. It requires the `redis` cargo
/// feature; without it, [`RedisSessionStore::connect`] fails fast.
#[cfg(feature = "redis")]
#[derive(Clone)]
pub struct RedisSessionStore {
    client: redis::Client,
}

/// A [`SessionStore`](crate::SessionStore) backed by a Redis server.
///
/// This is the feature-disabled stub: the `redis` cargo feature is off, so the
/// type carries no state and [`RedisSessionStore::connect`] always returns an
/// error directing the caller to enable the feature.
#[cfg(not(feature = "redis"))]
#[derive(Debug, Clone)]
pub struct RedisSessionStore {
    _private: (),
}

#[cfg(feature = "redis")]
impl RedisSessionStore {
    /// Connect to Redis using a standard `redis://` connection URL.
    ///
    /// The client is lazily pooled: no socket is opened until the first
    /// load/save/delete call.
    pub fn connect(url: &str) -> tomcatrs_core::Result<Self> {
        let client = redis::Client::open(url)
            .map_err(|e| tomcatrs_core::Error::Other(format!("redis connect error: {e}")))?;
        Ok(Self { client })
    }

    /// The Redis key under which a session id is stored.
    fn key(id: &str) -> String {
        format!("{KEY_PREFIX}{id}")
    }
}

#[cfg(feature = "redis")]
#[async_trait::async_trait]
impl crate::SessionStore for RedisSessionStore {
    async fn load(&self, id: &str) -> tomcatrs_core::Result<Option<SessionData>> {
        use redis::AsyncCommands;

        let mut conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| tomcatrs_core::Error::Other(format!("redis connection error: {e}")))?;

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

        let mut conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| tomcatrs_core::Error::Other(format!("redis connection error: {e}")))?;

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
            conn.set::<_, _, ()>(key, json)
                .await
                .map_err(|e| tomcatrs_core::Error::Other(format!("redis SET error: {e}")))?;
        }
        Ok(())
    }

    async fn delete(&self, id: &str) -> tomcatrs_core::Result<()> {
        use redis::AsyncCommands;

        let mut conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| tomcatrs_core::Error::Other(format!("redis connection error: {e}")))?;

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
