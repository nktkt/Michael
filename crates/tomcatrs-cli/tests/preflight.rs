//! Integration test for the `tomcatrs preflight` subcommand.
//!
//! Boots the compiled CLI binary as a subprocess (Cargo wires the path
//! into `CARGO_BIN_EXE_tomcatrs`) and asserts on stdout / exit code for
//! two configurations:
//!
//! * The repository's shipped `conf/server.xml` — a known-good sample.
//!   Warnings about defaults / non-root / jvm-feature are acceptable;
//!   failures are not.
//! * A synthesised `server.xml` that adds an AJP connector without a
//!   `secret="..."` attribute. The preflight must flag this with `[FAIL]`
//!   and exit non-zero under `--strict`.
//!
//! No external crates beyond the workspace dependencies are pulled in.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Find the compiled `tomcatrs` binary the harness builds for us.
fn tomcatrs_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tomcatrs"))
}

/// Locate the workspace root by walking up from the test binary's
/// crate directory until we find a `conf/server.xml`. The CWD when
/// `cargo test` runs is the crate's own directory, so the file lives
/// two levels up.
fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut here: &Path = &manifest_dir;
    loop {
        if here.join("conf").join("server.xml").is_file() {
            return here.to_path_buf();
        }
        match here.parent() {
            Some(p) => here = p,
            None => panic!(
                "could not find workspace root containing conf/server.xml from {}",
                manifest_dir.display()
            ),
        }
    }
}

/// Make a unique temp directory under `std::env::temp_dir()`. Mirrors the
/// pattern used by `tests/e2e.rs` so we don't bring in `tempfile`.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "tomcatrs-cli-preflight-{}-{}-{}",
            label,
            std::process::id(),
            nanos,
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
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

/// Run `tomcatrs preflight --config <path> [--strict]` and capture
/// (exit_code, stdout, stderr).
fn run_preflight(config: &Path, strict: bool) -> (i32, String, String) {
    let mut cmd = Command::new(tomcatrs_bin());
    cmd.arg("preflight").arg("--config").arg(config);
    if strict {
        cmd.arg("--strict");
    }
    let out = cmd.output().expect("spawn tomcatrs preflight");
    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    (code, stdout, stderr)
}

#[test]
fn preflight_on_shipped_conf_succeeds_under_non_strict() {
    let root = workspace_root();
    let conf = root.join("conf").join("server.xml");
    assert!(conf.is_file(), "expected {} to exist", conf.display());

    // The shipped server.xml uses default RequestLimits and the test binary
    // is not built with `--features jvm`, so we expect warnings — they are
    // fine in non-strict mode. The exit code must be 0.
    let (code, stdout, _stderr) = run_preflight(&conf, /*strict=*/ false);
    assert_eq!(
        code, 0,
        "non-strict preflight on conf/server.xml should exit 0; output was:\n{stdout}"
    );

    // The first OK row should announce the parse succeeded.
    assert!(
        stdout.contains("[OK]: server.xml parses"),
        "expected an OK row about server.xml parsing, got:\n{stdout}"
    );

    // The summary line should always be printed.
    assert!(
        stdout.contains("preflight:"),
        "expected a summary line, got:\n{stdout}"
    );

    // There must be no FAIL rows on a clean config.
    assert!(
        !stdout.contains("[FAIL]"),
        "shipped server.xml should not produce any [FAIL] rows; got:\n{stdout}"
    );
}

#[test]
fn preflight_on_ajp_without_secret_fails_under_strict() {
    let tmp = TempDir::new("ajp-no-secret");
    let webapps = tmp.path().join("webapps");
    std::fs::create_dir_all(&webapps).expect("create webapps");

    // Hand-built server.xml: one HTTP/1.1 connector so the "no HTTP
    // connector" check stays green, and one AJP connector deliberately
    // missing a `secret="..."` attribute.
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Server port="8005" shutdown="SHUTDOWN">
  <Service name="Catalina">
    <Connector port="8080" protocol="HTTP/1.1" />
    <Connector port="8009" protocol="AJP/1.3" address="127.0.0.1" />
    <Engine name="Catalina" defaultHost="localhost">
      <Host name="localhost" appBase="{}" autoDeploy="true" />
    </Engine>
  </Service>
</Server>
"#,
        webapps.display()
    );
    let config = tmp.path().join("server.xml");
    std::fs::write(&config, xml).expect("write temp server.xml");

    // Strict mode: AJP without secret must produce a FAIL, and that has
    // to drive a non-zero exit code.
    let (code, stdout, stderr) = run_preflight(&config, /*strict=*/ true);
    assert_ne!(
        code, 0,
        "strict preflight on AJP-without-secret should exit non-zero;\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("[FAIL]") && stdout.contains("AJP connector"),
        "expected an AJP [FAIL] row, got:\n{stdout}"
    );

    // Non-strict mode: same failure, same non-zero exit (a FAIL always
    // fails regardless of strictness).
    let (code2, stdout2, _) = run_preflight(&config, /*strict=*/ false);
    assert_ne!(
        code2, 0,
        "non-strict preflight on AJP-without-secret should still exit non-zero;\nstdout:\n{stdout2}"
    );
}

#[test]
fn preflight_with_ajp_secret_passes_non_strict() {
    // Sanity check the inverse: an AJP connector WITH a secret should
    // not produce a FAIL on the AJP check. This guards against the
    // raw-XML helper over-reporting.
    let tmp = TempDir::new("ajp-with-secret");
    let webapps = tmp.path().join("webapps");
    std::fs::create_dir_all(&webapps).expect("create webapps");

    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Server port="8005" shutdown="SHUTDOWN">
  <Service name="Catalina">
    <Connector port="8080" protocol="HTTP/1.1" />
    <Connector port="8009" protocol="AJP/1.3" address="127.0.0.1" secret="topsecret" />
    <Engine name="Catalina" defaultHost="localhost">
      <Host name="localhost" appBase="{}" autoDeploy="true" />
    </Engine>
  </Service>
</Server>
"#,
        webapps.display()
    );
    let config = tmp.path().join("server.xml");
    std::fs::write(&config, xml).expect("write temp server.xml");

    let (code, stdout, _stderr) = run_preflight(&config, /*strict=*/ false);
    assert_eq!(
        code, 0,
        "non-strict preflight on AJP-with-secret should exit 0; output was:\n{stdout}"
    );
    assert!(
        stdout.contains("AJP connector(s) have a secret"),
        "expected the 'AJP has secret' OK row, got:\n{stdout}"
    );
}
