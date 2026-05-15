//! Minimal hand-rolled HTTP/1.1 client.
//!
//! The soaker doesn't need keep-alive, chunked uploads, HTTPS, or anything
//! else `reqwest` / `hyper` brings in. One `GET --target` per request is all
//! we drive, and we open a fresh TCP connection each time so the measurement
//! always includes the full connect-establish + request-send + response-read
//! round-trip. (A real-world client would pool; we don't, because the soaker
//! is testing the *server's* stability, not its own client efficiency.)

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// A parsed `http://host:port/path?query` target.
#[derive(Debug, Clone)]
pub struct HttpTarget {
    /// The hostname or IP literal we connect to.
    pub host: String,
    /// The TCP port (default 80 if the URL omits it).
    pub port: u16,
    /// The path + optional query string to send on the request line,
    /// always non-empty (defaults to `/`).
    pub path_and_query: String,
    /// The `Host:` header value (host plus port when non-default).
    pub host_header: String,
}

impl HttpTarget {
    /// Parse a fully-qualified `http://…` URL into its connect tuple. We
    /// intentionally don't pull in a URL crate: the soaker only ever consumes
    /// the URL shape `http://HOST[:PORT][/PATH[?QUERY]]`, and rejecting the
    /// rest is fine.
    pub fn parse(url: &str) -> Result<Self, String> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| "only http:// URLs are supported".to_string())?;
        let (authority, path_and_query) = match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            return Err("missing host".to_string());
        }
        let (host, port) = match authority.rfind(':') {
            Some(idx) => {
                let host = &authority[..idx];
                let port: u16 = authority[idx + 1..]
                    .parse()
                    .map_err(|e| format!("bad port: {e}"))?;
                (host.to_string(), port)
            }
            None => (authority.to_string(), 80u16),
        };
        let host_header = if port == 80 {
            host.clone()
        } else {
            format!("{}:{}", host, port)
        };
        Ok(Self {
            host,
            port,
            path_and_query: path_and_query.to_string(),
            host_header,
        })
    }
}

/// Outcome of a single request issued by a worker.
#[derive(Debug)]
pub enum RequestOutcome {
    /// We got a complete HTTP/1.1 response with this status code.
    Ok(u16),
    /// We failed before getting a parseable response — connect refused,
    /// timed out, bad framing, etc.
    Err(String),
}

/// Open a TCP connection, send `GET path HTTP/1.1`, read just enough of the
/// response to learn the status code, and discard the rest. Bounded by
/// `request_timeout`.
pub async fn issue_request(target: &HttpTarget, request_timeout: Duration) -> RequestOutcome {
    match timeout(request_timeout, do_one(target)).await {
        Ok(Ok(status)) => RequestOutcome::Ok(status),
        Ok(Err(e)) => RequestOutcome::Err(e),
        Err(_) => RequestOutcome::Err("request timed out".into()),
    }
}

async fn do_one(target: &HttpTarget) -> Result<u16, String> {
    let addr = format!("{}:{}", target.host, target.port);
    let mut stream = TcpStream::connect(&addr)
        .await
        .map_err(|e| format!("connect {addr}: {e}"))?;
    // Disable Nagle so the request goes out in a single send and we don't pay
    // 40ms delay on machines where the kernel batches small writes.
    let _ = stream.set_nodelay(true);

    let request = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         User-Agent: tomcatrs-soak/1.0\r\n\
         Accept: */*\r\n\
         Connection: close\r\n\
         \r\n",
        path = target.path_and_query,
        host = target.host_header,
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| format!("write: {e}"))?;
    stream.flush().await.map_err(|e| format!("flush: {e}"))?;

    // Read until we have at least the status line. The first ~512 bytes are
    // usually enough; we keep reading if not.
    let mut buf = Vec::with_capacity(1024);
    let status = loop {
        let mut tmp = [0u8; 1024];
        let n = stream
            .read(&mut tmp)
            .await
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("connection closed before status line".into());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(idx) = buf.iter().position(|&b| b == b'\n') {
            let line = &buf[..idx];
            // "HTTP/1.1 200 OK\r"
            let s = std::str::from_utf8(line).map_err(|_| "non-utf8 status line".to_string())?;
            let code = s
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse::<u16>().ok())
                .ok_or_else(|| format!("malformed status line: {s:?}"))?;
            break code;
        }
        if buf.len() > 64 * 1024 {
            return Err("status line exceeded 64 KiB".into());
        }
    };

    // Drain the rest of the response so the server-side write doesn't block.
    // We bound the drain so a pathological server can't pin a worker forever.
    let mut sink = [0u8; 16 * 1024];
    let mut drained = 0usize;
    loop {
        match stream.read(&mut sink).await {
            Ok(0) => break,
            Ok(n) => {
                drained += n;
                if drained > 16 * 1024 * 1024 {
                    // 16 MiB ought to be enough for the responses we hit.
                    break;
                }
            }
            Err(_) => break,
        }
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_url() {
        let t = HttpTarget::parse("http://127.0.0.1:8080/").unwrap();
        assert_eq!(t.host, "127.0.0.1");
        assert_eq!(t.port, 8080);
        assert_eq!(t.path_and_query, "/");
        assert_eq!(t.host_header, "127.0.0.1:8080");
    }

    #[test]
    fn defaults_port_80_and_path_slash() {
        let t = HttpTarget::parse("http://example.com").unwrap();
        assert_eq!(t.port, 80);
        assert_eq!(t.path_and_query, "/");
        assert_eq!(t.host_header, "example.com");
    }

    #[test]
    fn keeps_query_string() {
        let t = HttpTarget::parse("http://h:1/path?a=b").unwrap();
        assert_eq!(t.path_and_query, "/path?a=b");
    }

    #[test]
    fn rejects_https() {
        assert!(HttpTarget::parse("https://example.com").is_err());
    }
}
