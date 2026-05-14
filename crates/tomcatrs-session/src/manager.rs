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
/// created or looked up ([`SessionManager::index`]). `reap_expired` walks that
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
}
