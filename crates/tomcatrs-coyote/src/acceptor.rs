//! The TCP accept loop.
//!
//! [`Acceptor`] owns a bound [`tokio::net::TcpListener`] and, for every accepted
//! connection, spawns a detached tokio task that runs the protocol handler.
//! This mirrors Tomcat's `Acceptor` thread plus the `NioEndpoint` worker pool,
//! collapsed onto tokio's task scheduler.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use tokio::net::TcpListener;
#[cfg(feature = "tls")]
use tokio::net::TcpStream;

use tomcatrs_config::{ConnectorConfig, Protocol};
use tomcatrs_core::{Error, Result};

use crate::ajp::AjpConnection;
use crate::protocol;
use crate::Adapter;

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
    /// # Errors
    ///
    /// Returns [`Error::Io`] only on an error that makes the listener itself
    /// unusable.
    pub async fn run(self) -> Result<()> {
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

        loop {
            match listener.accept().await {
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
                        tokio::spawn(async move {
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
                        tokio::spawn(async move {
                            if let Err(e) =
                                AjpConnection::serve(stream, adapter, peer_addr, None).await
                            {
                                tracing::warn!(%peer_addr, error = %e, "AJP connection handler failed");
                            }
                        });
                        continue;
                    }

                    tokio::spawn(async move {
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
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        }
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

    #[test]
    fn fatal_classification() {
        let aborted = std::io::Error::from(std::io::ErrorKind::ConnectionAborted);
        assert!(!is_fatal_accept_error(&aborted));
        let other = std::io::Error::from(std::io::ErrorKind::AddrNotAvailable);
        assert!(is_fatal_accept_error(&other));
    }
}
