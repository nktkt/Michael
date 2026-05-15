//! Clustered session store, mirroring Tomcat's session replication.
//!
//! Tomcat ships two cluster managers:
//!
//! * **`DeltaManager`** — *all-to-all* replication: every node holds a copy of
//!   every session. Writes are broadcast to the whole cluster.
//! * **`BackupManager`** — *primary-backup* replication: each session lives on
//!   exactly one primary node plus one backup node. Writes go to the backup;
//!   a node that does not have a session asks its peers for it.
//!
//! [`ClusterSessionStore`] implements both, selected by [`ReplicationMode`].
//! It wraps a local [`MemorySessionStore`] for the node's own copy and a
//! [`ClusterTransport`] for talking to peers.
//!
//! # Transport abstraction
//!
//! Real Tomcat clustering rides on the Tribes group-communication stack. Here
//! the network is abstracted behind the [`ClusterTransport`] trait
//! (`broadcast` / `send_to` / `receive`), so the replication logic is
//! transport-agnostic and fully testable. [`InMemoryClusterTransport`] is a
//! pure-Rust, [`tokio::sync`]-channel implementation used both by the test
//! suite and by single-process multi-"node" setups; a production deployment
//! would supply a TCP/Tribes-style transport instead.
//!
//! No cargo feature gates this module — it depends only on `tokio` channels,
//! which are always available.
//!
//! # Receiving replicated writes
//!
//! [`ClusterTransport::receive`] yields [`ClusterMessage`]s sent by peers. A
//! deployment is expected to run a small loop — see
//! [`ClusterSessionStore::apply_incoming`] and
//! [`ClusterSessionStore::run_receiver`] — that feeds each incoming message
//! back into the local store. The test suite drives this explicitly so the
//! replication behaviour is deterministic.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, trace, warn};

use crate::{MemorySessionStore, SessionData, SessionStore};

/// Which replication strategy a [`ClusterSessionStore`] uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicationMode {
    /// *All-to-all* (Tomcat's `DeltaManager`): every node holds every session.
    /// `save`/`delete` are broadcast to the entire cluster.
    AllToAll,
    /// *Primary-backup* (Tomcat's `BackupManager`): the writing node is the
    /// primary and keeps a local copy; the write is additionally sent to a
    /// single backup peer. A node missing a session fetches it from peers on
    /// `load`.
    PrimaryBackup,
}

/// A message exchanged between cluster nodes.
///
/// The transport is responsible only for delivery; interpreting these is the
/// job of [`ClusterSessionStore::apply_incoming`] (for writes) and
/// [`ClusterSessionStore::load`] (for the request/response fetch pair).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClusterMessage {
    /// A session was created or updated on a peer; the recipient should store
    /// it locally.
    Save(SessionData),
    /// A session was invalidated on a peer; the recipient should drop it.
    Delete(String),
    /// A peer is asking whoever has session `id` to reply with it. The
    /// `reply_to` names the requesting node so a holder can `send_to` it.
    FetchRequest {
        /// The session id being requested.
        id: String,
        /// The node id to send the [`ClusterMessage::FetchResponse`] back to.
        reply_to: String,
    },
    /// A reply to a [`ClusterMessage::FetchRequest`]: `Some` if the responding
    /// node had the session, `None` if it did not.
    FetchResponse {
        /// The session id that was requested.
        id: String,
        /// The session, if the responding peer held it.
        session: Option<SessionData>,
    },
}

/// An async, object-safe cluster transport.
///
/// An implementation moves [`ClusterMessage`]s between nodes. Each node owns
/// one transport endpoint, identified by [`node_id`](ClusterTransport::node_id).
///
/// * [`broadcast`](ClusterTransport::broadcast) — deliver to every *other*
///   node.
/// * [`send_to`](ClusterTransport::send_to) — deliver to one named node.
/// * [`receive`](ClusterTransport::receive) — take the next message addressed
///   to this node, or `None` once every peer endpoint has been dropped.
#[async_trait]
pub trait ClusterTransport: Send + Sync {
    /// This endpoint's node id.
    fn node_id(&self) -> &str;

    /// Send `msg` to every other node in the cluster.
    async fn broadcast(&self, msg: ClusterMessage) -> tomcatrs_core::Result<()>;

    /// Send `msg` to the single node named `node_id`.
    async fn send_to(&self, node_id: &str, msg: ClusterMessage) -> tomcatrs_core::Result<()>;

    /// Await the next message addressed to this node.
    ///
    /// Returns `Ok(None)` when the cluster has shut down (all senders gone),
    /// which a receive loop should treat as "stop".
    async fn receive(&self) -> tomcatrs_core::Result<Option<ClusterMessage>>;
}

/// A pure-Rust [`ClusterTransport`] built on [`tokio::sync::mpsc`] channels.
///
/// Endpoints are minted together by [`InMemoryClusterTransport::cluster`] (or
/// added later with [`InMemoryClusterTransport::join`]) so that every endpoint
/// knows how to reach every other. It is intended for tests and for
/// single-process multi-node simulations; it does not cross process or machine
/// boundaries.
pub struct InMemoryClusterTransport {
    node_id: String,
    /// Senders to every node in the cluster, *including* this one — the local
    /// entry is simply never used by `broadcast`.
    peers: Arc<Mutex<Vec<(String, mpsc::UnboundedSender<ClusterMessage>)>>>,
    /// This node's own inbound queue.
    inbox: Mutex<mpsc::UnboundedReceiver<ClusterMessage>>,
}

impl InMemoryClusterTransport {
    /// Build a fully-connected cluster of `node_ids`, returning one transport
    /// endpoint per id, in the same order.
    ///
    /// # Panics
    ///
    /// Panics if `node_ids` is empty or contains duplicates — a malformed
    /// cluster definition is a programming error, not a runtime condition.
    pub fn cluster<I, S>(node_ids: I) -> Vec<InMemoryClusterTransport>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let ids: Vec<String> = node_ids.into_iter().map(Into::into).collect();
        assert!(!ids.is_empty(), "a cluster needs at least one node");
        {
            let mut seen = std::collections::HashSet::new();
            for id in &ids {
                assert!(seen.insert(id.clone()), "duplicate cluster node id: {id}");
            }
        }

        // One inbound channel per node.
        let mut receivers = Vec::with_capacity(ids.len());
        let mut senders: Vec<(String, mpsc::UnboundedSender<ClusterMessage>)> =
            Vec::with_capacity(ids.len());
        for id in &ids {
            let (tx, rx) = mpsc::unbounded_channel();
            senders.push((id.clone(), tx));
            receivers.push(rx);
        }
        let shared = Arc::new(Mutex::new(senders));

        ids.into_iter()
            .zip(receivers)
            .map(|(node_id, rx)| InMemoryClusterTransport {
                node_id,
                peers: Arc::clone(&shared),
                inbox: Mutex::new(rx),
            })
            .collect()
    }

    /// Add a new node to an existing cluster and return its endpoint.
    ///
    /// The new endpoint shares the same peer registry, so existing nodes can
    /// immediately reach it and vice versa.
    ///
    /// # Panics
    ///
    /// Panics if `node_id` is already present in the cluster.
    pub async fn join(&self, node_id: impl Into<String>) -> InMemoryClusterTransport {
        let node_id = node_id.into();
        let (tx, rx) = mpsc::unbounded_channel();
        {
            let mut peers = self.peers.lock().await;
            assert!(
                !peers.iter().any(|(id, _)| id == &node_id),
                "duplicate cluster node id: {node_id}"
            );
            peers.push((node_id.clone(), tx));
        }
        InMemoryClusterTransport {
            node_id,
            peers: Arc::clone(&self.peers),
            inbox: Mutex::new(rx),
        }
    }
}

#[async_trait]
impl ClusterTransport for InMemoryClusterTransport {
    fn node_id(&self) -> &str {
        &self.node_id
    }

    async fn broadcast(&self, msg: ClusterMessage) -> tomcatrs_core::Result<()> {
        let peers = self.peers.lock().await;
        for (id, tx) in peers.iter() {
            if id == &self.node_id {
                continue;
            }
            // A closed peer channel means that node has shut down; that is not
            // fatal to the rest of the cluster, so log and carry on.
            if tx.send(msg.clone()).is_err() {
                warn!(peer = %id, "cluster peer unreachable during broadcast");
            }
        }
        Ok(())
    }

    async fn send_to(&self, node_id: &str, msg: ClusterMessage) -> tomcatrs_core::Result<()> {
        let peers = self.peers.lock().await;
        match peers.iter().find(|(id, _)| id == node_id) {
            Some((_, tx)) => tx.send(msg).map_err(|_| {
                tomcatrs_core::Error::Other(format!("cluster peer {node_id} unreachable"))
            }),
            None => Err(tomcatrs_core::Error::Other(format!(
                "unknown cluster node: {node_id}"
            ))),
        }
    }

    async fn receive(&self) -> tomcatrs_core::Result<Option<ClusterMessage>> {
        Ok(self.inbox.lock().await.recv().await)
    }
}

/// A [`SessionStore`] that replicates writes to peer nodes per a
/// [`ReplicationMode`].
///
/// A `ClusterSessionStore` is one cluster node. It keeps the node's own copy
/// of sessions in an inner [`MemorySessionStore`] and uses a
/// [`ClusterTransport`] to push writes to — and pull missing sessions from —
/// its peers.
///
/// # Replication on write
///
/// * [`ReplicationMode::AllToAll`]: `save` and `delete` are broadcast to every
///   peer, so each node converges to holding every session.
/// * [`ReplicationMode::PrimaryBackup`]: `save`/`delete` are sent to a single
///   configured backup peer ([`with_backup`](ClusterSessionStore::with_backup)).
///   If no backup is configured the write stays local — useful for the last
///   node standing or for tests.
///
/// # Replication on read
///
/// [`load`](ClusterSessionStore::load) first checks the local store. On a miss
/// it broadcasts a [`ClusterMessage::FetchRequest`] and waits (briefly) for a
/// [`ClusterMessage::FetchResponse`]; a peer that holds the session replies
/// with it and the local node caches the result. The fetch wait is bounded by
/// a timeout so a `load` cannot hang if no peer answers.
///
/// # Applying peer writes
///
/// Incoming [`ClusterMessage`]s must be fed back in via
/// [`apply_incoming`](ClusterSessionStore::apply_incoming). Spawn
/// [`run_receiver`](ClusterSessionStore::run_receiver) on an `Arc<Self>` to do
/// this automatically for the lifetime of the node.
pub struct ClusterSessionStore {
    local: MemorySessionStore,
    transport: Arc<dyn ClusterTransport>,
    mode: ReplicationMode,
    /// The backup peer for [`ReplicationMode::PrimaryBackup`], if configured.
    backup: Option<String>,
    /// How long [`load`](Self::load) waits for a peer to answer a fetch.
    fetch_timeout: std::time::Duration,
}

impl ClusterSessionStore {
    /// Default time [`load`](Self::load) waits for a peer fetch response.
    pub const DEFAULT_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

    /// Build a cluster node over `transport`, replicating per `mode`.
    pub fn new(transport: Arc<dyn ClusterTransport>, mode: ReplicationMode) -> Self {
        Self {
            local: MemorySessionStore::new(),
            transport,
            mode,
            backup: None,
            fetch_timeout: Self::DEFAULT_FETCH_TIMEOUT,
        }
    }

    /// Set the backup peer used by [`ReplicationMode::PrimaryBackup`] writes.
    ///
    /// Ignored (but harmless) under [`ReplicationMode::AllToAll`].
    pub fn with_backup(mut self, node_id: impl Into<String>) -> Self {
        self.backup = Some(node_id.into());
        self
    }

    /// Override how long [`load`](Self::load) waits for a peer fetch response.
    pub fn with_fetch_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.fetch_timeout = timeout;
        self
    }

    /// This node's id, as reported by its transport.
    pub fn node_id(&self) -> &str {
        self.transport.node_id()
    }

    /// The replication mode in effect.
    pub fn mode(&self) -> ReplicationMode {
        self.mode
    }

    /// Borrow the node's local in-memory store (its own copy of the data).
    pub fn local(&self) -> &MemorySessionStore {
        &self.local
    }

    /// Apply a [`ClusterMessage`] received from a peer to this node.
    ///
    /// * [`ClusterMessage::Save`] / [`ClusterMessage::Delete`] update the local
    ///   store. They are **not** re-replicated — doing so would loop the
    ///   cluster forever.
    /// * [`ClusterMessage::FetchRequest`] is answered with a
    ///   [`ClusterMessage::FetchResponse`] sent straight back to the requester.
    /// * [`ClusterMessage::FetchResponse`] is ignored here: it is consumed
    ///   inline by the [`load`](Self::load) that issued the request. (A stray
    ///   response — e.g. a second peer answering after the first — is simply
    ///   dropped.)
    pub async fn apply_incoming(&self, msg: ClusterMessage) -> tomcatrs_core::Result<()> {
        match msg {
            ClusterMessage::Save(session) => {
                trace!(node = %self.node_id(), session.id = %session.id,
                    "applying replicated save");
                self.local.save(session).await
            }
            ClusterMessage::Delete(id) => {
                trace!(node = %self.node_id(), session.id = %id,
                    "applying replicated delete");
                self.local.delete(&id).await
            }
            ClusterMessage::FetchRequest { id, reply_to } => {
                let session = self.local.load(&id).await?;
                trace!(node = %self.node_id(), session.id = %id, requester = %reply_to,
                    found = session.is_some(), "answering fetch request");
                self.transport
                    .send_to(&reply_to, ClusterMessage::FetchResponse { id, session })
                    .await
            }
            ClusterMessage::FetchResponse { .. } => Ok(()),
        }
    }

    /// Run a receive loop that applies every inbound peer message until the
    /// transport shuts down.
    ///
    /// Intended to be `tokio::spawn`ed on an `Arc<ClusterSessionStore>` for the
    /// lifetime of the node:
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use tomcatrs_session::{ClusterSessionStore, ClusterTransport};
    /// # async fn demo(store: Arc<ClusterSessionStore>) {
    /// tokio::spawn(ClusterSessionStore::run_receiver(Arc::clone(&store)));
    /// # }
    /// ```
    pub async fn run_receiver(self: Arc<Self>) {
        loop {
            match self.transport.receive().await {
                Ok(Some(msg)) => {
                    if let Err(e) = self.apply_incoming(msg).await {
                        warn!(node = %self.node_id(), error = %e,
                            "failed to apply replicated message");
                    }
                }
                Ok(None) => {
                    debug!(node = %self.node_id(), "cluster transport closed; receiver stopping");
                    break;
                }
                Err(e) => {
                    warn!(node = %self.node_id(), error = %e, "cluster receive error");
                    break;
                }
            }
        }
    }

    /// Replicate a write to peers according to the active [`ReplicationMode`].
    async fn replicate(&self, msg: ClusterMessage) -> tomcatrs_core::Result<()> {
        match self.mode {
            ReplicationMode::AllToAll => self.transport.broadcast(msg).await,
            ReplicationMode::PrimaryBackup => match &self.backup {
                Some(backup) => self.transport.send_to(backup, msg).await,
                // No backup configured: the local copy is the only copy.
                None => Ok(()),
            },
        }
    }
}

#[async_trait]
impl SessionStore for ClusterSessionStore {
    async fn load(&self, id: &str) -> tomcatrs_core::Result<Option<SessionData>> {
        // Local copy wins.
        if let Some(session) = self.local.load(id).await? {
            return Ok(Some(session));
        }

        // Miss: ask the cluster. Under all-to-all a miss usually means the
        // session genuinely does not exist, but issuing the fetch anyway keeps
        // both modes on one code path and recovers a node that joined late.
        trace!(node = %self.node_id(), session.id = %id, "local miss; fetching from peers");
        self.transport
            .broadcast(ClusterMessage::FetchRequest {
                id: id.to_string(),
                reply_to: self.node_id().to_string(),
            })
            .await?;

        // Wait for a matching FetchResponse, bounded by `fetch_timeout`.
        let deadline = tokio::time::Instant::now() + self.fetch_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                trace!(node = %self.node_id(), session.id = %id,
                    "peer fetch timed out");
                return Ok(None);
            }
            match tokio::time::timeout(remaining, self.transport.receive()).await {
                // Transport closed.
                Ok(Ok(None)) => return Ok(None),
                // Timed out waiting for any message.
                Err(_) => return Ok(None),
                Ok(Err(e)) => return Err(e),
                Ok(Ok(Some(msg))) => match msg {
                    ClusterMessage::FetchResponse {
                        id: resp_id,
                        session,
                    } if resp_id == id => {
                        if let Some(ref s) = session {
                            // Cache the fetched session locally.
                            self.local.save(s.clone()).await?;
                        }
                        return Ok(session);
                    }
                    // Some other message arrived while we were waiting — apply
                    // it so it is not lost, then keep waiting for our response.
                    other => self.apply_incoming(other).await?,
                },
            }
        }
    }

    async fn save(&self, session: SessionData) -> tomcatrs_core::Result<()> {
        // Local copy first, then replicate.
        self.local.save(session.clone()).await?;
        self.replicate(ClusterMessage::Save(session)).await
    }

    async fn delete(&self, id: &str) -> tomcatrs_core::Result<()> {
        self.local.delete(id).await?;
        self.replicate(ClusterMessage::Delete(id.to_string())).await
    }

    /// Enumerate the sessions this node holds locally.
    ///
    /// Under [`ReplicationMode::AllToAll`] that is (eventually) the whole
    /// cluster's session set; under [`ReplicationMode::PrimaryBackup`] it is
    /// the sessions for which this node is a primary or backup. No peer fan-out
    /// is performed.
    async fn load_all(&self) -> tomcatrs_core::Result<Vec<SessionData>> {
        self.local.load_all().await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drain every message currently queued for `store` and apply it. Returns
    /// once the inbox is momentarily empty.
    async fn drain(store: &ClusterSessionStore) {
        loop {
            match tokio::time::timeout(
                std::time::Duration::from_millis(50),
                store.transport.receive(),
            )
            .await
            {
                Ok(Ok(Some(msg))) => store.apply_incoming(msg).await.unwrap(),
                _ => break,
            }
        }
    }

    #[tokio::test]
    async fn all_to_all_replicates_save_to_every_node() {
        let mut endpoints = InMemoryClusterTransport::cluster(["A", "B", "C"]);
        let tc = endpoints.pop().unwrap();
        let tb = endpoints.pop().unwrap();
        let ta = endpoints.pop().unwrap();

        let a = ClusterSessionStore::new(Arc::new(ta), ReplicationMode::AllToAll);
        let b = ClusterSessionStore::new(Arc::new(tb), ReplicationMode::AllToAll);
        let c = ClusterSessionStore::new(Arc::new(tc), ReplicationMode::AllToAll);

        // Save on node A.
        let mut session = SessionData::new("CLUSTER1".to_string());
        session.attributes.insert("k".to_string(), "v".to_string());
        a.save(session.clone()).await.unwrap();

        // B and C receive the broadcast and apply it.
        drain(&b).await;
        drain(&c).await;

        let on_b = b.load("CLUSTER1").await.unwrap().unwrap();
        let on_c = c.load("CLUSTER1").await.unwrap().unwrap();
        assert_eq!(on_b.id, "CLUSTER1");
        assert_eq!(on_b.attributes.get("k").map(String::as_str), Some("v"));
        assert_eq!(on_c.id, "CLUSTER1");

        // A delete on B propagates to A and C as well.
        b.delete("CLUSTER1").await.unwrap();
        drain(&a).await;
        drain(&c).await;
        assert!(a.local().load("CLUSTER1").await.unwrap().is_none());
        assert!(c.local().load("CLUSTER1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn primary_backup_sends_to_backup_and_third_node_fetches() {
        let mut endpoints = InMemoryClusterTransport::cluster(["A", "B", "C"]);
        let tc = endpoints.pop().unwrap();
        let tb = endpoints.pop().unwrap();
        let ta = endpoints.pop().unwrap();

        // A is primary, B is its backup.
        let a =
            ClusterSessionStore::new(Arc::new(ta), ReplicationMode::PrimaryBackup).with_backup("B");
        let b = ClusterSessionStore::new(Arc::new(tb), ReplicationMode::PrimaryBackup);
        let c = Arc::new(
            ClusterSessionStore::new(Arc::new(tc), ReplicationMode::PrimaryBackup)
                .with_fetch_timeout(std::time::Duration::from_secs(1)),
        );

        // Save on A: local copy on A, replicated copy on backup B, nothing on C.
        let session = SessionData::new("PB1".to_string());
        a.save(session.clone()).await.unwrap();
        drain(&b).await;

        assert!(a.local().load("PB1").await.unwrap().is_some());
        assert!(b.local().load("PB1").await.unwrap().is_some());
        assert!(c.local().load("PB1").await.unwrap().is_none());

        // C does not have PB1 locally. A `load` on C broadcasts a fetch; run a
        // receiver on A and B so a holder answers, and C picks up the response.
        let a = Arc::new(a);
        let b = Arc::new(b);
        let ra = tokio::spawn(ClusterSessionStore::run_receiver(Arc::clone(&a)));
        let rb = tokio::spawn(ClusterSessionStore::run_receiver(Arc::clone(&b)));

        let fetched = c.load("PB1").await.unwrap();
        assert!(fetched.is_some(), "C should fetch PB1 from a peer");
        assert_eq!(fetched.unwrap().id, "PB1");
        // And C has cached it locally now.
        assert!(c.local().load("PB1").await.unwrap().is_some());

        // A genuinely-unknown id fetch times out to None.
        let missing = c.load("NOPE").await.unwrap();
        assert!(missing.is_none());

        ra.abort();
        rb.abort();
    }

    #[tokio::test]
    async fn load_all_reports_local_sessions() {
        let endpoints = InMemoryClusterTransport::cluster(["solo"]);
        let store = ClusterSessionStore::new(
            Arc::new(endpoints.into_iter().next().unwrap()),
            ReplicationMode::AllToAll,
        );
        store
            .save(SessionData::new("S1".to_string()))
            .await
            .unwrap();
        store
            .save(SessionData::new("S2".to_string()))
            .await
            .unwrap();
        let mut ids: Vec<String> = store
            .load_all()
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["S1", "S2"]);
    }
}
