//! TLS termination — **scaffold only** (not implemented in v0.1.0).
//!
//! When a [`tomcatrs_config::ConnectorConfig`] carries a
//! [`tomcatrs_config::TlsConfig`], the connector is expected to terminate TLS
//! before handing the plaintext stream to a protocol handler. v0.1.0 does not
//! yet do this: [`Acceptor::bind`](crate::acceptor::Acceptor::bind) rejects any
//! config with TLS via [`unsupported`].
//!
//! # Planned design
//!
//! The real implementation will integrate [`rustls`](https://docs.rs/rustls)
//! via `tokio-rustls`:
//!
//! 1. At bind time, load the certificate chain from `TlsConfig::cert_file` and
//!    the private key from `TlsConfig::key_file`, building a
//!    `rustls::ServerConfig`.
//! 2. Configure ALPN with `h2` and `http/1.1` so [`crate::http2`] negotiation
//!    can key off the negotiated protocol.
//! 3. Wrap each accepted `TcpStream` in a `tokio_rustls::server::TlsStream`,
//!    perform the handshake, then dispatch on the ALPN result.
//!
//! Because [`crate::http1::serve_connection`] is generic over
//! `AsyncRead + AsyncWrite`, a `TlsStream` will drop straight into the existing
//! HTTP/1.1 path with no further changes.

use std::path::Path;

use tomcatrs_core::{Error, Result};

/// Placeholder for the future TLS acceptor.
///
/// In the real implementation this will own a `rustls::ServerConfig` (an
/// `Arc<ServerConfig>`) and expose an `accept(TcpStream)` method returning a
/// handshaken `TlsStream`.
#[derive(Debug)]
pub struct TlsAcceptor {
    _private: (),
}

impl TlsAcceptor {
    /// Build a TLS acceptor from a certificate and key file.
    ///
    /// # Errors
    ///
    /// Always returns [`Error::Protocol`] in v0.1.0: TLS termination is not yet
    /// implemented. The signature is stable so callers can be written now.
    pub fn from_pem_files(_cert_file: &Path, _key_file: &Path) -> Result<Self> {
        Err(unsupported())
    }
}

/// The canonical "TLS is not available yet" error.
pub fn unsupported() -> Error {
    Error::protocol("TLS termination not implemented in v0.1.0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_is_a_protocol_error() {
        assert!(matches!(unsupported(), Error::Protocol(_)));
    }

    #[test]
    fn from_pem_files_is_unsupported() {
        let r = TlsAcceptor::from_pem_files(Path::new("c.pem"), Path::new("k.pem"));
        assert!(matches!(r, Err(Error::Protocol(_))));
    }
}
