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
//! It deliberately does **not** implement chunked transfer-encoding decoding,
//! `Expect: 100-continue`, or trailers yet — those are tracked for a later
//! revision. A request using chunked encoding is rejected with `411`.
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
/// `Ok(None)` means the peer closed the connection before sending anything
/// (a clean idle-timeout / keep-alive shutdown).
async fn read_head<S>(
    stream: &mut S,
    keep_alive_timeout: Duration,
) -> std::result::Result<Option<(Vec<u8>, Vec<u8>)>, Response>
where
    S: AsyncReadExt + Unpin,
{
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];
    let mut first_read = true;

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

/// Parse the `Content-Length` header, if present.
///
/// Returns `Ok(None)` when absent, `Ok(Some(n))` when valid, and an `Err`
/// response when malformed.
fn content_length(headers: &[(String, String)]) -> std::result::Result<Option<usize>, Response> {
    // Reject chunked encoding explicitly — not supported in v0.1.0.
    if let Some((_, te)) = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding"))
    {
        if te.to_ascii_lowercase().contains("chunked") {
            return Err(Response::with_body(
                411,
                "chunked transfer-encoding is not supported in v0.1.0",
            ));
        }
    }

    let mut found: Option<usize> = None;
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("content-length") {
            let parsed: usize = v
                .trim()
                .parse()
                .map_err(|_| bad_request("invalid Content-Length"))?;
            if let Some(prev) = found {
                if prev != parsed {
                    return Err(bad_request("conflicting Content-Length headers"));
                }
            }
            found = Some(parsed);
        }
    }
    Ok(found)
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

/// Read one full request — head + body — off the connection.
async fn read_request<S>(
    stream: &mut S,
    peer_addr: SocketAddr,
    limits: &RequestLimits,
) -> ReadOutcome
where
    S: AsyncReadExt + Unpin,
{
    let (head_bytes, mut leftover) = match read_head(stream, limits.keep_alive_timeout).await {
        Ok(Some(parts)) => parts,
        Ok(None) => return ReadOutcome::ConnectionClosed,
        Err(resp) => return ReadOutcome::Reject(resp),
    };

    let head = match parse_head(&head_bytes, limits) {
        Ok(h) => h,
        Err(resp) => return ReadOutcome::Reject(resp),
    };

    let body_len = match content_length(&head.headers) {
        Ok(len) => len.unwrap_or(0),
        Err(resp) => return ReadOutcome::Reject(resp),
    };

    if body_len > limits.max_post_size {
        // 413 Payload Too Large
        return ReadOutcome::Reject(Response::with_body(413, "Payload Too Large"));
    }

    // Read the remaining body bytes (those not already in `leftover`).
    let mut body = leftover.split_off(leftover.len().min(body_len));
    std::mem::swap(&mut body, &mut leftover);
    // `body` now holds at most `body_len` bytes already received.
    if body.len() > body_len {
        body.truncate(body_len);
    }
    let mut chunk = [0u8; 8192];
    while body.len() < body_len {
        let remaining = body_len - body.len();
        let want = remaining.min(chunk.len());
        match timeout(limits.request_timeout, stream.read(&mut chunk[..want])).await {
            Ok(Ok(0)) => {
                return ReadOutcome::Reject(bad_request("connection closed mid-body"));
            }
            Ok(Ok(n)) => body.extend_from_slice(&chunk[..n]),
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "socket read error while reading body");
                return ReadOutcome::Reject(bad_request("socket read error reading body"));
            }
            Err(_) => {
                return ReadOutcome::Reject(Response::with_body(408, "Request Timeout"));
            }
        }
    }

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
    loop {
        match read_request(&mut stream, peer_addr, limits).await {
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
    fn content_length_parsing() {
        let headers = vec![("Content-Length".to_string(), "42".to_string())];
        assert_eq!(content_length(&headers).unwrap(), Some(42));

        let none: Vec<(String, String)> = vec![];
        assert_eq!(content_length(&none).unwrap(), None);

        let bad = vec![("Content-Length".to_string(), "abc".to_string())];
        assert_eq!(content_length(&bad).unwrap_err().status, 400);

        let conflict = vec![
            ("Content-Length".to_string(), "1".to_string()),
            ("Content-Length".to_string(), "2".to_string()),
        ];
        assert_eq!(content_length(&conflict).unwrap_err().status, 400);
    }

    #[test]
    fn chunked_encoding_is_rejected_with_411() {
        let headers = vec![("Transfer-Encoding".to_string(), "chunked".to_string())];
        assert_eq!(content_length(&headers).unwrap_err().status, 411);
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
        match read_request(&mut cursor, peer, &limits()).await {
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
        match read_request(&mut cursor, peer, &l).await {
            ReadOutcome::Reject(resp) => assert_eq!(resp.status, 413),
            _ => panic!("expected rejection"),
        }
    }
}
