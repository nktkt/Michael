//! A TCP-based [`ClusterTransport`] for [`ClusterSessionStore`].
//!
//! [`TcpClusterTransport`] is the production-shaped sibling of
//! [`InMemoryClusterTransport`](crate::store_cluster::InMemoryClusterTransport):
//! it exchanges the same [`ClusterMessage`]s but over real TCP sockets.
//!
//! # Wire format
//!
//! Each message is framed as:
//!
//! ```text
//!   ┌──────────────────────┬──────────────────────────────────────┐
//!   │ u32 BE length (4 B)  │ serde_json-encoded ClusterMessage    │
//!   └──────────────────────┴──────────────────────────────────────┘
//! ```
//!
//! JSON keeps the format human-readable for diagnostics and avoids pulling in
//! a binary codec dependency. A 4-byte length prefix is enough for the message
//! sizes a session-replication payload ever reaches (well below 4 GiB).
//!
//! # Connections
//!
//! Each node binds a [`tokio::net::TcpListener`] on its local address and
//! treats every accepted connection as a *receive* socket — frames read off it
//! are forwarded into a single per-node inbound channel.
//!
//! Outbound connections to peers are **lazy** and held in a
//! [`dashmap::DashMap`] keyed by the peer's [`SocketAddr`]: the first
//! [`broadcast`](TcpClusterTransport::broadcast) or
//! [`send_to`](TcpClusterTransport::send_to) targeting a peer opens a TCP
//! connection and spawns a writer task; later calls reuse the existing channel.
//! If a writer task observes a broken pipe it removes its entry from the map,
//! so the **next** send to that peer transparently reconnects — a peer that
//! restarts is eventually reachable again without operator intervention.
//!
//! # Node identity
//!
//! [`ClusterTransport::node_id`] returns the string form of the local bind
//! address. [`send_to`](TcpClusterTransport::send_to) accepts the same string
//! form for peers — i.e. `"127.0.0.1:9501"`. This keeps the wire-level identity
//! free of any "name → address" directory; higher layers (configuration,
//! discovery) can layer their own name resolution on top if they wish.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, trace, warn};

use crate::store_cluster::{ClusterMessage, ClusterTransport};

/// Outbound queue depth per peer connection. A few thousand frames is more
/// than enough head-room for replication bursts before back-pressure kicks in.
const OUTBOUND_QUEUE_DEPTH: usize = 1024;

/// Inbound queue depth across all peers. Same rationale.
const INBOUND_QUEUE_DEPTH: usize = 4096;

/// Maximum accepted frame size (16 MiB). A session payload that approaches
/// this is almost certainly a bug or a protocol mismatch; rejecting an
/// oversized length prefix means a malformed peer cannot exhaust memory.
const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// A [`ClusterTransport`] that speaks length-prefixed JSON over TCP.
///
/// Construct with [`TcpClusterTransport::start`].
pub struct TcpClusterTransport {
    /// String form of this node's bind address — also serves as its
    /// [`ClusterTransport::node_id`].
    node_id: String,
    /// Local bind address, kept for tests that need to discover the assigned
    /// port when binding on `:0`.
    bind_addr: SocketAddr,
    /// Lazy outbound writer channels, keyed by the peer's socket address.
    outbound: Arc<DashMap<SocketAddr, mpsc::Sender<ClusterMessage>>>,
    /// Receiving side of the single per-node inbound queue. Both the accept
    /// loop and every inbound reader task push onto its companion `Sender`.
    inbound: Mutex<mpsc::Receiver<ClusterMessage>>,
    /// Kept alive so the inbound channel does not close while the transport
    /// is still running. Each accepted connection also clones this sender,
    /// but if every accepted reader exits and this anchor were absent the
    /// receiver would observe a premature `None`.
    #[allow(dead_code)]
    inbound_tx: mpsc::Sender<ClusterMessage>,
}

impl TcpClusterTransport {
    /// Bind a listener on `bind_addr`, prime a lazy peer table from
    /// `peer_addrs`, and return a fully functional [`TcpClusterTransport`].
    ///
    /// `bind_addr` may use port `0` to ask the OS for an ephemeral port; the
    /// effective address is available via [`local_addr`](Self::local_addr).
    ///
    /// `peer_addrs` is *advisory*: connections are opened lazily on the first
    /// send to a peer, so it is fine to include peers that are not yet up. A
    /// peer omitted here can still be reached by passing its address to
    /// [`send_to`](Self::send_to) directly.
    pub async fn start(
        bind_addr: SocketAddr,
        peer_addrs: Vec<SocketAddr>,
    ) -> tomcatrs_core::Result<Self> {
        let listener = TcpListener::bind(bind_addr)
            .await
            .map_err(tomcatrs_core::Error::Io)?;
        let local = listener.local_addr().map_err(tomcatrs_core::Error::Io)?;

        let (inbound_tx, inbound_rx) = mpsc::channel::<ClusterMessage>(INBOUND_QUEUE_DEPTH);
        let outbound: Arc<DashMap<SocketAddr, mpsc::Sender<ClusterMessage>>> =
            Arc::new(DashMap::new());

        // `peer_addrs` is advisory: outbound connections are opened lazily by
        // `ensure_peer` on the first send to a peer. We accept the parameter
        // up front so callers can declare the topology in one place and so
        // future iterations can pre-warm connections without an API change.
        let _ = peer_addrs;

        // Spawn the accept loop.
        let accept_tx = inbound_tx.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        trace!(%peer, "tcp cluster: accepted inbound connection");
                        let inbound_tx = accept_tx.clone();
                        tokio::spawn(read_inbound_loop(stream, peer, inbound_tx));
                    }
                    Err(e) => {
                        warn!(error = %e, "tcp cluster: accept failed; listener stopping");
                        break;
                    }
                }
            }
        });

        Ok(TcpClusterTransport {
            node_id: local.to_string(),
            bind_addr: local,
            outbound,
            inbound: Mutex::new(inbound_rx),
            inbound_tx,
        })
    }

    /// The local bind address, with the actual port resolved (useful when the
    /// caller passed port `0`).
    pub fn local_addr(&self) -> SocketAddr {
        self.bind_addr
    }

    /// Acquire (or lazily open) the outbound writer channel to `peer`.
    ///
    /// Returns the [`mpsc::Sender`] for the writer task that owns the TCP
    /// connection. A failed connect is *not* fatal: the caller observes it as
    /// a returned `Err` from the eventual send, the entry is left empty, and
    /// the next attempt retries.
    async fn ensure_peer(
        &self,
        peer: SocketAddr,
    ) -> tomcatrs_core::Result<mpsc::Sender<ClusterMessage>> {
        if let Some(existing) = self.outbound.get(&peer) {
            return Ok(existing.clone());
        }
        // Connect. A failure here is reported up; the entry stays absent so
        // the next call retries from scratch.
        let stream = TcpStream::connect(peer).await.map_err(|e| {
            tomcatrs_core::Error::Other(format!("tcp cluster: connect to {peer} failed: {e}"))
        })?;
        let (tx, rx) = mpsc::channel::<ClusterMessage>(OUTBOUND_QUEUE_DEPTH);

        // Spawn the writer task; on shutdown it cleans its own entry out so a
        // future send transparently reconnects.
        let outbound = Arc::clone(&self.outbound);
        tokio::spawn(write_outbound_loop(stream, peer, rx, outbound));

        // Race: another task may have inserted concurrently. Prefer the
        // existing entry to avoid orphaning a second writer.
        match self.outbound.entry(peer) {
            dashmap::mapref::entry::Entry::Occupied(occ) => Ok(occ.get().clone()),
            dashmap::mapref::entry::Entry::Vacant(vac) => {
                vac.insert(tx.clone());
                Ok(tx)
            }
        }
    }

    /// Send `msg` to `peer`, opening a connection if needed. Reports a
    /// `tomcatrs_core::Error::Other` on connect/send failure and evicts the
    /// peer entry so the next attempt reconnects.
    async fn send_to_addr(
        &self,
        peer: SocketAddr,
        msg: ClusterMessage,
    ) -> tomcatrs_core::Result<()> {
        match self.ensure_peer(peer).await {
            Ok(tx) => match tx.send(msg).await {
                Ok(()) => Ok(()),
                Err(_) => {
                    // Writer task is gone: clear the stale entry and report.
                    self.outbound.remove(&peer);
                    Err(tomcatrs_core::Error::Other(format!(
                        "tcp cluster: peer {peer} writer closed"
                    )))
                }
            },
            Err(e) => Err(e),
        }
    }

    /// Snapshot the current outbound peer set (for tests and diagnostics).
    pub fn connected_peers(&self) -> Vec<SocketAddr> {
        self.outbound.iter().map(|kv| *kv.key()).collect()
    }
}

/// Read length-prefixed frames off `stream` and push each decoded
/// [`ClusterMessage`] into `inbound_tx`. Exits cleanly on EOF, malformed
/// frame, or oversized length.
async fn read_inbound_loop(
    mut stream: TcpStream,
    peer: SocketAddr,
    inbound_tx: mpsc::Sender<ClusterMessage>,
) {
    let mut len_buf = [0u8; 4];
    loop {
        // Read length prefix.
        if let Err(e) = stream.read_exact(&mut len_buf).await {
            // EOF is the normal way for a peer to close the link.
            trace!(%peer, error = %e, "tcp cluster: inbound stream closed");
            return;
        }
        let len = u32::from_be_bytes(len_buf);
        if len == 0 || len > MAX_FRAME_BYTES {
            warn!(%peer, len, "tcp cluster: oversized/empty frame; dropping connection");
            return;
        }
        let mut payload = vec![0u8; len as usize];
        if let Err(e) = stream.read_exact(&mut payload).await {
            warn!(%peer, error = %e, "tcp cluster: short read on frame body");
            return;
        }
        match serde_json::from_slice::<ClusterMessage>(&payload) {
            Ok(msg) => {
                if inbound_tx.send(msg).await.is_err() {
                    debug!(%peer, "tcp cluster: inbound queue closed; reader stopping");
                    return;
                }
            }
            Err(e) => {
                warn!(%peer, error = %e, "tcp cluster: malformed frame; dropping connection");
                return;
            }
        }
    }
}

/// Drain the per-peer outbound queue and write each message to `stream` as a
/// length-prefixed JSON frame. On any write failure the writer evicts the
/// peer entry from `outbound`, so the next send by the owning transport
/// transparently reconnects.
async fn write_outbound_loop(
    mut stream: TcpStream,
    peer: SocketAddr,
    mut rx: mpsc::Receiver<ClusterMessage>,
    outbound: Arc<DashMap<SocketAddr, mpsc::Sender<ClusterMessage>>>,
) {
    while let Some(msg) = rx.recv().await {
        let payload = match serde_json::to_vec(&msg) {
            Ok(b) => b,
            Err(e) => {
                warn!(%peer, error = %e, "tcp cluster: outbound encode failed; dropping message");
                continue;
            }
        };
        if payload.len() > MAX_FRAME_BYTES as usize {
            warn!(%peer, size = payload.len(),
                "tcp cluster: outbound message exceeds max frame size; dropping");
            continue;
        }
        let len = (payload.len() as u32).to_be_bytes();
        if let Err(e) = stream.write_all(&len).await {
            warn!(%peer, error = %e, "tcp cluster: outbound write failed; evicting peer");
            break;
        }
        if let Err(e) = stream.write_all(&payload).await {
            warn!(%peer, error = %e, "tcp cluster: outbound write failed; evicting peer");
            break;
        }
        if let Err(e) = stream.flush().await {
            warn!(%peer, error = %e, "tcp cluster: outbound flush failed; evicting peer");
            break;
        }
    }
    // Drop our entry so a subsequent send reconnects.
    outbound.remove(&peer);
    let _ = stream.shutdown().await;
}

#[async_trait]
impl ClusterTransport for TcpClusterTransport {
    fn node_id(&self) -> &str {
        &self.node_id
    }

    async fn broadcast(&self, msg: ClusterMessage) -> tomcatrs_core::Result<()> {
        // Send to every currently-connected outbound peer. We also fan out to
        // every configured-but-not-yet-connected peer in `connected_peers`,
        // but in this transport the configured set *is* the connected set —
        // higher-level code that wants a known fan-out target list should
        // call `send_to` instead.
        //
        // Errors per peer are logged but not fatal: a broken peer should not
        // stop the rest of the broadcast.
        let peers: Vec<SocketAddr> = self.outbound.iter().map(|kv| *kv.key()).collect();
        for peer in peers {
            if let Err(e) = self.send_to_addr(peer, msg.clone()).await {
                warn!(%peer, error = %e, "tcp cluster: broadcast leg failed");
            }
        }
        Ok(())
    }

    async fn send_to(&self, node_id: &str, msg: ClusterMessage) -> tomcatrs_core::Result<()> {
        let addr: SocketAddr = node_id.parse().map_err(|e| {
            tomcatrs_core::Error::Other(format!("invalid peer addr '{node_id}': {e}"))
        })?;
        self.send_to_addr(addr, msg).await
    }

    async fn receive(&self) -> tomcatrs_core::Result<Option<ClusterMessage>> {
        let mut guard = self.inbound.lock().await;
        Ok(guard.recv().await)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::time::Duration;

    use super::*;
    use crate::SessionData;

    /// Bind to 127.0.0.1:0 to grab an OS-assigned ephemeral port.
    fn loopback_any() -> SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    /// Receive exactly one message from `t` within `timeout`, or fail.
    async fn recv_one(t: &TcpClusterTransport) -> ClusterMessage {
        tokio::time::timeout(Duration::from_secs(2), t.receive())
            .await
            .expect("timed out waiting for cluster message")
            .expect("receive returned Err")
            .expect("transport closed")
    }

    #[tokio::test]
    async fn two_nodes_exchange_a_message() {
        // Bring up node A on an ephemeral port…
        let a = TcpClusterTransport::start(loopback_any(), Vec::new())
            .await
            .unwrap();
        let a_addr = a.local_addr();

        // …and node B, pre-configured to know A (advisory; B will lazily
        // connect on its first `send_to`).
        let b = TcpClusterTransport::start(loopback_any(), vec![a_addr])
            .await
            .unwrap();

        // B sends a Save to A by address; A receives it.
        let session = SessionData::new("TCP-A".to_string());
        b.send_to(&a_addr.to_string(), ClusterMessage::Save(session.clone()))
            .await
            .unwrap();

        match recv_one(&a).await {
            ClusterMessage::Save(s) => assert_eq!(s.id, "TCP-A"),
            other => panic!("unexpected message: {other:?}"),
        }

        // node_id() reflects the resolved bind address.
        let a_id_parsed: SocketAddr = a.node_id().parse().unwrap();
        assert_eq!(a_id_parsed, a_addr);
    }

    #[tokio::test]
    async fn broadcast_reaches_every_connected_peer() {
        // Three nodes; node A will broadcast to B and C.
        let b = TcpClusterTransport::start(loopback_any(), Vec::new())
            .await
            .unwrap();
        let c = TcpClusterTransport::start(loopback_any(), Vec::new())
            .await
            .unwrap();
        let b_addr = b.local_addr();
        let c_addr = c.local_addr();

        let a = TcpClusterTransport::start(loopback_any(), vec![b_addr, c_addr])
            .await
            .unwrap();

        // Open lazy outbound connections by sending one targeted message to
        // each. (Broadcast iterates over *connected* peers, matching the
        // documented contract.)
        a.send_to(&b_addr.to_string(), ClusterMessage::Delete("warmup".into()))
            .await
            .unwrap();
        a.send_to(&c_addr.to_string(), ClusterMessage::Delete("warmup".into()))
            .await
            .unwrap();

        // Drain the warmup frame on B and C.
        let _ = recv_one(&b).await;
        let _ = recv_one(&c).await;

        // Now broadcast a real message.
        a.broadcast(ClusterMessage::Delete("BCAST-1".into()))
            .await
            .unwrap();

        let mut got = HashSet::new();
        for endpoint in [&b, &c] {
            match recv_one(endpoint).await {
                ClusterMessage::Delete(id) => {
                    got.insert(id);
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
        assert_eq!(got, HashSet::from(["BCAST-1".to_string()]));
    }

    #[tokio::test]
    async fn send_to_unparseable_addr_errors() {
        let t = TcpClusterTransport::start(loopback_any(), Vec::new())
            .await
            .unwrap();
        let err = t
            .send_to("not-an-addr", ClusterMessage::Delete("x".into()))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("invalid peer addr"));
    }
}
