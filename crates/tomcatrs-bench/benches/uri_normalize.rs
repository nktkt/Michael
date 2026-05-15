//! Bench: `normalize_target` over a corpus of representative request URIs.
//!
//! The corpus exercises every code path that matters in production:
//!
//! * plain absolute paths,
//! * percent-encoded segments,
//! * `.` and `..` collapsing inside the root,
//! * duplicate slashes,
//! * preserved trailing slashes,
//! * a query string split.

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use tomcatrs_coyote::normalize::normalize_target;

/// A representative mix of URIs the parser is likely to see.
fn corpus() -> Vec<&'static str> {
    vec![
        "/",
        "/index.html",
        "/app/static/js/main.bundle.js",
        "/api/v1/users/12345/profile",
        "/search?q=hello+world&n=20&page=3",
        "/a%20b/c%20d/e%20f",
        "/a/b/./c/./d",
        "/a/b/c/../d/e",
        "/a//b///c/d",
        "/app/docs/",
        "/static/css/site.min.css?v=2024-01-01",
        "/health",
        "/metrics",
        "/favicon.ico",
        "/robots.txt",
        "/users/alice/posts/2024-01-15/comments",
    ]
}

fn bench_normalize_corpus(c: &mut Criterion) {
    // Workload constructed outside the inner loop.
    let uris = corpus();

    c.bench_function("uri_normalize_corpus", |b| {
        b.iter(|| {
            // Walk every URI in the corpus once per iteration so the measured
            // unit is "one full pass" — easy to compare across runs.
            for uri in black_box(&uris) {
                let n = normalize_target(uri).unwrap();
                black_box(n);
            }
        });
    });
}

fn bench_normalize_single_plain(c: &mut Criterion) {
    // The hot path: a simple absolute path with nothing to decode.
    let uri = "/app/api/users/12345/profile";

    c.bench_function("uri_normalize_plain_path", |b| {
        b.iter(|| {
            let n = normalize_target(black_box(uri)).unwrap();
            black_box(n);
        });
    });
}

criterion_group!(
    benches,
    bench_normalize_corpus,
    bench_normalize_single_plain
);
criterion_main!(benches);
