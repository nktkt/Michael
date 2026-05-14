//! Build script for `tomcatrs-servlet-bridge`.
//!
//! When the `jvm` feature is enabled (`CARGO_FEATURE_JVM` present in the
//! environment), this compiles the Java bridge sources under `java/` —
//! the `jakarta.servlet.*` compile-time stubs plus the
//! `org.apache.tomcatrs.bridge.*` facades — with `javac`, and packages the
//! resulting classes into `$OUT_DIR/tomcatrs-bridge.jar`. The jar path is
//! exported to Rust code as the `TOMCATRS_BRIDGE_JAR` env var
//! (`env!("TOMCATRS_BRIDGE_JAR")`).
//!
//! With default features (no `jvm`) this is a complete no-op: no JDK is needed.
//!
//! It is *deliberately tolerant*: if `javac` / `jar` are missing or fail, it
//! prints a `cargo:warning=` and exits successfully. A missing JDK must never
//! fail the build — the crate still compiles, it just won't have a bundled jar
//! (the embedding application can supply one out-of-band).

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // The `java/` tree is an input whenever we might build the jar.
    println!("cargo:rerun-if-changed=java");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_JVM");

    if env::var_os("CARGO_FEATURE_JVM").is_none() {
        // Default features: nothing to do, no JDK required.
        return;
    }

    if let Err(msg) = build_bridge_jar() {
        // Never fail the build: warn and carry on without a bundled jar.
        println!("cargo:warning=tomcatrs-servlet-bridge: {msg}");
        println!(
            "cargo:warning=tomcatrs-bridge.jar was NOT built; \
             supply it on the JVM classpath out-of-band."
        );
    }
}

/// Compile the Java sources and package them into `$OUT_DIR/tomcatrs-bridge.jar`.
/// Returns a human-readable error string on any failure (caller downgrades it
/// to a warning).
fn build_bridge_jar() -> Result<(), String> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").map_err(|e| e.to_string())?);
    let out_dir = PathBuf::from(env::var("OUT_DIR").map_err(|e| e.to_string())?);

    let java_root = manifest_dir.join("java");
    let stubs_root = java_root.join("jakarta-stubs");
    let facades_root = java_root.join("org");

    if !java_root.is_dir() {
        return Err(format!(
            "java source dir not found: {}",
            java_root.display()
        ));
    }

    // Locate the JDK tools. Honour `JAVA_HOME` if set, else rely on `PATH`.
    let javac = tool_path("javac");
    let jar = tool_path("jar");

    // Probe `javac` first so a missing JDK degrades to a warning, not an error.
    let probe = Command::new(&javac).arg("-version").output();
    match probe {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            return Err(format!(
                "`{} -version` failed (status {}); skipping jar build",
                javac.display(),
                out.status
            ));
        }
        Err(e) => {
            return Err(format!(
                "`{}` not runnable ({e}); skipping jar build",
                javac.display()
            ));
        }
    }

    // Collect every `.java` file under the stubs + facades trees.
    let mut sources: Vec<PathBuf> = Vec::new();
    collect_java(&stubs_root, &mut sources);
    collect_java(&facades_root, &mut sources);
    if sources.is_empty() {
        return Err(format!(
            "no .java sources found under {}",
            java_root.display()
        ));
    }

    let classes_dir = out_dir.join("classes");
    // Start from a clean classes dir so stale `.class` files never leak in.
    let _ = std::fs::remove_dir_all(&classes_dir);
    std::fs::create_dir_all(&classes_dir)
        .map_err(|e| format!("cannot create {}: {e}", classes_dir.display()))?;

    // Compile. The stubs provide `jakarta.servlet.*` on the classpath, so no
    // external jar is needed for a standalone build.
    let mut javac_cmd = Command::new(&javac);
    javac_cmd
        .arg("-d")
        .arg(&classes_dir)
        .arg("-encoding")
        .arg("UTF-8");
    for src in &sources {
        javac_cmd.arg(src);
    }
    run(&mut javac_cmd, "javac")?;

    // Package the compiled classes into the bridge jar.
    let jar_path = out_dir.join("tomcatrs-bridge.jar");
    let mut jar_cmd = Command::new(&jar);
    jar_cmd
        .arg("cf")
        .arg(&jar_path)
        .arg("-C")
        .arg(&classes_dir)
        .arg(".");
    run(&mut jar_cmd, "jar")?;

    if !jar_path.is_file() {
        return Err(format!(
            "jar reported success but {} is missing",
            jar_path.display()
        ));
    }

    // Hand the jar location to Rust code: `env!("TOMCATRS_BRIDGE_JAR")`.
    println!("cargo:rustc-env=TOMCATRS_BRIDGE_JAR={}", jar_path.display());
    Ok(())
}

/// Resolve a JDK tool name to a path, preferring `$JAVA_HOME/bin/<tool>` when
/// `JAVA_HOME` is set; otherwise fall back to the bare name (found via `PATH`).
fn tool_path(tool: &str) -> PathBuf {
    if let Some(java_home) = env::var_os("JAVA_HOME") {
        let candidate = Path::new(&java_home).join("bin").join(tool);
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from(tool)
}

/// Recursively collect `*.java` files under `dir` into `out`.
fn collect_java(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_java(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("java") {
            out.push(path);
        }
    }
}

/// Run a command, mapping a non-zero exit or spawn failure to an error string.
fn run(cmd: &mut Command, label: &str) -> Result<(), String> {
    let output = cmd
        .output()
        .map_err(|e| format!("failed to spawn `{label}`: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "`{label}` failed (status {}):\n{}",
        output.status,
        stderr.trim()
    ))
}
