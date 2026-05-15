//! Graceful-shutdown end-to-end test for the `tomcatrs` CLI binary.
//!
//! This test boots the compiled CLI as a subprocess, confirms it is serving
//! traffic, then on Unix sends `SIGTERM` (the signal Docker / systemd /
//! Kubernetes deliver for a graceful stop) and asserts that:
//!
//! 1. The process exits cleanly within `--shutdown-timeout + small slop`.
//! 2. The port is immediately re-bindable afterwards — i.e. the listener was
//!    actually released, not leaked behind a half-dead process.
//!
//! Windows targets cannot portably deliver `SIGTERM` to a subprocess, so the
//! signal half of the test is `#[cfg(unix)]`-gated. The lighter sanity check
//! (`--help` exposes `--shutdown-timeout`) runs everywhere.

use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Kill-on-drop guard around a spawned child process so a panic mid-test
/// never orphans the server.
struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    /// Take the child out of the guard (e.g. so the caller can `wait()` it
    /// explicitly without the `Drop` running afterwards).
    fn take(&mut self) -> Option<Child> {
        self.0.take()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Reserve a free TCP port by binding to `127.0.0.1:0` and immediately
/// dropping the listener.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
}

/// Poll `127.0.0.1:port` until a TCP connect succeeds or the deadline elapses.
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
fn run_help_advertises_shutdown_timeout_flag() {
    // Sanity check available on every platform: the new flag must appear in
    // `--help` so operators discover it.
    let bin = env!("CARGO_BIN_EXE_tomcatrs");
    let out = Command::new(bin)
        .args(["run", "--help"])
        .output()
        .expect("spawn tomcatrs run --help");
    assert!(out.status.success(), "run --help should exit zero");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--shutdown-timeout"),
        "run --help should list --shutdown-timeout; got:\n{stdout}",
    );
}

#[cfg(unix)]
#[test]
fn sigterm_triggers_graceful_shutdown_within_timeout() {
    use std::io::ErrorKind;

    // Lay out a tiny app-base so the CLI has something to serve.
    let tmp = std::env::temp_dir().join(format!(
        "tomcatrs-graceful-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let _tmp_guard = scopeguard::OnDrop::new(&tmp);
    std::fs::write(tmp.join("index.html"), b"hello").expect("write index.html");

    let port = free_port();
    let bin = env!("CARGO_BIN_EXE_tomcatrs");
    // A short shutdown-timeout keeps the test snappy; even a slow CI runner
    // shouldn't take more than a second or two to drain zero requests.
    let shutdown_timeout_secs: u64 = 3;
    let child = Command::new(bin)
        .arg("run")
        .arg("--port")
        .arg(port.to_string())
        .arg("--app-base")
        .arg(&tmp)
        .arg("--log-level")
        .arg("warn")
        .arg("--shutdown-timeout")
        .arg(shutdown_timeout_secs.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tomcatrs");
    let pid = child.id() as i32;
    let mut guard = ChildGuard::new(child);

    // Wait for the listener to be ready. A cold debug build needs a generous
    // budget.
    assert!(
        wait_until_listening(port, Instant::now() + Duration::from_secs(30)),
        "tomcatrs never started listening on 127.0.0.1:{port}",
    );

    // Hit the server once to confirm it's actually serving.
    {
        use std::io::{Read, Write};
        let mut s = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut buf = [0u8; 64];
        let n = s.read(&mut buf).unwrap();
        let head = std::str::from_utf8(&buf[..n]).unwrap_or("");
        assert!(
            head.starts_with("HTTP/1.1 200"),
            "expected 200 from pre-shutdown probe, got: {head:?}",
        );
    }

    // Send SIGTERM. We use raw `libc::kill` rather than pulling in `nix` so
    // there are no new workspace deps.
    let send_signal = |sig: i32| {
        // SAFETY: `kill` is a thin libc wrapper. `pid` is the child we just
        // spawned and which has not been reaped, so its slot is valid for
        // the duration of this call.
        let rc = unsafe { libc::kill(pid, sig) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            // ESRCH means the child already exited — fine for our purposes.
            if err.kind() != ErrorKind::NotFound {
                panic!("kill({pid}, {sig}) failed: {err}");
            }
        }
    };
    send_signal(libc::SIGTERM);

    // Wait for the child to exit within the shutdown budget plus a safety
    // margin. The CLI's own outer budget is `drain_timeout + 2s`; we also
    // need a little slop for child-process teardown and the Server::stop /
    // destroy lifecycle.
    let exit_budget = Duration::from_secs(shutdown_timeout_secs + 8);
    let start = Instant::now();
    let exit_status = loop {
        let child = guard.0.as_mut().expect("child still tracked");
        match child.try_wait().expect("try_wait") {
            Some(status) => break status,
            None => {
                if start.elapsed() >= exit_budget {
                    panic!(
                        "tomcatrs did not exit within {}s of SIGTERM (shutdown-timeout={}s)",
                        exit_budget.as_secs(),
                        shutdown_timeout_secs,
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    // The CLI's `run` returns `Ok(())` on a clean shutdown, which becomes
    // exit status 0.
    assert!(
        exit_status.success(),
        "tomcatrs exited non-zero after SIGTERM: {exit_status:?}",
    );

    // Avoid the `Drop` calling `kill()` on an already-exited child; we've
    // got the status, so reaping is done.
    let _ = guard.take();

    // The port should be immediately re-bindable: the listener was actually
    // released, not leaked behind a half-dead process.
    let rebind = TcpListener::bind(format!("127.0.0.1:{port}"));
    assert!(
        rebind.is_ok(),
        "could not rebind 127.0.0.1:{port} after graceful shutdown: {:?}",
        rebind.err(),
    );
}

/// A tiny `Drop`-on-scope-exit helper used to clean up the test's temp dir
/// without pulling in `tempfile` or `scopeguard` as a real dep. Kept local
/// so its scope is obviously limited to this test file.
#[cfg(unix)]
mod scopeguard {
    use std::path::Path;

    pub struct OnDrop<'a> {
        path: &'a Path,
    }

    impl<'a> OnDrop<'a> {
        pub fn new(path: &'a Path) -> Self {
            Self { path }
        }
    }

    impl Drop for OnDrop<'_> {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.path);
        }
    }
}
