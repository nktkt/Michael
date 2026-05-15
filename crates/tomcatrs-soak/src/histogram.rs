//! Hand-rolled latency histogram with logarithmic-ish fixed buckets.
//!
//! The soaker records millions of samples in a long run; allocating per
//! observation is wasteful, and pulling in `hdrhistogram` would add a real
//! dependency we don't otherwise need. Instead we use a fixed array of
//! `AtomicU64` counters arranged in three decades (100 µs / 1 ms / 10 ms / …)
//! plus an explicit overflow bucket.
//!
//! # Bucket layout
//!
//! Each "decade" has [`SUB`] sub-buckets spanning `[10^k, 10^(k+1))` µs.
//! Decade `k = 0` covers `[1 µs, 10 µs)`, `k = 1` covers `[10 µs, 100 µs)`,
//! and so on up through `[1 s, 10 s)`. Anything ≥ 10 s lands in the overflow
//! bucket. The maximum observation seen is also tracked separately so the
//! overflow case still reports a meaningful upper bound.
//!
//! That gives 7 × 9 = 63 buckets plus overflow + a max gauge, with worst-case
//! quantile-step resolution of roughly 11 % anywhere in `[1 µs, 10 s)`. That's
//! plenty for stability assertions; we'd want finer buckets only if we were
//! using this for serious latency comparison work.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;

/// Number of decades, covering 1µs .. 10s.
const DECADES: usize = 7;
/// Number of sub-buckets per decade. With 9 sub-buckets per decade each
/// bucket is at worst ~11% wide ((k+1)/k for k = 1..9).
const SUB: usize = 9;
/// Total bucket count, plus one for overflow (≥ 10s).
const BUCKETS: usize = DECADES * SUB + 1;

/// A lock-free fixed-bucket histogram. `record` is cheap and safe to call
/// from any thread.
pub struct Histogram {
    buckets: [AtomicU64; BUCKETS],
    count: AtomicU64,
    sum_us: AtomicU64,
    max_us: AtomicU64,
}

impl Histogram {
    /// Build an empty histogram.
    pub fn new() -> Self {
        // `AtomicU64` is not `Copy`, so we can't use `[AtomicU64::new(0); N]`.
        // Build via `std::array::from_fn` instead — stable since Rust 1.63.
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            count: AtomicU64::new(0),
            sum_us: AtomicU64::new(0),
            max_us: AtomicU64::new(0),
        }
    }

    /// Record a single observation. `Duration::ZERO` is recorded as 1 µs so
    /// it lands in the lowest bucket rather than getting silently dropped.
    pub fn record(&self, d: Duration) {
        let us = d.as_micros().max(1).min(u64::MAX as u128) as u64;
        let idx = bucket_for(us);
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(us, Ordering::Relaxed);
        // Lock-free max update via CAS loop. Bounded retries — under heavy
        // contention we may briefly see a stale max, but the loop converges
        // and the worst case is one extra read.
        let mut cur = self.max_us.load(Ordering::Relaxed);
        while us > cur {
            match self
                .max_us
                .compare_exchange_weak(cur, us, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(observed) => cur = observed,
            }
        }
    }

    /// Compute percentiles + summary at this instant. Safe to call while
    /// `record` is in flight; a few samples may race in or out of the snapshot
    /// but the totals stay self-consistent.
    pub fn snapshot(&self) -> HistogramSnapshot {
        let counts: Vec<u64> = self
            .buckets
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .collect();
        let total: u64 = counts.iter().sum();
        let sum_us = self.sum_us.load(Ordering::Relaxed);
        let max_us = self.max_us.load(Ordering::Relaxed);

        let p50_us = percentile(&counts, total, 0.50);
        let p90_us = percentile(&counts, total, 0.90);
        let p99_us = percentile(&counts, total, 0.99);
        let p99_9_us = percentile(&counts, total, 0.999);

        let mean_us = sum_us.checked_div(total).unwrap_or(0);

        HistogramSnapshot {
            count: total,
            mean_us,
            p50_us,
            p90_us,
            p99_us,
            p99_9_us,
            max_us,
        }
    }
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

/// JSON-shaped snapshot of a histogram's summary statistics. All times are in
/// microseconds; consumers can divide by 1000 for milliseconds.
#[derive(Debug, Clone, Serialize)]
pub struct HistogramSnapshot {
    /// Total number of observations recorded.
    pub count: u64,
    /// Mean observation, in microseconds.
    pub mean_us: u64,
    /// 50th-percentile (median) observation, in microseconds.
    pub p50_us: u64,
    /// 90th-percentile observation, in microseconds.
    pub p90_us: u64,
    /// 99th-percentile observation, in microseconds.
    pub p99_us: u64,
    /// 99.9th-percentile observation, in microseconds.
    pub p99_9_us: u64,
    /// The largest observation seen, in microseconds (exact, not bucketed).
    pub max_us: u64,
}

/// Map a microsecond observation to its bucket index. See the module docs for
/// the bucket layout.
fn bucket_for(us: u64) -> usize {
    if us == 0 {
        return 0;
    }
    // Decade `k` covers `[10^k µs, 10^(k+1) µs)`.
    let mut decade = 0usize;
    let mut floor: u64 = 1;
    while decade < DECADES && us >= floor * 10 {
        decade += 1;
        floor = floor.saturating_mul(10);
    }
    if decade >= DECADES {
        return DECADES * SUB;
    }
    // Sub-bucket within the decade: linear in `(us - floor) / floor`. For us
    // in `[floor, floor*10)` that lands in `0..9`.
    let sub = ((us - floor) / floor) as usize;
    let sub = sub.min(SUB - 1);
    decade * SUB + sub
}

/// The *upper* edge (exclusive) of a bucket, in microseconds. Quantile
/// estimates use this as the conservative reported value: we always
/// over-report rather than under-report.
fn bucket_upper(idx: usize) -> u64 {
    if idx >= DECADES * SUB {
        // Overflow: report 10s as the floor; callers who want a tighter
        // bound should consult `max_us`.
        return 10_000_000;
    }
    let decade = idx / SUB;
    let sub = idx % SUB;
    let floor = 10u64.pow(decade as u32);
    floor + floor * (sub as u64 + 1)
}

fn percentile(counts: &[u64], total: u64, q: f64) -> u64 {
    if total == 0 {
        return 0;
    }
    let target = ((q * total as f64).ceil() as u64).max(1);
    let mut cum = 0u64;
    for (idx, c) in counts.iter().enumerate() {
        cum += c;
        if cum >= target {
            return bucket_upper(idx);
        }
    }
    bucket_upper(counts.len() - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_for_simple_values() {
        // Decade 0 covers [1, 10); the sub-bucket is `(us - 1) / 1`, so
        // 1 lands in sub-bucket 0 and 9 lands in sub-bucket 8 (the top of
        // the decade).
        assert_eq!(bucket_for(1), 0);
        assert_eq!(bucket_for(9), SUB - 1);
        // Decade 1 covers [10, 100); sub-bucket is `(us - 10) / 10`.
        assert_eq!(bucket_for(10), SUB);
        assert_eq!(bucket_for(11), SUB);
        assert_eq!(bucket_for(20), SUB + 1);
        // Decade 2 covers [100, 1000).
        assert_eq!(bucket_for(100), 2 * SUB);
    }

    #[test]
    fn overflow_lands_in_last_bucket() {
        // 10 s = 10_000_000 µs is the overflow boundary.
        assert_eq!(bucket_for(10_000_000), DECADES * SUB);
        assert_eq!(bucket_for(20_000_000), DECADES * SUB);
    }

    #[test]
    fn histogram_percentiles_are_monotonic() {
        let h = Histogram::new();
        for us in [50u64, 100, 200, 500, 1_000, 5_000, 50_000, 100_000] {
            for _ in 0..1000 {
                h.record(Duration::from_micros(us));
            }
        }
        let s = h.snapshot();
        assert!(s.count == 8000);
        assert!(s.p50_us <= s.p90_us);
        assert!(s.p90_us <= s.p99_us);
        assert!(s.p99_us <= s.p99_9_us);
        assert!(s.max_us >= 100_000);
    }

    #[test]
    fn zero_duration_records_as_one_microsecond() {
        let h = Histogram::new();
        h.record(Duration::ZERO);
        let s = h.snapshot();
        assert_eq!(s.count, 1);
        assert!(s.p50_us >= 1);
    }
}
