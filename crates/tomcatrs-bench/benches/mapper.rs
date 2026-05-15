//! Bench: [`Mapper::map`] against a synthesized engine of
//! 100 hosts × 50 contexts × 20 wrappers per context.
//!
//! That's 100k servlet registrations end-to-end — a stress-shaped
//! configuration designed to exercise host lookup, longest-prefix context
//! resolution, and wrapper-pattern scoring all on the same call.

use std::path::PathBuf;
use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use tomcatrs_catalina::mapper::UrlPattern;
use tomcatrs_catalina::{Context, Engine, Host, Mapper, Wrapper};

/// Build an engine with `hosts` hosts, each carrying `contexts` contexts, each
/// carrying `wrappers` servlets with one exact + one prefix + one extension
/// pattern.
///
/// Names are deterministic so the bench corpus is reproducible:
/// `host-{h}.example.com`, context `/ctx-{c}`, wrapper servlet
/// `srv-{w}` with patterns `/exact-{w}`, `/prefix-{w}/*`, `*.ext{w}`.
fn build_engine(hosts: usize, contexts: usize, wrappers: usize) -> Arc<Engine> {
    let engine = Engine::new("Catalina", "host-0.example.com");
    for h in 0..hosts {
        let host_name = format!("host-{h}.example.com");
        let mut ctx_list: Vec<Arc<Context>> = Vec::with_capacity(contexts);
        for c in 0..contexts {
            let ctx_path = format!("/ctx-{c}");
            let mut wraps: Vec<Arc<Wrapper>> = Vec::with_capacity(wrappers);
            for w in 0..wrappers {
                let mut wrapper = Wrapper::new(
                    format!("srv-{w}"),
                    "com.example.Servlet",
                    vec![
                        UrlPattern::Exact(format!("/exact-{w}")),
                        UrlPattern::Prefix(format!("/prefix-{w}")),
                        UrlPattern::Extension(format!("ext{w}")),
                    ],
                );
                // Ensure at least one wrapper carries a default pattern so
                // unmatched paths still route somewhere.
                if w == 0 {
                    wrapper.add_mapping(UrlPattern::Default);
                }
                wraps.push(Arc::new(wrapper));
            }
            ctx_list.push(Arc::new(Context::new(
                ctx_path,
                PathBuf::from("/tmp"),
                false,
                wraps,
            )));
        }
        let host = Host::new(host_name, PathBuf::from("/tmp"), Vec::new(), ctx_list);
        engine.add_host(Arc::new(host));
    }
    Arc::new(engine)
}

fn bench_mapper_resolve(c: &mut Criterion) {
    // Workload constructed outside the inner loop: a 100 × 50 × 20 tree.
    let engine = build_engine(100, 50, 20);
    let mapper = Mapper::new(engine);

    // A handful of representative `(host, uri)` lookups spanning the tree:
    // first/middle/last host, first/middle/last context, and matches against
    // each of the three pattern kinds.
    let queries: Vec<(&'static str, String)> = vec![
        ("host-0.example.com", "/ctx-0/exact-0".to_string()),
        (
            "host-49.example.com",
            "/ctx-25/prefix-10/sub/path".to_string(),
        ),
        ("host-99.example.com", "/ctx-49/some-file.ext19".to_string()),
        ("host-50.example.com", "/ctx-30/exact-19".to_string()),
        ("host-25.example.com", "/ctx-49/prefix-0/x/y/z".to_string()),
    ];

    c.bench_function("mapper_resolve_100x50x20", |b| {
        b.iter(|| {
            for (host, uri) in black_box(&queries) {
                let result = mapper.map(black_box(host), black_box(uri.as_str()));
                black_box(result);
            }
        });
    });
}

criterion_group!(benches, bench_mapper_resolve);
criterion_main!(benches);
