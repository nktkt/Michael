//! In-process, lock-free session store backed by [`dashmap::DashMap`].

use std::time::SystemTime;

use dashmap::DashMap;

use crate::{SessionData, SessionStore};

/// A [`SessionStore`] that keeps every session in a concurrent in-memory map.
///
/// This is the default backend: it is the fastest option and requires no
/// external service, at the cost of losing all sessions when the process
/// exits. It is the natural choice for a single-node deployment.
///
/// Because [`SessionStore`] deliberately has no `list` method, this type
/// additionally exposes the inherent [`MemorySessionStore::reap`] and
/// [`MemorySessionStore::len`] helpers, which the
/// [`SessionManager`](crate::SessionManager) uses to implement
/// `reap_expired` for memory-backed deployments.
#[derive(Debug, Default)]
pub struct MemorySessionStore {
    sessions: DashMap<String, SessionData>,
}

impl MemorySessionStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of sessions currently held.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Whether the store currently holds no sessions.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Remove every session that is expired as of `now`, returning the number
    /// of sessions evicted.
    ///
    /// This is an inherent method rather than part of the [`SessionStore`]
    /// trait because enumerating every key only makes sense for an in-memory
    /// backend; file and Redis stores expire entries lazily or via TTLs.
    pub fn reap(&self, now: SystemTime) -> usize {
        let expired: Vec<String> = self
            .sessions
            .iter()
            .filter(|entry| entry.value().is_expired(now))
            .map(|entry| entry.key().clone())
            .collect();

        for id in &expired {
            self.sessions.remove(id);
        }
        expired.len()
    }
}

#[async_trait::async_trait]
impl SessionStore for MemorySessionStore {
    async fn load(&self, id: &str) -> tomcatrs_core::Result<Option<SessionData>> {
        Ok(self.sessions.get(id).map(|e| e.value().clone()))
    }

    async fn save(&self, session: SessionData) -> tomcatrs_core::Result<()> {
        self.sessions.insert(session.id.clone(), session);
        Ok(())
    }

    async fn delete(&self, id: &str) -> tomcatrs_core::Result<()> {
        self.sessions.remove(id);
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn save_load_delete_round_trip() {
        let store = MemorySessionStore::new();
        let mut session = SessionData::new("ABC123".to_string());
        session.attributes.insert("k".to_string(), "v".to_string());

        store.save(session.clone()).await.unwrap();
        let loaded = store.load("ABC123").await.unwrap();
        assert_eq!(loaded, Some(session));

        store.delete("ABC123").await.unwrap();
        assert_eq!(store.load("ABC123").await.unwrap(), None);
        // Deleting a missing session is not an error.
        store.delete("ABC123").await.unwrap();
    }

    #[tokio::test]
    async fn reap_evicts_only_expired_sessions() {
        let store = MemorySessionStore::new();

        let live = SessionData::new("LIVE".to_string());
        store.save(live).await.unwrap();

        let mut dead = SessionData::new("DEAD".to_string());
        dead.max_inactive_interval = Duration::from_secs(1);
        dead.last_accessed = SystemTime::now() - Duration::from_secs(3600);
        store.save(dead).await.unwrap();

        assert_eq!(store.len(), 2);
        let removed = store.reap(SystemTime::now());
        assert_eq!(removed, 1);
        assert_eq!(store.len(), 1);
        assert!(store.load("LIVE").await.unwrap().is_some());
        assert!(store.load("DEAD").await.unwrap().is_none());
    }
}
