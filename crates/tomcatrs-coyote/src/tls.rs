//! TLS termination via [`rustls`](https://docs.rs/rustls).
//!
//! When a [`tomcatrs_config::ConnectorConfig`] carries a
//! [`tomcatrs_config::TlsConfig`], the connector terminates TLS before handing
//! the now-plaintext stream to a protocol handler. This module owns that work:
//!
//! 1. At bind time, [`build_server_config`] loads the certificate chain from
//!    `TlsConfig::cert_file` and the private key from `TlsConfig::key_file`,
//!    producing a shared [`rustls::ServerConfig`].
//! 2. ALPN is configured (typically `h2` then `http/1.1`) so the connection
//!    layer can dispatch on the negotiated protocol after the handshake.
//! 3. [`TlsAcceptor`] wraps [`tokio_rustls::TlsAcceptor`]; [`TlsAcceptor::accept`]
//!    performs the handshake on an accepted stream and reports the negotiated
//!    ALPN protocol.
//!
//! Because [`crate::http1::serve_connection`] is generic over
//! `AsyncRead + AsyncWrite`, the [`tokio_rustls::server::TlsStream`] returned by
//! [`TlsAcceptor::accept`] drops straight into the existing HTTP/1.1 path.
//!
//! # Crypto backend
//!
//! `rustls` is built against the `ring` crypto provider (not the default
//! `aws-lc-rs`) to minimise build dependencies — see this crate's `Cargo.toml`.
//!
//! # Feature gating
//!
//! Everything here is gated on the `tls` cargo feature, which is **on by
//! default**. With the feature disabled the [`TlsAcceptor`] type still exists
//! (so dependent code keeps compiling) but every operation fails fast with a
//! [`tomcatrs_core::Error::Protocol`]; see the `cfg(not(feature = "tls"))`
//! block at the bottom of this file.

#[cfg(not(feature = "tls"))]
use std::path::Path;

use tomcatrs_core::Error;
#[cfg(not(feature = "tls"))]
use tomcatrs_core::Result;

/// The canonical "TLS is not available" error.
///
/// Retained as a stable helper for callers (e.g. [`crate::acceptor`]) that need
/// a uniform error when TLS cannot be used — either because the `tls` feature
/// was compiled out, or because a build step failed.
pub fn unsupported() -> Error {
    #[cfg(feature = "tls")]
    {
        Error::protocol("TLS support is compiled in but the connector could not be initialised")
    }
    #[cfg(not(feature = "tls"))]
    {
        Error::protocol("TLS support not compiled in; rebuild with --features tls")
    }
}

// ===========================================================================
// Feature-enabled implementation
// ===========================================================================

#[cfg(feature = "tls")]
mod imp {
    use std::path::Path;
    use std::sync::Arc;

    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use rustls::ServerConfig;
    use tokio::io::{AsyncRead, AsyncWrite};
    use tokio_rustls::server::TlsStream;

    use tomcatrs_core::{Error, Result};

    /// Map any `rustls` / `tokio-rustls` error onto [`Error::Protocol`].
    fn tls_err(context: &str, e: impl std::fmt::Display) -> Error {
        Error::protocol(format!("{context}: {e}"))
    }

    /// Load a PEM-encoded certificate chain from `path`.
    ///
    /// Every `CERTIFICATE` block in the file is returned, in file order, so the
    /// leaf certificate should appear first followed by any intermediates.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the file cannot be read and [`Error::Protocol`]
    /// if it contains no certificates or is not valid PEM.
    pub fn load_cert_chain(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
        let pem = std::fs::read(path).map_err(Error::Io)?;
        let mut reader = std::io::BufReader::new(&pem[..]);
        let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| tls_err("reading certificate chain", e))?;
        if certs.is_empty() {
            return Err(Error::protocol(format!(
                "no PEM certificates found in {}",
                path.display()
            )));
        }
        Ok(certs)
    }

    /// Load a PEM-encoded private key from `path`.
    ///
    /// The first key found is used. PKCS#8 (`PRIVATE KEY`), PKCS#1
    /// (`RSA PRIVATE KEY`), and SEC1 (`EC PRIVATE KEY`) encodings are all
    /// accepted.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the file cannot be read and [`Error::Protocol`]
    /// if it contains no usable private key or is not valid PEM.
    pub fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
        let pem = std::fs::read(path).map_err(Error::Io)?;
        let mut reader = std::io::BufReader::new(&pem[..]);
        let key = rustls_pemfile::private_key(&mut reader)
            .map_err(|e| tls_err("reading private key", e))?;
        key.ok_or_else(|| {
            Error::protocol(format!(
                "no PKCS#8 / PKCS#1 / SEC1 private key found in {}",
                path.display()
            ))
        })
    }

    /// Build a shared [`rustls::ServerConfig`] from a [`tomcatrs_config::TlsConfig`].
    ///
    /// The certificate chain and private key are loaded from the configured
    /// files, and `alpn` is installed as the advertised ALPN protocol list (for
    /// example `[b"h2".to_vec(), b"http/1.1".to_vec()]`). The resulting config
    /// uses rustls' safe defaults, which negotiate TLS 1.3 and TLS 1.2 only.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Protocol`] if the certificate/key files cannot be parsed
    /// or the key does not match the certificate, and [`Error::Io`] if a file
    /// cannot be read.
    pub fn build_server_config(
        tls: &tomcatrs_config::TlsConfig,
        alpn: &[Vec<u8>],
    ) -> Result<Arc<ServerConfig>> {
        let certs = load_cert_chain(&tls.cert_file)?;
        let key = load_private_key(&tls.key_file)?;

        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| tls_err("building rustls server config", e))?;

        config.alpn_protocols = alpn.to_vec();

        Ok(Arc::new(config))
    }

    /// A TLS terminator: wraps [`tokio_rustls::TlsAcceptor`] and performs the
    /// server-side handshake on accepted streams.
    ///
    /// Construct one with [`TlsAcceptor::new`] from a config built by
    /// [`build_server_config`], then call [`TlsAcceptor::accept`] per connection.
    #[derive(Clone)]
    pub struct TlsAcceptor {
        inner: tokio_rustls::TlsAcceptor,
    }

    impl std::fmt::Debug for TlsAcceptor {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("TlsAcceptor").finish_non_exhaustive()
        }
    }

    impl TlsAcceptor {
        /// Create a TLS acceptor from a prepared [`rustls::ServerConfig`].
        pub fn new(config: Arc<ServerConfig>) -> Self {
            TlsAcceptor {
                inner: tokio_rustls::TlsAcceptor::from(config),
            }
        }

        /// Perform the TLS handshake on `stream`, returning the encrypted
        /// [`TlsStream`] ready for plaintext I/O.
        ///
        /// The stream may be any `AsyncRead + AsyncWrite` — a `TcpStream` in
        /// production, or an in-memory duplex pipe in tests. After this returns
        /// `Ok`, the negotiated ALPN protocol (if any) can be read with
        /// [`alpn_protocol`].
        ///
        /// # Errors
        ///
        /// Returns [`Error::Protocol`] if the handshake fails (bad client,
        /// certificate problem, protocol mismatch, premature EOF, ...).
        pub async fn accept<S>(&self, stream: S) -> Result<TlsStream<S>>
        where
            S: AsyncRead + AsyncWrite + Unpin,
        {
            self.inner
                .accept(stream)
                .await
                .map_err(|e| tls_err("TLS handshake failed", e))
        }
    }

    /// Return the ALPN protocol negotiated on an already-handshaken
    /// [`TlsStream`], or `None` if the peer did not negotiate one.
    ///
    /// Typical values are `b"h2"` and `b"http/1.1"`.
    pub fn alpn_protocol<S>(stream: &TlsStream<S>) -> Option<&[u8]> {
        let (_, conn) = stream.get_ref();
        conn.alpn_protocol()
    }
}

#[cfg(feature = "tls")]
pub use imp::{alpn_protocol, build_server_config, load_cert_chain, load_private_key, TlsAcceptor};

// ===========================================================================
// Feature-disabled stub
// ===========================================================================

/// Inert [`TlsAcceptor`] used when the `tls` feature is disabled.
///
/// The type name is preserved so dependent code keeps compiling, but every
/// operation returns [`Error::Protocol`] instructing the operator to rebuild
/// with `--features tls`.
#[cfg(not(feature = "tls"))]
#[derive(Debug, Clone)]
pub struct TlsAcceptor {
    _private: (),
}

#[cfg(not(feature = "tls"))]
impl TlsAcceptor {
    /// Always fails: TLS support was compiled out.
    ///
    /// The signature deliberately accepts a unit so callers do not need a
    /// `rustls` type in scope.
    ///
    /// # Errors
    ///
    /// Always returns [`Error::Protocol`].
    pub fn new(_config: ()) -> Result<Self> {
        Err(Error::protocol(
            "TLS support not compiled in; rebuild with --features tls",
        ))
    }

    /// Always fails: TLS support was compiled out.
    ///
    /// # Errors
    ///
    /// Always returns [`Error::Protocol`].
    pub async fn accept<S>(&self, _stream: S) -> Result<S> {
        Err(Error::protocol(
            "TLS support not compiled in; rebuild with --features tls",
        ))
    }
}

/// Stub for the feature-disabled build: TLS material cannot be loaded.
///
/// # Errors
///
/// Always returns [`Error::Protocol`].
#[cfg(not(feature = "tls"))]
pub fn build_server_config(_tls: &tomcatrs_config::TlsConfig, _alpn: &[Vec<u8>]) -> Result<()> {
    Err(Error::protocol(
        "TLS support not compiled in; rebuild with --features tls",
    ))
}

/// Stub for the feature-disabled build.
///
/// # Errors
///
/// Always returns [`Error::Protocol`].
#[cfg(not(feature = "tls"))]
pub fn load_cert_chain(_path: &Path) -> Result<()> {
    Err(Error::protocol(
        "TLS support not compiled in; rebuild with --features tls",
    ))
}

/// Stub for the feature-disabled build.
///
/// # Errors
///
/// Always returns [`Error::Protocol`].
#[cfg(not(feature = "tls"))]
pub fn load_private_key(_path: &Path) -> Result<()> {
    Err(Error::protocol(
        "TLS support not compiled in; rebuild with --features tls",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_is_a_protocol_error() {
        assert!(matches!(unsupported(), Error::Protocol(_)));
    }

    #[cfg(not(feature = "tls"))]
    #[test]
    fn stub_accept_and_config_fail() {
        let r = TlsAcceptor::new(());
        assert!(matches!(r, Err(Error::Protocol(_))));
    }

    #[cfg(feature = "tls")]
    mod tls_enabled {
        use std::io::Write;
        use std::sync::Arc;

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio_rustls::rustls::pki_types::ServerName;

        use super::*;

        /// Generate a self-signed cert + key for `localhost` and write them to
        /// freshly-created temp PEM files. Returns the two temp file handles
        /// (kept alive by the caller) plus the parsed DER cert for the client
        /// trust store.
        fn self_signed_pem() -> (
            tempfile::NamedTempFile,
            tempfile::NamedTempFile,
            rustls::pki_types::CertificateDer<'static>,
        ) {
            let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
                .expect("generate self-signed cert");
            let cert_pem = cert.cert.pem();
            let key_pem = cert.key_pair.serialize_pem();

            let mut cert_file = tempfile::NamedTempFile::new().expect("temp cert file");
            cert_file
                .write_all(cert_pem.as_bytes())
                .expect("write cert pem");
            cert_file.flush().expect("flush cert pem");

            let mut key_file = tempfile::NamedTempFile::new().expect("temp key file");
            key_file
                .write_all(key_pem.as_bytes())
                .expect("write key pem");
            key_file.flush().expect("flush key pem");

            let der = cert.cert.der().clone();
            (cert_file, key_file, der)
        }

        #[test]
        fn load_private_key_reads_pkcs8() {
            let (_cert_file, key_file, _der) = self_signed_pem();
            // rcgen emits PKCS#8 `PRIVATE KEY` blocks.
            let key = load_private_key(key_file.path()).expect("load pkcs#8 key");
            assert!(matches!(key, rustls::pki_types::PrivateKeyDer::Pkcs8(_)));
        }

        #[test]
        fn load_cert_chain_reads_leaf() {
            let (cert_file, _key_file, _der) = self_signed_pem();
            let chain = load_cert_chain(cert_file.path()).expect("load cert chain");
            assert_eq!(chain.len(), 1);
        }

        #[test]
        fn load_cert_chain_rejects_empty() {
            let empty = tempfile::NamedTempFile::new().unwrap();
            assert!(matches!(
                load_cert_chain(empty.path()),
                Err(Error::Protocol(_))
            ));
        }

        #[tokio::test]
        async fn real_handshake_negotiates_alpn_over_duplex() {
            let (cert_file, key_file, cert_der) = self_signed_pem();
            let tls_cfg = tomcatrs_config::TlsConfig {
                cert_file: cert_file.path().to_path_buf(),
                key_file: key_file.path().to_path_buf(),
            };

            // Server advertises h2 then http/1.1.
            let server_cfg = build_server_config(&tls_cfg, &[b"h2".to_vec(), b"http/1.1".to_vec()])
                .expect("build server config");
            let acceptor = TlsAcceptor::new(server_cfg);

            // Client trusts the self-signed cert and offers only http/1.1, so
            // that protocol must be the negotiated result.
            let mut roots = rustls::RootCertStore::empty();
            roots.add(cert_der).expect("add root");
            let mut client_cfg = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            client_cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
            let connector = tokio_rustls::TlsConnector::from(Arc::new(client_cfg));

            let (client_io, server_io) = tokio::io::duplex(16 * 1024);

            let server = tokio::spawn(async move {
                let mut tls = acceptor.accept(server_io).await.expect("server handshake");
                let negotiated = alpn_protocol(&tls).map(|p| p.to_vec());
                // Echo one byte so the client can confirm a working channel.
                let mut buf = [0u8; 1];
                tls.read_exact(&mut buf).await.expect("server read");
                tls.write_all(&buf).await.expect("server write");
                tls.flush().await.expect("server flush");
                negotiated
            });

            let server_name = ServerName::try_from("localhost").unwrap();
            let mut client_tls = connector
                .connect(server_name, client_io)
                .await
                .expect("client handshake");

            // Client side ALPN view.
            let client_alpn = client_tls.get_ref().1.alpn_protocol().map(|p| p.to_vec());
            assert_eq!(client_alpn.as_deref(), Some(&b"http/1.1"[..]));

            client_tls.write_all(b"x").await.expect("client write");
            client_tls.flush().await.expect("client flush");
            let mut echo = [0u8; 1];
            client_tls.read_exact(&mut echo).await.expect("client read");
            assert_eq!(&echo, b"x");

            let server_alpn = server.await.expect("join server");
            assert_eq!(server_alpn.as_deref(), Some(&b"http/1.1"[..]));
        }
    }
}
