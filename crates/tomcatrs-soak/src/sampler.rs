//! Periodic RSS / thread-count sampler for the process under test.
//!
//! We deliberately avoid platform-specific syscall crates (`procfs`, `mach`,
//! …) and shell out to `ps`/`/proc` so the soaker stays portable and easy to
//! audit. The sampler runs on its own task at a fixed cadence and appends to
//! a `Vec<ProcessSample>` it owns; the caller polls the vec at end-of-run.
//!
//! * **Linux**: parse `/proc/<pid>/status` for `VmRSS:` and `Threads:`. This
//!   is the cheapest possible sample (a single small file read, no fork).
//! * **macOS** and other Unix: shell out to `ps` for RSS, and `ps -M` for
//!   the per-thread listing. `ps -M -p PID` prints a header line followed by
//!   one row per kernel thread, so `lines - 1` is the live thread count.
//!
//! If a sample collection fails we log at `debug` and skip the tick — a
//! long soak should tolerate transient `ps` hiccups rather than crashing.

#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// A single (timestamp, RSS, threads) row in the sampler's time series.
#[derive(Debug, Clone, Serialize)]
pub struct ProcessSample {
    /// Wall-clock time of the sample, in milliseconds since the Unix epoch.
    pub at_unix_ms: u128,
    /// Resident-set size, in kibibytes (matches what `/proc/.../status:VmRSS`
    /// and `ps -o rss` report on Linux and macOS respectively).
    pub rss_kib: u64,
    /// Live kernel-visible thread count.
    pub threads: u32,
}

/// Periodically polls the target process for RSS + thread count. Created
/// with `new`, driven by [`Self::run`], which returns the collected samples
/// when the caller fires the stop notification.
pub struct ProcessSampler {
    pid: u32,
}

impl ProcessSampler {
    /// Build a sampler for the given OS process id.
    pub fn new(pid: u32) -> Self {
        Self { pid }
    }

    /// Sample once per `interval`. The sampler exits within roughly one
    /// `interval` of `stop` flipping to `true`. We poll the atomic at each
    /// tick rather than using `Notify` because the supervisor sets the flag
    /// *before* signalling — `Notify::notify_waiters` only wakes existing
    /// waiters, so a race where the sampler hasn't yet entered `notified()`
    /// would lose the wakeup. An atomic + a short tick is the simple,
    /// foot-gun-free option.
    pub async fn run(self, interval: Duration, stop: Arc<AtomicBool>) -> Vec<ProcessSample> {
        let mut series = Vec::new();
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            if stop.load(Ordering::Relaxed) {
                // One last sample so the final window always has a fresh row
                // even if the soak duration was a multiple of `interval`.
                if let Ok(s) = sample_once(self.pid) {
                    series.push(s);
                }
                break;
            }
            match sample_once(self.pid) {
                Ok(s) => series.push(s),
                Err(e) => tracing::debug!(pid = self.pid, error = %e, "sampler tick failed"),
            }
        }
        series
    }
}

fn sample_once(pid: u32) -> Result<ProcessSample, String> {
    let at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    #[cfg(target_os = "linux")]
    {
        let (rss_kib, threads) = sample_linux(pid)?;
        return Ok(ProcessSample {
            at_unix_ms,
            rss_kib,
            threads,
        });
    }

    #[cfg(not(target_os = "linux"))]
    {
        let (rss_kib, threads) = sample_unix_ps(pid)?;
        Ok(ProcessSample {
            at_unix_ms,
            rss_kib,
            threads,
        })
    }
}

#[cfg(target_os = "linux")]
fn sample_linux(pid: u32) -> Result<(u64, u32), String> {
    let path = PathBuf::from(format!("/proc/{pid}/status"));
    let body =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut rss_kib: Option<u64> = None;
    let mut threads: Option<u32> = None;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            // "  12345 kB"
            if let Some(tok) = rest.split_whitespace().next() {
                if let Ok(v) = tok.parse::<u64>() {
                    rss_kib = Some(v);
                }
            }
        } else if let Some(rest) = line.strip_prefix("Threads:") {
            if let Some(tok) = rest.split_whitespace().next() {
                if let Ok(v) = tok.parse::<u32>() {
                    threads = Some(v);
                }
            }
        }
        if rss_kib.is_some() && threads.is_some() {
            break;
        }
    }
    Ok((
        rss_kib.ok_or_else(|| "VmRSS not found in /proc/.../status".to_string())?,
        threads.ok_or_else(|| "Threads not found in /proc/.../status".to_string())?,
    ))
}

/// macOS / BSD path. We don't have `/proc`, so we shell out to `ps` twice:
///   - `ps -o rss= -p PID` → RSS in KiB on a single line.
///   - `ps -M -p PID` → header + one row per kernel thread; thread count is
///     `lines - 1`.
#[cfg(not(target_os = "linux"))]
fn sample_unix_ps(pid: u32) -> Result<(u64, u32), String> {
    use std::process::Command;

    // The `-o rss=` form suppresses the column header, so we parse a single
    // whitespace-trimmed integer. Be defensive: some `ps` builds pad.
    let rss_out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .map_err(|e| format!("spawn ps -o rss: {e}"))?;
    if !rss_out.status.success() {
        return Err(format!(
            "ps -o rss exited {:?} (process {pid} may have died)",
            rss_out.status.code()
        ));
    }
    let rss_str = String::from_utf8_lossy(&rss_out.stdout);
    let rss_kib: u64 = rss_str
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("ps RSS output not parseable: {rss_str:?}"))?;

    // `ps -M -p PID` lists one line per thread, prefixed with a header line.
    let thr_out = Command::new("ps")
        .args(["-M", "-p", &pid.to_string()])
        .output()
        .map_err(|e| format!("spawn ps -M: {e}"))?;
    if !thr_out.status.success() {
        return Err(format!(
            "ps -M exited {:?} (process {pid} may have died)",
            thr_out.status.code()
        ));
    }
    let thr_str = String::from_utf8_lossy(&thr_out.stdout);
    let lines = thr_str.lines().count();
    let threads: u32 = if lines == 0 {
        0
    } else {
        // One header line; everything after is a thread row.
        (lines - 1) as u32
    };

    Ok((rss_kib, threads))
}
