//! Protocol dispatch.
//!
//! Tomcat's `ProtocolHandler` abstraction lets one connector speak HTTP/1.1,
//! HTTP/2, or AJP. This build wires up plaintext HTTP/1.1 and HTTP/2 (h2c,
//! prior-knowledge); AJP is still recognized but rejected with a clear
//! [`tomcatrs_core::Error::Protocol`].
//!
//! This module is intentionally thin: it maps a [`tomcatrs_config::Protocol`]
//! onto a handler and, for an accepted connection, delegates to that handler.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpStream;

use tomcatrs_config::{Protocol, RequestLimits};
use tomcatrs_core::{Error, Result};

use crate::{ajp, http1, http2_conn, Adapter};

/// Verify that `protocol` is supported by this build of the connector.
///
/// # Errors
///
/// Returns [`Error::Protocol`] for AJP, which is still a scaffold. HTTP/1.1 and
/// HTTP/2 are both supported.
pub fn ensure_supported(protocol: Protocol) -> Result<()> {
    match protocol {
        Protocol::Http11 => Ok(()),
        Protocol::Http2 => Ok(()),
        Protocol::Ajp => Err(ajp::unsupported()),
    }
}

/// Handle one accepted connection, dispatching on the connector's configured
/// protocol.
///
/// A [`Protocol::Http2`] connector routes the connection straight into the
/// HTTP/2 state machine ([`http2_conn::Http2Connection::serve`]), which expects
/// the client connection preface (prior-knowledge / h2c). [`Protocol::Http11`]
/// drives the HTTP/1.1 state machine unchanged.
///
/// # Errors
///
/// Propagates an unrecoverable I/O error from the protocol handler. Protocol
/// violations are handled in-band (a 4xx response, or an HTTP/2 `RST_STREAM` /
/// `GOAWAY`) and do **not** surface here.
pub async fn handle_connection(
    stream: TcpStream,
    peer_addr: SocketAddr,
    adapter: Arc<dyn Adapter>,
    limits: &RequestLimits,
    protocol: Protocol,
) -> Result<()> {
    match protocol {
        Protocol::Http2 => http2_conn::Http2Connection::serve(stream, adapter, peer_addr).await,
        // AJP is rejected at bind time; any connection reaching here with a
        // non-HTTP/2 protocol is plaintext HTTP/1.1.
        Protocol::Http11 | Protocol::Ajp => {
            http1::serve_connection(stream, peer_addr, adapter.as_ref(), limits)
                .await
                .map_err(|e| match e {
                    Error::Io(io) => Error::Io(io),
                    other => other,
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http11_and_http2_are_supported() {
        assert!(ensure_supported(Protocol::Http11).is_ok());
        assert!(ensure_supported(Protocol::Http2).is_ok());
    }

    #[test]
    fn ajp_is_rejected() {
        assert!(matches!(
            ensure_supported(Protocol::Ajp),
            Err(Error::Protocol(_))
        ));
    }
}
