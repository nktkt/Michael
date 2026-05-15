//! AJP/1.3 (Apache JServ Protocol) connector — a working server-side
//! implementation.
//!
//! AJP is the compact binary protocol Tomcat speaks to a fronting
//! `httpd`/`nginx` via `mod_jk` / `mod_proxy_ajp`. This module implements the
//! **container side**: it reads `Forward Request` packets off a connection,
//! reconstructs a protocol-agnostic [`crate::Request`], runs it through an
//! [`Adapter`], and streams the [`crate::Response`] back as `Send Headers` +
//! `Send Body Chunk` + `End Response`.
//!
//! # Wire format
//!
//! Every AJP packet is length-prefixed:
//!
//! ```text
//! server → container:   0x12 0x34  <u16 len>  <payload>
//! container → server:   'A'  'B'   <u16 len>  <payload>
//! ```
//!
//! `len` counts only the payload bytes. The first payload byte is normally the
//! message type code; the exception is a *body data* packet from the server,
//! whose payload is a raw length-prefixed byte chunk with no type code.
//!
//! # Security posture — read this before deploying
//!
//! AJP is **clear-text and unauthenticated by design**. It exists to be spoken
//! across a trusted link between a reverse proxy and the container, and it
//! grants the peer the ability to set request attributes that map directly onto
//! servlet-visible state. A misconfigured AJP connector reachable from an
//! untrusted network is the root cause of **CVE-2020-1938 ("Ghostcat")**, which
//! let an attacker turn arbitrary-file-read into remote code execution by
//! injecting the `javax.servlet.include.*` request attributes.
//!
//! This implementation therefore:
//!
//! * **Enforces a shared secret.** When a `secret` is configured (Tomcat's
//!   `requiredSecret` / `required-secret` connector attribute), every forwarded
//!   request must carry a matching `secret` attribute (`0x0C`) or it is
//!   rejected before the [`Adapter`] is ever invoked. The comparison is
//!   length-independent to avoid leaking timing information.
//! * **Refuses to forward unknown / dangerous attributes.** Only an explicit
//!   allow-list of AJP attributes is honoured. The `req_attribute` (`0x0A`)
//!   generic name/value mechanism — the exact Ghostcat vector — is parsed but
//!   **dropped**, never surfaced to the servlet container as an arbitrary
//!   attribute. `remote_user`, `auth_type`, `query_string`, `route`,
//!   `ssl_cert`, and the other typed attributes are mapped onto well-defined
//!   fields only.
//! * **Validates the request URI.** A URI containing NUL, control characters,
//!   or non-UTF-8 bytes is rejected; the path is normalized with the same
//!   dot-segment collapsing the HTTP/1.1 connector uses, so AJP cannot escape
//!   the document root either.
//!
//! Operators should additionally bind AJP connectors to a loopback or private
//! address — never `0.0.0.0` — and keep a `secret` configured.

use std::net::SocketAddr;

use bytes::{BufMut, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use tomcatrs_core::{Error, Result};

use crate::normalize::normalize_target;
use crate::{Adapter, Request, Response};

/// Maximum AJP payload length. The 2-byte length prefix caps a packet at
/// 65535 bytes; Tomcat's own default `packetSize` is 8 KiB. We accept up to the
/// protocol maximum but never emit a body chunk larger than [`MAX_SEND_CHUNK`].
const MAX_PACKET_PAYLOAD: usize = 65535;

/// Largest response body chunk we put in a single `Send Body Chunk` packet.
///
/// A `Send Body Chunk` payload is `type(1) + len(2) + data + NUL(1)`, so the
/// data is capped a little under the 65535-byte packet limit.
const MAX_SEND_CHUNK: usize = 8192;

/// Magic prefix on packets travelling **server → container** (`0x12 0x34`).
const MAGIC_IN: [u8; 2] = [0x12, 0x34];

/// Magic prefix on packets travelling **container → server** (`AB`).
const MAGIC_OUT: [u8; 2] = [b'A', b'B'];

// ---------------------------------------------------------------------------
// Message type codes
// ---------------------------------------------------------------------------

/// AJP packet type codes.
///
/// The numeric values are the on-the-wire bytes. Direction is noted per
/// variant; a couple of codes are reused in both directions historically but
/// AJP/1.3 keeps them distinct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AjpMessageType {
    /// `0x02` — server → container: a forwarded request.
    ForwardRequest,
    /// `0x07` — server → container: orderly shutdown request.
    Shutdown,
    /// `0x08` — server → container: CPing keep-alive probe.
    CPing,
    /// `0x09` — container → server: CPong reply to a [`CPing`](Self::CPing).
    CPong,
    /// `0x03` — container → server: a chunk of response body.
    SendBodyChunk,
    /// `0x04` — container → server: response status line + headers.
    SendHeaders,
    /// `0x05` — container → server: the response is complete.
    EndResponse,
    /// `0x06` — container → server: "send me more request body".
    GetBodyChunk,
}

impl AjpMessageType {
    /// The on-the-wire byte for this message type.
    pub fn code(self) -> u8 {
        match self {
            AjpMessageType::ForwardRequest => 0x02,
            AjpMessageType::Shutdown => 0x07,
            AjpMessageType::CPing => 0x08,
            AjpMessageType::CPong => 0x09,
            AjpMessageType::SendBodyChunk => 0x03,
            AjpMessageType::SendHeaders => 0x04,
            AjpMessageType::EndResponse => 0x05,
            AjpMessageType::GetBodyChunk => 0x06,
        }
    }

    /// Decode a server → container type byte, if recognised.
    ///
    /// Only the codes a container can legitimately *receive* are mapped here;
    /// container → server codes return `None`.
    pub fn from_code(code: u8) -> Option<AjpMessageType> {
        match code {
            0x02 => Some(AjpMessageType::ForwardRequest),
            0x07 => Some(AjpMessageType::Shutdown),
            0x08 => Some(AjpMessageType::CPing),
            _ => None,
        }
    }
}

/// Translate an AJP method code into its HTTP method name.
///
/// AJP encodes the request method as a single byte drawn from a fixed table
/// (see `org.apache.coyote.ajp.Constants`). Returns `None` for an unknown code;
/// the caller rejects such requests rather than guessing.
pub fn ajp_method_name(code: u8) -> Option<&'static str> {
    Some(match code {
        1 => "OPTIONS",
        2 => "GET",
        3 => "HEAD",
        4 => "POST",
        5 => "PUT",
        6 => "DELETE",
        7 => "TRACE",
        8 => "PROPFIND",
        9 => "PROPPATCH",
        10 => "MKCOL",
        11 => "COPY",
        12 => "MOVE",
        13 => "LOCK",
        14 => "UNLOCK",
        15 => "ACL",
        16 => "REPORT",
        17 => "VERSION-CONTROL",
        18 => "CHECKIN",
        19 => "CHECKOUT",
        20 => "UNCHECKOUT",
        21 => "SEARCH",
        22 => "MKWORKSPACE",
        23 => "UPDATE",
        24 => "LABEL",
        25 => "MERGE",
        26 => "BASELINE-CONTROL",
        27 => "MKACTIVITY",
        _ => return None,
    })
}

/// Translate an AJP common-header code (`0xA0xx`) into the HTTP header name.
///
/// AJP compresses the most common request headers to a 2-byte code; any other
/// header is sent as a length-prefixed string. Returns `None` if `code` is not
/// a known common-header code.
pub fn ajp_request_header_name(code: u16) -> Option<&'static str> {
    Some(match code {
        0xA001 => "accept",
        0xA002 => "accept-charset",
        0xA003 => "accept-encoding",
        0xA004 => "accept-language",
        0xA005 => "authorization",
        0xA006 => "connection",
        0xA007 => "content-type",
        0xA008 => "content-length",
        0xA009 => "cookie",
        0xA00A => "cookie2",
        0xA00B => "host",
        0xA00C => "pragma",
        0xA00D => "referer",
        0xA00E => "user-agent",
        _ => return None,
    })
}

/// The AJP common-header code for a *response* header, if one exists.
///
/// `Send Headers` may compress these response header names the same way the
/// request side compresses request headers. We always send full strings
/// (simpler and equally valid), but the table is exposed for completeness and
/// testing.
pub fn ajp_response_header_code(name: &str) -> Option<u16> {
    Some(match () {
        _ if name.eq_ignore_ascii_case("content-type") => 0xA001,
        _ if name.eq_ignore_ascii_case("content-language") => 0xA002,
        _ if name.eq_ignore_ascii_case("content-length") => 0xA003,
        _ if name.eq_ignore_ascii_case("date") => 0xA004,
        _ if name.eq_ignore_ascii_case("last-modified") => 0xA005,
        _ if name.eq_ignore_ascii_case("location") => 0xA006,
        _ if name.eq_ignore_ascii_case("set-cookie") => 0xA007,
        _ if name.eq_ignore_ascii_case("set-cookie2") => 0xA008,
        _ if name.eq_ignore_ascii_case("servlet-engine") => 0xA009,
        _ if name.eq_ignore_ascii_case("status") => 0xA00A,
        _ if name.eq_ignore_ascii_case("www-authenticate") => 0xA00B,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// AjpMessage — framed packet read/write
// ---------------------------------------------------------------------------

/// One framed AJP packet: the magic prefix is stripped, leaving just the
/// length-prefixed payload.
///
/// `AjpMessage` is a thin cursor over an owned payload buffer. Reading routines
/// (`read_u8`, `read_u16`, `read_string`, `read_bytes`) advance an internal
/// position; writing routines append to the payload. A finished message is
/// serialised with [`AjpMessage::encode`], which prepends the requested magic
/// and the 2-byte length.
#[derive(Debug, Clone, Default)]
pub struct AjpMessage {
    /// The packet payload, *excluding* magic and length prefix.
    payload: BytesMut,
    /// Current read cursor into `payload`.
    pos: usize,
}

impl AjpMessage {
    /// Create an empty message ready to be written into.
    pub fn new() -> Self {
        AjpMessage {
            payload: BytesMut::new(),
            pos: 0,
        }
    }

    /// Wrap an already-decoded payload (magic + length already stripped).
    pub fn from_payload(payload: impl Into<BytesMut>) -> Self {
        AjpMessage {
            payload: payload.into(),
            pos: 0,
        }
    }

    /// The full payload bytes, regardless of read position.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Bytes remaining to be read from the current cursor.
    pub fn remaining(&self) -> usize {
        self.payload.len().saturating_sub(self.pos)
    }

    /// The message type code: the first payload byte.
    ///
    /// Returns `None` for an empty payload.
    pub fn message_type(&self) -> Option<u8> {
        self.payload.first().copied()
    }

    // ---- read side -------------------------------------------------------

    /// Read a single byte, advancing the cursor.
    fn read_u8(&mut self) -> Result<u8> {
        if self.pos >= self.payload.len() {
            return Err(truncated("u8"));
        }
        let b = self.payload[self.pos];
        self.pos += 1;
        Ok(b)
    }

    /// Read a big-endian `u16`, advancing the cursor.
    fn read_u16(&mut self) -> Result<u16> {
        if self.pos + 2 > self.payload.len() {
            return Err(truncated("u16"));
        }
        let v = u16::from_be_bytes([self.payload[self.pos], self.payload[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    /// Read `n` raw bytes, advancing the cursor.
    fn read_bytes(&mut self, n: usize) -> Result<Bytes> {
        if self.pos + n > self.payload.len() {
            return Err(truncated("bytes"));
        }
        let out = Bytes::copy_from_slice(&self.payload[self.pos..self.pos + n]);
        self.pos += n;
        Ok(out)
    }

    /// Read an AJP string: a big-endian `u16` length, that many bytes, and a
    /// trailing NUL terminator. A length of `0xFFFF` denotes a *null* string
    /// and yields `None`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Protocol`] if the buffer is truncated, the NUL
    /// terminator is missing, or the bytes are not valid UTF-8.
    fn read_string(&mut self) -> Result<Option<String>> {
        let len = self.read_u16()?;
        if len == 0xFFFF {
            return Ok(None);
        }
        let raw = self.read_bytes(len as usize)?;
        // Consume the mandatory NUL terminator.
        let nul = self.read_u8()?;
        if nul != 0 {
            return Err(Error::protocol("AJP string missing NUL terminator"));
        }
        let s = std::str::from_utf8(&raw)
            .map_err(|_| Error::protocol("AJP string is not valid UTF-8"))?
            .to_string();
        Ok(Some(s))
    }

    /// Like [`read_string`](Self::read_string) but treats a null string as an
    /// error — used where AJP guarantees a value is present.
    fn read_required_string(&mut self) -> Result<String> {
        self.read_string()?
            .ok_or_else(|| Error::protocol("expected AJP string, found null"))
    }

    // ---- write side ------------------------------------------------------

    /// Append a single byte.
    fn write_u8(&mut self, b: u8) {
        self.payload.put_u8(b);
    }

    /// Append a big-endian `u16`.
    fn write_u16(&mut self, v: u16) {
        self.payload.put_u16(v);
    }

    /// Append an AJP string: `u16` length + bytes + NUL. `None` is encoded as
    /// the `0xFFFF` null marker with no body.
    fn write_string(&mut self, s: Option<&str>) {
        match s {
            None => self.write_u16(0xFFFF),
            Some(s) => {
                self.write_u16(s.len() as u16);
                self.payload.put_slice(s.as_bytes());
                self.payload.put_u8(0);
            }
        }
    }

    /// Append raw bytes verbatim (no length prefix).
    fn write_raw(&mut self, b: &[u8]) {
        self.payload.put_slice(b);
    }

    /// Serialise this message into a complete, framed packet.
    ///
    /// `magic` is `MAGIC_IN` or `MAGIC_OUT`. The returned buffer is
    /// `magic (2) + length (2) + payload`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Protocol`] if the payload exceeds `MAX_PACKET_PAYLOAD`.
    pub fn encode(&self, magic: [u8; 2]) -> Result<Bytes> {
        if self.payload.len() > MAX_PACKET_PAYLOAD {
            return Err(Error::protocol("AJP packet payload exceeds 65535 bytes"));
        }
        let mut out = BytesMut::with_capacity(4 + self.payload.len());
        out.put_slice(&magic);
        out.put_u16(self.payload.len() as u16);
        out.put_slice(&self.payload);
        Ok(out.freeze())
    }

    /// Read one full framed packet from `stream`, validating the magic prefix.
    ///
    /// Returns `Ok(None)` on a clean EOF *before any bytes of a packet were
    /// read* (the peer closed an idle connection). A partial packet followed by
    /// EOF is an error.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Protocol`] on a bad magic prefix or a truncated packet,
    /// and [`Error::Io`] on a socket failure.
    pub async fn read_from<S>(stream: &mut S, expected_magic: [u8; 2]) -> Result<Option<AjpMessage>>
    where
        S: AsyncRead + Unpin,
    {
        let mut header = [0u8; 4];
        // Read the 4-byte frame header, tolerating a clean EOF on the very
        // first byte.
        match stream.read(&mut header[..1]).await {
            Ok(0) => return Ok(None),
            Ok(_) => {}
            Err(e) => return Err(Error::Io(e)),
        }
        stream
            .read_exact(&mut header[1..])
            .await
            .map_err(Error::Io)?;

        if header[0] != expected_magic[0] || header[1] != expected_magic[1] {
            return Err(Error::protocol("AJP packet has invalid magic prefix"));
        }
        let len = u16::from_be_bytes([header[2], header[3]]) as usize;
        let mut payload = vec![0u8; len];
        if len > 0 {
            stream.read_exact(&mut payload).await.map_err(Error::Io)?;
        }
        Ok(Some(AjpMessage::from_payload(BytesMut::from(&payload[..]))))
    }

    /// Encode this message and write it to `stream`, then flush.
    pub async fn write_to<S>(&self, stream: &mut S, magic: [u8; 2]) -> Result<()>
    where
        S: AsyncWrite + Unpin,
    {
        let framed = self.encode(magic)?;
        stream.write_all(&framed).await.map_err(Error::Io)?;
        stream.flush().await.map_err(Error::Io)?;
        Ok(())
    }
}

/// Build a "truncated packet" protocol error.
fn truncated(what: &str) -> Error {
    Error::protocol(format!("AJP packet truncated while reading {what}"))
}

// ---------------------------------------------------------------------------
// Forward Request decoding
// ---------------------------------------------------------------------------

/// A decoded `Forward Request` packet.
///
/// This is the structured result of parsing a type-`0x02` message: enough to
/// build a [`Request`], plus the protocol-level facts (`is_ssl`,
/// `content_length`, the validated `secret`) the connection driver needs.
#[derive(Debug, Clone)]
pub struct ForwardRequest {
    /// HTTP method name (e.g. `GET`).
    pub method: String,
    /// HTTP version token (e.g. `HTTP/1.1`).
    pub protocol: String,
    /// Request URI path as sent by the proxy (not yet normalized).
    pub req_uri: String,
    /// Remote client address string.
    pub remote_addr: String,
    /// Remote client host name, if the proxy resolved one.
    pub remote_host: Option<String>,
    /// `Host`-equivalent server name.
    pub server_name: String,
    /// Server port the proxy believes it is fronting.
    pub server_port: u16,
    /// Whether the original client connection was TLS.
    pub is_ssl: bool,
    /// Request headers, in arrival order, names lower-cased.
    pub headers: Vec<(String, String)>,
    /// Query string (from the `query_string` attribute), if any.
    pub query_string: Option<String>,
    /// `remote_user` attribute, if the proxy authenticated the client.
    pub remote_user: Option<String>,
    /// `auth_type` attribute (e.g. `BASIC`), if any.
    pub auth_type: Option<String>,
    /// `route` attribute used for sticky-session load balancing.
    pub route: Option<String>,
    /// `ssl_cert` attribute: the client certificate in PEM form.
    pub ssl_cert: Option<String>,
    /// The `secret` attribute, if the proxy supplied one. Validated by the
    /// connection driver against the configured secret.
    pub secret: Option<String>,
    /// Declared request-body length, derived from the `Content-Length` header.
    /// `0` means no body.
    pub content_length: usize,
}

/// Attribute type codes that appear in a `Forward Request` packet.
mod attr {
    /// `?context` — unused by the servlet container; parsed and dropped.
    pub const CONTEXT: u8 = 0x01;
    /// `?servlet_path` — parsed and dropped.
    pub const SERVLET_PATH: u8 = 0x02;
    /// `?remote_user`.
    pub const REMOTE_USER: u8 = 0x03;
    /// `?auth_type`.
    pub const AUTH_TYPE: u8 = 0x04;
    /// `?query_string`.
    pub const QUERY_STRING: u8 = 0x05;
    /// `?route`.
    pub const ROUTE: u8 = 0x06;
    /// `?ssl_cert`.
    pub const SSL_CERT: u8 = 0x07;
    /// `?ssl_cipher`.
    pub const SSL_CIPHER: u8 = 0x08;
    /// `?ssl_session`.
    pub const SSL_SESSION: u8 = 0x09;
    /// `?req_attribute` — the generic name/value pair. **Security-sensitive:**
    /// this is the Ghostcat injection vector and is deliberately dropped.
    pub const REQ_ATTRIBUTE: u8 = 0x0A;
    /// `?ssl_key_size`.
    pub const SSL_KEY_SIZE: u8 = 0x0B;
    /// `?secret` — the shared secret a trusted proxy presents.
    pub const SECRET: u8 = 0x0C;
    /// `?stored_method` — the real method when it is not in the code table.
    pub const STORED_METHOD: u8 = 0x0D;
    /// Terminator: no more attributes.
    pub const ARE_DONE: u8 = 0xFF;
}

impl ForwardRequest {
    /// Decode a `Forward Request` from a freshly read [`AjpMessage`].
    ///
    /// The message's cursor must be positioned at the start of the payload; the
    /// type byte is consumed and verified here.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Protocol`] for a wrong message type, an unknown method
    /// code, a truncated buffer, or a malformed string/attribute.
    pub fn decode(msg: &mut AjpMessage) -> Result<ForwardRequest> {
        let ty = msg.read_u8()?;
        if ty != AjpMessageType::ForwardRequest.code() {
            return Err(Error::protocol(format!(
                "expected Forward Request (0x02), got 0x{ty:02x}"
            )));
        }

        let method_code = msg.read_u8()?;
        let mut method = ajp_method_name(method_code)
            .ok_or_else(|| Error::protocol(format!("unknown AJP method code {method_code}")))?
            .to_string();

        let protocol = msg.read_required_string()?;
        let req_uri = msg.read_required_string()?;
        let remote_addr = msg.read_required_string()?;
        let remote_host = msg.read_string()?;
        let server_name = msg.read_required_string()?;
        let server_port = msg.read_u16()?;
        let is_ssl = msg.read_u8()? != 0;

        // ---- headers ------------------------------------------------------
        let num_headers = msg.read_u16()? as usize;
        let mut headers: Vec<(String, String)> = Vec::with_capacity(num_headers);
        let mut content_length: usize = 0;
        for _ in 0..num_headers {
            // A header name is either a 0xA0xx common-header code or a normal
            // AJP string. The two are disambiguated by the first byte: 0xA0
            // marks a code.
            let first = msg.read_u8()?;
            let name: String = if first == 0xA0 {
                let second = msg.read_u8()?;
                let code = u16::from_be_bytes([first, second]);
                ajp_request_header_name(code)
                    .ok_or_else(|| {
                        Error::protocol(format!("unknown AJP header code 0x{code:04x}"))
                    })?
                    .to_string()
            } else {
                // `first` was the high byte of a u16 string length.
                let second = msg.read_u8()?;
                let len = u16::from_be_bytes([first, second]) as usize;
                let raw = msg.read_bytes(len)?;
                let nul = msg.read_u8()?;
                if nul != 0 {
                    return Err(Error::protocol("AJP header name missing NUL terminator"));
                }
                let s = std::str::from_utf8(&raw)
                    .map_err(|_| Error::protocol("AJP header name is not valid UTF-8"))?;
                s.to_ascii_lowercase()
            };
            let value = msg.read_required_string()?;
            if name == "content-length" {
                content_length = value.trim().parse().unwrap_or(0);
            }
            headers.push((name, value));
        }

        // ---- attributes ---------------------------------------------------
        let mut query_string = None;
        let mut remote_user = None;
        let mut auth_type = None;
        let mut route = None;
        let mut ssl_cert = None;
        let mut secret = None;
        loop {
            let code = msg.read_u8()?;
            match code {
                attr::ARE_DONE => break,
                attr::REMOTE_USER => remote_user = msg.read_string()?,
                attr::AUTH_TYPE => auth_type = msg.read_string()?,
                attr::QUERY_STRING => query_string = msg.read_string()?,
                attr::ROUTE => route = msg.read_string()?,
                attr::SSL_CERT => ssl_cert = msg.read_string()?,
                attr::SECRET => secret = msg.read_string()?,
                attr::STORED_METHOD => {
                    // The real method when it is outside the numeric table.
                    if let Some(m) = msg.read_string()? {
                        method = m;
                    }
                }
                attr::CONTEXT
                | attr::SERVLET_PATH
                | attr::SSL_CIPHER
                | attr::SSL_SESSION
                | attr::SSL_KEY_SIZE => {
                    // Recognised but not surfaced: consume the value and drop.
                    let _ = msg.read_string()?;
                }
                attr::REQ_ATTRIBUTE => {
                    // SECURITY: the generic name/value attribute is the
                    // Ghostcat (CVE-2020-1938) injection vector. We parse both
                    // halves to stay in sync with the wire, then discard them
                    // — a fronting proxy may NOT inject arbitrary servlet
                    // request attributes through this connector.
                    let name = msg.read_required_string()?;
                    let _value = msg.read_required_string()?;
                    tracing::warn!(
                        attribute = %name,
                        "dropping AJP req_attribute: arbitrary attribute injection is refused"
                    );
                }
                other => {
                    return Err(Error::protocol(format!(
                        "unknown AJP attribute code 0x{other:02x}"
                    )));
                }
            }
        }

        Ok(ForwardRequest {
            method,
            protocol,
            req_uri,
            remote_addr,
            remote_host,
            server_name,
            server_port,
            is_ssl,
            headers,
            query_string,
            remote_user,
            auth_type,
            route,
            ssl_cert,
            secret,
            content_length,
        })
    }

    /// Turn this decoded forward request into a protocol-agnostic [`Request`].
    ///
    /// The URI is validated (no NUL / control bytes) and normalized with the
    /// shared [`normalize_target`] routine so AJP requests cannot escape the
    /// document root. Typed attributes that have a natural header representation
    /// (`remote_user`, `auth_type`) are surfaced as conventional `X-Forwarded-*`
    /// / synthetic headers; nothing is injected as an arbitrary servlet
    /// attribute.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Protocol`] if the request URI is malformed.
    pub fn into_request(self, peer_addr: SocketAddr) -> Result<Request> {
        // Reject control characters / NUL in the URI outright.
        if self.req_uri.bytes().any(|b| b == 0 || b.is_ascii_control()) {
            return Err(Error::protocol("AJP request URI contains control bytes"));
        }

        // Reassemble a request target so normalize_target can do path +
        // dot-segment handling exactly as for HTTP/1.1.
        let target = match &self.query_string {
            Some(q) => format!("{}?{}", self.req_uri, q),
            None => self.req_uri.clone(),
        };
        let normalized = normalize_target(&target)
            .map_err(|e| Error::protocol(format!("AJP request URI rejected: {e}")))?;

        let mut headers = self.headers;
        // Ensure a Host header reflects the proxy's view of the server.
        if !headers.iter().any(|(k, _)| k == "host") {
            let host = if self.server_port == 80 || self.server_port == 443 {
                self.server_name.clone()
            } else {
                format!("{}:{}", self.server_name, self.server_port)
            };
            headers.push(("host".to_string(), host));
        }
        // Surface proxy-derived facts as well-defined synthetic headers only.
        if self.is_ssl {
            headers.push(("x-forwarded-proto".to_string(), "https".to_string()));
        }
        if let Some(rh) = &self.remote_host {
            headers.push(("x-forwarded-host".to_string(), rh.clone()));
        }
        if let Some(user) = &self.remote_user {
            headers.push(("x-tomcatrs-remote-user".to_string(), user.clone()));
        }
        if let Some(at) = &self.auth_type {
            headers.push(("x-tomcatrs-auth-type".to_string(), at.clone()));
        }
        if let Some(route) = &self.route {
            headers.push(("x-tomcatrs-route".to_string(), route.clone()));
        }
        if self.ssl_cert.is_some() {
            // Presence only — never echo the certificate into a header value.
            headers.push((
                "x-tomcatrs-ssl-client-cert".to_string(),
                "present".to_string(),
            ));
        }

        Ok(Request {
            method: self.method,
            uri: target,
            path: normalized.path,
            query: normalized.query,
            version: self.protocol,
            headers,
            body: Bytes::new(),
            peer_addr,
        })
    }
}

// ---------------------------------------------------------------------------
// Container → server message encoders
// ---------------------------------------------------------------------------

/// Encode a `Send Headers` (`0x04`) packet for `resp`.
///
/// Layout: `type(1) status(u16) status_msg(string) num_headers(u16)
/// [name value]*`. Header names are always sent as full AJP strings (the
/// common-header compression in [`ajp_response_header_code`] is optional and
/// omitted here for simplicity).
pub fn encode_send_headers(resp: &Response) -> Result<AjpMessage> {
    let mut msg = AjpMessage::new();
    msg.write_u8(AjpMessageType::SendHeaders.code());
    msg.write_u16(resp.status);
    msg.write_string(Some(reason_phrase(resp.status)));

    msg.write_u16(resp.headers.len() as u16);
    for (name, value) in &resp.headers {
        msg.write_string(Some(name));
        msg.write_string(Some(value));
    }
    Ok(msg)
}

/// Encode one `Send Body Chunk` (`0x03`) packet.
///
/// Layout: `type(1) len(u16) data NUL`. `data` must not exceed
/// `MAX_SEND_CHUNK`; callers split larger bodies across packets.
pub fn encode_send_body_chunk(data: &[u8]) -> Result<AjpMessage> {
    if data.len() > MAX_SEND_CHUNK {
        return Err(Error::protocol("AJP body chunk exceeds maximum size"));
    }
    let mut msg = AjpMessage::new();
    msg.write_u8(AjpMessageType::SendBodyChunk.code());
    msg.write_u16(data.len() as u16);
    msg.write_raw(data);
    msg.write_u8(0);
    Ok(msg)
}

/// Encode an `End Response` (`0x05`) packet.
///
/// `reuse` tells the proxy whether the connection may be kept alive for a
/// subsequent request.
pub fn encode_end_response(reuse: bool) -> AjpMessage {
    let mut msg = AjpMessage::new();
    msg.write_u8(AjpMessageType::EndResponse.code());
    msg.write_u8(u8::from(reuse));
    msg
}

/// Encode a `Get Body Chunk` (`0x06`) packet asking the proxy for up to
/// `requested` more bytes of request body.
pub fn encode_get_body_chunk(requested: u16) -> AjpMessage {
    let mut msg = AjpMessage::new();
    msg.write_u8(AjpMessageType::GetBodyChunk.code());
    msg.write_u16(requested);
    msg
}

/// Encode a `CPong` (`0x09`) reply to a CPing probe.
pub fn encode_cpong() -> AjpMessage {
    let mut msg = AjpMessage::new();
    msg.write_u8(AjpMessageType::CPong.code());
    msg
}

/// Map a status code to its canonical reason phrase.
///
/// Mirrors the HTTP/1.1 connector's table so both protocols report identical
/// phrases; unknown codes fall back to a generic phrase.
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Payload Too Large",
        414 => "URI Too Long",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        505 => "HTTP Version Not Supported",
        _ => "Status",
    }
}

/// Read a request *body data* packet from the proxy.
///
/// In response to a `Get Body Chunk`, the proxy sends a packet whose payload is
/// `len(u16) data` — note there is **no** message-type byte. A payload of zero
/// length (or an empty packet) signals end-of-body.
///
/// Returns the body bytes (possibly empty when the body is exhausted).
fn decode_body_data(msg: &AjpMessage) -> Result<Bytes> {
    let payload = msg.payload();
    if payload.len() < 2 {
        // An empty packet is the proxy's way of saying "no more body".
        return Ok(Bytes::new());
    }
    let len = u16::from_be_bytes([payload[0], payload[1]]) as usize;
    if payload.len() < 2 + len {
        return Err(Error::protocol("AJP body data packet truncated"));
    }
    Ok(Bytes::copy_from_slice(&payload[2..2 + len]))
}

/// Compare two secrets in constant time relative to the configured secret's
/// length, so a timing side-channel does not leak how many leading bytes
/// matched.
fn secret_matches(configured: &str, presented: Option<&str>) -> bool {
    let presented = match presented {
        Some(p) => p,
        None => return false,
    };
    let a = configured.as_bytes();
    let b = presented.as_bytes();
    // Length mismatch is itself a non-match, but still walk `a` fully.
    let mut diff = (a.len() ^ b.len()) as u8;
    for (i, &x) in a.iter().enumerate() {
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// AjpConnection — the per-connection driver
// ---------------------------------------------------------------------------

/// Drives a single AJP/1.3 connection from a fronting proxy.
///
/// The type is generic over any `AsyncRead + AsyncWrite` transport so it can be
/// exercised over a real `TcpStream` in production and over
/// [`tokio::io::duplex`] in tests.
#[derive(Debug)]
pub struct AjpConnection<S> {
    stream: S,
    peer_addr: SocketAddr,
}

impl<S> AjpConnection<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Serve a connection: loop reading `Forward Request` packets, dispatching
    /// each through `adapter`, and streaming the response back, until the proxy
    /// closes the connection or asks for shutdown.
    ///
    /// `secret` is the configured shared secret (`requiredSecret`); when `Some`,
    /// every forwarded request must present a matching `secret` attribute or it
    /// is rejected with `403` *before* the adapter is invoked.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] on an unrecoverable transport failure and
    /// [`Error::Protocol`] on a malformed packet. A request that merely fails
    /// validation (bad secret, bad URI) is answered with a 4xx response and the
    /// connection continues.
    pub async fn serve(
        stream: S,
        adapter: std::sync::Arc<dyn Adapter>,
        peer_addr: SocketAddr,
        secret: Option<&str>,
    ) -> Result<()> {
        let mut conn = AjpConnection { stream, peer_addr };
        conn.run(adapter.as_ref(), secret).await
    }

    /// The connection event loop.
    async fn run(&mut self, adapter: &dyn Adapter, secret: Option<&str>) -> Result<()> {
        loop {
            let mut msg = match AjpMessage::read_from(&mut self.stream, MAGIC_IN).await? {
                Some(m) => m,
                None => {
                    tracing::trace!(peer = %self.peer_addr, "AJP connection closed by peer");
                    return Ok(());
                }
            };

            let ty_byte = match msg.message_type() {
                Some(b) => b,
                None => return Err(Error::protocol("empty AJP packet")),
            };

            match AjpMessageType::from_code(ty_byte) {
                Some(AjpMessageType::CPing) => {
                    // Liveness probe: reply with CPong and keep the connection.
                    encode_cpong().write_to(&mut self.stream, MAGIC_OUT).await?;
                    continue;
                }
                Some(AjpMessageType::Shutdown) => {
                    tracing::info!(peer = %self.peer_addr, "AJP shutdown packet received");
                    return Ok(());
                }
                Some(AjpMessageType::ForwardRequest) => {
                    // fall through to request handling below
                }
                _ => {
                    return Err(Error::protocol(format!(
                        "unexpected AJP packet type 0x{ty_byte:02x}"
                    )));
                }
            }

            let fwd = ForwardRequest::decode(&mut msg)?;

            // ---- SECURITY: enforce the shared secret -----------------------
            if let Some(configured) = secret {
                if !secret_matches(configured, fwd.secret.as_deref()) {
                    tracing::warn!(
                        peer = %self.peer_addr,
                        "rejecting AJP request: missing or mismatched secret"
                    );
                    // Answer 403 and keep the connection so a momentarily
                    // misconfigured proxy can recover, matching Tomcat.
                    self.write_response(&Response::with_body(403, "Forbidden"))
                        .await?;
                    continue;
                }
            }

            let content_length = fwd.content_length;
            let mut request = match fwd.into_request(self.peer_addr) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(peer = %self.peer_addr, error = %e, "rejecting AJP request");
                    self.write_response(&Response::with_body(400, "Bad Request"))
                        .await?;
                    continue;
                }
            };

            // ---- pull the request body, if any -----------------------------
            if content_length > 0 {
                match self.read_body(content_length).await {
                    Ok(body) => request.body = body,
                    Err(e) => {
                        tracing::warn!(peer = %self.peer_addr, error = %e,
                            "failed to read AJP request body");
                        return Err(e);
                    }
                }
            }

            // ---- dispatch and stream the response back ---------------------
            let response = adapter.service(request).await;
            self.write_response(&response).await?;
            // AJP connections are persistent; loop for the next request.
        }
    }

    /// Read exactly `content_length` body bytes by issuing `Get Body Chunk`
    /// requests and consuming the proxy's body-data packets.
    async fn read_body(&mut self, content_length: usize) -> Result<Bytes> {
        let mut body = BytesMut::with_capacity(content_length.min(64 * 1024));
        while body.len() < content_length {
            let want = (content_length - body.len()).min(MAX_SEND_CHUNK) as u16;
            encode_get_body_chunk(want)
                .write_to(&mut self.stream, MAGIC_OUT)
                .await?;
            let msg = match AjpMessage::read_from(&mut self.stream, MAGIC_IN).await? {
                Some(m) => m,
                None => return Err(Error::protocol("AJP connection closed mid-body")),
            };
            let chunk = decode_body_data(&msg)?;
            if chunk.is_empty() {
                // Proxy signalled end-of-body before content_length was met.
                break;
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body.freeze())
    }

    /// Stream a [`Response`] back to the proxy: `Send Headers`, zero or more
    /// `Send Body Chunk`s, then `End Response`.
    async fn write_response(&mut self, resp: &Response) -> Result<()> {
        encode_send_headers(resp)?
            .write_to(&mut self.stream, MAGIC_OUT)
            .await?;

        for chunk in resp.body.chunks(MAX_SEND_CHUNK) {
            encode_send_body_chunk(chunk)?
                .write_to(&mut self.stream, MAGIC_OUT)
                .await?;
        }

        // `reuse = true`: AJP connections are pooled by the proxy.
        encode_end_response(true)
            .write_to(&mut self.stream, MAGIC_OUT)
            .await?;
        Ok(())
    }
}

/// The canonical "AJP not available" error.
///
/// Retained because [`crate::protocol`] still references it for its bind-time
/// check; the [`AjpConnection`] driver above is the real implementation and is
/// wired up through [`crate::acceptor`].
pub fn unsupported() -> Error {
    Error::protocol("AJP connector must be routed through AjpConnection::serve")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn peer() -> SocketAddr {
        "127.0.0.1:34567".parse().unwrap()
    }

    // ---- framing round-trip ---------------------------------------------

    #[test]
    fn message_framing_round_trip() {
        let mut msg = AjpMessage::new();
        msg.write_u8(0x04);
        msg.write_u16(0xBEEF);
        msg.write_string(Some("hello"));
        msg.write_string(None);

        let framed = msg.encode(MAGIC_OUT).unwrap();
        // magic(2) + len(2) + payload
        assert_eq!(&framed[0..2], &MAGIC_OUT);
        let len = u16::from_be_bytes([framed[2], framed[3]]) as usize;
        assert_eq!(len, framed.len() - 4);

        // Decode the payload back.
        let mut decoded = AjpMessage::from_payload(BytesMut::from(&framed[4..]));
        assert_eq!(decoded.read_u8().unwrap(), 0x04);
        assert_eq!(decoded.read_u16().unwrap(), 0xBEEF);
        assert_eq!(decoded.read_string().unwrap(), Some("hello".to_string()));
        assert_eq!(decoded.read_string().unwrap(), None);
        assert_eq!(decoded.remaining(), 0);
    }

    #[tokio::test]
    async fn message_read_from_validates_magic() {
        // A packet with the wrong magic must be rejected.
        let bad = [0xAB, 0xCD, 0x00, 0x00];
        let mut cursor = std::io::Cursor::new(bad.to_vec());
        let err = AjpMessage::read_from(&mut cursor, MAGIC_IN)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Protocol(_)));
    }

    #[tokio::test]
    async fn message_read_from_clean_eof_is_none() {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        let got = AjpMessage::read_from(&mut cursor, MAGIC_IN).await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn message_write_then_read_round_trip() {
        let mut out = AjpMessage::new();
        out.write_u8(0x08); // CPing
        let framed = out.encode(MAGIC_IN).unwrap();

        let mut cursor = std::io::Cursor::new(framed.to_vec());
        let back = AjpMessage::read_from(&mut cursor, MAGIC_IN)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(back.message_type(), Some(0x08));
    }

    // ---- code tables -----------------------------------------------------

    #[test]
    fn method_code_table() {
        assert_eq!(ajp_method_name(2), Some("GET"));
        assert_eq!(ajp_method_name(4), Some("POST"));
        assert_eq!(ajp_method_name(5), Some("PUT"));
        assert_eq!(ajp_method_name(6), Some("DELETE"));
        assert_eq!(ajp_method_name(1), Some("OPTIONS"));
        assert_eq!(ajp_method_name(27), Some("MKACTIVITY"));
        assert_eq!(ajp_method_name(0), None);
        assert_eq!(ajp_method_name(200), None);
    }

    #[test]
    fn request_header_code_table() {
        assert_eq!(ajp_request_header_name(0xA001), Some("accept"));
        assert_eq!(ajp_request_header_name(0xA00B), Some("host"));
        assert_eq!(ajp_request_header_name(0xA008), Some("content-length"));
        assert_eq!(ajp_request_header_name(0xA00E), Some("user-agent"));
        assert_eq!(ajp_request_header_name(0xA0FF), None);
    }

    #[test]
    fn response_header_code_table() {
        assert_eq!(ajp_response_header_code("Content-Type"), Some(0xA001));
        assert_eq!(ajp_response_header_code("content-length"), Some(0xA003));
        assert_eq!(ajp_response_header_code("Set-Cookie"), Some(0xA007));
        assert_eq!(ajp_response_header_code("X-Custom"), None);
    }

    #[test]
    fn message_type_codes() {
        assert_eq!(AjpMessageType::ForwardRequest.code(), 0x02);
        assert_eq!(AjpMessageType::SendBodyChunk.code(), 0x03);
        assert_eq!(AjpMessageType::SendHeaders.code(), 0x04);
        assert_eq!(AjpMessageType::EndResponse.code(), 0x05);
        assert_eq!(AjpMessageType::GetBodyChunk.code(), 0x06);
        assert_eq!(AjpMessageType::CPing.code(), 0x08);
        assert_eq!(AjpMessageType::CPong.code(), 0x09);
        assert_eq!(
            AjpMessageType::from_code(0x02),
            Some(AjpMessageType::ForwardRequest)
        );
        assert_eq!(AjpMessageType::from_code(0x08), Some(AjpMessageType::CPing));
        assert_eq!(AjpMessageType::from_code(0x04), None);
    }

    // ---- Forward Request decoding ---------------------------------------

    /// Hand-build a `Forward Request` payload for `GET /` with two headers and
    /// one attribute (`query_string`), optionally carrying a `secret`.
    fn build_forward_request(secret: Option<&str>) -> AjpMessage {
        let mut m = AjpMessage::new();
        m.write_u8(AjpMessageType::ForwardRequest.code());
        m.write_u8(2); // method code: GET
        m.write_string(Some("HTTP/1.1")); // protocol
        m.write_string(Some("/")); // req_uri
        m.write_string(Some("10.0.0.7")); // remote_addr
        m.write_string(None); // remote_host (null)
        m.write_string(Some("example.com")); // server_name
        m.write_u16(8009); // server_port
        m.write_u8(0); // is_ssl = false

        // two headers: a common-coded user-agent + a plain X-Custom
        m.write_u16(2);
        m.write_u16(0xA00E); // user-agent code
        m.write_string(Some("curl/8"));
        m.write_string(Some("X-Custom")); // plain name
        m.write_string(Some("yes"));

        // attribute: query_string = "a=1"
        m.write_u8(attr::QUERY_STRING);
        m.write_string(Some("a=1"));
        if let Some(s) = secret {
            m.write_u8(attr::SECRET);
            m.write_string(Some(s));
        }
        m.write_u8(attr::ARE_DONE);
        m
    }

    #[test]
    fn decode_forward_request_get_root() {
        let mut m = build_forward_request(None);
        let fwd = ForwardRequest::decode(&mut m).expect("decode");
        assert_eq!(fwd.method, "GET");
        assert_eq!(fwd.protocol, "HTTP/1.1");
        assert_eq!(fwd.req_uri, "/");
        assert_eq!(fwd.remote_addr, "10.0.0.7");
        assert_eq!(fwd.remote_host, None);
        assert_eq!(fwd.server_name, "example.com");
        assert_eq!(fwd.server_port, 8009);
        assert!(!fwd.is_ssl);
        assert_eq!(fwd.query_string.as_deref(), Some("a=1"));
        assert_eq!(fwd.headers.len(), 2);
        assert_eq!(
            fwd.headers[0],
            ("user-agent".to_string(), "curl/8".to_string())
        );
        assert_eq!(fwd.headers[1], ("x-custom".to_string(), "yes".to_string()));

        let req = fwd.into_request(peer()).unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/");
        assert_eq!(req.query.as_deref(), Some("a=1"));
        assert_eq!(req.header("host"), Some("example.com:8009"));
        assert_eq!(req.header("user-agent"), Some("curl/8"));
    }

    #[test]
    fn decode_forward_request_rejects_unknown_method() {
        let mut m = AjpMessage::new();
        m.write_u8(AjpMessageType::ForwardRequest.code());
        m.write_u8(0); // invalid method code
        let err = ForwardRequest::decode(&mut m).unwrap_err();
        assert!(matches!(err, Error::Protocol(_)));
    }

    #[test]
    fn req_attribute_injection_is_dropped() {
        // SECURITY regression test: a req_attribute (Ghostcat vector) must be
        // parsed-and-dropped, never surfaced.
        let mut m = AjpMessage::new();
        m.write_u8(AjpMessageType::ForwardRequest.code());
        m.write_u8(2); // GET
        m.write_string(Some("HTTP/1.1"));
        m.write_string(Some("/"));
        m.write_string(Some("10.0.0.7"));
        m.write_string(None);
        m.write_string(Some("h"));
        m.write_u16(80);
        m.write_u8(0);
        m.write_u16(0); // no headers
                        // a malicious generic attribute
        m.write_u8(attr::REQ_ATTRIBUTE);
        m.write_string(Some("javax.servlet.include.request_uri"));
        m.write_string(Some("/WEB-INF/web.xml"));
        m.write_u8(attr::ARE_DONE);

        let fwd = ForwardRequest::decode(&mut m).unwrap();
        let req = fwd.into_request(peer()).unwrap();
        // The injected attribute appears nowhere in the resulting request.
        assert!(req
            .headers
            .iter()
            .all(|(k, v)| !k.contains("javax") && !v.contains("web.xml")));
    }

    #[test]
    fn into_request_rejects_control_bytes_in_uri() {
        let fwd = ForwardRequest {
            method: "GET".into(),
            protocol: "HTTP/1.1".into(),
            req_uri: "/bad\u{0}path".into(),
            remote_addr: "1.2.3.4".into(),
            remote_host: None,
            server_name: "h".into(),
            server_port: 80,
            is_ssl: false,
            headers: vec![],
            query_string: None,
            remote_user: None,
            auth_type: None,
            route: None,
            ssl_cert: None,
            secret: None,
            content_length: 0,
        };
        assert!(matches!(fwd.into_request(peer()), Err(Error::Protocol(_))));
    }

    // ---- response encoders ----------------------------------------------

    #[test]
    fn encode_send_headers_layout() {
        let mut resp = Response::with_body(200, "hi");
        resp.set_header("Content-Type", "text/plain");
        let msg = encode_send_headers(&resp).unwrap();
        let mut m = AjpMessage::from_payload(BytesMut::from(msg.payload()));
        assert_eq!(m.read_u8().unwrap(), AjpMessageType::SendHeaders.code());
        assert_eq!(m.read_u16().unwrap(), 200);
        assert_eq!(m.read_string().unwrap(), Some("OK".to_string()));
        assert_eq!(m.read_u16().unwrap(), 1); // one header
        assert_eq!(m.read_string().unwrap(), Some("Content-Type".to_string()));
        assert_eq!(m.read_string().unwrap(), Some("text/plain".to_string()));
    }

    #[test]
    fn encode_send_body_chunk_layout() {
        let msg = encode_send_body_chunk(b"abc").unwrap();
        let payload = msg.payload();
        assert_eq!(payload[0], AjpMessageType::SendBodyChunk.code());
        assert_eq!(u16::from_be_bytes([payload[1], payload[2]]), 3);
        assert_eq!(&payload[3..6], b"abc");
        assert_eq!(payload[6], 0); // trailing NUL
    }

    #[test]
    fn encode_send_body_chunk_rejects_oversize() {
        let big = vec![0u8; MAX_SEND_CHUNK + 1];
        assert!(encode_send_body_chunk(&big).is_err());
    }

    #[test]
    fn encode_end_response_layout() {
        let msg = encode_end_response(true);
        assert_eq!(msg.payload(), &[AjpMessageType::EndResponse.code(), 1]);
        let msg = encode_end_response(false);
        assert_eq!(msg.payload(), &[AjpMessageType::EndResponse.code(), 0]);
    }

    #[test]
    fn encode_get_body_chunk_layout() {
        let msg = encode_get_body_chunk(8192);
        let p = msg.payload();
        assert_eq!(p[0], AjpMessageType::GetBodyChunk.code());
        assert_eq!(u16::from_be_bytes([p[1], p[2]]), 8192);
    }

    #[test]
    fn encode_cpong_layout() {
        assert_eq!(encode_cpong().payload(), &[AjpMessageType::CPong.code()]);
    }

    // ---- secret matching -------------------------------------------------

    #[test]
    fn secret_matches_logic() {
        assert!(secret_matches("s3cr3t", Some("s3cr3t")));
        assert!(!secret_matches("s3cr3t", Some("wrong")));
        assert!(!secret_matches("s3cr3t", Some("s3cr3")));
        assert!(!secret_matches("s3cr3t", Some("s3cr3tX")));
        assert!(!secret_matches("s3cr3t", None));
    }

    // ---- trivial adapter for integration-style tests --------------------

    struct EchoAdapter;

    #[async_trait::async_trait]
    impl Adapter for EchoAdapter {
        async fn service(&self, req: Request) -> Response {
            let mut resp =
                Response::with_body(200, format!("method={} path={}", req.method, req.path));
            resp.set_header("Content-Type", "text/plain");
            resp
        }
    }

    /// Drive one request/response cycle over `tokio::io::duplex`.
    #[tokio::test]
    async fn serve_one_request_over_duplex() {
        let (mut client, server) = tokio::io::duplex(64 * 1024);

        let server_task = tokio::spawn(async move {
            AjpConnection::serve(server, Arc::new(EchoAdapter), peer(), None).await
        });

        // Send a Forward Request, no secret configured.
        let fwd = build_forward_request(None);
        client
            .write_all(&fwd.encode(MAGIC_IN).unwrap())
            .await
            .unwrap();

        // Read Send Headers.
        let headers = AjpMessage::read_from(&mut client, MAGIC_OUT)
            .await
            .unwrap()
            .unwrap();
        let mut h = AjpMessage::from_payload(BytesMut::from(headers.payload()));
        assert_eq!(h.read_u8().unwrap(), AjpMessageType::SendHeaders.code());
        assert_eq!(h.read_u16().unwrap(), 200);

        // Read Send Body Chunk.
        let body = AjpMessage::read_from(&mut client, MAGIC_OUT)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(body.payload()[0], AjpMessageType::SendBodyChunk.code());
        let blen = u16::from_be_bytes([body.payload()[1], body.payload()[2]]) as usize;
        let text = std::str::from_utf8(&body.payload()[3..3 + blen]).unwrap();
        assert_eq!(text, "method=GET path=/");

        // Read End Response.
        let end = AjpMessage::read_from(&mut client, MAGIC_OUT)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(end.payload()[0], AjpMessageType::EndResponse.code());

        // Close the client; the server loop should end cleanly.
        drop(client);
        server_task.await.unwrap().unwrap();
    }

    /// CPing → CPong over the live connection driver.
    #[tokio::test]
    async fn serve_answers_cping_with_cpong() {
        let (mut client, server) = tokio::io::duplex(4096);
        let server_task = tokio::spawn(async move {
            AjpConnection::serve(server, Arc::new(EchoAdapter), peer(), None).await
        });

        let mut ping = AjpMessage::new();
        ping.write_u8(AjpMessageType::CPing.code());
        client
            .write_all(&ping.encode(MAGIC_IN).unwrap())
            .await
            .unwrap();

        let pong = AjpMessage::read_from(&mut client, MAGIC_OUT)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pong.payload(), &[AjpMessageType::CPong.code()]);

        drop(client);
        server_task.await.unwrap().unwrap();
    }

    /// Secret enforcement: a request without the configured secret is rejected
    /// with 403; the same request *with* the secret is served normally.
    #[tokio::test]
    async fn serve_enforces_secret() {
        // --- without the secret: expect 403 ---
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let task = tokio::spawn(async move {
            AjpConnection::serve(server, Arc::new(EchoAdapter), peer(), Some("topsecret")).await
        });
        let fwd_no_secret = build_forward_request(None);
        client
            .write_all(&fwd_no_secret.encode(MAGIC_IN).unwrap())
            .await
            .unwrap();
        let headers = AjpMessage::read_from(&mut client, MAGIC_OUT)
            .await
            .unwrap()
            .unwrap();
        let mut h = AjpMessage::from_payload(BytesMut::from(headers.payload()));
        assert_eq!(h.read_u8().unwrap(), AjpMessageType::SendHeaders.code());
        assert_eq!(h.read_u16().unwrap(), 403, "missing secret must be 403");
        drop(client);
        task.await.unwrap().unwrap();

        // --- with the matching secret: expect 200 ---
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let task = tokio::spawn(async move {
            AjpConnection::serve(server, Arc::new(EchoAdapter), peer(), Some("topsecret")).await
        });
        let fwd_ok = build_forward_request(Some("topsecret"));
        client
            .write_all(&fwd_ok.encode(MAGIC_IN).unwrap())
            .await
            .unwrap();
        let headers = AjpMessage::read_from(&mut client, MAGIC_OUT)
            .await
            .unwrap()
            .unwrap();
        let mut h = AjpMessage::from_payload(BytesMut::from(headers.payload()));
        assert_eq!(h.read_u8().unwrap(), AjpMessageType::SendHeaders.code());
        assert_eq!(h.read_u16().unwrap(), 200, "matching secret must be 200");
        drop(client);
        task.await.unwrap().unwrap();
    }

    /// A POST with a body: the driver must pull the body via Get Body Chunk and
    /// the proxy answers with a body-data packet.
    #[tokio::test]
    async fn serve_pulls_request_body() {
        struct BodyLenAdapter;
        #[async_trait::async_trait]
        impl Adapter for BodyLenAdapter {
            async fn service(&self, req: Request) -> Response {
                Response::with_body(200, format!("got {} bytes", req.body.len()))
            }
        }

        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let task = tokio::spawn(async move {
            AjpConnection::serve(server, Arc::new(BodyLenAdapter), peer(), None).await
        });

        // Forward Request for POST / with Content-Length: 5.
        let mut m = AjpMessage::new();
        m.write_u8(AjpMessageType::ForwardRequest.code());
        m.write_u8(4); // POST
        m.write_string(Some("HTTP/1.1"));
        m.write_string(Some("/submit"));
        m.write_string(Some("10.0.0.1"));
        m.write_string(None);
        m.write_string(Some("h"));
        m.write_u16(80);
        m.write_u8(0);
        m.write_u16(1); // one header
        m.write_u16(0xA008); // content-length code
        m.write_string(Some("5"));
        m.write_u8(attr::ARE_DONE);
        client
            .write_all(&m.encode(MAGIC_IN).unwrap())
            .await
            .unwrap();

        // The server should ask for the body.
        let get = AjpMessage::read_from(&mut client, MAGIC_OUT)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(get.payload()[0], AjpMessageType::GetBodyChunk.code());

        // Answer with a body-data packet: len(u16) + "hello" (no type byte).
        let mut bd = AjpMessage::new();
        bd.write_u16(5);
        bd.write_raw(b"hello");
        client
            .write_all(&bd.encode(MAGIC_IN).unwrap())
            .await
            .unwrap();

        // Now read the response.
        let headers = AjpMessage::read_from(&mut client, MAGIC_OUT)
            .await
            .unwrap()
            .unwrap();
        let mut h = AjpMessage::from_payload(BytesMut::from(headers.payload()));
        assert_eq!(h.read_u8().unwrap(), AjpMessageType::SendHeaders.code());
        assert_eq!(h.read_u16().unwrap(), 200);

        let body = AjpMessage::read_from(&mut client, MAGIC_OUT)
            .await
            .unwrap()
            .unwrap();
        let blen = u16::from_be_bytes([body.payload()[1], body.payload()[2]]) as usize;
        let text = std::str::from_utf8(&body.payload()[3..3 + blen]).unwrap();
        assert_eq!(text, "got 5 bytes");

        let _end = AjpMessage::read_from(&mut client, MAGIC_OUT)
            .await
            .unwrap()
            .unwrap();
        drop(client);
        task.await.unwrap().unwrap();
    }
}
