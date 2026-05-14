//! The TCP accept loop.
//!
//! [`Acceptor`] owns a bound [`tokio::net::TcpListener`] and, for every accepted
//! connection, spawns a detached tokio task that runs the protocol handler.
//! This mirrors Tomcat's `Acceptor` thread plus the `NioEndpoint` worker pool,
//! collapsed onto tokio's task scheduler.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use tokio::net::TcpListener;

use tomcatrs_config::ConnectorConfig;
use tomcatrs_core::{Error, Result};

use crate::protocol;
use crate::Adapter;

/// A bound listener plus the shared state every connection task needs.
pub struct Acceptor {
    listener: TcpListener,
    adapter: Arc<dyn Adapter>,
    cfg: ConnectorConfig,
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
        // v0.1.0 only terminates plaintext HTTP/1.1. TLS and HTTP/2/AJP are
        // surfaced as explicit protocol errors rather than silent fallbacks.
        if cfg.tls.is_some() {
            return Err(crate::tls::unsupported());
        }
        protocol::ensure_supported(cfg.protocol)?;

        let ip = cfg.address.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        let addr = SocketAddr::new(ip, cfg.port);
        let listener = TcpListener::bind(addr).await.map_err(Error::Io)?;
        tracing::info!(local_addr = %listener.local_addr().map_err(Error::Io)?,
            "coyote HTTP/1.1 connector bound");

        Ok(Acceptor {
            listener,
            adapter,
            cfg: cfg.clone(),
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
        } = self;
        let limits = Arc::new(cfg.limits.clone());

        loop {
            match listener.accept().await {
                Ok((stream, peer_addr)) => {
                    // Disable Nagle's algorithm for lower request latency.
                    if let Err(e) = stream.set_nodelay(true) {
                        tracing::debug!(error = %e, "failed to set TCP_NODELAY");
                    }
                    let adapter = adapter.clone();
                    let limits = limits.clone();
                    tokio::spawn(async move {
                        if let Err(e) =
                            protocol::handle_connection(stream, peer_addr, adapter, &limits).await
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
