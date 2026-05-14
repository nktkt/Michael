//! RFC 6455 §4 — the WebSocket opening handshake (server side).
//!
//! A WebSocket connection begins life as an ordinary HTTP/1.1 request carrying
//! an `Upgrade: websocket` header. The server validates the request and proves
//! it understood the protocol by echoing a transformed copy of the client's
//! `Sec-WebSocket-Key` back in the `Sec-WebSocket-Accept` header.
//!
//! This module is fully working: [`accept_key`] performs the key derivation and
//! [`handshake_response`] performs request validation plus response synthesis.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use sha1::{Digest, Sha1};
use tomcatrs_core::{Error, Result};

/// The "magic" GUID from RFC 6455 §4.2.2, concatenated with the client key
/// before hashing. It is a fixed protocol constant and never changes.
pub const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Derive the `Sec-WebSocket-Accept` value from a client's `Sec-WebSocket-Key`.
///
/// The algorithm (RFC 6455 §4.2.2, step 5) is:
///
/// 1. Concatenate the trimmed client key with [`WEBSOCKET_GUID`].
/// 2. Take the SHA-1 digest of that ASCII string.
/// 3. Base64-encode the 20-byte digest.
///
/// # Examples
///
/// ```
/// use tomcatrs_websocket::handshake::accept_key;
///
/// assert_eq!(
///     accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
///     "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=",
/// );
/// ```
pub fn accept_key(client_key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(client_key.trim().as_bytes());
    hasher.update(WEBSOCKET_GUID.as_bytes());
    let digest = hasher.finalize();
    BASE64.encode(digest)
}

/// A validated, ready-to-serialize `101 Switching Protocols` response.
///
/// Construct one with [`handshake_response`]; turn it into bytes with
/// [`HandshakeResponse::to_http`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeResponse {
    /// The computed `Sec-WebSocket-Accept` header value.
    pub accept: String,
    /// The negotiated sub-protocol, if the client offered one we echo back.
    ///
    /// This crate performs no sub-protocol *selection* policy — that belongs to
    /// the application layer — so it stays `None` unless explicitly set.
    pub protocol: Option<String>,
}

impl HandshakeResponse {
    /// Serialize this response as a complete HTTP/1.1 message, ready to write
    /// to the socket. The message is terminated with the required blank line.
    pub fn to_http(&self) -> String {
        let mut out = String::with_capacity(160);
        out.push_str("HTTP/1.1 101 Switching Protocols\r\n");
        out.push_str("Upgrade: websocket\r\n");
        out.push_str("Connection: Upgrade\r\n");
        out.push_str("Sec-WebSocket-Accept: ");
        out.push_str(&self.accept);
        out.push_str("\r\n");
        if let Some(proto) = &self.protocol {
            out.push_str("Sec-WebSocket-Protocol: ");
            out.push_str(proto);
            out.push_str("\r\n");
        }
        out.push_str("\r\n");
        out
    }
}

/// Look up a header by name, case-insensitively, in a slice of `(name, value)`
/// pairs. HTTP header field names are case-insensitive (RFC 9110 §5.1).
fn header<'a>(headers: &'a [(&'a str, &'a str)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| *v)
}

/// Validate a client upgrade request and build the matching [`HandshakeResponse`].
///
/// `headers` is the request's header block as `(name, value)` pairs. The
/// following are checked, per RFC 6455 §4.2.1:
///
/// * `Upgrade` contains the token `websocket` (case-insensitive).
/// * `Connection` contains the token `Upgrade` (case-insensitive).
/// * `Sec-WebSocket-Version` is exactly `13`.
/// * `Sec-WebSocket-Key` is present and non-empty.
///
/// Any failure yields [`Error::Protocol`]. On success the returned response
/// carries the derived [`accept_key`] value.
pub fn handshake_response(headers: &[(&str, &str)]) -> Result<HandshakeResponse> {
    let upgrade = header(headers, "Upgrade")
        .ok_or_else(|| Error::protocol("websocket handshake: missing Upgrade header"))?;
    if !upgrade
        .split(',')
        .any(|tok| tok.trim().eq_ignore_ascii_case("websocket"))
    {
        return Err(Error::protocol(format!(
            "websocket handshake: Upgrade header must offer 'websocket', got '{upgrade}'"
        )));
    }

    let connection = header(headers, "Connection")
        .ok_or_else(|| Error::protocol("websocket handshake: missing Connection header"))?;
    if !connection
        .split(',')
        .any(|tok| tok.trim().eq_ignore_ascii_case("upgrade"))
    {
        return Err(Error::protocol(format!(
            "websocket handshake: Connection header must contain 'Upgrade', got '{connection}'"
        )));
    }

    let version = header(headers, "Sec-WebSocket-Version")
        .ok_or_else(|| Error::protocol("websocket handshake: missing Sec-WebSocket-Version"))?;
    if version.trim() != "13" {
        return Err(Error::protocol(format!(
            "websocket handshake: unsupported Sec-WebSocket-Version '{version}', expected 13"
        )));
    }

    let key = header(headers, "Sec-WebSocket-Key")
        .ok_or_else(|| Error::protocol("websocket handshake: missing Sec-WebSocket-Key"))?;
    if key.trim().is_empty() {
        return Err(Error::protocol(
            "websocket handshake: empty Sec-WebSocket-Key",
        ));
    }

    Ok(HandshakeResponse {
        accept: accept_key(key),
        protocol: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_key_matches_rfc_6455_example() {
        // The canonical example from RFC 6455 §1.3.
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn accept_key_trims_whitespace() {
        assert_eq!(
            accept_key("  dGhlIHNhbXBsZSBub25jZQ==  "),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn handshake_response_accepts_valid_request() {
        let headers = [
            ("Host", "example.com"),
            ("Upgrade", "websocket"),
            ("Connection", "Upgrade"),
            ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("Sec-WebSocket-Version", "13"),
        ];
        let resp = handshake_response(&headers).expect("valid handshake");
        assert_eq!(resp.accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        let http = resp.to_http();
        assert!(http.starts_with("HTTP/1.1 101 Switching Protocols\r\n"));
        assert!(http.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"));
        assert!(http.ends_with("\r\n\r\n"));
    }

    #[test]
    fn handshake_response_handles_combined_connection_token() {
        let headers = [
            ("Upgrade", "websocket"),
            ("Connection", "keep-alive, Upgrade"),
            ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("Sec-WebSocket-Version", "13"),
        ];
        assert!(handshake_response(&headers).is_ok());
    }

    #[test]
    fn handshake_response_rejects_bad_version() {
        let headers = [
            ("Upgrade", "websocket"),
            ("Connection", "Upgrade"),
            ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("Sec-WebSocket-Version", "8"),
        ];
        assert!(handshake_response(&headers).is_err());
    }

    #[test]
    fn handshake_response_rejects_missing_upgrade() {
        let headers = [
            ("Connection", "Upgrade"),
            ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("Sec-WebSocket-Version", "13"),
        ];
        assert!(handshake_response(&headers).is_err());
    }
}
