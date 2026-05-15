//! Bench: parsing a typical HTTP/1.1 request off an in-memory byte buffer.
//!
//! The HTTP/1.1 parser lives behind [`tomcatrs_coyote::http1::serve_connection`]
//! and isn't directly exported. To exercise it we drive `serve_connection` with
//! a [`tokio::io::DuplexStream`] preloaded with a representative request and a
//! no-op adapter that immediately returns `204 No Content`. The work measured
//! is dominated by request parsing — header tokenization, body framing,
//! URI normalization — plus a fixed-size response write.

use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use tokio::io::AsyncWriteExt;

use tomcatrs_coyote::{http1, Adapter, Request, Response};

/// A trivial adapter that ignores the request and returns `204 No Content`.
struct NoopAdapter;

#[async_trait::async_trait]
impl Adapter for NoopAdapter {
    async fn service(&self, _req: Request) -> Response {
        Response::new(204)
    }
}

/// A representative real-world HTTP/1.1 request: a `GET` with a typical browser
/// header set and `Connection: close` so the parser sees exactly one request.
const TYPICAL_REQUEST: &[u8] = b"GET /index.html HTTP/1.1\r\n\
Host: www.example.com\r\n\
User-Agent: Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36\r\n\
Accept: text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8\r\n\
Accept-Language: en-US,en;q=0.5\r\n\
Accept-Encoding: gzip, deflate, br\r\n\
Connection: close\r\n\
Cache-Control: max-age=0\r\n\
Upgrade-Insecure-Requests: 1\r\n\
Cookie: JSESSIONID=ABCDEF1234567890; theme=dark\r\n\
\r\n";

fn bench_parse_typical_request(c: &mut Criterion) {
    // Build a multi-thread runtime once, outside the inner measurement loop.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // Workload setup outside the inner loop: a fresh `RequestLimits` and the
    // adapter. The request bytes are reused per iteration via `black_box`.
    let limits = tomcatrs_config::RequestLimits::default();
    let adapter: Arc<dyn Adapter> = Arc::new(NoopAdapter);
    let peer = "127.0.0.1:65535".parse().unwrap();

    c.bench_function("http1_parse_typical_get", |b| {
        b.to_async(&rt).iter(|| {
            let limits = limits.clone();
            let adapter = Arc::clone(&adapter);
            async move {
                // A `DuplexStream` is an in-memory bidirectional pipe; the
                // client side writes the request, the server side reads it.
                let (mut client, server) = tokio::io::duplex(8192);
                client.write_all(black_box(TYPICAL_REQUEST)).await.unwrap();
                drop(client); // EOF after the request → parser returns
                http1::serve_connection(server, peer, adapter.as_ref(), &limits)
                    .await
                    .unwrap();
            }
        });
    });
}

criterion_group!(benches, bench_parse_typical_request);
criterion_main!(benches);
