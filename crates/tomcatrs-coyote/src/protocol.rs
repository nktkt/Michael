//! Protocol dispatch.
//!
//! Tomcat's `ProtocolHandler` abstraction lets one connector speak HTTP/1.1,
//! HTTP/2, or AJP. In v0.1.0 only HTTP/1.1 is actually wired up; the other two
//! are recognized but rejected with a clear [`tomcatrs_core::Error::Protocol`].
//!
//! This module is intentionally thin: it maps a [`tomcatrs_config::Protocol`]
//! onto a handler and, for an accepted connection, delegates to that handler.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpStream;

use tomcatrs_config::{Protocol, RequestLimits};
use tomcatrs_core::{Error, Result};

use crate::{ajp, http1, http2, Adapter};

/// Verify that `protocol` is supported by this build of the connector.
///
/// # Errors
///
/// Returns [`Error::Protocol`] for HTTP/2 and AJP, which are scaffolds in
/// v0.1.0.
pub fn ensure_supported(protocol: Protocol) -> Result<()> {
    match protocol {
        Protocol::Http11 => Ok(()),
        Protocol::Http2 => Err(http2::unsupported()),
        Protocol::Ajp => Err(ajp::unsupported()),
    }
}

/// Handle one accepted connection.
///
/// Currently this always drives the HTTP/1.1 state machine; once
/// [`ensure_supported`] has been checked at bind time, a connection reaching
/// here is guaranteed to be plaintext HTTP/1.1.
///
/// # Errors
///
/// Propagates an unrecoverable I/O error from the protocol handler. Protocol
/// violations are handled in-band (a 4xx response) and do **not** surface here.
pub async fn handle_connection(
    stream: TcpStream,
    peer_addr: SocketAddr,
    adapter: Arc<dyn Adapter>,
    limits: &RequestLimits,
) -> Result<()> {
    http1::serve_connection(stream, peer_addr, adapter.as_ref(), limits)
        .await
        .map_err(|e| match e {
            Error::Io(io) => Error::Io(io),
            other => other,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http11_is_supported() {
        assert!(ensure_supported(Protocol::Http11).is_ok());
    }

    #[test]
    fn http2_and_ajp_are_rejected() {
        assert!(matches!(
            ensure_supported(Protocol::Http2),
            Err(Error::Protocol(_))
        ));
        assert!(matches!(
            ensure_supported(Protocol::Ajp),
            Err(Error::Protocol(_))
        ));
    }
}
