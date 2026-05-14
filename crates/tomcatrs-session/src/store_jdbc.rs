//! JDBC-backed session store, mirroring Tomcat's `JDBCStore`.
//!
//! # Why a trait boundary
//!
//! Tomcat's `JDBCStore` talks to a relational database through `java.sql`.
//! In Tomcat-RS the equivalent capability lives on the far side of the
//! JVM bridge — but `tomcatrs-session` deliberately does **not** depend on
//! the servlet-bridge crate: doing so would create a dependency cycle and
//! pull a heavyweight JNI dependency into a crate that is otherwise pure,
//! synchronous-free Rust.
//!
//! Instead, the database is modelled as a trait object. [`JdbcExecutor`] is a
//! minimal async "run SQL with typed parameters" interface;
//! [`JdbcSessionStore`] is generic over any `E: JdbcExecutor`. The real
//! executor — one that marshals calls across the JVM bridge to a JDBC
//! `DataSource` — is supplied by the catalina/bridge layer at assembly time.
//! This crate ships only [`NullJdbcExecutor`], a stub that always fails, so
//! the store type is constructible and unit-testable without a database.
//!
//! # Schema
//!
//! The store assumes a table shaped like Tomcat's default:
//!
//! ```sql
//! CREATE TABLE tomcatrs_sessions (
//!     session_id        VARCHAR(100) PRIMARY KEY,
//!     session_data      TEXT    NOT NULL,  -- JSON-encoded SessionData
//!     last_accessed     BIGINT  NOT NULL,  -- Unix-epoch millis
//!     max_inactive      BIGINT  NOT NULL   -- whole seconds
//! );
//! ```
//!
//! The session id, last-accessed time, and max-inactive interval are stored
//! as their own columns (so a DBA can reason about them and a reaper can
//! query them) in addition to the full JSON blob that `load` deserialises.

use std::time::UNIX_EPOCH;

use async_trait::async_trait;
use tomcatrs_core::Error;

use crate::{SessionData, SessionStore};

/// The table the [`JdbcSessionStore`] reads and writes.
const TABLE: &str = "tomcatrs_sessions";

/// A bound SQL parameter passed to a [`JdbcExecutor`].
///
/// The variant set is intentionally tiny — exactly what the session store
/// needs. A real bridge executor maps each variant onto the corresponding
/// `PreparedStatement.setXxx` call.
#[derive(Debug, Clone, PartialEq)]
pub enum JdbcParam {
    /// A textual parameter (`setString`).
    Text(String),
    /// A 64-bit integer parameter (`setLong`).
    Long(i64),
}

impl From<&str> for JdbcParam {
    fn from(s: &str) -> Self {
        JdbcParam::Text(s.to_string())
    }
}

impl From<String> for JdbcParam {
    fn from(s: String) -> Self {
        JdbcParam::Text(s)
    }
}

impl From<i64> for JdbcParam {
    fn from(v: i64) -> Self {
        JdbcParam::Long(v)
    }
}

/// One row returned by [`JdbcExecutor::query`].
///
/// Columns are addressed positionally, in `SELECT`-list order. Each cell is
/// the column's textual rendering (the bridge is free to stringify numerics);
/// the session store only ever reads a JSON `session_data` column, so a single
/// accessor suffices.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JdbcRow {
    /// The column values, in `SELECT`-list order.
    pub columns: Vec<String>,
}

impl JdbcRow {
    /// Build a row from an iterator of column values.
    pub fn new(columns: impl IntoIterator<Item = String>) -> Self {
        Self {
            columns: columns.into_iter().collect(),
        }
    }

    /// Borrow the column at `index`, or `None` if the row is shorter.
    pub fn get(&self, index: usize) -> Option<&str> {
        self.columns.get(index).map(String::as_str)
    }
}

/// An async, object-safe "run SQL" boundary used by [`JdbcSessionStore`].
///
/// Implementors marshal the call to a real database. Two methods cover every
/// operation the session store needs:
///
/// * [`execute`](JdbcExecutor::execute) — run an `INSERT`/`UPDATE`/`DELETE`
///   (or DDL) and report the affected-row count.
/// * [`query`](JdbcExecutor::query) — run a `SELECT` and return its rows.
///
/// Both take the SQL text plus a slice of positional [`JdbcParam`]s, exactly
/// as a JDBC `PreparedStatement` would. Failures must map to
/// [`tomcatrs_core::Error`] — typically [`Error::Bridge`] for an executor that
/// crosses the JVM boundary.
#[async_trait]
pub trait JdbcExecutor: Send + Sync {
    /// Execute a statement that does not return rows, yielding the number of
    /// rows affected.
    async fn execute(&self, sql: &str, params: &[JdbcParam]) -> tomcatrs_core::Result<u64>;

    /// Execute a `SELECT` and return every matching row.
    async fn query(&self, sql: &str, params: &[JdbcParam]) -> tomcatrs_core::Result<Vec<JdbcRow>>;
}

/// A [`JdbcExecutor`] that is not wired to any database.
///
/// Every call returns [`Error::Other`] with the message
/// `"JDBC executor not configured"`. This is the default executor: it lets
/// [`JdbcSessionStore`] be constructed and exercised in unit tests, and gives
/// a clear, actionable error in a deployment where the catalina/bridge layer
/// forgot to install a real executor.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullJdbcExecutor;

impl NullJdbcExecutor {
    /// Create the stub executor.
    pub fn new() -> Self {
        Self
    }

    /// The error every operation on this executor produces.
    fn not_configured() -> Error {
        Error::Other("JDBC executor not configured".to_string())
    }
}

#[async_trait]
impl JdbcExecutor for NullJdbcExecutor {
    async fn execute(&self, _sql: &str, _params: &[JdbcParam]) -> tomcatrs_core::Result<u64> {
        Err(Self::not_configured())
    }

    async fn query(
        &self,
        _sql: &str,
        _params: &[JdbcParam],
    ) -> tomcatrs_core::Result<Vec<JdbcRow>> {
        Err(Self::not_configured())
    }
}

/// A [`SessionStore`] that persists sessions to a relational database through
/// a [`JdbcExecutor`].
///
/// Construct it with a real executor supplied by the catalina/bridge layer, or
/// with [`NullJdbcExecutor`] for tests:
///
/// ```
/// use tomcatrs_session::{JdbcSessionStore, NullJdbcExecutor};
///
/// let store = JdbcSessionStore::new(NullJdbcExecutor::new());
/// // `store` is a fully-typed `SessionStore`; every call will report
/// // "JDBC executor not configured" until a real executor is installed.
/// let _ = store;
/// ```
#[derive(Debug, Clone)]
pub struct JdbcSessionStore<E: JdbcExecutor> {
    executor: E,
}

impl<E: JdbcExecutor> JdbcSessionStore<E> {
    /// Build a store over the given executor.
    pub fn new(executor: E) -> Self {
        Self { executor }
    }

    /// Borrow the underlying executor.
    pub fn executor(&self) -> &E {
        &self.executor
    }

    /// Consume the store, returning the executor.
    pub fn into_executor(self) -> E {
        self.executor
    }
}

#[async_trait]
impl<E: JdbcExecutor + 'static> SessionStore for JdbcSessionStore<E> {
    async fn load(&self, id: &str) -> tomcatrs_core::Result<Option<SessionData>> {
        let sql = format!("SELECT session_data FROM {TABLE} WHERE session_id = ?");
        let rows = self.executor.query(&sql, &[id.into()]).await?;
        match rows.into_iter().next() {
            None => Ok(None),
            Some(row) => {
                let json = row.get(0).ok_or_else(|| {
                    Error::Other(format!(
                        "JDBC row for session {id} has no session_data column"
                    ))
                })?;
                let session = serde_json::from_str(json)
                    .map_err(|e| Error::Other(format!("corrupt JDBC session {id}: {e}")))?;
                Ok(Some(session))
            }
        }
    }

    async fn save(&self, session: SessionData) -> tomcatrs_core::Result<()> {
        let json = serde_json::to_string(&session)
            .map_err(|e| Error::Other(format!("failed to serialise session: {e}")))?;
        let last_accessed = session
            .last_accessed
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let max_inactive = session.max_inactive_interval.as_secs() as i64;

        // UPSERT: try an UPDATE first, fall back to INSERT when no row matched.
        // This keeps the store portable across databases without relying on a
        // vendor-specific `MERGE` / `ON CONFLICT` clause.
        let update_sql = format!(
            "UPDATE {TABLE} SET session_data = ?, last_accessed = ?, max_inactive = ? \
             WHERE session_id = ?"
        );
        let updated = self
            .executor
            .execute(
                &update_sql,
                &[
                    json.clone().into(),
                    last_accessed.into(),
                    max_inactive.into(),
                    session.id.clone().into(),
                ],
            )
            .await?;

        if updated == 0 {
            let insert_sql = format!(
                "INSERT INTO {TABLE} \
                 (session_id, session_data, last_accessed, max_inactive) \
                 VALUES (?, ?, ?, ?)"
            );
            self.executor
                .execute(
                    &insert_sql,
                    &[
                        session.id.clone().into(),
                        json.into(),
                        last_accessed.into(),
                        max_inactive.into(),
                    ],
                )
                .await?;
        }
        Ok(())
    }

    async fn delete(&self, id: &str) -> tomcatrs_core::Result<()> {
        let sql = format!("DELETE FROM {TABLE} WHERE session_id = ?");
        // A delete that matches no rows is not an error, mirroring every other
        // backend.
        self.executor.execute(&sql, &[id.into()]).await?;
        Ok(())
    }

    /// Load every persisted session with a single `SELECT`.
    ///
    /// Unlike a Redis key scan, a full-table `SELECT` is a perfectly ordinary
    /// database operation, so the JDBC backend supports the optional bulk-load
    /// path.
    async fn load_all(&self) -> tomcatrs_core::Result<Vec<SessionData>> {
        let sql = format!("SELECT session_data FROM {TABLE}");
        let rows = self.executor.query(&sql, &[]).await?;
        let mut sessions = Vec::with_capacity(rows.len());
        for row in rows {
            let json = row
                .get(0)
                .ok_or_else(|| Error::Other("JDBC row has no session_data column".to_string()))?;
            let session = serde_json::from_str(json)
                .map_err(|e| Error::Other(format!("corrupt JDBC session row: {e}")))?;
            sessions.push(session);
        }
        Ok(sessions)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[tokio::test]
    async fn null_executor_reports_not_configured() {
        let store = JdbcSessionStore::new(NullJdbcExecutor::new());

        let load_err = store.load("ABC").await.unwrap_err();
        assert!(load_err
            .to_string()
            .contains("JDBC executor not configured"));

        let save_err = store
            .save(SessionData::new("ABC".to_string()))
            .await
            .unwrap_err();
        assert!(save_err
            .to_string()
            .contains("JDBC executor not configured"));

        let delete_err = store.delete("ABC").await.unwrap_err();
        assert!(delete_err
            .to_string()
            .contains("JDBC executor not configured"));

        let load_all_err = store.load_all().await.unwrap_err();
        assert!(load_all_err
            .to_string()
            .contains("JDBC executor not configured"));
    }

    /// A tiny in-memory fake executor that records the SQL it was handed and
    /// serves canned rows, used to exercise the store's SQL/round-trip logic
    /// without a database.
    #[derive(Default)]
    struct FakeExecutor {
        /// `(sql, params)` of every `execute` call.
        executed: Mutex<Vec<(String, Vec<JdbcParam>)>>,
        /// Canned rows returned by `query`.
        rows: Mutex<Vec<JdbcRow>>,
        /// Affected-row count returned by `execute`.
        affected: Mutex<u64>,
    }

    #[async_trait]
    impl JdbcExecutor for FakeExecutor {
        async fn execute(&self, sql: &str, params: &[JdbcParam]) -> tomcatrs_core::Result<u64> {
            self.executed
                .lock()
                .unwrap()
                .push((sql.to_string(), params.to_vec()));
            Ok(*self.affected.lock().unwrap())
        }

        async fn query(
            &self,
            _sql: &str,
            _params: &[JdbcParam],
        ) -> tomcatrs_core::Result<Vec<JdbcRow>> {
            Ok(self.rows.lock().unwrap().clone())
        }
    }

    #[tokio::test]
    async fn save_inserts_when_update_matches_no_rows() {
        let store = JdbcSessionStore::new(FakeExecutor::default()); // affected = 0
        store
            .save(SessionData::new("SID1".to_string()))
            .await
            .unwrap();
        let executed = store.executor().executed.lock().unwrap();
        // First an UPDATE (0 rows), then a fallback INSERT.
        assert_eq!(executed.len(), 2);
        assert!(executed[0].0.starts_with("UPDATE"));
        assert!(executed[1].0.starts_with("INSERT"));
    }

    #[tokio::test]
    async fn save_skips_insert_when_update_hits() {
        let store = JdbcSessionStore::new(FakeExecutor::default());
        *store.executor().affected.lock().unwrap() = 1;
        store
            .save(SessionData::new("SID1".to_string()))
            .await
            .unwrap();
        let executed = store.executor().executed.lock().unwrap();
        // Only the UPDATE runs.
        assert_eq!(executed.len(), 1);
        assert!(executed[0].0.starts_with("UPDATE"));
    }

    #[tokio::test]
    async fn load_deserialises_session_data_column() {
        let store = JdbcSessionStore::new(FakeExecutor::default());
        let session = SessionData::new("SID1".to_string());
        let json = serde_json::to_string(&session).unwrap();
        store
            .executor()
            .rows
            .lock()
            .unwrap()
            .push(JdbcRow::new([json]));

        let loaded = store.load("SID1").await.unwrap().unwrap();
        assert_eq!(loaded.id, "SID1");

        // No rows -> None.
        store.executor().rows.lock().unwrap().clear();
        assert!(store.load("SID1").await.unwrap().is_none());
    }
}
