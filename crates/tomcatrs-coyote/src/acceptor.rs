//! The TCP accept loop.
//!
//! [`Acceptor`] owns a bound [`tokio::net::TcpListener`] and, for every accepted
//! connection, spawns a detached tokio task that runs the protocol handler.
//! This mirrors Tomcat's `Acceptor` thread plus the `NioEndpoint` worker pool,
//! collapsed onto tokio's task scheduler.
//!
//! ## Cooperative shutdown
//!
//! The accept loop watches a [`tokio::sync::watch::Receiver<bool>`]: while the
//! latest value is `false` it continues to accept new connections; once the
//! value flips to `true` the listener is dropped immediately (so a kernel
//! `SYN` to the port fails fast with `ECONNREFUSED`), and the loop waits up
//! to a configurable `drain_timeout` for already-accepted per-connection
//! tasks to finish in-flight requests. Tasks still running when the timeout
//! fires are aborted so the process can make forward progress to a clean
//! `Server::stop()` / `destroy()`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
#[cfg(feature = "tls")]
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::task::JoinSet;

use tomcatrs_config::{ConnectorConfig, Protocol};
use tomcatrs_core::{Error, Result};

use crate::ajp::AjpConnection;
use crate::protocol;
use crate::Adapter;

/// The graceful-shutdown signal an [`Acceptor`] (and therefore an
/// [`crate::HttpConnector`]) watches.
///
/// A [`Shutdown`] is just a `tokio::sync::watch::Sender<bool>` that starts
/// `false`; calling [`Shutdown::trigger`] (or sending `true` directly) flips
/// it, which the accept loop observes via the paired [`watch::Receiver`] from
/// [`Shutdown::subscribe`]. Drop one when you no longer need to fire it.
///
/// The CLI (or any embedder) constructs a single `Shutdown`, hands a clone of
/// its [`Shutdown::subscribe`] receiver to every connector via
/// [`crate::HttpConnector::serve_with_shutdown`], and calls
/// [`Shutdown::trigger`] when an OS signal is received. Each connector then
/// drains its in-flight requests within its configured `drain_timeout`.
#[derive(Debug, Clone)]
pub struct Shutdown {
    tx: watch::Sender<bool>,
}

impl Shutdown {
    /// Create a fresh, un-triggered shutdown handle.
    pub fn new() -> Self {
        let (tx, _) = watch::channel(false);
        Self { tx }
    }

    /// Subscribe to the shutdown flag. Each connector should hold its own
    /// receiver — they all observe the same flip.
    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.tx.subscribe()
    }

    /// Flip the flag to `true`, signalling every subscribed accept loop to
    /// stop accepting and drain. Idempotent.
    pub fn trigger(&self) {
        // Ignore the receiver-count: even if no one's subscribed yet, future
        // subscribers will see `true` immediately.
        let _ = self.tx.send(true);
    }

    /// Whether the shutdown has already been triggered.
    pub fn is_triggered(&self) -> bool {
        *self.tx.borrow()
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

/// A bound listener plus the shared state every connection task needs.
pub struct Acceptor {
    listener: TcpListener,
    adapter: Arc<dyn Adapter>,
    cfg: ConnectorConfig,
    /// When the connector is configured with TLS *and* the `tls` feature is
    /// compiled in, this holds the prepared terminator used to wrap every
    /// accepted stream. `None` means plaintext.
    #[cfg(feature = "tls")]
    tls_acceptor: Option<crate::tls::TlsAcceptor>,
}

impl Acceptor {
    /// Bind the socket described by `cfg` and return a ready-to-run [`Acceptor`].
    ///
    /// The bind address defaults to `0.0.0.0` when `cfg.address` is `None`.
    ///
    /// # Errors
    ///
    /// * [`Error::Protocol`] if `cfg` requests TLS (not supported in v0.1.0) or
    ///   a non-HTTP/1.1 protocol.
    /// * [`Error::Io`] if the socket cannot be bound.
    pub async fn bind(cfg: &ConnectorConfig, adapter: Arc<dyn Adapter>) -> Result<Self> {
        // AJP is implemented by `crate::ajp::AjpConnection` and dispatched
        // directly from the accept loop below, so it bypasses the
        // `ensure_supported` gate (which still reports AJP as a scaffold for
        // callers that route through `protocol::handle_connection`).
        if cfg.protocol != Protocol::Ajp {
            protocol::ensure_supported(cfg.protocol)?;
        }

        // If the connector is configured with TLS, build the terminator now so
        // a bad certificate fails the bind rather than every connection. When
        // the `tls` feature is compiled out, a TLS-configured connector is
        // logged and skipped (it binds nothing) rather than failing the bind.
        #[cfg(feature = "tls")]
        let tls_acceptor = match &cfg.tls {
            Some(tls) => {
                // Advertise both protocols; the connection layer dispatches on
                // the negotiated result.
                let alpn = [b"h2".to_vec(), b"http/1.1".to_vec()];
                let server_config = crate::tls::build_server_config(tls, &alpn)?;
                Some(crate::tls::TlsAcceptor::new(server_config))
            }
            None => None,
        };
        #[cfg(not(feature = "tls"))]
        if cfg.tls.is_some() {
            tracing::error!(
                "connector configured with TLS but the `tls` feature is not compiled in; \
                 skipping — rebuild with --features tls"
            );
            return Err(crate::tls::unsupported());
        }

        let ip = cfg.address.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        let addr = SocketAddr::new(ip, cfg.port);
        let listener = TcpListener::bind(addr).await.map_err(Error::Io)?;
        tracing::info!(local_addr = %listener.local_addr().map_err(Error::Io)?,
            tls = cfg.tls.is_some(),
            "coyote connector bound");

        Ok(Acceptor {
            listener,
            adapter,
            cfg: cfg.clone(),
            #[cfg(feature = "tls")]
            tls_acceptor,
        })
    }

    /// The concrete address the listener is bound to (resolves port `0`).
    pub fn local_addr(&self) -> SocketAddr {
        self.listener
            .local_addr()
            .expect("a bound TcpListener always has a local address")
    }

    /// Run the accept loop until the listener errors fatally.
    ///
    /// Each accepted connection is handed to a freshly spawned task; a single
    /// misbehaving connection therefore cannot stall the loop. Transient
    /// per-accept errors are logged and the loop continues.
    ///
    /// This is the legacy entry point with no graceful-shutdown signalling.
    /// New callers should prefer [`Acceptor::run_with_shutdown`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] only on an error that makes the listener itself
    /// unusable.
    pub async fn run(self) -> Result<()> {
        // A never-triggered shutdown source: kept alive in scope so the
        // receiver never observes a `Sender` drop (which would itself wake
        // the watch). The chosen drain timeout is irrelevant — it never fires.
        let shutdown = Shutdown::new();
        let rx = shutdown.subscribe();
        self.run_with_shutdown(rx, Duration::from_secs(30)).await
    }

    /// Run the accept loop, watching `shutdown_rx` for a cooperative drain.
    ///
    /// While `shutdown_rx` reads `false` the loop accepts new connections
    /// exactly like [`Acceptor::run`]. Once it flips to `true`:
    ///
    /// 1. The listener is dropped immediately; further `connect()` attempts
    ///    against this port get the OS-level `ECONNREFUSED` (or equivalent).
    /// 2. The loop awaits already-accepted per-connection tasks for up to
    ///    `drain_timeout`. Each task finishing in time is logged at `debug`.
    /// 3. On timeout, remaining tasks are aborted; a `warn` line names how
    ///    many in-flight requests were cut.
    ///
    /// Returns `Ok(())` once the drain phase finishes (cleanly or by
    /// timeout). Listener-fatal accept errors before shutdown still propagate
    /// as [`Error::Io`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] only on an error that makes the listener itself
    /// unusable *before* shutdown is triggered. Once shutdown is triggered,
    /// the drain always completes (cleanly or by timeout) with `Ok(())`.
    pub async fn run_with_shutdown(
        self,
        mut shutdown_rx: watch::Receiver<bool>,
        drain_timeout: Duration,
    ) -> Result<()> {
        let Acceptor {
            listener,
            adapter,
            cfg,
            #[cfg(feature = "tls")]
            tls_acceptor,
        } = self;
        let limits = Arc::new(cfg.limits.clone());
        // The connector's configured wire protocol; every accepted connection
        // is dispatched on this (prior-knowledge HTTP/2 vs. HTTP/1.1).
        let protocol = cfg.protocol;
        let local_addr = listener.local_addr().ok();

        // If shutdown was triggered *before* we even entered the loop, drain
        // out without ever accepting. Cheap fast path; also matches what
        // operators expect when a second SIGTERM races the bind.
        if *shutdown_rx.borrow() {
            tracing::info!(
                ?local_addr,
                "coyote connector shutdown already requested before accept loop started"
            );
            return Ok(());
        }

        // Per-connection tasks live here so the drain phase can await (and
        // abort on timeout) precisely the set of in-flight requests.
        let mut conns: JoinSet<()> = JoinSet::new();

        let listener_addr = local_addr;
        loop {
            tokio::select! {
                biased;

                // Watch shutdown first so a flip is acted on even under heavy
                // accept pressure.
                changed = shutdown_rx.changed() => {
                    // The sender side dropped or sent a value. Either way:
                    // if the value is now `true`, drain. If it isn't (sender
                    // dropped without a flip), drain anyway — there's nothing
                    // left to flip it later, and continuing to accept makes
                    // no sense.
                    let triggered = changed.is_ok() && *shutdown_rx.borrow();
                    if triggered {
                        tracing::info!(
                            ?listener_addr,
                            in_flight = conns.len(),
                            drain_timeout_secs = drain_timeout.as_secs(),
                            "coyote connector shutdown requested; dropping listener, draining in-flight requests",
                        );
                    } else {
                        tracing::info!(
                            ?listener_addr,
                            in_flight = conns.len(),
                            "coyote connector shutdown channel closed; draining in-flight requests",
                        );
                    }
                    break;
                }

                accept = listener.accept() => {
                    match accept {
                        Ok((stream, peer_addr)) => {
                            // Disable Nagle's algorithm for lower request latency.
                            if let Err(e) = stream.set_nodelay(true) {
                                tracing::debug!(error = %e, "failed to set TCP_NODELAY");
                            }
                            let adapter = adapter.clone();
                            let limits = limits.clone();

                            // TLS connector: terminate TLS, then dispatch on the
                            // negotiated ALPN protocol before handing the plaintext
                            // stream to the protocol layer.
                            #[cfg(feature = "tls")]
                            if let Some(tls_acceptor) = tls_acceptor.clone() {
                                conns.spawn(async move {
                                    if let Err(e) = handle_tls_connection(
                                        tls_acceptor,
                                        stream,
                                        peer_addr,
                                        adapter,
                                        &limits,
                                    )
                                    .await
                                    {
                                        tracing::warn!(%peer_addr, error = %e, "TLS connection handler failed");
                                    }
                                });
                                continue;
                            }

                            // AJP/1.3 connector: drive the connection through the
                            // dedicated AJP state machine. AJP is plaintext, trusted-
                            // network only; no secret is plumbed through
                            // `ConnectorConfig` yet, so the shared-secret check is
                            // off unless a future config field supplies one.
                            if protocol == Protocol::Ajp {
                                conns.spawn(async move {
                                    if let Err(e) =
                                        AjpConnection::serve(stream, adapter, peer_addr, None).await
                                    {
                                        tracing::warn!(%peer_addr, error = %e, "AJP connection handler failed");
                                    }
                                });
                                continue;
                            }

                            conns.spawn(async move {
                                if let Err(e) = protocol::handle_connection(
                                    stream, peer_addr, adapter, &limits, protocol,
                                )
                                .await
                                {
                                    tracing::warn!(%peer_addr, error = %e, "connection handler failed");
                                }
                            });
                        }
                        Err(e) => {
                            // ECONNABORTED and friends are transient — keep accepting.
                            // A persistently failing accept would spin; back off a touch.
                            tracing::warn!(error = %e, "accept() failed");
                            if is_fatal_accept_error(&e) {
                                return Err(Error::Io(e));
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    }
                }

                // Reap finished connection tasks so `conns.len()` stays a
                // faithful in-flight count and the JoinSet doesn't grow
                // without bound on a long-lived listener.
                Some(_) = conns.join_next(), if !conns.is_empty() => {
                    // Nothing to do: the per-connection task already logged
                    // its own outcome. We just want to drain the join slot.
                }
            }
        }

        // Drop the listener *now*, before we start the drain wait, so the
        // kernel stops responding to SYNs on our port immediately.
        drop(listener);

        let in_flight = conns.len();
        if in_flight == 0 {
            tracing::info!(
                ?listener_addr,
                "coyote connector drained (no in-flight requests)"
            );
            return Ok(());
        }

        // Phase 2: wait up to `drain_timeout` for in-flight tasks to finish.
        let drain = async { while conns.join_next().await.is_some() {} };
        match tokio::time::timeout(drain_timeout, drain).await {
            Ok(()) => {
                tracing::info!(
                    ?listener_addr,
                    drained = in_flight,
                    "coyote connector drained cleanly",
                );
            }
            Err(_) => {
                let remaining = conns.len();
                tracing::warn!(
                    ?listener_addr,
                    drain_timeout_secs = drain_timeout.as_secs(),
                    aborted = remaining,
                    "coyote connector drain timed out; aborting in-flight requests",
                );
                conns.shutdown().await;
            }
        }

        Ok(())
    }
}

/// Classify whether an `accept()` error should terminate the loop.
///
/// Resource-exhaustion and connection-abort errors are transient; anything else
/// (e.g. the listener being closed) is treated as fatal.
fn is_fatal_accept_error(e: &std::io::Error) -> bool {
    !matches!(
        e.kind(),
        std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock
    )
}

/// Terminate TLS on `stream`, then dispatch the plaintext connection on the
/// ALPN protocol negotiated during the handshake.
///
/// `http/1.1` (and an absent ALPN, the common case for plain HTTPS clients) is
/// served by the HTTP/1.1 state machine, which is generic over any
/// `AsyncRead + AsyncWrite` and so accepts the TLS stream directly. A
/// negotiated `h2` is routed into the HTTP/2 state machine, which is likewise
/// generic over the stream type.
///
/// # Errors
///
/// Returns [`Error::Protocol`] if the handshake fails or the peer negotiated a
/// protocol this build cannot serve.
#[cfg(feature = "tls")]
async fn handle_tls_connection(
    tls_acceptor: crate::tls::TlsAcceptor,
    stream: TcpStream,
    peer_addr: SocketAddr,
    adapter: Arc<dyn Adapter>,
    limits: &tomcatrs_config::RequestLimits,
) -> Result<()> {
    let tls_stream = tls_acceptor.accept(stream).await?;

    match crate::tls::alpn_protocol(&tls_stream) {
        Some(b"h2") => {
            // HTTP/2 negotiated via ALPN: drive the HTTP/2 state machine.
            crate::http2_conn::Http2Connection::serve(tls_stream, adapter, peer_addr).await
        }
        // `http/1.1`, or no ALPN at all (plain HTTPS client): serve HTTP/1.1.
        _ => crate::http1::serve_connection(tls_stream, peer_addr, adapter.as_ref(), limits).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn fatal_classification() {
        let aborted = std::io::Error::from(std::io::ErrorKind::ConnectionAborted);
        assert!(!is_fatal_accept_error(&aborted));
        let other = std::io::Error::from(std::io::ErrorKind::AddrNotAvailable);
        assert!(is_fatal_accept_error(&other));
    }

    #[test]
    fn shutdown_starts_untriggered_and_flips_once() {
        let s = Shutdown::new();
        assert!(!s.is_triggered());
        let mut rx = s.subscribe();
        assert!(!*rx.borrow_and_update());
        s.trigger();
        assert!(s.is_triggered());
        // Receiver sees the flip on the next changed().
        // Use try_recv via has_changed for a sync check.
        assert!(rx.has_changed().unwrap_or(false));
        assert!(*rx.borrow());
        // Idempotent.
        s.trigger();
        assert!(s.is_triggered());
    }

    /// An adapter whose `service()` future deliberately stalls long enough to
    /// look like an in-flight request when shutdown fires.
    struct SlowAdapter {
        delay: Duration,
    }

    #[async_trait::async_trait]
    impl crate::Adapter for SlowAdapter {
        async fn service(&self, _req: crate::Request) -> crate::Response {
            tokio::time::sleep(self.delay).await;
            crate::Response::with_body(200, "slow-ok")
        }
    }

    fn h11_cfg(port: u16) -> ConnectorConfig {
        ConnectorConfig {
            protocol: Protocol::Http11,
            address: Some("127.0.0.1".parse().unwrap()),
            port,
            tls: None,
            limits: tomcatrs_config::RequestLimits::default(),
        }
    }

    /// Read the headers of an HTTP/1.1 response off `stream` and return the
    /// status code. Used to confirm an in-flight request completed end-to-end
    /// even after shutdown was requested.
    async fn read_status_line(stream: &mut tokio::net::TcpStream) -> u16 {
        let mut buf = [0u8; 4096];
        let mut total = Vec::new();
        loop {
            let n = stream.read(&mut buf).await.expect("read");
            if n == 0 {
                break;
            }
            total.extend_from_slice(&buf[..n]);
            if total.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let head = std::str::from_utf8(&total).expect("utf8 head");
        let status_line = head.split("\r\n").next().expect("status line");
        status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .expect("parsed status")
    }

    /// Verify that triggering shutdown mid-test:
    ///   (a) lets an already-accepted slow request complete cleanly, and
    ///   (b) refuses a fresh `connect()` promptly.
    #[tokio::test]
    async fn shutdown_drains_in_flight_and_refuses_new_connections() {
        let cfg = h11_cfg(0);
        let acceptor = Acceptor::bind(
            &cfg,
            Arc::new(SlowAdapter {
                delay: Duration::from_millis(400),
            }),
        )
        .await
        .expect("bind");
        let addr = acceptor.local_addr();
        let shutdown = Shutdown::new();
        let rx = shutdown.subscribe();
        let drain_timeout = Duration::from_secs(5);

        let server =
            tokio::spawn(async move { acceptor.run_with_shutdown(rx, drain_timeout).await });

        // (1) Send a slow request and immediately trigger shutdown. The
        // already-accepted request must still complete with 200.
        let mut client = tokio::net::TcpStream::connect(addr).await.expect("connect");
        client
            .write_all(b"GET /slow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("write");

        // Give the acceptor a moment to actually accept and start the handler.
        // 50ms is long enough for tokio's scheduler on a quiet test runner.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Fire shutdown mid-flight.
        shutdown.trigger();

        // (2) The in-flight slow request must still return 200, *not* 5xx or
        // an abrupt connection close.
        let status = read_status_line(&mut client).await;
        assert_eq!(
            status, 200,
            "in-flight request must complete after shutdown",
        );

        // The accept loop should now have drained and exited cleanly.
        let result = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("server task should finish promptly after drain")
            .expect("server task did not panic");
        assert!(result.is_ok(), "run_with_shutdown returned {:?}", result);
    }

    /// Verify that a fresh connect() immediately after shutdown is refused
    /// (and not silently 5xx'd or hung): exercises the "promptly refused"
    /// half of the cooperative-shutdown contract explicitly.
    #[tokio::test]
    async fn shutdown_refuses_new_connections_promptly() {
        let cfg = h11_cfg(0);
        let acceptor = Acceptor::bind(
            &cfg,
            Arc::new(SlowAdapter {
                delay: Duration::from_millis(10),
            }),
        )
        .await
        .expect("bind");
        let addr = acceptor.local_addr();
        let shutdown = Shutdown::new();
        let rx = shutdown.subscribe();

        let server =
            tokio::spawn(
                async move { acceptor.run_with_shutdown(rx, Duration::from_secs(2)).await },
            );

        // Confirm we *can* connect before shutdown, so we know the listener
        // is up.
        let pre = tokio::net::TcpStream::connect(addr).await;
        assert!(pre.is_ok(), "should be able to connect before shutdown");
        drop(pre);

        shutdown.trigger();

        // Wait until the listener is actually closed. The drop happens once
        // the select! wakes on the watch, which is essentially "next tick".
        let mut refused = false;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            match tokio::time::timeout(
                Duration::from_millis(100),
                tokio::net::TcpStream::connect(addr),
            )
            .await
            {
                Ok(Err(_)) => {
                    refused = true;
                    break;
                }
                Err(_) => {
                    // connect timed out — also acceptable; no listener.
                    refused = true;
                    break;
                }
                Ok(Ok(_)) => continue,
            }
        }
        assert!(refused, "post-shutdown connect should fail promptly");

        let result = tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .expect("server task should finish");
        let result = result.expect("server task did not panic");
        assert!(result.is_ok());
    }
}
