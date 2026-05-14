//! A lightweight, atomic-backed metrics registry.
//!
//! This module is fully working. It provides exactly the two metric kinds the
//! runtime needs to start with:
//!
//! * **Counters** — monotonically increasing `u64` values (requests served,
//!   bytes sent, errors, …).
//! * **Gauges** — `i64` values that go up and down (active connections,
//!   thread-pool size, …).
//!
//! Metrics are keyed by name in a `dashmap::DashMap`, so registration and
//! lookup are lock-free for readers and safe to share across tasks. The whole
//! registry renders to the
//! [Prometheus text exposition format](https://prometheus.io/docs/instrumenting/exposition_formats/)
//! via [`MetricsRegistry::render_prometheus`].

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;

/// A monotonically increasing counter.
///
/// Cloning a `Counter` is cheap — clones share the same underlying atomic, so
/// every holder observes and contributes to the same value.
#[derive(Debug, Clone)]
pub struct Counter {
    value: Arc<AtomicU64>,
}

impl Counter {
    fn new() -> Self {
        Counter {
            value: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Add one to the counter.
    pub fn inc(&self) {
        self.add(1);
    }

    /// Add `n` to the counter.
    pub fn add(&self, n: u64) {
        self.value.fetch_add(n, Ordering::Relaxed);
    }

    /// Read the current value.
    pub fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }
}

/// A gauge: an arbitrary value that can move in either direction.
///
/// Like [`Counter`], clones share the same backing atomic.
#[derive(Debug, Clone)]
pub struct Gauge {
    value: Arc<AtomicI64>,
}

impl Gauge {
    fn new() -> Self {
        Gauge {
            value: Arc::new(AtomicI64::new(0)),
        }
    }

    /// Overwrite the gauge with `v`.
    pub fn set(&self, v: i64) {
        self.value.store(v, Ordering::Relaxed);
    }

    /// Add `delta` (which may be negative) to the gauge.
    pub fn add(&self, delta: i64) {
        self.value.fetch_add(delta, Ordering::Relaxed);
    }

    /// Increment the gauge by one.
    pub fn inc(&self) {
        self.add(1);
    }

    /// Decrement the gauge by one.
    pub fn dec(&self) {
        self.add(-1);
    }

    /// Read the current value.
    pub fn get(&self) -> i64 {
        self.value.load(Ordering::Relaxed)
    }
}

/// The metric kind, used internally to drive `# TYPE` lines on export.
#[derive(Debug, Clone)]
enum Metric {
    Counter(Counter),
    Gauge(Gauge),
}

/// A registry of named counters and gauges.
///
/// Construct one with [`MetricsRegistry::new`], then call [`counter`] /
/// [`gauge`] to obtain handles. Calling either with a name that already exists
/// returns a handle to the *same* metric, so registration is idempotent.
///
/// [`counter`]: MetricsRegistry::counter
/// [`gauge`]: MetricsRegistry::gauge
#[derive(Debug, Default)]
pub struct MetricsRegistry {
    metrics: DashMap<String, Metric>,
}

impl MetricsRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        MetricsRegistry {
            metrics: DashMap::new(),
        }
    }

    /// Get (or lazily create) the counter named `name`.
    ///
    /// # Panics
    ///
    /// Panics if `name` is already registered as a gauge — a metric name must
    /// have a single, stable type for the lifetime of the process.
    pub fn counter(&self, name: &str) -> Counter {
        let entry = self
            .metrics
            .entry(name.to_string())
            .or_insert_with(|| Metric::Counter(Counter::new()));
        match entry.value() {
            Metric::Counter(c) => c.clone(),
            Metric::Gauge(_) => {
                panic!("metric '{name}' is already registered as a gauge, not a counter")
            }
        }
    }

    /// Get (or lazily create) the gauge named `name`.
    ///
    /// # Panics
    ///
    /// Panics if `name` is already registered as a counter.
    pub fn gauge(&self, name: &str) -> Gauge {
        let entry = self
            .metrics
            .entry(name.to_string())
            .or_insert_with(|| Metric::Gauge(Gauge::new()));
        match entry.value() {
            Metric::Gauge(g) => g.clone(),
            Metric::Counter(_) => {
                panic!("metric '{name}' is already registered as a counter, not a gauge")
            }
        }
    }

    /// Number of distinct metrics currently registered.
    pub fn len(&self) -> usize {
        self.metrics.len()
    }

    /// `true` if no metrics have been registered.
    pub fn is_empty(&self) -> bool {
        self.metrics.is_empty()
    }

    /// Render every registered metric in the Prometheus text exposition format.
    ///
    /// Each metric contributes a `# TYPE <name> <kind>` line followed by a
    /// `<name> <value>` sample line. Output is sorted by metric name so the
    /// result is deterministic (useful for tests and diffs).
    pub fn render_prometheus(&self) -> String {
        // Collect into a sorted vec for stable ordering.
        let mut entries: Vec<(String, Metric)> = self
            .metrics
            .iter()
            .map(|kv| (kv.key().clone(), kv.value().clone()))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let mut out = String::new();
        for (name, metric) in entries {
            match metric {
                Metric::Counter(c) => {
                    out.push_str("# TYPE ");
                    out.push_str(&name);
                    out.push_str(" counter\n");
                    out.push_str(&name);
                    out.push(' ');
                    out.push_str(&c.get().to_string());
                    out.push('\n');
                }
                Metric::Gauge(g) => {
                    out.push_str("# TYPE ");
                    out.push_str(&name);
                    out.push_str(" gauge\n");
                    out.push_str(&name);
                    out.push(' ');
                    out.push_str(&g.get().to_string());
                    out.push('\n');
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_increments_and_shares_state() {
        let reg = MetricsRegistry::new();
        let c = reg.counter("http_requests_total");
        c.inc();
        c.add(4);
        // A second handle to the same name sees the same value.
        assert_eq!(reg.counter("http_requests_total").get(), 5);
    }

    #[test]
    fn gauge_moves_both_ways() {
        let reg = MetricsRegistry::new();
        let g = reg.gauge("active_connections");
        g.set(10);
        g.inc();
        g.dec();
        g.add(-3);
        assert_eq!(g.get(), 7);
    }

    #[test]
    fn prometheus_render_contains_metrics_with_type_lines() {
        let reg = MetricsRegistry::new();
        reg.counter("http_requests_total").add(42);
        reg.gauge("active_connections").set(3);

        let text = reg.render_prometheus();
        assert!(text.contains("# TYPE http_requests_total counter\n"));
        assert!(text.contains("http_requests_total 42\n"));
        assert!(text.contains("# TYPE active_connections gauge\n"));
        assert!(text.contains("active_connections 3\n"));
    }

    #[test]
    fn render_is_sorted_and_deterministic() {
        let reg = MetricsRegistry::new();
        reg.counter("zzz_last").inc();
        reg.counter("aaa_first").inc();
        let text = reg.render_prometheus();
        let aaa = text.find("aaa_first").unwrap();
        let zzz = text.find("zzz_last").unwrap();
        assert!(aaa < zzz);
    }

    #[test]
    #[should_panic(expected = "already registered as a counter")]
    fn type_conflict_panics() {
        let reg = MetricsRegistry::new();
        reg.counter("x");
        reg.gauge("x");
    }
}
