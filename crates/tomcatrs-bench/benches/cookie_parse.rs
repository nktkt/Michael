//! Bench: parsing a realistic `Cookie:` request header.
//!
//! The header value mixes session, preference, analytics, and tracking
//! cookies — the shape typical browsers send to a real application.

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use tomcatrs_coyote::cookies::parse_cookie_header;

/// A realistic `Cookie:` header value: a session id plus a handful of
/// preference and analytics cookies.
const TYPICAL_COOKIE_HEADER: &str = "JSESSIONID=ABCDEF1234567890FEDCBA0987654321; \
theme=dark; \
lang=en-US; \
_ga=GA1.2.1234567890.1700000000; \
_gid=GA1.2.0987654321.1700000000; \
_fbp=fb.1.1700000000000.123456789; \
_hjSessionUser_12345=eyJpZCI6IjEyMzQ1Njc4OTAifQ==; \
csrf_token=a1b2c3d4e5f6g7h8i9j0; \
remember_me=1; \
last_visit=2024-01-15T12%3A34%3A56Z";

fn bench_cookie_parse(c: &mut Criterion) {
    // Workload constructed outside the inner loop.
    let header = TYPICAL_COOKIE_HEADER;

    c.bench_function("cookie_parse_typical", |b| {
        b.iter(|| {
            let cookies = parse_cookie_header(black_box(header));
            black_box(cookies);
        });
    });
}

fn bench_cookie_parse_single(c: &mut Criterion) {
    // The hot single-cookie path that a login-only request hits.
    let header = "JSESSIONID=ABCDEF1234567890FEDCBA0987654321";

    c.bench_function("cookie_parse_single", |b| {
        b.iter(|| {
            let cookies = parse_cookie_header(black_box(header));
            black_box(cookies);
        });
    });
}

criterion_group!(benches, bench_cookie_parse, bench_cookie_parse_single);
criterion_main!(benches);
