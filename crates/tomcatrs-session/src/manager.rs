//! The [`SessionManager`] — the façade webapps use to manage session
//! lifecycle on top of a pluggable [`SessionStore`].

use std::sync::Arc;
use std::time::SystemTime;

use parking_lot::Mutex;
use tracing::{debug, trace};

use crate::{generate_session_id, MemorySessionStore, SessionData, SessionStore};

/// Orchestrates session creation, lookup, persistence, and expiry on top of a
/// [`SessionStore`].
///
/// A manager owns an `Arc<dyn SessionStore>` plus a small amount of
/// configuration. It is cheap to clone-share via `Arc` and is fully
/// `Send + Sync`.
///
/// # Expiry strategy
///
/// [`SessionStore`] deliberately exposes no `list` operation, so the manager
/// cannot enumerate a backend generically. To make [`reap_expired`] work for
/// *any* backend, the manager keeps an **in-memory index** of the ids it has
/// created or looked up (`SessionManager::index`). `reap_expired` walks that
/// index, loads each session, and deletes the expired ones.
///
/// As an optimisation, when the underlying store is a [`MemorySessionStore`]
/// the manager delegates straight to its inherent `reap`, which is a single
/// lock-free pass and does not depend on the index being populated.
///
/// [`reap_expired`]: SessionManager::reap_expired
pub struct SessionManager {
    store: Arc<dyn SessionStore>,
    /// Best-effort index of known session ids, used by `reap_expired` for
    /// non-memory backends. Guarded by a fast non-async mutex because every
    /// critical section is a handful of `HashSet` operations.
    index: Mutex<std::collections::HashSet<String>>,
}

impl SessionManager {
    /// Create a manager over the given store.
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self {
            store,
            index: Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// The store this manager persists sessions to.
    pub fn store(&self) -> &Arc<dyn SessionStore> {
        &self.store
    }

    /// Number of session ids currently tracked in the in-memory index.
    ///
    /// This is a diagnostic counter, not an authoritative session count: it
    /// reflects ids this manager instance has created or observed and not yet
    /// reaped.
    pub fn tracked_ids(&self) -> usize {
        self.index.lock().len()
    }

    /// Create a brand-new session: generate a cryptographically-strong id,
    /// build an empty [`SessionData`], persist it, and return it.
    pub async fn create(&self) -> tomcatrs_core::Result<SessionData> {
        let id = generate_session_id();
        let session = SessionData::new(id.clone());
        self.store.save(session.clone()).await?;
        self.index.lock().insert(id.clone());
        debug!(session.id = %id, "created session");
        Ok(session)
    }

    /// Look up a session by id.
    ///
    /// A found session is recorded in the in-memory index so that a later
    /// [`reap_expired`](SessionManager::reap_expired) can reach it even if it
    /// was created by a different manager instance or in a previous process.
    pub async fn find(&self, id: &str) -> tomcatrs_core::Result<Option<SessionData>> {
        let session = self.store.load(id).await?;
        if session.is_some() {
            self.index.lock().insert(id.to_string());
            trace!(session.id = %id, "session found");
        } else {
            trace!(session.id = %id, "session not found");
        }
        Ok(session)
    }

    /// Persist `session`, overwriting any existing entry with the same id.
    pub async fn save(&self, session: SessionData) -> tomcatrs_core::Result<()> {
        let id = session.id.clone();
        self.store.save(session).await?;
        self.index.lock().insert(id);
        Ok(())
    }

    /// Invalidate (delete) the session with the given id.
    ///
    /// The id is also dropped from the in-memory index. Invalidating a missing
    /// session is not an error.
    pub async fn invalidate(&self, id: &str) -> tomcatrs_core::Result<()> {
        self.store.delete(id).await?;
        self.index.lock().remove(id);
        debug!(session.id = %id, "invalidated session");
        Ok(())
    }

    /// Remove every expired session, returning the number reaped.
    ///
    /// For a [`MemorySessionStore`] this delegates to the store's inherent,
    /// single-pass `reap`. For any other backend the manager walks its
    /// in-memory id index, loading each session and deleting the ones that
    /// [`SessionData::is_expired`] reports as stale.
    pub async fn reap_expired(&self) -> tomcatrs_core::Result<usize> {
        let now = SystemTime::now();

        // Fast path: the memory store can enumerate itself.
        if let Some(mem) = self.store.as_any().downcast_ref::<MemorySessionStore>() {
            let removed = mem.reap(now);
            if removed > 0 {
                // Keep the index from growing unbounded with dead ids.
                let index = self.index.lock();
                let live: Vec<String> = index.iter().cloned().collect();
                drop(index);
                self.prune_index_against_store(&live, now).await;
            }
            debug!(removed, "reaped expired sessions (memory fast path)");
            return Ok(removed);
        }

        // Generic path: consult the in-memory index.
        let candidates: Vec<String> = self.index.lock().iter().cloned().collect();
        let mut removed = 0usize;
        for id in candidates {
            match self.store.load(&id).await? {
                None => {
                    // Already gone from the store; drop from the index too.
                    self.index.lock().remove(&id);
                }
                Some(session) if session.is_expired(now) => {
                    self.store.delete(&id).await?;
                    self.index.lock().remove(&id);
                    removed += 1;
                }
                Some(_) => { /* still live */ }
            }
        }
        debug!(removed, "reaped expired sessions");
        Ok(removed)
    }

    /// Reload **every** session the backing store can enumerate, repopulating
    /// the in-memory id index.
    ///
    /// This is the swap-in half of Tomcat's `PersistentManager` behaviour: a
    /// manager calls it once at startup so a process restart does not lose
    /// sessions held by a durable store ([`FileSessionStore`], a JDBC store,
    /// …). It returns the number of sessions loaded into the index.
    ///
    /// Backends that cannot enumerate themselves (e.g. [`RedisSessionStore`],
    /// which expires keys via TTL) return
    /// [`tomcatrs_core::Error::Other`]`("load_all not supported …")` from
    /// [`SessionStore::load_all`]; `reload_all` treats that specific error as
    /// "nothing to reload" and returns `Ok(0)` rather than propagating it, so
    /// it is always safe to call regardless of the configured store.
    ///
    /// [`FileSessionStore`]: crate::FileSessionStore
    /// [`RedisSessionStore`]: crate::RedisSessionStore
    pub async fn reload_all(&self) -> tomcatrs_core::Result<usize> {
        let sessions = match self.store.load_all().await {
            Ok(sessions) => sessions,
            // The store does not support enumeration: nothing to reload.
            Err(tomcatrs_core::Error::Other(msg)) if msg.contains("load_all not supported") => {
                debug!("reload_all: store does not support enumeration; skipping");
                return Ok(0);
            }
            Err(e) => return Err(e),
        };

        let mut index = self.index.lock();
        for session in &sessions {
            index.insert(session.id.clone());
        }
        let count = sessions.len();
        drop(index);
        debug!(count, "reloaded sessions from store");
        Ok(count)
    }

    /// Persist **every** session currently tracked by this manager back to the
    /// store, returning the number persisted.
    ///
    /// This is the swap-out half of `PersistentManager`: call it on shutdown
    /// so in-memory state reaches a durable backend before the process exits.
    ///
    /// The manager has no generic way to enumerate live `SessionData`, so it
    /// walks its in-memory id index, (re)loads each session from the store and
    /// writes it straight back. For a [`MemorySessionStore`] this is a
    /// round-trip through the same map; the operation is most useful when the
    /// manager sits in front of a store whose writes are buffered or when an
    /// index entry needs its `last_accessed` flushed. Ids whose session has
    /// already vanished from the store are pruned from the index.
    ///
    /// Note: a more typical deployment persists by configuring the manager
    /// with a durable store from the start, in which case every `save` is
    /// already durable and `persist_all` is a no-op safety net.
    pub async fn persist_all(&self) -> tomcatrs_core::Result<usize> {
        let candidates: Vec<String> = self.index.lock().iter().cloned().collect();
        let mut persisted = 0usize;
        for id in candidates {
            match self.store.load(&id).await? {
                Some(session) => {
                    self.store.save(session).await?;
                    persisted += 1;
                }
                None => {
                    // Already gone: keep the index from carrying a dead id.
                    self.index.lock().remove(&id);
                }
            }
        }
        debug!(persisted, "persisted tracked sessions to store");
        Ok(persisted)
    }

    /// Drop index entries whose backing session is gone or expired. Used only
    /// by the memory fast path to keep the index tidy.
    async fn prune_index_against_store(&self, ids: &[String], now: SystemTime) {
        for id in ids {
            let stale = match self.store.load(id).await {
                Ok(None) => true,
                Ok(Some(session)) => session.is_expired(now),
                Err(_) => false,
            };
            if stale {
                self.index.lock().remove(id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::MemorySessionStore;

    #[tokio::test]
    async fn create_then_find() {
        let manager = SessionManager::new(Arc::new(MemorySessionStore::new()));
        let session = manager.create().await.unwrap();
        assert_eq!(session.id.len(), 32);

        let found = manager.find(&session.id).await.unwrap();
        assert_eq!(found.map(|s| s.id), Some(session.id.clone()));

        // Unknown id resolves to None.
        assert!(manager.find("does-not-exist").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn save_and_invalidate() {
        let manager = SessionManager::new(Arc::new(MemorySessionStore::new()));
        let mut session = manager.create().await.unwrap();
        session.attributes.insert("k".to_string(), "v".to_string());
        manager.save(session.clone()).await.unwrap();

        let reloaded = manager.find(&session.id).await.unwrap().unwrap();
        assert_eq!(reloaded.attributes.get("k").map(String::as_str), Some("v"));

        manager.invalidate(&session.id).await.unwrap();
        assert!(manager.find(&session.id).await.unwrap().is_none());
        // Idempotent.
        manager.invalidate(&session.id).await.unwrap();
    }

    #[tokio::test]
    async fn reap_expired_removes_stale_sessions() {
        let manager = SessionManager::new(Arc::new(MemorySessionStore::new()));

        // A live session.
        let live = manager.create().await.unwrap();

        // A session with a tiny inactive interval and a backdated last access.
        let mut dead = manager.create().await.unwrap();
        dead.max_inactive_interval = Duration::from_secs(1);
        dead.last_accessed = SystemTime::now() - Duration::from_secs(3600);
        manager.save(dead.clone()).await.unwrap();

        let removed = manager.reap_expired().await.unwrap();
        assert_eq!(removed, 1);
        assert!(manager.find(&live.id).await.unwrap().is_some());
        assert!(manager.find(&dead.id).await.unwrap().is_none());

        // Reaping again finds nothing new.
        assert_eq!(manager.reap_expired().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn reap_expired_generic_path_via_file_store() {
        // FileSessionStore is not a MemorySessionStore, so this exercises the
        // index-driven generic reap path.
        let dir = std::env::temp_dir().join(format!(
            "tomcatrs-session-mgr-reap-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = crate::FileSessionStore::new(&dir).unwrap();
        let manager = SessionManager::new(Arc::new(store));

        let mut dead = manager.create().await.unwrap();
        dead.max_inactive_interval = Duration::from_secs(1);
        dead.last_accessed = SystemTime::now() - Duration::from_secs(3600);
        manager.save(dead.clone()).await.unwrap();

        let live = manager.create().await.unwrap();

        let removed = manager.reap_expired().await.unwrap();
        assert_eq!(removed, 1);
        assert!(manager.find(&dead.id).await.unwrap().is_none());
        assert!(manager.find(&live.id).await.unwrap().is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reload_all_and_persist_all_round_trip_via_file_store() {
        let dir = std::env::temp_dir().join(format!(
            "tomcatrs-session-mgr-persist-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        // First manager: create three sessions on a file store, then "shut
        // down" by persisting everything it tracked.
        {
            let store = crate::FileSessionStore::new(&dir).unwrap();
            let manager = SessionManager::new(Arc::new(store));
            for _ in 0..3 {
                manager.create().await.unwrap();
            }
            assert_eq!(manager.tracked_ids(), 3);
            assert_eq!(manager.persist_all().await.unwrap(), 3);
        }

        // Second manager over the *same* directory: it starts with an empty
        // index, and reload_all repopulates it from disk.
        {
            let store = crate::FileSessionStore::new(&dir).unwrap();
            let manager = SessionManager::new(Arc::new(store));
            assert_eq!(manager.tracked_ids(), 0);
            let reloaded = manager.reload_all().await.unwrap();
            assert_eq!(reloaded, 3);
            assert_eq!(manager.tracked_ids(), 3);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reload_all_is_a_noop_for_stores_without_enumeration() {
        // A store that keeps the default `load_all` (returning "not
        // supported"): reload_all must treat that as "nothing to reload",
        // not propagate it as an error.
        struct NoEnumStore;

        #[async_trait::async_trait]
        impl SessionStore for NoEnumStore {
            async fn load(&self, _id: &str) -> tomcatrs_core::Result<Option<SessionData>> {
                Ok(None)
            }
            async fn save(&self, _session: SessionData) -> tomcatrs_core::Result<()> {
                Ok(())
            }
            async fn delete(&self, _id: &str) -> tomcatrs_core::Result<()> {
                Ok(())
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }

        let manager = SessionManager::new(Arc::new(NoEnumStore));
        assert_eq!(manager.reload_all().await.unwrap(), 0);
    }
}
