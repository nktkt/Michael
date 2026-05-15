//! `tomcatrs-soak` — a soak / load-test harness for the Tomcat-RS runtime.
//!
//! # What it is
//!
//! A self-contained HTTP/1.1 client binary that drives sustained traffic at a
//! running `tomcatrs` server (or any HTTP/1.1 server, really) and asserts the
//! runtime is *stable*: no memory leaks, no thread leaks, no slowly-degrading
//! tail latencies. The soaker deliberately depends on no `tomcatrs-*` crate so
//! that:
//!
//! * the server runs in a separate OS process (the same shape an operator runs
//!   in production),
//! * the soaker can be cross-compiled and copied to a CI runner without
//!   pulling in the whole workspace,
//! * a buggy server cannot crash the measurement harness.
//!
//! # How it works
//!
//! 1. A pool of `--concurrency` tokio tasks loops, each opening a fresh TCP
//!    connection, sending `GET <path>`, reading the response, and recording
//!    the status code and end-to-end latency into a lock-free fixed-bucket
//!    histogram. (Closed-loop mode uses no pacing; open-loop pacing uses a
//!    shared token-bucket so the aggregate throughput approaches `--rps`.)
//!
//! 2. A periodic sampler ticks once a second, snapshots the target server's
//!    resident-set size and thread count via `ps`/`/proc`, and appends them to
//!    a time series.
//!
//! 3. At end-of-run we partition the time series into a *baseline* window
//!    (the first `--warmup` seconds — measurements here don't count toward
//!    leak assertions) and a *final* window (the last 60s before shutdown).
//!    We then assert:
//!
//!    * 2xx rate ≥ 99.5% over the post-warmup window,
//!    * final RSS ≤ baseline RSS × (1 + `--leak-threshold-rss-pct` / 100),
//!    * final thread count ≤ baseline + `--leak-threshold-threads`,
//!    * final p99 latency ≤ baseline × (1 + `--p99-regression-pct` / 100).
//!
//!    Each assertion prints a green `[OK]` or red `[FAIL]` line. Any failure
//!    sets the process exit code to a non-zero value so CI can tell.
//!
//! 4. The full numeric record is written to `--report` as JSON, suitable for
//!    archival and post-mortem comparison across runs.
//!
//! # Non-goals
//!
//! * Not a benchmarking tool. Throughput numbers come out, but `wrk` / `bombardier`
//!   / `criterion` are better choices when you actually want to *measure* peak
//!   throughput. The soaker pessimises throughput in favour of *stability*.
//! * No HTTPS / HTTP/2 / chunked-body uploads. The MVP is intentionally narrow.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod client;
mod histogram;
mod sampler;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use serde::Serialize;
use tokio::task::JoinSet;

use crate::client::{HttpTarget, RequestOutcome};
use crate::histogram::{Histogram, HistogramSnapshot};
use crate::sampler::{ProcessSample, ProcessSampler};

/// Command-line arguments for `tomcatrs-soak`.
#[derive(Debug, Parser)]
#[command(
    name = "tomcatrs-soak",
    version,
    about = "Soak / load-test harness for the Tomcat-RS runtime",
    long_about = None,
)]
struct Args {
    /// Fully-qualified URL to hit, e.g. `http://127.0.0.1:8080/`.
    #[arg(long, value_name = "URL", default_value = "http://127.0.0.1:8080/")]
    target: String,

    /// Total wall-clock duration of the soak, in seconds. The warmup window
    /// is included; pick `--duration > --warmup + 60` so the final 60s window
    /// has real data.
    #[arg(long, value_name = "SECS", default_value_t = 300)]
    duration: u64,

    /// Maximum number of in-flight requests.
    #[arg(long, value_name = "N", default_value_t = 32)]
    concurrency: usize,

    /// Open-loop target rate in requests per second. `0` switches to
    /// closed-loop (each worker fires as fast as it can; aggregate rate is
    /// bounded only by `--concurrency` and server throughput).
    #[arg(long, value_name = "N", default_value_t = 200)]
    rps: u64,

    /// Warmup window, in seconds. Latency / RSS / thread samples collected
    /// during the first `--warmup` seconds form the *baseline* against which
    /// later samples are compared. Pick this to be at least as long as the
    /// JIT / page-cache / connection-pool warm-up of the system under test.
    #[arg(long, value_name = "SECS", default_value_t = 30)]
    warmup: u64,

    /// Maximum acceptable monotonic RSS growth, as a percentage of the
    /// warmup-window baseline.
    #[arg(long, value_name = "PCT", default_value_t = 15.0)]
    leak_threshold_rss_pct: f64,

    /// Maximum acceptable thread-count growth (absolute, not percentage).
    #[arg(long, value_name = "N", default_value_t = 8)]
    leak_threshold_threads: i64,

    /// Maximum acceptable p99-latency growth from baseline to final window,
    /// as a percentage of the baseline p99.
    #[arg(long, value_name = "PCT", default_value_t = 50.0)]
    p99_regression_pct: f64,

    /// Output path for the JSON soak report.
    #[arg(long, value_name = "PATH", default_value = "soak-report.json")]
    report: PathBuf,

    /// PID of the target server process. Required for RSS / thread-count
    /// assertions; when absent the soaker still records latencies but skips
    /// the leak checks and prints a warning.
    #[arg(long, value_name = "PID")]
    target_pid: Option<u32>,

    /// Per-request connect+read timeout, in seconds. A request that doesn't
    /// complete inside this window is recorded as a failure (`status = 0`).
    #[arg(long, value_name = "SECS", default_value_t = 10)]
    request_timeout: u64,

    /// Logging verbosity (`trace`, `debug`, `info`, `warn`, `error`).
    #[arg(long, value_name = "LEVEL", default_value = "info")]
    log_level: String,
}

/// Top-level entry point. Returns `ExitCode::SUCCESS` if every assertion
/// passed, `ExitCode::FAILURE` otherwise.
#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    init_tracing(&args.log_level);

    match run(args).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("tomcatrs-soak: fatal: {e}");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing(level: &str) {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("tomcatrs_soak={level},warn")));
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
}

/// Drive the entire soak run end-to-end. Returns `Ok(true)` if every
/// assertion held, `Ok(false)` if one or more failed.
async fn run(args: Args) -> Result<bool, String> {
    let target = HttpTarget::parse(&args.target)
        .map_err(|e| format!("failed to parse --target {:?}: {e}", args.target))?;
    let total_duration = Duration::from_secs(args.duration);
    let warmup = Duration::from_secs(args.warmup);
    let request_timeout = Duration::from_secs(args.request_timeout);
    let final_window = Duration::from_secs(60);

    if warmup >= total_duration {
        return Err(format!(
            "--warmup ({} s) must be less than --duration ({} s)",
            args.warmup, args.duration,
        ));
    }
    if args.concurrency == 0 {
        return Err("--concurrency must be > 0".to_string());
    }

    tracing::info!(
        target = %args.target,
        duration_s = args.duration,
        warmup_s = args.warmup,
        concurrency = args.concurrency,
        rps = args.rps,
        target_pid = ?args.target_pid,
        "soak run starting",
    );

    // ---------------------------------------------------------------
    // Shared state. Each worker writes into the histograms; the sampler
    // appends to its own time series.
    // ---------------------------------------------------------------
    let warmup_hist = Arc::new(Histogram::new());
    let post_hist = Arc::new(Histogram::new());
    let final_hist = Arc::new(Histogram::new());
    let ok_2xx = Arc::new(AtomicU64::new(0));
    let total_after_warmup = Arc::new(AtomicU64::new(0));
    let total_overall = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    // Open-loop pacing: a single shared token bucket. When `--rps = 0` we run
    // closed-loop and skip the bucket entirely.
    let pacer = if args.rps > 0 {
        Some(Arc::new(TokenBucket::new(args.rps)))
    } else {
        None
    };

    let start = Instant::now();
    let start_unix_ms = now_unix_ms();
    let warmup_end = start + warmup;
    let final_start = start + total_duration - final_window;
    let run_deadline = start + total_duration;

    // ---------------------------------------------------------------
    // Spawn the periodic process sampler. If `--target-pid` wasn't given the
    // sampler still ticks (so we can log "PID unset") but records nothing.
    // ---------------------------------------------------------------
    let sampler_handle = if let Some(pid) = args.target_pid {
        let sampler = ProcessSampler::new(pid);
        let stop_for_sampler = Arc::clone(&stop);
        Some(tokio::spawn(async move {
            sampler.run(Duration::from_secs(1), stop_for_sampler).await
        }))
    } else {
        tracing::warn!(
            "--target-pid not given; RSS / thread-count leak assertions will be skipped"
        );
        None
    };

    // ---------------------------------------------------------------
    // Spawn the worker pool.
    // ---------------------------------------------------------------
    let mut workers: JoinSet<()> = JoinSet::new();
    for worker_id in 0..args.concurrency {
        let target = target.clone();
        let warmup_hist = Arc::clone(&warmup_hist);
        let post_hist = Arc::clone(&post_hist);
        let final_hist = Arc::clone(&final_hist);
        let ok_2xx = Arc::clone(&ok_2xx);
        let total_after_warmup = Arc::clone(&total_after_warmup);
        let total_overall = Arc::clone(&total_overall);
        let stop = Arc::clone(&stop);
        let pacer = pacer.clone();
        workers.spawn(async move {
            run_worker(
                worker_id,
                target,
                request_timeout,
                warmup_end,
                final_start,
                run_deadline,
                pacer,
                warmup_hist,
                post_hist,
                final_hist,
                ok_2xx,
                total_after_warmup,
                total_overall,
                stop,
            )
            .await;
        });
    }

    // ---------------------------------------------------------------
    // Time-based supervisor: wait the requested duration, then signal
    // every worker to stop. We don't use `tokio::time::timeout` around the
    // whole run because we want clean shutdown of the histograms.
    // ---------------------------------------------------------------
    tokio::time::sleep_until(tokio::time::Instant::from_std(run_deadline)).await;
    stop.store(true, Ordering::Relaxed);

    // Give workers a brief grace window to finish in-flight requests so we
    // don't lose the last second of measurements to JoinSet::abort.
    let grace = Duration::from_secs(2);
    let grace_deadline = Instant::now() + grace;
    while !workers.is_empty() {
        match tokio::time::timeout_at(
            tokio::time::Instant::from_std(grace_deadline),
            workers.join_next(),
        )
        .await
        {
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => {
                workers.shutdown().await;
                break;
            }
        }
    }

    let samples = if let Some(handle) = sampler_handle {
        match handle.await {
            Ok(samples) => samples,
            Err(e) => {
                tracing::warn!(error = %e, "sampler task failed; continuing with empty series");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    // ---------------------------------------------------------------
    // Roll the histograms up into snapshots and emit the report.
    // ---------------------------------------------------------------
    let warmup_snap = warmup_hist.snapshot();
    let post_snap = post_hist.snapshot();
    let final_snap = final_hist.snapshot();

    let baseline_proc = window_median(&samples, start, warmup_end, start, start_unix_ms);
    let final_proc = window_median(&samples, final_start, run_deadline, start, start_unix_ms);

    let total_after_warmup = total_after_warmup.load(Ordering::Relaxed);
    let total_overall = total_overall.load(Ordering::Relaxed);
    let ok_2xx = ok_2xx.load(Ordering::Relaxed);

    let two_xx_rate = if total_after_warmup > 0 {
        ok_2xx as f64 / total_after_warmup as f64
    } else {
        0.0
    };

    let report = SoakReport {
        target: args.target.clone(),
        target_pid: args.target_pid,
        started_at_unix_ms: start_unix_ms,
        duration_s: args.duration,
        warmup_s: args.warmup,
        concurrency: args.concurrency,
        rps_target: args.rps,
        total_requests: total_overall,
        post_warmup_requests: total_after_warmup,
        two_xx_count: ok_2xx,
        two_xx_rate,
        warmup_latency: warmup_snap.clone(),
        post_warmup_latency: post_snap.clone(),
        final_latency: final_snap.clone(),
        baseline_process: baseline_proc,
        final_process: final_proc,
        process_samples: samples,
        thresholds: ReportThresholds {
            min_two_xx_rate: 0.995,
            leak_threshold_rss_pct: args.leak_threshold_rss_pct,
            leak_threshold_threads: args.leak_threshold_threads,
            p99_regression_pct: args.p99_regression_pct,
        },
    };

    write_report(&args.report, &report)?;

    // ---------------------------------------------------------------
    // Print a human summary + green/red assertion lines.
    // ---------------------------------------------------------------
    print_summary(&report);
    let ok = print_assertions(&report);
    Ok(ok)
}

#[allow(clippy::too_many_arguments)]
async fn run_worker(
    worker_id: usize,
    target: HttpTarget,
    request_timeout: Duration,
    warmup_end: Instant,
    final_start: Instant,
    run_deadline: Instant,
    pacer: Option<Arc<TokenBucket>>,
    warmup_hist: Arc<Histogram>,
    post_hist: Arc<Histogram>,
    final_hist: Arc<Histogram>,
    ok_2xx: Arc<AtomicU64>,
    total_after_warmup: Arc<AtomicU64>,
    total_overall: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        if Instant::now() >= run_deadline {
            break;
        }
        if let Some(p) = pacer.as_ref() {
            // Open-loop pacing — wait for our turn. The bucket also returns
            // immediately once the run is over so workers don't deadlock.
            if !p.acquire(&stop).await {
                break;
            }
        }

        let req_start = Instant::now();
        let outcome = client::issue_request(&target, request_timeout).await;
        let elapsed = req_start.elapsed();

        total_overall.fetch_add(1, Ordering::Relaxed);

        let phase = if req_start < warmup_end {
            Phase::Warmup
        } else if req_start >= final_start {
            Phase::Final
        } else {
            Phase::Steady
        };

        match phase {
            Phase::Warmup => warmup_hist.record(elapsed),
            Phase::Steady => post_hist.record(elapsed),
            Phase::Final => {
                post_hist.record(elapsed);
                final_hist.record(elapsed);
            }
        }

        if !matches!(phase, Phase::Warmup) {
            total_after_warmup.fetch_add(1, Ordering::Relaxed);
            if let RequestOutcome::Ok(status) = outcome {
                if (200..300).contains(&status) {
                    ok_2xx.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        // Tracing only on rare events to keep stdout quiet during a long run.
        if let RequestOutcome::Err(ref msg) = outcome {
            tracing::debug!(worker = worker_id, error = %msg, "request failed");
        }
    }
}

#[derive(Copy, Clone, Debug)]
enum Phase {
    Warmup,
    Steady,
    Final,
}

// ---------------------------------------------------------------------------
// Open-loop pacing — a simple token bucket shared across all workers.
// ---------------------------------------------------------------------------

/// A coarse-grained open-loop pacer. Refills `rate` tokens every second; each
/// `acquire` consumes one. The implementation is deliberately simple (a
/// single mutex around the available-token count + a next-refill instant);
/// the soaker doesn't need sub-millisecond accuracy.
struct TokenBucket {
    rate: u64,
    state: tokio::sync::Mutex<TokenBucketState>,
}

struct TokenBucketState {
    available: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(rate: u64) -> Self {
        Self {
            rate,
            state: tokio::sync::Mutex::new(TokenBucketState {
                available: rate as f64,
                last_refill: Instant::now(),
            }),
        }
    }

    /// Wait until at least one token is available, then consume it.
    /// Returns `false` if the run was asked to stop while we were waiting.
    async fn acquire(&self, stop: &AtomicBool) -> bool {
        loop {
            if stop.load(Ordering::Relaxed) {
                return false;
            }
            let sleep_for = {
                let mut st = self.state.lock().await;
                let now = Instant::now();
                let elapsed = now.duration_since(st.last_refill).as_secs_f64();
                st.available = (st.available + elapsed * self.rate as f64).min(self.rate as f64);
                st.last_refill = now;
                if st.available >= 1.0 {
                    st.available -= 1.0;
                    return true;
                }
                // Sleep for just long enough to accrue the missing fraction
                // of a token. Bounded below at 1ms so we don't spin.
                let need = 1.0 - st.available;
                let secs = (need / self.rate as f64).max(0.001);
                Duration::from_secs_f64(secs)
            };
            tokio::time::sleep(sleep_for).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Reporting.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct SoakReport {
    target: String,
    target_pid: Option<u32>,
    started_at_unix_ms: u128,
    duration_s: u64,
    warmup_s: u64,
    concurrency: usize,
    rps_target: u64,
    total_requests: u64,
    post_warmup_requests: u64,
    two_xx_count: u64,
    two_xx_rate: f64,
    warmup_latency: HistogramSnapshot,
    post_warmup_latency: HistogramSnapshot,
    final_latency: HistogramSnapshot,
    baseline_process: Option<ProcessSample>,
    final_process: Option<ProcessSample>,
    process_samples: Vec<ProcessSample>,
    thresholds: ReportThresholds,
}

#[derive(Debug, Clone, Serialize)]
struct ReportThresholds {
    min_two_xx_rate: f64,
    leak_threshold_rss_pct: f64,
    leak_threshold_threads: i64,
    p99_regression_pct: f64,
}

fn write_report(path: &PathBuf, report: &SoakReport) -> Result<(), String> {
    let body =
        serde_json::to_string_pretty(report).map_err(|e| format!("serialize report: {e}"))?;
    std::fs::write(path, body).map_err(|e| format!("write {}: {e}", path.display()))?;
    tracing::info!(path = %path.display(), "soak report written");
    Ok(())
}

fn print_summary(r: &SoakReport) {
    println!();
    println!("=== tomcatrs-soak summary ===");
    println!("target                 : {}", r.target);
    if let Some(pid) = r.target_pid {
        println!("target pid             : {}", pid);
    } else {
        println!("target pid             : (unset — leak checks skipped)");
    }
    println!(
        "duration / warmup      : {} s / {} s",
        r.duration_s, r.warmup_s,
    );
    println!(
        "concurrency / rps      : {} / {}",
        r.concurrency,
        if r.rps_target == 0 {
            "closed-loop".to_string()
        } else {
            r.rps_target.to_string()
        },
    );
    println!(
        "requests (total / post): {} / {}",
        r.total_requests, r.post_warmup_requests,
    );
    println!(
        "2xx rate (post-warmup) : {:>6.3}%  ({}/{} ok)",
        r.two_xx_rate * 100.0,
        r.two_xx_count,
        r.post_warmup_requests,
    );
    println!();
    println!("latency (post-warmup):");
    print_lat("  steady ", &r.post_warmup_latency);
    print_lat("  warmup ", &r.warmup_latency);
    print_lat("  final  ", &r.final_latency);

    if let (Some(base), Some(end)) = (r.baseline_process.as_ref(), r.final_process.as_ref()) {
        println!();
        println!("process samples:");
        println!(
            "  baseline (median over warmup) : rss={} KiB, threads={}",
            base.rss_kib, base.threads,
        );
        println!(
            "  final    (median over last 60s): rss={} KiB, threads={}",
            end.rss_kib, end.threads,
        );
        let rss_growth_pct = if base.rss_kib > 0 {
            (end.rss_kib as f64 - base.rss_kib as f64) / base.rss_kib as f64 * 100.0
        } else {
            0.0
        };
        let thread_delta = end.threads as i64 - base.threads as i64;
        println!(
            "  delta                          : rss={:+.2}%, threads={:+}",
            rss_growth_pct, thread_delta,
        );
    }
}

fn print_lat(label: &str, s: &HistogramSnapshot) {
    println!(
        "{}n={:>6}  p50={:>7.2}ms  p90={:>7.2}ms  p99={:>7.2}ms  p99.9={:>7.2}ms  max={:>7.2}ms",
        label,
        s.count,
        us_to_ms(s.p50_us),
        us_to_ms(s.p90_us),
        us_to_ms(s.p99_us),
        us_to_ms(s.p99_9_us),
        us_to_ms(s.max_us),
    );
}

fn us_to_ms(us: u64) -> f64 {
    us as f64 / 1000.0
}

/// Returns `true` if every assertion held.
fn print_assertions(r: &SoakReport) -> bool {
    println!();
    println!("=== assertions ===");
    let mut all_ok = true;

    // 1. 2xx rate over the post-warmup window.
    let ok = r.two_xx_rate >= r.thresholds.min_two_xx_rate;
    print_check(
        ok,
        &format!(
            "2xx rate >= {:.3}%   (actual {:.3}%)",
            r.thresholds.min_two_xx_rate * 100.0,
            r.two_xx_rate * 100.0,
        ),
    );
    all_ok &= ok;

    // 2. p99 regression.
    let base_p99 = r.warmup_latency.p99_us;
    let final_p99 = r.final_latency.p99_us;
    let p99_ok = if base_p99 == 0 {
        // No warmup data — can't compare; treat as non-fatal but warn.
        println!("[WARN] p99 regression check skipped: baseline p99 = 0 (no warmup data?)",);
        true
    } else {
        let allowed = base_p99 as f64 * (1.0 + r.thresholds.p99_regression_pct / 100.0);
        let ok = (final_p99 as f64) <= allowed;
        print_check(
            ok,
            &format!(
                "p99 latency <= baseline x {:.2}   (baseline {:.2}ms, final {:.2}ms, allowed {:.2}ms)",
                1.0 + r.thresholds.p99_regression_pct / 100.0,
                us_to_ms(base_p99),
                us_to_ms(final_p99),
                allowed / 1000.0,
            ),
        );
        ok
    };
    all_ok &= p99_ok;

    // 3 + 4. Process growth — only when we have samples.
    match (r.baseline_process.as_ref(), r.final_process.as_ref()) {
        (Some(base), Some(end)) => {
            let allowed_rss =
                base.rss_kib as f64 * (1.0 + r.thresholds.leak_threshold_rss_pct / 100.0);
            let rss_ok = (end.rss_kib as f64) <= allowed_rss;
            print_check(
                rss_ok,
                &format!(
                    "RSS growth <= {:.2}%   (baseline {} KiB, final {} KiB, allowed {:.0} KiB)",
                    r.thresholds.leak_threshold_rss_pct, base.rss_kib, end.rss_kib, allowed_rss,
                ),
            );
            all_ok &= rss_ok;

            let allowed_threads = base.threads as i64 + r.thresholds.leak_threshold_threads;
            let thr_ok = (end.threads as i64) <= allowed_threads;
            print_check(
                thr_ok,
                &format!(
                    "thread growth <= +{}   (baseline {}, final {}, allowed <= {})",
                    r.thresholds.leak_threshold_threads, base.threads, end.threads, allowed_threads,
                ),
            );
            all_ok &= thr_ok;
        }
        _ => {
            println!("[WARN] RSS / thread-count checks skipped (no --target-pid or no samples)",);
        }
    }

    println!();
    println!(
        "overall: {}",
        if all_ok {
            "[OK] all soak assertions passed"
        } else {
            "[FAIL] one or more soak assertions FAILED"
        },
    );
    all_ok
}

fn print_check(ok: bool, msg: &str) {
    let tag = if ok { "[OK]  " } else { "[FAIL]" };
    println!("{} {}", tag, msg);
}

/// Median process sample over a half-open `[lo, hi)` window. Returns `None`
/// if no sample falls inside the window.
///
/// `anchor_instant` and `anchor_unix_ms` describe a single point in time so we
/// can translate the `Instant`-typed window bounds into the `unix_ms`-stamped
/// sampler series.
fn window_median(
    samples: &[ProcessSample],
    lo: Instant,
    hi: Instant,
    anchor_instant: Instant,
    anchor_unix_ms: u128,
) -> Option<ProcessSample> {
    let lo_ms = instant_to_unix_ms(lo, anchor_instant, anchor_unix_ms);
    let hi_ms = instant_to_unix_ms(hi, anchor_instant, anchor_unix_ms);

    let mut rss: Vec<u64> = Vec::new();
    let mut thr: Vec<u32> = Vec::new();
    for s in samples {
        if s.at_unix_ms >= lo_ms && s.at_unix_ms < hi_ms {
            rss.push(s.rss_kib);
            thr.push(s.threads);
        }
    }
    if rss.is_empty() {
        return None;
    }
    rss.sort_unstable();
    thr.sort_unstable();
    let mid = rss.len() / 2;
    Some(ProcessSample {
        at_unix_ms: (lo_ms + hi_ms) / 2,
        rss_kib: rss[mid],
        threads: thr[mid],
    })
}

/// Translate `t` to its wall-clock `unix_ms`, using `(anchor_instant,
/// anchor_unix_ms)` as the calibration point. `Instant` arithmetic is
/// monotonic but `Instant::duration_since` panics if the right-hand side is
/// later than the left, so we branch on the relative order.
fn instant_to_unix_ms(t: Instant, anchor_instant: Instant, anchor_unix_ms: u128) -> u128 {
    if t >= anchor_instant {
        anchor_unix_ms + t.duration_since(anchor_instant).as_millis()
    } else {
        anchor_unix_ms.saturating_sub(anchor_instant.duration_since(t).as_millis())
    }
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
