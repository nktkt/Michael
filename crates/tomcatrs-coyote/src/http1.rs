//! A real, working HTTP/1.1 parser and writer.
//!
//! This module is the heart of the v0.1.0 connector. It implements just enough
//! of [RFC 9112] to serve real clients:
//!
//! * parse the request line (`METHOD SP request-target SP HTTP-version CRLF`),
//! * parse header fields until the terminating blank line,
//! * enforce every limit in [`tomcatrs_config::RequestLimits`], rejecting
//!   offending requests with the appropriate 4xx status,
//! * read the request body honoring `Content-Length`,
//! * support HTTP/1.1 keep-alive (and `Connection: close`),
//! * write a well-formed response (status line + headers + body).
//!
//! Request bodies framed with `Transfer-Encoding: chunked` are decoded via
//! [`crate::chunked::ChunkedDecoder`], including parsed-and-ignored chunk
//! extensions and collected trailer headers. A request that supplies both
//! `Content-Length` and `Transfer-Encoding` is rejected with `400` as a
//! request-smuggling defense.
//!
//! It deliberately does **not** implement `Expect: 100-continue` yet — that is
//! tracked for a later revision.
//!
//! [RFC 9112]: https://www.rfc-editor.org/rfc/rfc9112

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

use tomcatrs_config::RequestLimits;
use tomcatrs_core::Result;

use crate::normalize::normalize_target;
use crate::{Adapter, Request, Response};

/// Maximum number of bytes we will buffer while looking for the end of the
/// header block, as a hard backstop independent of `max_header_size` (which is
/// per-header). Keeps a malicious peer from forcing unbounded memory use.
const HEADER_BLOCK_HARD_CAP: usize = 1024 * 1024;

/// The outcome of attempting to parse one request off the connection.
#[derive(Debug)]
enum ReadOutcome {
    /// A complete request was parsed.
    Request(Request),
    /// The peer closed the connection cleanly before sending another request.
    ConnectionClosed,
    /// The request was malformed or violated a limit; serve this response and
    /// then close the connection.
    Reject(Response),
}

/// A parsed request line plus any leftover bytes already read past it.
#[derive(Debug)]
struct ParsedHead {
    method: String,
    target: String,
    version: String,
    headers: Vec<(String, String)>,
}

/// Parse a request line and header block from an in-memory byte buffer.
///
/// `buf` must contain the bytes up to and including the terminating
/// `CRLF CRLF`. This is the pure, synchronous, unit-testable core of the
/// parser; the async plumbing in [`serve_connection`] is responsible for
/// reading those bytes off the socket.
///
/// # Errors
///
/// Returns a [`Response`] (wrapped in `Err`) carrying the status code the
/// offending request should be answered with.
fn parse_head(buf: &[u8], limits: &RequestLimits) -> std::result::Result<ParsedHead, Response> {
    let text = std::str::from_utf8(buf)
        .map_err(|_| bad_request("request head is not valid UTF-8/ASCII"))?;

    let mut lines = text.split("\r\n");

    // ---- request line -----------------------------------------------------
    let request_line = lines
        .next()
        .filter(|l| !l.is_empty())
        .ok_or_else(|| bad_request("empty request line"))?;

    let mut parts = request_line.splitn(3, ' ');
    let method = parts
        .next()
        .ok_or_else(|| bad_request("missing method"))?
        .to_string();
    let target = parts
        .next()
        .ok_or_else(|| bad_request("missing request target"))?
        .to_string();
    let version = parts
        .next()
        .ok_or_else(|| bad_request("missing HTTP version"))?
        .to_string();

    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(bad_request("invalid method token"));
    }
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(Response::with_body(505, "HTTP Version Not Supported"));
    }
    if target.len() > limits.max_uri_len {
        // 414 URI Too Long
        return Err(Response::with_body(414, "URI Too Long"));
    }

    // ---- header fields ----------------------------------------------------
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if line.is_empty() {
            // The blank line terminates the header block.
            break;
        }
        if headers.len() >= limits.max_header_count {
            // 431 Request Header Fields Too Large
            return Err(Response::with_body(431, "Too Many Request Header Fields"));
        }
        if line.len() > limits.max_header_size {
            return Err(Response::with_body(431, "Request Header Field Too Large"));
        }
        let colon = line
            .find(':')
            .ok_or_else(|| bad_request("header missing ':' separator"))?;
        let name = line[..colon].trim();
        let value = line[colon + 1..].trim();
        if name.is_empty() || !name.bytes().all(is_token_char) {
            return Err(bad_request("invalid header field name"));
        }
        headers.push((name.to_string(), value.to_string()));
    }

    Ok(ParsedHead {
        method,
        target,
        version,
        headers,
    })
}

/// Is `b` a valid HTTP token character (RFC 9110 §5.6.2)?
fn is_token_char(b: u8) -> bool {
    matches!(b,
        b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' |
        b'^' | b'_' | b'`' | b'|' | b'~' |
        b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z')
}

/// Build a `400 Bad Request` response with a plain-text body.
fn bad_request(reason: &str) -> Response {
    tracing::debug!(reason, "rejecting request with 400");
    Response::with_body(400, format!("400 Bad Request: {reason}"))
}

/// Read bytes from `stream` until the header-terminating `CRLF CRLF` is found.
///
/// Returns `(head_bytes, leftover_body_bytes)` where `head_bytes` includes the
/// trailing `CRLF CRLF` and `leftover_body_bytes` is any body data that arrived
/// in the same read(s).
///
/// `carryover` holds bytes that were read past the previous request on this
/// connection (relevant for pipelined requests); they are consumed first,
/// before touching the socket. `Ok(None)` means the peer closed the connection
/// before sending anything (a clean idle-timeout / keep-alive shutdown).
async fn read_head<S>(
    stream: &mut S,
    carryover: &mut Vec<u8>,
    keep_alive_timeout: Duration,
) -> std::result::Result<Option<(Vec<u8>, Vec<u8>)>, Response>
where
    S: AsyncReadExt + Unpin,
{
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];
    // Seed from any bytes carried over from the previous request.
    let mut first_read = carryover.is_empty();
    if !carryover.is_empty() {
        buf.append(carryover);
        if let Some(pos) = find_header_end(&buf) {
            let leftover = buf.split_off(pos);
            return Ok(Some((buf, leftover)));
        }
    }

    loop {
        let read_result = if first_read {
            // Wait up to the keep-alive timeout for the *start* of a request.
            match timeout(keep_alive_timeout, stream.read(&mut chunk)).await {
                Ok(r) => r,
                Err(_) => {
                    // Idle keep-alive connection timed out: treat as a clean close.
                    return Ok(None);
                }
            }
        } else {
            stream.read(&mut chunk).await
        };

        let n = match read_result {
            Ok(0) => {
                if buf.is_empty() {
                    return Ok(None);
                }
                return Err(bad_request("connection closed mid-headers"));
            }
            Ok(n) => n,
            Err(e) => {
                tracing::debug!(error = %e, "socket read error while reading headers");
                return Err(bad_request("socket read error"));
            }
        };
        first_read = false;
        buf.extend_from_slice(&chunk[..n]);

        if buf.len() > HEADER_BLOCK_HARD_CAP {
            return Err(Response::with_body(431, "Request Header Block Too Large"));
        }

        if let Some(pos) = find_header_end(&buf) {
            let leftover = buf.split_off(pos);
            return Ok(Some((buf, leftover)));
        }
    }
}

/// Find the index just past the `CRLF CRLF` that ends the header block.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

/// How the request body is framed on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyFraming {
    /// No body, or an explicit `Content-Length: 0`.
    None,
    /// A fixed-length body of exactly `n` bytes (`Content-Length: n`).
    Fixed(usize),
    /// A `Transfer-Encoding: chunked` body to be decoded incrementally.
    Chunked,
}

/// Determine how the request body is framed from its headers.
///
/// Implements the precedence rules of [RFC 9112 §6]: a `chunked`
/// transfer-coding wins, a `Content-Length` gives a fixed length, and the two
/// appearing together is treated as a request-smuggling attempt and rejected
/// with `400`.
///
/// # Errors
///
/// Returns a [`Response`] (wrapped in `Err`) carrying the status the offending
/// request should be answered with.
fn body_framing(headers: &[(String, String)]) -> std::result::Result<BodyFraming, Response> {
    // Collect every transfer-coding declared across all Transfer-Encoding
    // headers (a peer may legally split them, e.g. "gzip" then "chunked").
    let mut has_transfer_encoding = false;
    let mut is_chunked = false;
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("transfer-encoding") {
            has_transfer_encoding = true;
            let lower = v.to_ascii_lowercase();
            for coding in lower.split(',') {
                let coding = coding.trim();
                if coding == "chunked" {
                    is_chunked = true;
                } else if !coding.is_empty() {
                    // We only implement the `chunked` coding; anything else
                    // (gzip, deflate, …) we cannot decode.
                    return Err(Response::with_body(
                        501,
                        format!("unsupported transfer-coding: {coding}"),
                    ));
                }
            }
        }
    }
    if has_transfer_encoding && !is_chunked {
        // Per RFC 9112 §6.1, if a Transfer-Encoding is present its final coding
        // must be `chunked`; otherwise the message length is undeterminable.
        return Err(bad_request(
            "Transfer-Encoding present without final 'chunked' coding",
        ));
    }

    let mut content_length: Option<usize> = None;
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("content-length") {
            let parsed: usize = v
                .trim()
                .parse()
                .map_err(|_| bad_request("invalid Content-Length"))?;
            if let Some(prev) = content_length {
                if prev != parsed {
                    return Err(bad_request("conflicting Content-Length headers"));
                }
            }
            content_length = Some(parsed);
        }
    }

    // Content-Length together with Transfer-Encoding is a classic request
    // smuggling vector — reject it outright.
    if is_chunked && content_length.is_some() {
        return Err(bad_request(
            "Content-Length and Transfer-Encoding must not both be present",
        ));
    }

    if is_chunked {
        Ok(BodyFraming::Chunked)
    } else {
        match content_length {
            Some(0) | None => Ok(BodyFraming::None),
            Some(n) => Ok(BodyFraming::Fixed(n)),
        }
    }
}

/// Decide whether the connection should be kept alive after this request.
fn wants_keep_alive(version: &str, headers: &[(String, String)]) -> bool {
    let connection = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("connection"))
        .map(|(_, v)| v.to_ascii_lowercase());
    match version {
        "HTTP/1.1" => connection.as_deref() != Some("close"),
        // HTTP/1.0 defaults to close unless explicitly kept alive.
        _ => connection.as_deref() == Some("keep-alive"),
    }
}

/// Read a fixed-length (`Content-Length`-framed) request body.
///
/// `leftover` carries body bytes that already arrived alongside the header
/// block; the remainder is read from `stream`. The returned vector is exactly
/// `body_len` bytes long. Any bytes in `leftover` beyond `body_len` belong to a
/// pipelined request and are appended to `carryover` for the next
/// [`read_head`] call.
///
/// # Errors
///
/// Returns a [`Response`] on a premature close, socket error, or timeout.
async fn read_fixed_body<S>(
    stream: &mut S,
    mut leftover: Vec<u8>,
    carryover: &mut Vec<u8>,
    body_len: usize,
    limits: &RequestLimits,
) -> std::result::Result<Vec<u8>, Response>
where
    S: AsyncReadExt + Unpin,
{
    if leftover.len() > body_len {
        // Bytes past this request's body belong to the next pipelined request.
        carryover.extend_from_slice(&leftover[body_len..]);
        leftover.truncate(body_len);
    }
    let mut body = leftover;
    let mut chunk = [0u8; 8192];
    while body.len() < body_len {
        let remaining = body_len - body.len();
        let want = remaining.min(chunk.len());
        match timeout(limits.request_timeout, stream.read(&mut chunk[..want])).await {
            Ok(Ok(0)) => return Err(bad_request("connection closed mid-body")),
            Ok(Ok(n)) => body.extend_from_slice(&chunk[..n]),
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "socket read error while reading body");
                return Err(bad_request("socket read error reading body"));
            }
            Err(_) => return Err(Response::with_body(408, "Request Timeout")),
        }
    }
    Ok(body)
}

/// Read and decode a `Transfer-Encoding: chunked` request body.
///
/// `leftover` carries body bytes that already arrived alongside the header
/// block; further bytes are read from `stream` until the
/// [`ChunkedDecoder`](crate::chunked::ChunkedDecoder) reports the chunked body
/// complete. The decoder bounds the cumulative decoded size by
/// `limits.max_post_size`. Any bytes read past the terminating CRLF belong to
/// the next pipelined request and are appended to `carryover`.
///
/// # Errors
///
/// Returns a `413` for an oversized body, a `400` for a malformed chunk
/// stream, a `408` on timeout, and a `400` on a premature close or socket
/// error.
async fn read_chunked_body<S>(
    stream: &mut S,
    leftover: Vec<u8>,
    carryover: &mut Vec<u8>,
    limits: &RequestLimits,
) -> std::result::Result<Vec<u8>, Response>
where
    S: AsyncReadExt + Unpin,
{
    use crate::chunked::{ChunkedDecoder, ChunkedError};

    /// Translate a decoder error into the right HTTP rejection response.
    fn reject(e: ChunkedError) -> Response {
        match e {
            ChunkedError::TooLarge(_) => Response::with_body(413, "Payload Too Large"),
            ChunkedError::Malformed(msg) => bad_request(&format!("chunked body: {msg}")),
        }
    }

    let mut decoder = ChunkedDecoder::new(limits.max_post_size);

    // Feed whatever already arrived with the header block.
    if !leftover.is_empty() {
        let consumed = decoder.push(&leftover).map_err(reject)?;
        if decoder.is_complete() {
            carryover.extend_from_slice(&leftover[consumed..]);
            return Ok(decoder.into_body());
        }
    }

    let mut chunk = [0u8; 8192];
    while !decoder.is_complete() {
        match timeout(limits.request_timeout, stream.read(&mut chunk)).await {
            Ok(Ok(0)) => return Err(bad_request("connection closed mid-chunked-body")),
            Ok(Ok(n)) => {
                let consumed = decoder.push(&chunk[..n]).map_err(reject)?;
                if decoder.is_complete() {
                    // Bytes past the chunked body belong to the next request.
                    carryover.extend_from_slice(&chunk[consumed..n]);
                }
            }
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "socket read error while reading chunked body");
                return Err(bad_request("socket read error reading chunked body"));
            }
            Err(_) => return Err(Response::with_body(408, "Request Timeout")),
        }
    }

    Ok(decoder.into_body())
}

/// Read one full request — head + body — off the connection.
///
/// `carryover` holds any bytes read past the previous request on this
/// connection (for pipelined requests) and, on return, holds any bytes read
/// past *this* request's body.
async fn read_request<S>(
    stream: &mut S,
    carryover: &mut Vec<u8>,
    peer_addr: SocketAddr,
    limits: &RequestLimits,
) -> ReadOutcome
where
    S: AsyncReadExt + Unpin,
{
    let (head_bytes, leftover) = match read_head(stream, carryover, limits.keep_alive_timeout).await
    {
        Ok(Some(parts)) => parts,
        Ok(None) => return ReadOutcome::ConnectionClosed,
        Err(resp) => return ReadOutcome::Reject(resp),
    };

    let head = match parse_head(&head_bytes, limits) {
        Ok(h) => h,
        Err(resp) => return ReadOutcome::Reject(resp),
    };

    let framing = match body_framing(&head.headers) {
        Ok(f) => f,
        Err(resp) => return ReadOutcome::Reject(resp),
    };

    let body = match framing {
        BodyFraming::None => Vec::new(),
        BodyFraming::Fixed(body_len) => {
            if body_len > limits.max_post_size {
                // 413 Payload Too Large
                return ReadOutcome::Reject(Response::with_body(413, "Payload Too Large"));
            }
            match read_fixed_body(stream, leftover, carryover, body_len, limits).await {
                Ok(body) => body,
                Err(resp) => return ReadOutcome::Reject(resp),
            }
        }
        BodyFraming::Chunked => {
            match read_chunked_body(stream, leftover, carryover, limits).await {
                Ok(body) => body,
                Err(resp) => return ReadOutcome::Reject(resp),
            }
        }
    };

    // Normalize the request target into path + query.
    let normalized = match normalize_target(&head.target) {
        Ok(n) => n,
        Err(e) => {
            return ReadOutcome::Reject(bad_request(&e.to_string()));
        }
    };

    ReadOutcome::Request(Request {
        method: head.method,
        uri: head.target,
        path: normalized.path,
        query: normalized.query,
        version: head.version,
        headers: head.headers,
        body: Bytes::from(body),
        peer_addr,
    })
}

/// Serialize a [`Response`] onto the wire and flush it.
///
/// Always emits a `Content-Length`, a `Server` header, a `Date`-less but
/// `Connection` header reflecting `keep_alive`, and the body. The caller is
/// responsible for any framing concerns beyond a single response.
pub async fn write_response<S>(stream: &mut S, resp: &Response, keep_alive: bool) -> Result<()>
where
    S: AsyncWriteExt + Unpin,
{
    let reason = reason_phrase(resp.status);
    let mut out = Vec::with_capacity(256 + resp.body.len());
    out.extend_from_slice(format!("HTTP/1.1 {} {}\r\n", resp.status, reason).as_bytes());

    let mut wrote_content_length = false;
    let mut wrote_server = false;
    let mut wrote_connection = false;
    for (k, v) in &resp.headers {
        if k.eq_ignore_ascii_case("content-length") {
            wrote_content_length = true;
        }
        if k.eq_ignore_ascii_case("server") {
            wrote_server = true;
        }
        if k.eq_ignore_ascii_case("connection") {
            wrote_connection = true;
        }
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(v.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    if !wrote_content_length {
        out.extend_from_slice(format!("Content-Length: {}\r\n", resp.body.len()).as_bytes());
    }
    if !wrote_server {
        out.extend_from_slice(format!("Server: {}\r\n", tomcatrs_core::SERVER_INFO).as_bytes());
    }
    if !wrote_connection {
        let conn = if keep_alive { "keep-alive" } else { "close" };
        out.extend_from_slice(format!("Connection: {conn}\r\n").as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(&resp.body);

    stream.write_all(&out).await?;
    stream.flush().await?;
    Ok(())
}

/// Drive a single accepted connection: read requests, service them through the
/// [`Adapter`], write responses, and loop while keep-alive permits.
///
/// This is the per-connection entry point invoked by the [`acceptor`](crate::acceptor).
///
/// # Errors
///
/// Returns an error only on an unrecoverable I/O failure while writing; normal
/// protocol violations are answered with a 4xx response and the connection is
/// closed gracefully.
pub async fn serve_connection<S>(
    mut stream: S,
    peer_addr: SocketAddr,
    adapter: &dyn Adapter,
    limits: &RequestLimits,
) -> Result<()>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    // Bytes read past one request's body that belong to the next pipelined
    // request on this keep-alive connection.
    let mut carryover: Vec<u8> = Vec::new();
    loop {
        match read_request(&mut stream, &mut carryover, peer_addr, limits).await {
            ReadOutcome::ConnectionClosed => {
                tracing::trace!(%peer_addr, "connection closed by peer");
                return Ok(());
            }
            ReadOutcome::Reject(resp) => {
                // Always close after an error response.
                write_response(&mut stream, &resp, false).await?;
                return Ok(());
            }
            ReadOutcome::Request(req) => {
                let keep_alive = wants_keep_alive(&req.version, &req.headers);
                tracing::debug!(
                    method = %req.method,
                    path = %req.path,
                    %peer_addr,
                    keep_alive,
                    "servicing request"
                );
                let resp = adapter.service(req).await;
                write_response(&mut stream, &resp, keep_alive).await?;
                if !keep_alive {
                    return Ok(());
                }
            }
        }
    }
}

/// Map a status code to its canonical reason phrase.
///
/// Covers the codes the connector itself emits plus the common success and
/// redirect codes; anything unknown falls back to a generic phrase.
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
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
        505 => "HTTP Version Not Supported",
        _ => "Status",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> RequestLimits {
        RequestLimits::default()
    }

    #[test]
    fn parses_simple_get_request_line_and_headers() {
        let raw = b"GET /index.html HTTP/1.1\r\nHost: example.com\r\nAccept: */*\r\n\r\n";
        let head = parse_head(raw, &limits()).expect("should parse");
        assert_eq!(head.method, "GET");
        assert_eq!(head.target, "/index.html");
        assert_eq!(head.version, "HTTP/1.1");
        assert_eq!(head.headers.len(), 2);
        assert_eq!(
            head.headers[0],
            ("Host".to_string(), "example.com".to_string())
        );
        assert_eq!(head.headers[1], ("Accept".to_string(), "*/*".to_string()));
    }

    #[test]
    fn parses_request_with_no_headers() {
        let raw = b"GET / HTTP/1.0\r\n\r\n";
        let head = parse_head(raw, &limits()).expect("should parse");
        assert_eq!(head.method, "GET");
        assert_eq!(head.version, "HTTP/1.0");
        assert!(head.headers.is_empty());
    }

    #[test]
    fn header_values_are_trimmed() {
        let raw = b"GET / HTTP/1.1\r\nX-Pad:    spaced   \r\n\r\n";
        let head = parse_head(raw, &limits()).unwrap();
        assert_eq!(head.headers[0].1, "spaced");
    }

    #[test]
    fn rejects_missing_version() {
        let raw = b"GET /\r\n\r\n";
        let resp = parse_head(raw, &limits()).unwrap_err();
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn rejects_unknown_http_version() {
        let raw = b"GET / HTTP/3.0\r\n\r\n";
        let resp = parse_head(raw, &limits()).unwrap_err();
        assert_eq!(resp.status, 505);
    }

    #[test]
    fn rejects_header_missing_colon() {
        let raw = b"GET / HTTP/1.1\r\nBadHeader\r\n\r\n";
        let resp = parse_head(raw, &limits()).unwrap_err();
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn enforces_max_uri_len_with_414() {
        let mut l = limits();
        l.max_uri_len = 8;
        let raw = b"GET /this-is-way-too-long HTTP/1.1\r\n\r\n";
        let resp = parse_head(raw, &l).unwrap_err();
        assert_eq!(resp.status, 414);
    }

    #[test]
    fn enforces_max_header_count_with_431() {
        let mut l = limits();
        l.max_header_count = 1;
        let raw = b"GET / HTTP/1.1\r\nA: 1\r\nB: 2\r\n\r\n";
        let resp = parse_head(raw, &l).unwrap_err();
        assert_eq!(resp.status, 431);
    }

    #[test]
    fn enforces_max_header_size_with_431() {
        let mut l = limits();
        l.max_header_size = 5;
        let raw = b"GET / HTTP/1.1\r\nX: aaaaaaaaaaaaaaaaaa\r\n\r\n";
        let resp = parse_head(raw, &l).unwrap_err();
        assert_eq!(resp.status, 431);
    }

    #[test]
    fn body_framing_content_length() {
        let headers = vec![("Content-Length".to_string(), "42".to_string())];
        assert_eq!(body_framing(&headers).unwrap(), BodyFraming::Fixed(42));

        let none: Vec<(String, String)> = vec![];
        assert_eq!(body_framing(&none).unwrap(), BodyFraming::None);

        let zero = vec![("Content-Length".to_string(), "0".to_string())];
        assert_eq!(body_framing(&zero).unwrap(), BodyFraming::None);

        let bad = vec![("Content-Length".to_string(), "abc".to_string())];
        assert_eq!(body_framing(&bad).unwrap_err().status, 400);

        let conflict = vec![
            ("Content-Length".to_string(), "1".to_string()),
            ("Content-Length".to_string(), "2".to_string()),
        ];
        assert_eq!(body_framing(&conflict).unwrap_err().status, 400);
    }

    #[test]
    fn body_framing_recognizes_chunked() {
        let headers = vec![("Transfer-Encoding".to_string(), "chunked".to_string())];
        assert_eq!(body_framing(&headers).unwrap(), BodyFraming::Chunked);

        // Case-insensitive and tolerant of a leading transfer-coding.
        let mixed = vec![("transfer-encoding".to_string(), "CHUNKED".to_string())];
        assert_eq!(body_framing(&mixed).unwrap(), BodyFraming::Chunked);
    }

    #[test]
    fn body_framing_rejects_content_length_plus_chunked() {
        // Request smuggling defense: both framings together → 400.
        let headers = vec![
            ("Content-Length".to_string(), "5".to_string()),
            ("Transfer-Encoding".to_string(), "chunked".to_string()),
        ];
        assert_eq!(body_framing(&headers).unwrap_err().status, 400);
    }

    #[test]
    fn body_framing_rejects_unsupported_transfer_coding() {
        let headers = vec![("Transfer-Encoding".to_string(), "gzip".to_string())];
        assert_eq!(body_framing(&headers).unwrap_err().status, 501);
    }

    #[test]
    fn body_framing_rejects_te_without_chunked_final() {
        // A non-chunked transfer-coding leaves the length undeterminable.
        let headers = vec![("Transfer-Encoding".to_string(), "chunked, gzip".to_string())];
        // "gzip" is an unsupported coding → 501 before the final-coding check.
        assert_eq!(body_framing(&headers).unwrap_err().status, 501);
    }

    #[test]
    fn keep_alive_defaults_by_version() {
        assert!(wants_keep_alive("HTTP/1.1", &[]));
        assert!(!wants_keep_alive("HTTP/1.0", &[]));
        assert!(!wants_keep_alive(
            "HTTP/1.1",
            &[("Connection".into(), "close".into())]
        ));
        assert!(wants_keep_alive(
            "HTTP/1.0",
            &[("Connection".into(), "keep-alive".into())]
        ));
    }

    #[test]
    fn find_header_end_locates_blank_line() {
        assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n\r\nBODY"), Some(18));
        assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n"), None);
    }

    #[tokio::test]
    async fn write_response_emits_well_formed_bytes() {
        let mut buf: Vec<u8> = Vec::new();
        let mut resp = Response::with_body(200, "hi");
        resp.set_header("Content-Type", "text/plain");
        write_response(&mut buf, &resp, true).await.unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Type: text/plain\r\n"));
        assert!(text.contains("Content-Length: 2\r\n"));
        assert!(text.contains("Connection: keep-alive\r\n"));
        assert!(text.ends_with("\r\n\r\nhi"));
    }

    #[tokio::test]
    async fn read_request_reads_body_by_content_length() {
        // A POST with a 5-byte body, all in one buffer.
        let raw = b"POST /submit HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello".to_vec();
        let mut cursor = std::io::Cursor::new(raw);
        let peer: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let mut carry = Vec::new();
        match read_request(&mut cursor, &mut carry, peer, &limits()).await {
            ReadOutcome::Request(req) => {
                assert_eq!(req.method, "POST");
                assert_eq!(req.path, "/submit");
                assert_eq!(&req.body[..], b"hello");
            }
            _ => panic!("expected a parsed request"),
        }
    }

    #[tokio::test]
    async fn read_request_rejects_oversized_body_with_413() {
        let mut l = limits();
        l.max_post_size = 4;
        let raw = b"POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\n0123456789".to_vec();
        let mut cursor = std::io::Cursor::new(raw);
        let peer: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let mut carry = Vec::new();
        match read_request(&mut cursor, &mut carry, peer, &l).await {
            ReadOutcome::Reject(resp) => assert_eq!(resp.status, 413),
            _ => panic!("expected rejection"),
        }
    }

    #[tokio::test]
    async fn read_request_decodes_chunked_body() {
        // A POST whose body is chunked, with a chunk extension and a trailer.
        let raw = b"POST /upload HTTP/1.1\r\n\
            Host: example.com\r\n\
            Transfer-Encoding: chunked\r\n\
            \r\n\
            4;meta=1\r\nWiki\r\n5\r\npedia\r\n0\r\nX-Sum: 99\r\n\r\n"
            .to_vec();
        let mut cursor = std::io::Cursor::new(raw);
        let peer: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let mut carry = Vec::new();
        match read_request(&mut cursor, &mut carry, peer, &limits()).await {
            ReadOutcome::Request(req) => {
                assert_eq!(req.method, "POST");
                assert_eq!(req.path, "/upload");
                assert_eq!(&req.body[..], b"Wikipedia");
            }
            other => panic!("expected a parsed request, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_request_rejects_oversized_chunked_body_with_413() {
        let mut l = limits();
        l.max_post_size = 4;
        let raw = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n\
            a\r\n0123456789\r\n0\r\n\r\n"
            .to_vec();
        let mut cursor = std::io::Cursor::new(raw);
        let peer: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let mut carry = Vec::new();
        match read_request(&mut cursor, &mut carry, peer, &l).await {
            ReadOutcome::Reject(resp) => assert_eq!(resp.status, 413),
            other => panic!("expected 413 rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_request_rejects_malformed_chunked_body_with_400() {
        let raw = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n\
            zz\r\nabc\r\n0\r\n\r\n"
            .to_vec();
        let mut cursor = std::io::Cursor::new(raw);
        let peer: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let mut carry = Vec::new();
        match read_request(&mut cursor, &mut carry, peer, &limits()).await {
            ReadOutcome::Reject(resp) => assert_eq!(resp.status, 400),
            other => panic!("expected 400 rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn keep_alive_works_after_a_chunked_request() {
        // Two requests on one connection: a chunked POST, then a plain GET.
        // They arrive in one buffer (pipelined), so the chunked decoder must
        // hand the trailing GET bytes back via the carryover buffer.
        let raw = b"POST /a HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n\
            3\r\nabc\r\n0\r\n\r\n\
            GET /b HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
            .to_vec();
        let mut cursor = std::io::Cursor::new(raw);
        let peer: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let mut carry = Vec::new();

        match read_request(&mut cursor, &mut carry, peer, &limits()).await {
            ReadOutcome::Request(req) => {
                assert_eq!(req.path, "/a");
                assert_eq!(&req.body[..], b"abc");
            }
            other => panic!("expected first request, got {other:?}"),
        }
        match read_request(&mut cursor, &mut carry, peer, &limits()).await {
            ReadOutcome::Request(req) => {
                assert_eq!(req.path, "/b");
                assert!(req.body.is_empty());
            }
            other => panic!("expected second request, got {other:?}"),
        }
    }
}
