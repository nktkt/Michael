//! Integration test: drive a 30-second soak against a freshly-spawned
//! `tomcatrs` CLI subprocess and assert that:
//!
//! * the soaker exits with status 0,
//! * the JSON report records ≥ 99.5% 2xx,
//! * the JSON report records ≤ 15% RSS growth (when the sampler ran).
//!
//! The whole test is wall-clock bounded by `--duration 30 --warmup 10`, plus a
//! few seconds of CLI startup and soaker teardown, so the test budget stays
//! under ~60s on a developer machine.
//!
//! Both binaries are built by Cargo before this test runs; their absolute
//! paths arrive via `CARGO_BIN_EXE_<name>` env vars. We deliberately use the
//! soaker as a *child process* rather than calling its `main` function in
//! place — that's what an operator will actually run, and the test exercises
//! the JSON-report + exit-code surface CI relies on.

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Best-effort kill-on-drop guard for spawned child processes.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Unique temp directory cleaned up on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "tomcatrs-soak-it-{}-{}-{}",
            label,
            std::process::id(),
            nanos,
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Reserve a free TCP port by binding to `127.0.0.1:0` and immediately
/// closing it. There's a small race between us closing the port and the CLI
/// re-binding it; on a developer / CI machine that's effectively never hit.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
}

/// Poll `127.0.0.1:port` until a TCP connection succeeds or `deadline`
/// elapses. Returns `true` on success, `false` on timeout.
fn wait_until_listening(port: u16, deadline: Instant) -> bool {
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(
            &format!("127.0.0.1:{port}").parse().expect("parse addr"),
            Duration::from_millis(250),
        )
        .is_ok()
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[test]
fn short_soak_against_live_cli() {
    // ------------------------------------------------------------------
    // 1. Lay out an app-base with a known index.html so GET / returns 200.
    // ------------------------------------------------------------------
    let app_base = TempDir::new("appbase");
    std::fs::write(
        app_base.path().join("index.html"),
        b"<!doctype html><title>soak</title>soak-landing",
    )
    .expect("write index.html");

    // ------------------------------------------------------------------
    // 2. Spawn the CLI binary on a free port. CARGO_BIN_EXE_tomcatrs is
    //    populated by Cargo because we declare `tomcatrs-cli` as a build
    //    dep of this integration test via the workspace; for binaries in
    //    *other* crates we have to point Cargo at it explicitly.
    // ------------------------------------------------------------------
    let port = free_port();
    let cli_bin = locate_cli_binary();
    let cli_child = Command::new(&cli_bin)
        .arg("run")
        .arg("--port")
        .arg(port.to_string())
        .arg("--app-base")
        .arg(app_base.path())
        .arg("--log-level")
        .arg("warn")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn tomcatrs CLI at {}: {e}", cli_bin.display()));
    let cli_pid = cli_child.id();
    let _cli_guard = ChildGuard(cli_child);

    let deadline = Instant::now() + Duration::from_secs(30);
    assert!(
        wait_until_listening(port, deadline),
        "tomcatrs CLI never started listening on 127.0.0.1:{port}",
    );

    // ------------------------------------------------------------------
    // 3. Spawn the soaker.
    // ------------------------------------------------------------------
    let report_dir = TempDir::new("report");
    let report_path = report_dir.path().join("soak-report.json");
    let soak_bin = env!("CARGO_BIN_EXE_tomcatrs-soak");
    let mut soak_child = Command::new(soak_bin)
        .arg("--target")
        .arg(format!("http://127.0.0.1:{port}/"))
        .arg("--duration")
        .arg("30")
        .arg("--warmup")
        .arg("10")
        .arg("--concurrency")
        .arg("8")
        .arg("--rps")
        .arg("100")
        .arg("--report")
        .arg(&report_path)
        .arg("--target-pid")
        .arg(cli_pid.to_string())
        .arg("--log-level")
        .arg("warn")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tomcatrs-soak");

    // Bound the soaker wall-clock with a generous timeout — the configured
    // run is 30 s and end-of-run accounting is small, so 90 s is plenty.
    let started = Instant::now();
    let soak_timeout = Duration::from_secs(90);
    let status = loop {
        if let Some(s) = soak_child.try_wait().expect("try_wait soak") {
            break s;
        }
        if started.elapsed() > soak_timeout {
            let _ = soak_child.kill();
            panic!("tomcatrs-soak did not exit within {soak_timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(200));
    };

    let mut soak_stdout = String::new();
    if let Some(mut s) = soak_child.stdout.take() {
        let _ = s.read_to_string(&mut soak_stdout);
    }
    let mut soak_stderr = String::new();
    if let Some(mut s) = soak_child.stderr.take() {
        let _ = s.read_to_string(&mut soak_stderr);
    }

    assert!(
        status.success(),
        "tomcatrs-soak exited non-zero ({status:?})\n--- stdout ---\n{soak_stdout}\n--- stderr ---\n{soak_stderr}",
    );

    // ------------------------------------------------------------------
    // 4. Parse + assert on the JSON report.
    // ------------------------------------------------------------------
    let body = std::fs::read_to_string(&report_path)
        .unwrap_or_else(|e| panic!("read soak report {}: {e}", report_path.display()));
    let json: serde_json::Value =
        serde_json::from_str(&body).expect("soak report must be valid JSON");

    let two_xx_rate = json
        .get("two_xx_rate")
        .and_then(|v| v.as_f64())
        .expect("two_xx_rate field");
    let post_requests = json
        .get("post_warmup_requests")
        .and_then(|v| v.as_u64())
        .expect("post_warmup_requests field");

    assert!(
        post_requests > 100,
        "expected at least 100 post-warmup requests, got {post_requests}",
    );
    assert!(
        two_xx_rate >= 0.995,
        "2xx rate must be >= 99.5%; got {:.4}",
        two_xx_rate,
    );

    // RSS check is conditional on the sampler having produced data (it relies
    // on `ps` being available, which is true on every Unix CI runner we care
    // about). If both baseline + final samples exist, assert the 15% bound.
    if let (Some(base), Some(end)) = (
        json.get("baseline_process").and_then(|v| v.as_object()),
        json.get("final_process").and_then(|v| v.as_object()),
    ) {
        let base_rss = base.get("rss_kib").and_then(|v| v.as_u64()).unwrap_or(0);
        let end_rss = end.get("rss_kib").and_then(|v| v.as_u64()).unwrap_or(0);
        assert!(
            base_rss > 0,
            "baseline RSS should be > 0 (got {base_rss} from {body})",
        );
        let growth_pct = (end_rss as f64 - base_rss as f64) / base_rss as f64 * 100.0;
        assert!(
            growth_pct <= 15.0,
            "RSS growth must be <= 15%; got {growth_pct:.2}% (baseline {base_rss} KiB, final {end_rss} KiB)",
        );
    } else {
        // Sampler didn't produce data — log it so a developer can investigate.
        eprintln!(
            "soak report has no baseline/final process samples; \
             stdout was:\n{soak_stdout}\nstderr was:\n{soak_stderr}",
        );
    }
}

/// Locate the `tomcatrs` CLI binary. Cargo only populates `CARGO_BIN_EXE_*`
/// for binaries in *this* crate, so we walk up from the soaker binary path
/// (which Cargo *does* populate) to the workspace `target/<profile>/` dir
/// and pick the sibling `tomcatrs` binary. This works for both `cargo test`
/// and `cargo nextest run`.
fn locate_cli_binary() -> PathBuf {
    let soak_path = PathBuf::from(env!("CARGO_BIN_EXE_tomcatrs-soak"));
    let dir = soak_path
        .parent()
        .expect("CARGO_BIN_EXE_tomcatrs-soak has no parent");
    let candidate = dir.join(cli_exe_name());
    if candidate.exists() {
        return candidate;
    }
    // Fall back to invoking cargo to build (and locate) the binary. This
    // path is hit when the soaker's deps build before the CLI's — rare in
    // practice (CLI deps build first because the test asserts depends on
    // both), but explicit so we never silently fail.
    let out = Command::new(env!("CARGO"))
        .args([
            "build",
            "--quiet",
            "-p",
            "tomcatrs-cli",
            "--bin",
            "tomcatrs",
        ])
        .output()
        .expect("cargo build tomcatrs-cli");
    assert!(
        out.status.success(),
        "cargo build of tomcatrs-cli failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let candidate = dir.join(cli_exe_name());
    assert!(
        candidate.exists(),
        "tomcatrs CLI binary not found at {} after explicit build",
        candidate.display(),
    );
    candidate
}

#[cfg(windows)]
fn cli_exe_name() -> &'static str {
    "tomcatrs.exe"
}

#[cfg(not(windows))]
fn cli_exe_name() -> &'static str {
    "tomcatrs"
}
