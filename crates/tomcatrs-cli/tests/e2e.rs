//! End-to-end smoke test for the `tomcatrs` CLI binary.
//!
//! This test actually boots the compiled binary as a subprocess (Cargo
//! automatically builds it first and hands its path to the test via the
//! `CARGO_BIN_EXE_tomcatrs` env var), points it at a freshly-created temporary
//! `app-base`, then drives it over a real TCP socket with hand-rolled HTTP/1.1
//! requests. No `reqwest`/`hyper` is pulled in — the goal is to exercise the
//! same network surface a real client would, with only the dependencies the
//! workspace already provides (`tokio`).
//!
//! Scenarios covered:
//!
//! * `GET /`            → 200 with the `index.html` placed under app-base.
//! * `GET /WEB-INF/web.xml` → rejected (`404` since the file is absent under
//!   our minimal app-base; the request is *not* allowed to escape).
//! * `GET /../etc/passwd` → `403` (path traversal refusal).
//! * `HEAD /`           → headers only, empty body.
//! * 50 keep-alive `GET /` on a single connection → all 200.
//!
//! The child is killed on test completion via a `Drop` guard so a flaky test
//! never leaks a server process.

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Kill-on-drop guard around a spawned child process so a panic in the middle
/// of the test never orphans the server.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // Best-effort: the child may already have exited.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Reserve a free TCP port by binding to `127.0.0.1:0` and immediately
/// dropping the listener. There's a tiny race between us releasing the port
/// and the CLI binding it, but on a developer or CI machine that's effectively
/// never hit; the bigger concern (kernel re-handing the same port to another
/// process in the same millisecond) is acceptable for a smoke test.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
}

/// Make a unique temp directory under `std::env::temp_dir()` and return it.
/// Cleaned up on test completion via [`TempDir::Drop`].
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "tomcatrs-cli-e2e-{}-{}-{}",
            label,
            std::process::id(),
            nanos,
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Poll `127.0.0.1:port` until a TCP connection succeeds or `deadline`
/// elapses. Returns `true` on success, `false` on timeout.
fn wait_until_listening(port: u16, deadline: Instant) -> bool {
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(
            &format!("127.0.0.1:{port}").parse().expect("parse addr"),
            Duration::from_millis(250),
        )
        .is_ok()
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// A parsed HTTP/1.1 response: status code, raw headers block (lowercased
/// keys), and the body bytes that followed. Just enough for assertions.
struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        let needle = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k == &needle)
            .map(|(_, v)| v.as_str())
    }
}

/// Read exactly one HTTP/1.1 response off `stream`, honouring `Content-Length`
/// for the body (which is all our server emits — no chunked transfers on the
/// adapter responses we hit here).
fn read_response(stream: &mut TcpStream) -> std::io::Result<HttpResponse> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;

    // 1. Read until we've seen the end of the header block.
    let mut buf = Vec::with_capacity(8 * 1024);
    let header_end = loop {
        let mut tmp = [0u8; 4096];
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Err(std::io::Error::new(
                ErrorKind::UnexpectedEof,
                "connection closed before headers completed",
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(idx) = find_double_crlf(&buf) {
            break idx;
        }
    };

    let head = std::str::from_utf8(&buf[..header_end])
        .map_err(|_| std::io::Error::new(ErrorKind::InvalidData, "non-utf8 header block"))?;

    // 2. Parse the status line and the header pairs.
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidData, "missing status line"))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidData, "malformed status line"))?;

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }

    // 3. Read the body, length-delimited.
    let content_length: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);

    // 4 = the `\r\n\r\n` separator itself.
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let mut tmp = [0u8; 4096];
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Err(std::io::Error::new(
                ErrorKind::UnexpectedEof,
                "connection closed before body completed",
            ));
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);

    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Open a fresh TCP connection and send a single `Connection: close` request,
/// then read the lone response.
fn single_request(port: u16, request: &str) -> HttpResponse {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("set_write_timeout");
    stream.write_all(request.as_bytes()).expect("write");
    let resp = read_response(&mut stream).expect("read response");
    let _ = stream.shutdown(Shutdown::Both);
    resp
}

#[test]
fn cli_serves_real_http_traffic() {
    // 1. Lay out a tiny app-base with a known `index.html` at its root. The
    //    CLI's `default_dev` config has no contexts, so the Catalina adapter
    //    falls back to serving requests directly out of `--app-base`. With
    //    `GET /` that maps to `<app-base>/index.html`.
    let app_base = TempDir::new("appbase");
    let landing_body = b"<!doctype html><title>tomcatrs e2e</title>e2e-landing-marker";
    std::fs::write(app_base.path().join("index.html"), landing_body).expect("write index.html");

    // 2. Spawn the CLI on a free port. Cargo populates `CARGO_BIN_EXE_tomcatrs`
    //    after building the binary (which is implicit for integration tests in
    //    the binary crate).
    let port = free_port();
    let bin = env!("CARGO_BIN_EXE_tomcatrs");
    let child = Command::new(bin)
        .arg("run")
        .arg("--port")
        .arg(port.to_string())
        .arg("--app-base")
        .arg(app_base.path())
        .arg("--log-level")
        .arg("warn")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tomcatrs");
    let _guard = ChildGuard(child);

    // 3. Wait up to 30s for the listener to come up. A cold debug build on a
    //    slow machine plus the Catalina startup chain can be measurable.
    let deadline = Instant::now() + Duration::from_secs(30);
    assert!(
        wait_until_listening(port, deadline),
        "tomcatrs CLI never started listening on 127.0.0.1:{port}",
    );

    // --- Scenario 1: GET / returns the index.html we wrote -------------------
    let resp = single_request(
        port,
        "GET / HTTP/1.1\r\n\
         Host: localhost\r\n\
         Connection: close\r\n\
         \r\n",
    );
    assert_eq!(resp.status, 200, "GET / status");
    assert!(
        resp.body
            .windows(landing_body.len())
            .any(|w| w == landing_body)
            || resp.body == landing_body,
        "GET / body should contain our landing marker; got {} bytes",
        resp.body.len(),
    );
    let ctype = resp.header("content-type").unwrap_or("");
    assert!(
        ctype.starts_with("text/html"),
        "GET / content-type should be html, got {ctype:?}",
    );

    // --- Scenario 2: GET /WEB-INF/web.xml is not served ----------------------
    // Our temp app-base has no WEB-INF, so the static handler should 404 (and
    // crucially must not 200 by escaping into something else). 403 is also
    // acceptable if a future hardening layer guards the path explicitly.
    let resp = single_request(
        port,
        "GET /WEB-INF/web.xml HTTP/1.1\r\n\
         Host: localhost\r\n\
         Connection: close\r\n\
         \r\n",
    );
    assert!(
        resp.status == 403 || resp.status == 404,
        "GET /WEB-INF/web.xml expected 403/404, got {}",
        resp.status,
    );

    // --- Scenario 3: GET /../etc/passwd is forbidden -------------------------
    // The coyote normalizer rejects `..` traversal at the protocol layer with
    // `400 Bad Request` (so the request never reaches the adapter). If a
    // future hardening swap moves the check into the adapter itself it would
    // surface as `403`; either way the file must NOT be served, so we accept
    // both refusal codes and explicitly reject `2xx`.
    let resp = single_request(
        port,
        "GET /../etc/passwd HTTP/1.1\r\n\
         Host: localhost\r\n\
         Connection: close\r\n\
         \r\n",
    );
    assert!(
        resp.status == 400 || resp.status == 403,
        "GET /../etc/passwd must be refused (400/403), got {}",
        resp.status,
    );

    // --- Scenario 4: HEAD / returns headers but no body ----------------------
    let resp = single_request(
        port,
        "HEAD / HTTP/1.1\r\n\
         Host: localhost\r\n\
         Connection: close\r\n\
         \r\n",
    );
    assert_eq!(resp.status, 200, "HEAD / status");
    assert!(
        resp.body.is_empty(),
        "HEAD / body must be empty, got {} bytes",
        resp.body.len(),
    );
    // Content-Length should still describe the entity that would have been
    // returned for an equivalent GET — but at minimum it must be present.
    assert!(
        resp.header("content-length").is_some(),
        "HEAD / should expose a Content-Length header",
    );

    // --- Scenario 5: 50 keep-alive requests on one connection ----------------
    // Pipeline the next request only after reading the previous response, to
    // keep the test deterministic on slower listeners.
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect keep-alive");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("set_write_timeout");
    for i in 0..50 {
        let req = "GET / HTTP/1.1\r\n\
                   Host: localhost\r\n\
                   \r\n";
        stream
            .write_all(req.as_bytes())
            .unwrap_or_else(|e| panic!("write keep-alive #{i}: {e}"));
        let resp =
            read_response(&mut stream).unwrap_or_else(|e| panic!("read keep-alive #{i}: {e}"));
        assert_eq!(resp.status, 200, "keep-alive request #{i} status");
    }
    let _ = stream.shutdown(Shutdown::Both);

    // The `ChildGuard` SIGKILLs the server here.
}
