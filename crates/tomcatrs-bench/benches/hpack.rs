//! Bench: HPACK encode + decode round-trip of a typical header set.
//!
//! Measures the cost of compressing and expanding the four headers a normal
//! HTTP/2 `GET /` request carries: the `:method`, `:path`, plus a request
//! `host`, `user-agent`, and `accept`. Encoder and decoder are reconstructed
//! every iteration so the dynamic table starts empty — i.e. we measure the
//! steady-state per-block cost, not the first-block cost.

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use tomcatrs_coyote::hpack::{HpackDecoder, HpackEncoder};

/// The header set a typical browser HTTP/2 `GET /` sends.
fn typical_headers() -> Vec<(String, String)> {
    vec![
        (":method".to_string(), "GET".to_string()),
        (":scheme".to_string(), "https".to_string()),
        (":path".to_string(), "/".to_string()),
        (":authority".to_string(), "www.example.com".to_string()),
        (
            "user-agent".to_string(),
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
                .to_string(),
        ),
        (
            "accept".to_string(),
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,\
             image/webp,*/*;q=0.8"
                .to_string(),
        ),
    ]
}

fn bench_hpack_encode(c: &mut Criterion) {
    // Workload constructed outside the inner loop.
    let headers = typical_headers();

    c.bench_function("hpack_encode_typical", |b| {
        b.iter(|| {
            // Fresh encoder per iteration so each measurement is steady-state.
            let mut enc = HpackEncoder::new(4096);
            let block = enc.encode(black_box(&headers));
            black_box(block);
        });
    });
}

fn bench_hpack_decode(c: &mut Criterion) {
    let headers = typical_headers();
    // Encode once so the bench measures decode-only.
    let mut enc = HpackEncoder::new(4096);
    let block = enc.encode(&headers);

    c.bench_function("hpack_decode_typical", |b| {
        b.iter(|| {
            let mut dec = HpackDecoder::new(4096);
            let out = dec.decode(black_box(&block)).unwrap();
            black_box(out);
        });
    });
}

fn bench_hpack_round_trip(c: &mut Criterion) {
    let headers = typical_headers();

    c.bench_function("hpack_round_trip_typical", |b| {
        b.iter(|| {
            let mut enc = HpackEncoder::new(4096);
            let mut dec = HpackDecoder::new(4096);
            let block = enc.encode(black_box(&headers));
            let out = dec.decode(&block).unwrap();
            black_box(out);
        });
    });
}

criterion_group!(
    benches,
    bench_hpack_encode,
    bench_hpack_decode,
    bench_hpack_round_trip
);
criterion_main!(benches);
