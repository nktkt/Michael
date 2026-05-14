//! Filesystem-backed session store: one JSON document per session.

use std::path::{Path, PathBuf};

use tomcatrs_core::Error;

use crate::{SessionData, SessionStore};

/// A [`SessionStore`] that persists each session as a JSON file inside a
/// configurable directory.
///
/// This mirrors Tomcat's `FileStore`: it survives process restarts and needs
/// no external service, trading raw throughput for durability. Each session is
/// written to `<dir>/<id>.json`.
///
/// Session ids are validated before being used as file names (see
/// [`is_safe_id`]) so a hostile id can never escape the configured directory.
#[derive(Debug, Clone)]
pub struct FileSessionStore {
    dir: PathBuf,
}

impl FileSessionStore {
    /// Create a store rooted at `dir`, creating the directory (and any missing
    /// parents) if it does not already exist.
    pub fn new(dir: impl Into<PathBuf>) -> tomcatrs_core::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// The directory under which session files are stored.
    pub fn directory(&self) -> &Path {
        &self.dir
    }

    /// Resolve the on-disk path for a session id, rejecting unsafe ids.
    fn path_for(&self, id: &str) -> tomcatrs_core::Result<PathBuf> {
        if !is_safe_id(id) {
            return Err(Error::Other(format!(
                "refusing to use unsafe session id as a file name: {id:?}"
            )));
        }
        Ok(self.dir.join(format!("{id}.json")))
    }
}

/// Return `true` if `id` is safe to embed verbatim in a file name: non-empty
/// and composed solely of ASCII alphanumerics, `-` or `_`.
///
/// Ids produced by [`crate::generate_session_id`] (uppercase hex) always
/// satisfy this; the check exists to defend against ids that originate from
/// untrusted request cookies.
pub fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[async_trait::async_trait]
impl SessionStore for FileSessionStore {
    async fn load(&self, id: &str) -> tomcatrs_core::Result<Option<SessionData>> {
        let path = self.path_for(id)?;
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::Io(e)),
        };
        let session: SessionData = serde_json::from_slice(&bytes)
            .map_err(|e| Error::Other(format!("corrupt session file {path:?}: {e}")))?;
        Ok(Some(session))
    }

    async fn save(&self, session: SessionData) -> tomcatrs_core::Result<()> {
        let path = self.path_for(&session.id)?;
        let json = serde_json::to_vec_pretty(&session)
            .map_err(|e| Error::Other(format!("failed to serialise session: {e}")))?;
        tokio::fs::write(&path, json).await.map_err(Error::Io)
    }

    async fn delete(&self, id: &str) -> tomcatrs_core::Result<()> {
        let path = self.path_for(id)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            // Deleting a missing session is not an error.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;

    /// Build a unique temp directory path without pulling in an extra crate.
    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("tomcatrs-session-{tag}-{nanos}"))
    }

    #[tokio::test]
    async fn file_store_round_trip() {
        let dir = temp_dir("roundtrip");
        let store = FileSessionStore::new(&dir).unwrap();

        let mut session = SessionData::new("FILEABC123".to_string());
        session
            .attributes
            .insert("theme".to_string(), "dark".to_string());
        session.max_inactive_interval = Duration::from_secs(1234);

        // Missing session loads as None.
        assert_eq!(store.load("FILEABC123").await.unwrap(), None);

        store.save(session.clone()).await.unwrap();
        let loaded = store.load("FILEABC123").await.unwrap().unwrap();
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.attributes, session.attributes);
        assert_eq!(loaded.max_inactive_interval, session.max_inactive_interval);

        store.delete("FILEABC123").await.unwrap();
        assert_eq!(store.load("FILEABC123").await.unwrap(), None);
        // Idempotent delete.
        store.delete("FILEABC123").await.unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unsafe_ids_are_rejected() {
        let dir = temp_dir("unsafe");
        let store = FileSessionStore::new(&dir).unwrap();
        assert!(store.load("../../etc/passwd").await.is_err());
        assert!(store.delete("with/slash").await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn id_safety_check() {
        assert!(is_safe_id("ABC123DEF456"));
        assert!(is_safe_id("a-b_c"));
        assert!(!is_safe_id(""));
        assert!(!is_safe_id("../escape"));
        assert!(!is_safe_id("has space"));
    }
}
