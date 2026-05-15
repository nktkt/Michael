//! End-to-end protocol integration tests for the Coyote connector.
//!
//! These tests exercise the real connector code paths:
//!
//! * HTTP/1.1 is driven over a real `TcpStream` bound to `127.0.0.1:0`.
//! * HTTP/2 and AJP are driven over `tokio::io::duplex` halves, which lets us
//!   feed precisely-crafted byte sequences without depending on TCP.
//!
//! Every scenario uses the same simple [`EchoAdapter`] so the test focuses on
//! wire-protocol behaviour, not application logic. There are no new external
//! dependencies — only what the crate already declares (`tokio`, `bytes`,
//! `async-trait`).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use tomcatrs_config::{ConnectorConfig, Protocol, RequestLimits};
use tomcatrs_coyote::acceptor::Acceptor;
use tomcatrs_coyote::ajp::{AjpConnection, AjpMessage, AjpMessageType};
use tomcatrs_coyote::hpack::{HpackDecoder, HpackEncoder};
use tomcatrs_coyote::http2::{self, Frame, FRAME_HEADER_LEN};
use tomcatrs_coyote::http2_conn::Http2Connection;
use tomcatrs_coyote::{Adapter, Request, Response};

// ---------------------------------------------------------------------------
// Shared test fixtures
// ---------------------------------------------------------------------------

/// A simple adapter that echoes the method, path, and body length back. Mirrors
/// the trivial adapters used in the unit tests but lives at integration scope.
struct EchoAdapter;

#[async_trait::async_trait]
impl Adapter for EchoAdapter {
    async fn service(&self, req: Request) -> Response {
        let body = format!(
            "method={} path={} body_len={}",
            req.method,
            req.path,
            req.body.len()
        );
        let mut resp = Response::with_body(200, body);
        resp.set_header("Content-Type", "text/plain");
        resp
    }
}

fn peer() -> SocketAddr {
    "127.0.0.1:65000".parse().unwrap()
}

/// Build a default `HTTP/1.1` connector config on `127.0.0.1:0`.
fn http11_cfg() -> ConnectorConfig {
    ConnectorConfig {
        protocol: Protocol::Http11,
        address: Some("127.0.0.1".parse().unwrap()),
        port: 0,
        tls: None,
        limits: RequestLimits::default(),
    }
}

/// Bind an HTTP/1.1 connector on `127.0.0.1:0` and spawn its accept loop.
async fn spawn_http11_server(limits: RequestLimits) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let mut cfg = http11_cfg();
    cfg.limits = limits;
    let acceptor = Acceptor::bind(&cfg, Arc::new(EchoAdapter))
        .await
        .expect("HTTP/1.1 bind must succeed");
    let addr = acceptor.local_addr();
    let handle = tokio::spawn(async move {
        let _ = acceptor.run().await;
    });
    (addr, handle)
}

/// Read the response body off `client` until the peer closes.
async fn read_until_close(mut client: TcpStream) -> String {
    let mut buf = Vec::new();
    timeout(Duration::from_secs(5), client.read_to_end(&mut buf))
        .await
        .expect("server must close in time")
        .expect("read_to_end ok");
    String::from_utf8(buf).expect("response is utf-8")
}

/// Read exactly one HTTP/1.1 response off `client` on a keep-alive connection.
async fn read_one_response(client: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let mut head_end: Option<usize> = None;
    let mut total_needed: Option<usize> = None;

    loop {
        if let (Some(end), Some(need)) = (head_end, total_needed) {
            if buf.len() >= end + need {
                break;
            }
        }
        let n = timeout(Duration::from_secs(5), client.read(&mut tmp))
            .await
            .expect("read in time")
            .expect("read ok");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if head_end.is_none() {
            if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                head_end = Some(p + 4);
                let head_text = std::str::from_utf8(&buf[..p]).unwrap_or("");
                let mut cl: usize = 0;
                for line in head_text.split("\r\n") {
                    let lower = line.to_ascii_lowercase();
                    if let Some(rest) = lower.strip_prefix("content-length:") {
                        cl = rest.trim().parse().unwrap_or(0);
                        break;
                    }
                }
                total_needed = Some(cl);
            }
        }
    }
    String::from_utf8(buf).expect("utf-8 response")
}

// ===========================================================================
//                              HTTP/1.1 scenarios
// ===========================================================================

#[tokio::test]
async fn http11_get_returns_200_with_echoed_path() {
    let (addr, server) = spawn_http11_server(RequestLimits::default()).await;

    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(b"GET /hello HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let text = read_until_close(client).await;

    assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "got: {text:?}");
    assert!(text.contains("Content-Type: text/plain\r\n"));
    assert!(text.ends_with("method=GET path=/hello body_len=0"));

    server.abort();
}

#[tokio::test]
async fn http11_post_with_content_length_delivers_body() {
    let (addr, server) = spawn_http11_server(RequestLimits::default()).await;

    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(
            b"POST /submit HTTP/1.1\r\n\
              Host: x\r\n\
              Content-Length: 11\r\n\
              Connection: close\r\n\
              \r\n\
              hello world",
        )
        .await
        .unwrap();
    let text = read_until_close(client).await;

    assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(
        text.ends_with("method=POST path=/submit body_len=11"),
        "got: {text:?}"
    );

    server.abort();
}

#[tokio::test]
async fn http11_post_with_chunked_body_is_decoded() {
    let (addr, server) = spawn_http11_server(RequestLimits::default()).await;

    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(
            b"POST /upload HTTP/1.1\r\n\
              Host: x\r\n\
              Transfer-Encoding: chunked\r\n\
              Connection: close\r\n\
              \r\n\
              4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n",
        )
        .await
        .unwrap();
    let text = read_until_close(client).await;

    assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "got: {text:?}");
    assert!(text.ends_with("method=POST path=/upload body_len=9"));

    server.abort();
}

#[tokio::test]
async fn http11_keepalive_serves_three_requests_on_one_connection() {
    let (addr, server) = spawn_http11_server(RequestLimits::default()).await;

    let mut client = TcpStream::connect(addr).await.unwrap();
    for i in 0..3 {
        let last = i == 2;
        let connection = if last { "close" } else { "keep-alive" };
        let req = format!("GET /req/{i} HTTP/1.1\r\nHost: x\r\nConnection: {connection}\r\n\r\n",);
        client.write_all(req.as_bytes()).await.unwrap();

        if last {
            let text = read_until_close(client).await;
            assert!(text.contains("path=/req/2"), "third response: {text:?}");
            break;
        } else {
            let text = read_one_response(&mut client).await;
            assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
            assert!(text.contains(&format!("path=/req/{i}")));
            assert!(
                text.contains("Connection: keep-alive\r\n"),
                "expected keep-alive header on response {i}: {text:?}"
            );
        }
    }

    server.abort();
}

#[tokio::test]
async fn http11_head_request_reaches_adapter() {
    let (addr, server) = spawn_http11_server(RequestLimits::default()).await;

    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(b"HEAD /probe HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let text = read_until_close(client).await;

    assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "got: {text:?}");
    assert!(text.contains("method=HEAD path=/probe"));

    server.abort();
}

#[tokio::test]
async fn http11_options_concrete_path_succeeds() {
    let (addr, server) = spawn_http11_server(RequestLimits::default()).await;

    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(b"OPTIONS /api HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let text = read_until_close(client).await;

    assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "got: {text:?}");
    assert!(text.contains("method=OPTIONS path=/api"));

    server.abort();
}

#[tokio::test]
async fn http11_absolute_uri_form_in_request_line_is_rejected() {
    let (addr, server) = spawn_http11_server(RequestLimits::default()).await;

    // The Coyote normalizer is configured for `origin-form` only — schemes are
    // not stripped, so the absolute-URI form (`GET http://host/...`) is
    // rejected.
    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(b"GET http://example.com/x HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let text = read_until_close(client).await;

    assert!(
        text.starts_with("HTTP/1.1 400 "),
        "absolute-URI form must be rejected with 400; got: {text:?}"
    );

    server.abort();
}

#[tokio::test]
async fn http11_obsolete_header_continuation_is_rejected() {
    let (addr, server) = spawn_http11_server(RequestLimits::default()).await;

    // RFC 7230 deprecated obs-fold (multi-line header continuation via leading
    // whitespace). The Coyote parser does not implement it; a continuation line
    // is interpreted as a malformed header (no colon) → 400.
    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(
            b"GET / HTTP/1.1\r\n\
              Host: x\r\n\
              X-Folded: line1\r\n\
              \tcontinued\r\n\
              Connection: close\r\n\
              \r\n",
        )
        .await
        .unwrap();
    let text = read_until_close(client).await;

    assert!(
        text.starts_with("HTTP/1.1 400 "),
        "obs-fold continuation must be rejected; got: {text:?}"
    );

    server.abort();
}

#[tokio::test]
async fn http11_too_large_header_returns_431() {
    let mut limits = RequestLimits::default();
    limits.max_header_size = 32;
    let (addr, server) = spawn_http11_server(limits).await;

    let huge = "a".repeat(200);
    let req = format!("GET / HTTP/1.1\r\nHost: x\r\nX-Huge: {huge}\r\nConnection: close\r\n\r\n",);
    let mut client = TcpStream::connect(addr).await.unwrap();
    client.write_all(req.as_bytes()).await.unwrap();
    let text = read_until_close(client).await;

    assert!(
        text.starts_with("HTTP/1.1 431 "),
        "oversized header must be 431; got: {text:?}"
    );

    server.abort();
}

#[tokio::test]
async fn http11_too_long_uri_returns_414() {
    let mut limits = RequestLimits::default();
    limits.max_uri_len = 16;
    let (addr, server) = spawn_http11_server(limits).await;

    let long_path = "/".to_string() + &"x".repeat(200);
    let req = format!("GET {long_path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");

    let mut client = TcpStream::connect(addr).await.unwrap();
    client.write_all(req.as_bytes()).await.unwrap();
    let text = read_until_close(client).await;

    assert!(
        text.starts_with("HTTP/1.1 414 "),
        "oversized URI must be 414; got: {text:?}"
    );

    server.abort();
}

#[tokio::test]
async fn http11_smuggling_te_plus_cl_is_rejected_with_400() {
    let (addr, server) = spawn_http11_server(RequestLimits::default()).await;

    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(
            b"POST / HTTP/1.1\r\n\
              Host: x\r\n\
              Content-Length: 5\r\n\
              Transfer-Encoding: chunked\r\n\
              Connection: close\r\n\
              \r\n\
              0\r\n\r\n",
        )
        .await
        .unwrap();
    let text = read_until_close(client).await;

    assert!(
        text.starts_with("HTTP/1.1 400 "),
        "TE+CL must be rejected with 400; got: {text:?}"
    );

    server.abort();
}

// ===========================================================================
//                              HTTP/2 scenarios
// ===========================================================================

/// Parse every fully-buffered frame and pass each one to `f`.
fn for_each_frame(buf: &[u8], mut f: impl FnMut(&Frame)) {
    let mut off = 0;
    while off + FRAME_HEADER_LEN <= buf.len() {
        match Frame::parse(&buf[off..], http2::DEFAULT_MAX_FRAME_SIZE) {
            Ok(Some((frame, consumed))) => {
                f(&frame);
                off += consumed;
            }
            _ => break,
        }
    }
}

/// Drain enough server-emitted bytes off `client` to satisfy `pred`.
async fn read_until_h2<F, R>(client: &mut tokio::io::DuplexStream, mut pred: F) -> R
where
    F: FnMut(&[u8]) -> Option<R>,
{
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    for _ in 0..64 {
        if let Some(r) = pred(&buf) {
            return r;
        }
        let n = timeout(Duration::from_secs(2), client.read(&mut tmp))
            .await
            .expect("server must respond before timeout")
            .expect("read ok");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    pred(&buf).expect("predicate must eventually succeed")
}

#[tokio::test]
async fn http2_preface_and_settings_exchange_completes() {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let server_task =
        tokio::spawn(
            async move { Http2Connection::serve(server, Arc::new(EchoAdapter), peer()).await },
        );

    client.write_all(http2::PREFACE).await.unwrap();
    client
        .write_all(
            &Frame::Settings {
                ack: false,
                params: Vec::new(),
            }
            .encode(),
        )
        .await
        .unwrap();

    let _ = read_until_h2(&mut client, |b| {
        let mut saw_settings = false;
        let mut saw_ack = false;
        for_each_frame(b, |f| {
            if let Frame::Settings { ack, .. } = f {
                if *ack {
                    saw_ack = true;
                } else {
                    saw_settings = true;
                }
            }
        });
        if saw_settings && saw_ack {
            Some(())
        } else {
            None
        }
    })
    .await;

    drop(client);
    let _ = timeout(Duration::from_secs(2), server_task).await;
}

#[tokio::test]
async fn http2_get_request_yields_headers_and_data() {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let server_task =
        tokio::spawn(
            async move { Http2Connection::serve(server, Arc::new(EchoAdapter), peer()).await },
        );

    client.write_all(http2::PREFACE).await.unwrap();
    client
        .write_all(
            &Frame::Settings {
                ack: false,
                params: Vec::new(),
            }
            .encode(),
        )
        .await
        .unwrap();

    let mut enc = HpackEncoder::new(4096);
    let block = enc.encode(&[
        (":method".to_string(), "GET".to_string()),
        (":scheme".to_string(), "http".to_string()),
        (":authority".to_string(), "localhost".to_string()),
        (":path".to_string(), "/probe".to_string()),
    ]);
    client
        .write_all(
            &Frame::Headers {
                stream_id: 1,
                block: Bytes::from(block),
                end_stream: true,
                end_headers: true,
                priority: None,
            }
            .encode(),
        )
        .await
        .unwrap();

    let mut dec = HpackDecoder::new(4096);
    let mut saw_status = false;
    let mut saw_body = false;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];

    for _ in 0..64 {
        let n = timeout(Duration::from_secs(2), client.read(&mut tmp))
            .await
            .expect("server response within timeout")
            .expect("read ok");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        while let Some((frame, consumed)) =
            Frame::parse(&buf, http2::DEFAULT_MAX_FRAME_SIZE).expect("server frames parse")
        {
            buf.drain(..consumed);
            match frame {
                Frame::Headers { block, .. } => {
                    let fields = dec.decode(&block).expect("decode response headers");
                    if fields.iter().any(|(k, v)| k == ":status" && v == "200") {
                        saw_status = true;
                    }
                }
                Frame::Data { data, .. } => {
                    if std::str::from_utf8(data.as_ref())
                        .map(|s| s.contains("path=/probe"))
                        .unwrap_or(false)
                    {
                        saw_body = true;
                    }
                }
                _ => {}
            }
        }
        if saw_status && saw_body {
            break;
        }
    }

    assert!(saw_status, "expected :status 200 in response HEADERS");
    assert!(saw_body, "expected echoed body in DATA frame");

    drop(client);
    let _ = timeout(Duration::from_secs(2), server_task).await;
}

#[tokio::test]
async fn http2_ping_is_answered_with_ack() {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let server_task =
        tokio::spawn(
            async move { Http2Connection::serve(server, Arc::new(EchoAdapter), peer()).await },
        );

    client.write_all(http2::PREFACE).await.unwrap();
    client
        .write_all(
            &Frame::Settings {
                ack: false,
                params: Vec::new(),
            }
            .encode(),
        )
        .await
        .unwrap();
    let payload = [9u8, 8, 7, 6, 5, 4, 3, 2];
    client
        .write_all(
            &Frame::Ping {
                ack: false,
                payload,
            }
            .encode(),
        )
        .await
        .unwrap();

    let () = read_until_h2(&mut client, |b| {
        let mut found = false;
        for_each_frame(b, |f| {
            if let Frame::Ping { ack, payload: p } = f {
                if *ack && p == &payload {
                    found = true;
                }
            }
        });
        if found {
            Some(())
        } else {
            None
        }
    })
    .await;

    drop(client);
    let _ = timeout(Duration::from_secs(2), server_task).await;
}

// ===========================================================================
//                                AJP scenarios
// ===========================================================================

/// MAGIC_IN: server → container packets are prefixed with `0x12 0x34`.
const AJP_MAGIC_IN: [u8; 2] = [0x12, 0x34];
/// MAGIC_OUT: container → server packets are prefixed with `AB`.
const AJP_MAGIC_OUT: [u8; 2] = [b'A', b'B'];

/// AJP attribute codes used in the test forward-request builder.
const AJP_ATTR_SECRET: u8 = 0x0C;
const AJP_ATTR_ARE_DONE: u8 = 0xFF;

/// Append an AJP string (`u16` length + bytes + NUL terminator) to `b`. A
/// `None` value is encoded as the `0xFFFF` null marker.
fn put_ajp_string(b: &mut BytesMut, s: Option<&str>) {
    match s {
        None => b.put_u16(0xFFFF),
        Some(s) => {
            b.put_u16(s.len() as u16);
            b.put_slice(s.as_bytes());
            b.put_u8(0);
        }
    }
}

/// Hand-build the *payload* of a `Forward Request` packet for `GET /` with one
/// header (User-Agent via the common-header code), optionally followed by a
/// `secret` attribute.
///
/// The resulting `AjpMessage` is ready to be `encode(MAGIC_IN)`d.
fn build_ajp_get(secret: Option<&str>) -> AjpMessage {
    let mut p = BytesMut::new();
    p.put_u8(AjpMessageType::ForwardRequest.code());
    p.put_u8(2); // GET
    put_ajp_string(&mut p, Some("HTTP/1.1"));
    put_ajp_string(&mut p, Some("/"));
    put_ajp_string(&mut p, Some("10.0.0.1"));
    put_ajp_string(&mut p, None);
    put_ajp_string(&mut p, Some("example.com"));
    p.put_u16(8009);
    p.put_u8(0); // is_ssl = false

    // Exactly one header — User-Agent via the common-header code 0xA00E.
    p.put_u16(1);
    p.put_u16(0xA00E);
    put_ajp_string(&mut p, Some("integration-test"));

    if let Some(s) = secret {
        p.put_u8(AJP_ATTR_SECRET);
        put_ajp_string(&mut p, Some(s));
    }
    p.put_u8(AJP_ATTR_ARE_DONE);
    AjpMessage::from_payload(p)
}

/// Build a single CPing packet body.
fn build_ajp_cping() -> AjpMessage {
    let mut p = BytesMut::new();
    p.put_u8(AjpMessageType::CPing.code());
    AjpMessage::from_payload(p)
}

#[tokio::test]
async fn ajp_forward_request_yields_send_headers_body_end() {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        AjpConnection::serve(server, Arc::new(EchoAdapter), peer(), None).await
    });

    let fwd = build_ajp_get(None);
    client
        .write_all(&fwd.encode(AJP_MAGIC_IN).unwrap())
        .await
        .unwrap();

    // Read Send Headers.
    let headers = AjpMessage::read_from(&mut client, AJP_MAGIC_OUT)
        .await
        .unwrap()
        .unwrap();
    let p = headers.payload();
    assert_eq!(p[0], AjpMessageType::SendHeaders.code());
    let status = u16::from_be_bytes([p[1], p[2]]);
    assert_eq!(status, 200);

    // Read Send Body Chunk.
    let body = AjpMessage::read_from(&mut client, AJP_MAGIC_OUT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(body.payload()[0], AjpMessageType::SendBodyChunk.code());
    let blen = u16::from_be_bytes([body.payload()[1], body.payload()[2]]) as usize;
    let text = std::str::from_utf8(&body.payload()[3..3 + blen]).unwrap();
    assert!(
        text.contains("method=GET") && text.contains("path=/"),
        "got body: {text:?}"
    );

    // Read End Response.
    let end = AjpMessage::read_from(&mut client, AJP_MAGIC_OUT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(end.payload()[0], AjpMessageType::EndResponse.code());

    drop(client);
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn ajp_cping_is_answered_with_cpong() {
    let (mut client, server) = tokio::io::duplex(4096);
    let server_task = tokio::spawn(async move {
        AjpConnection::serve(server, Arc::new(EchoAdapter), peer(), None).await
    });

    let ping = build_ajp_cping();
    client
        .write_all(&ping.encode(AJP_MAGIC_IN).unwrap())
        .await
        .unwrap();

    let pong = AjpMessage::read_from(&mut client, AJP_MAGIC_OUT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pong.payload(), &[AjpMessageType::CPong.code()]);

    drop(client);
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn ajp_secret_enforcement_accepts_match_rejects_missing() {
    // --- Missing secret → 403 ---
    {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let task = tokio::spawn(async move {
            AjpConnection::serve(server, Arc::new(EchoAdapter), peer(), Some("topsecret")).await
        });
        let fwd = build_ajp_get(None);
        client
            .write_all(&fwd.encode(AJP_MAGIC_IN).unwrap())
            .await
            .unwrap();

        let headers = AjpMessage::read_from(&mut client, AJP_MAGIC_OUT)
            .await
            .unwrap()
            .unwrap();
        let p = headers.payload();
        assert_eq!(p[0], AjpMessageType::SendHeaders.code());
        let status = u16::from_be_bytes([p[1], p[2]]);
        assert_eq!(status, 403, "missing secret must be 403");

        drop(client);
        task.await.unwrap().unwrap();
    }

    // --- Correct secret → 200 ---
    {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let task = tokio::spawn(async move {
            AjpConnection::serve(server, Arc::new(EchoAdapter), peer(), Some("topsecret")).await
        });
        let fwd = build_ajp_get(Some("topsecret"));
        client
            .write_all(&fwd.encode(AJP_MAGIC_IN).unwrap())
            .await
            .unwrap();

        let headers = AjpMessage::read_from(&mut client, AJP_MAGIC_OUT)
            .await
            .unwrap()
            .unwrap();
        let p = headers.payload();
        assert_eq!(p[0], AjpMessageType::SendHeaders.code());
        let status = u16::from_be_bytes([p[1], p[2]]);
        assert_eq!(status, 200, "matching secret must be 200");

        // Drain the body chunk + end response so the server loop exits cleanly.
        let _body = AjpMessage::read_from(&mut client, AJP_MAGIC_OUT)
            .await
            .unwrap()
            .unwrap();
        let _end = AjpMessage::read_from(&mut client, AJP_MAGIC_OUT)
            .await
            .unwrap()
            .unwrap();

        drop(client);
        task.await.unwrap().unwrap();
    }
}
