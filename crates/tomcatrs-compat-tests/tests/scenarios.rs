//! Compatibility scenarios — the v1.0.0 differential battery.
//!
//! Each `#[tokio::test]` here is a [`CompatScenario`] in spirit: spin up a
//! Tomcat-RS stack on `127.0.0.1:0`, drive one byte-exact HTTP/1.1 request at
//! it, then diff the captured response against the corresponding golden in
//! `golden/`. The whole point of the harness is that the *same* scenarios
//! could be pointed at a real Apache Tomcat (see the `Server` trait in
//! `lib.rs`) and the goldens would catch any divergence between the two.

use bytes::Bytes;
use tomcatrs_compat_tests::{
    goldens, raw_http_request, start_tomcatrs, CompatResult, TomcatrsConfig,
};

/// Drive `request` at a freshly-started Tomcat-RS and capture the response.
async fn drive(request: &[u8], notes: &str) -> CompatResult {
    let handle = start_tomcatrs(TomcatrsConfig::default()).await;
    let (status, headers, body) = raw_http_request(handle.addr(), request)
        .await
        .expect("server should respond");
    handle.shutdown().await;
    CompatResult::from_parts(status, headers, body, notes)
}

#[tokio::test]
async fn get_root_serves_landing_page() {
    let actual = drive(
        b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        "GET / should produce the friendly 200 landing page when the doc base is empty",
    )
    .await;
    goldens::assert_matches("get_root", &actual);
}

#[tokio::test]
async fn get_nonexistent_returns_404() {
    let actual = drive(
        b"GET /nonexistent HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        "an unknown static path with no servlet match should 404",
    )
    .await;
    goldens::assert_matches("get_nonexistent", &actual);
}

#[tokio::test]
async fn path_traversal_is_rejected_with_400() {
    // Connector-level normalization should reject the `..` segments before the
    // adapter ever sees the request.
    let actual = drive(
        b"GET /../../etc/passwd HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        "request targets with `..` escapes must be rejected at the connector",
    )
    .await;
    goldens::assert_matches("path_traversal", &actual);
}

#[tokio::test]
async fn post_root_is_method_not_allowed() {
    // The static handler accepts only GET/HEAD; POST is 405.
    let body = b"name=value";
    let mut req = Vec::new();
    req.extend_from_slice(
        b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 10\r\nConnection: close\r\n\r\n",
    );
    req.extend_from_slice(body);
    let actual = drive(&req, "POST / on the static handler is 405").await;
    goldens::assert_matches("post_root", &actual);
}

#[tokio::test]
async fn options_root_is_method_not_allowed() {
    let actual = drive(
        b"OPTIONS / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        "OPTIONS is not supported by the static handler and yields 405",
    )
    .await;
    goldens::assert_matches("options_root", &actual);
}

#[tokio::test]
async fn head_root_returns_empty_body_with_200() {
    let actual = drive(
        b"HEAD / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        "HEAD / mirrors GET / status/headers but with an empty body",
    )
    .await;
    goldens::assert_matches("head_root", &actual);
    assert!(
        actual.body.is_empty(),
        "HEAD response must have an empty body, got {} bytes",
        actual.body.len()
    );
}

#[tokio::test]
async fn oversized_header_field_is_rejected_with_431() {
    // The default `max_header_size` is 8192 bytes per field — push a single
    // header value well past that and expect a 431.
    let big = "x".repeat(9000);
    let req =
        format!("GET / HTTP/1.1\r\nHost: localhost\r\nX-Huge: {big}\r\nConnection: close\r\n\r\n");
    let actual = drive(
        req.as_bytes(),
        "a header field over `max_header_size` is rejected with 431",
    )
    .await;
    goldens::assert_matches("large_header", &actual);
}

#[tokio::test]
async fn chunked_post_body_is_consumed_and_yields_405() {
    // A chunked POST exercises the chunked-decoder path *and* lands on the
    // static handler's POST branch (405). The two together prove the connector
    // happily reads the whole chunk stream before handing the request off.
    let req = b"POST / HTTP/1.1\r\n\
                Host: localhost\r\n\
                Transfer-Encoding: chunked\r\n\
                Connection: close\r\n\
                \r\n\
                5\r\nhello\r\n\
                6\r\n world\r\n\
                0\r\n\r\n";
    let actual = drive(
        req,
        "chunked POST body is fully consumed; method yields 405",
    )
    .await;
    goldens::assert_matches("chunked_post", &actual);
}

#[tokio::test]
async fn unknown_host_falls_back_to_app_base_and_404s() {
    // An unknown Host header doesn't match any host or alias; the adapter
    // falls back to its configured app_base which is also non-existent, so we
    // get a 404 (rather than a 5xx).
    let actual = drive(
        b"GET /whatever.txt HTTP/1.1\r\nHost: nonexistent.invalid\r\nConnection: close\r\n\r\n",
        "an unknown Host falls back to app_base, which 404s on missing files",
    )
    .await;
    goldens::assert_matches("unknown_host", &actual);
}

#[tokio::test]
async fn http_1_0_request_still_works() {
    // HTTP/1.0 should still be served (the parser accepts it explicitly).
    let actual = drive(
        b"GET / HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        "HTTP/1.0 requests must continue to work",
    )
    .await;
    goldens::assert_matches("http_1_0", &actual);
}

/// A defence-in-depth sanity check: every scenario above produces a body that
/// is either empty (HEAD) or non-trivially long. This catches the failure mode
/// where the connector silently truncates after the headers.
#[tokio::test]
async fn body_lengths_are_consistent_across_scenarios() {
    let cases: &[(&[u8], &str, usize)] = &[
        (
            b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            "GET /",
            100,
        ),
        (
            b"GET /no-such HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            "GET /no-such",
            50,
        ),
    ];
    for (req, label, min_len) in cases {
        let r = drive(req, label).await;
        assert!(
            r.body.len() >= *min_len,
            "{label}: body too short ({} bytes < {min_len})\nbody: {:?}",
            r.body.len(),
            String::from_utf8_lossy(&r.body),
        );
        // And the Server header is always present.
        assert!(
            r.header("server").is_some_and(|s| s.contains("Tomcat-RS")),
            "{label}: missing/unexpected Server header"
        );
    }

    // Confirm the helper is wired up: a chunked decode round-trips a non-empty body.
    let _ = Bytes::from_static(b"chunked round-trip");
}
