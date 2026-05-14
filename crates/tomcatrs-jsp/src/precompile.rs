//! [`PrecompileTask`] — ahead-of-time "JSP → servlet" compilation.
//!
//! The preferred Tomcat-RS deployment model is *precompilation*: every JSP in a
//! web application is compiled to a servlet before the application starts
//! serving traffic. This removes first-request compilation latency, removes the
//! need for a Java compiler in the production runtime, and turns JSP errors
//! into deploy-time failures instead of runtime 500s.
//!
//! Apache Tomcat ships this capability as `org.apache.jasper.JspC`, the
//! ahead-of-time JSP → `.java` → `.class` compiler. This module is the Rust
//! orchestration *around* `JspC`:
//!
//! * [`find_jsp_files`] — real recursive discovery of `*.jsp` / `*.jspx`
//!   sources under a webapp, with deterministic ordering.
//! * [`PrecompileConfig`] — the precompile knobs (output directory, generated
//!   package prefix, target VM, fail-fast, web.xml fragment generation).
//! * [`PrecompileTask::discover`] — discovers every JSP and computes the
//!   generated servlet class name for each, applying *Jasper's* identifier
//!   mangling so the names match what `JspC` itself would emit.
//! * [`PrecompileTask::run`] — drives compilation. With the `jvm` feature it
//!   builds the `org.apache.jasper.JspC` command line and shells out to `java`
//!   *if* a Jasper classpath is configured, otherwise it returns a report
//!   marked `skipped` rather than failing the build. Without the `jvm` feature
//!   it produces the same [`PrecompileReport`] describing what *would* be
//!   compiled, fully offline and testable.
//! * [`PrecompileTask::web_xml_fragment`] — generates the
//!   `<servlet>` / `<servlet-mapping>` `web.xml` fragment mapping each JSP path
//!   to its precompiled servlet class — exactly what `JspC -webxml` produces.
//!
//! ## Jasper name mangling
//!
//! Jasper derives a servlet class from a JSP's *context-relative* path. The
//! directory components become a Java package appended to a base prefix
//! (default `org.apache.jsp`), and the file name (without extension) becomes
//! the class name with `_jsp` appended. Every path segment is run through
//! Jasper's identifier mangler ([`mangle_identifier`]): a leading digit is
//! prefixed with `_`, and any character that is not a valid Java identifier
//! part is replaced with `_` followed by the lower-case, zero-padded
//! 4-digit hex of its code point (e.g. `-` → `_002d`). This matches
//! `org.apache.jasper.compiler.JspUtil.makeJavaIdentifier`.

use std::path::{Path, PathBuf};

use tomcatrs_core::{Error, Result};

/// JSP file extensions recognised by the precompiler, lower-cased.
const JSP_EXTENSIONS: [&str; 2] = ["jsp", "jspx"];

/// The default Java package prefix Jasper places generated servlets under.
pub const DEFAULT_PACKAGE_PREFIX: &str = "org.apache.jsp";

/// The default JVM bytecode target Jasper compiles against.
pub const DEFAULT_TARGET_VM: &str = "17";

/// Environment variable consulted by [`PrecompileTask::run`] (under the `jvm`
/// feature) for the classpath that hosts `org.apache.jasper.JspC` and its
/// dependencies. When unset, `run` skips compilation gracefully.
pub const JASPER_CLASSPATH_ENV: &str = "TOMCATRS_JASPER_CLASSPATH";

/// Environment variable consulted by [`PrecompileTask::run`] (under the `jvm`
/// feature) for a Jasper installation root. When set, `<JASPER_HOME>/lib/*`
/// is used as the classpath if [`JASPER_CLASSPATH_ENV`] is not set.
pub const JASPER_HOME_ENV: &str = "JASPER_HOME";

/// Configuration for an ahead-of-time JSP precompilation run.
///
/// Mirrors the knobs `org.apache.jasper.JspC` exposes on its command line.
#[derive(Debug, Clone)]
pub struct PrecompileConfig {
    /// The web application root to scan for JSP sources.
    pub webapp_root: PathBuf,
    /// Directory generated `.java` / `.class` files are written to. This is
    /// `JspC`'s `-d` output directory.
    pub output_dir: PathBuf,
    /// Java package prefix for generated servlets (`JspC`'s `-p`). Defaults to
    /// [`DEFAULT_PACKAGE_PREFIX`] (`org.apache.jsp`).
    pub package_prefix: String,
    /// JVM bytecode target the generated sources are compiled against
    /// (`JspC`'s `-compileSourceVM` / `-compileTargetVM`).
    pub target_vm: String,
    /// Abort the whole run on the first JSP that fails to compile (`JspC`'s
    /// `-failFast`). When `false`, every JSP is attempted and failures are
    /// collected.
    pub fail_fast: bool,
    /// Emit a `web.xml` fragment mapping each JSP to its generated servlet
    /// (`JspC`'s `-webxml`). See [`PrecompileTask::web_xml_fragment`].
    pub generate_web_xml_fragment: bool,
}

impl PrecompileConfig {
    /// Build a configuration with Jasper-matching defaults: the standard
    /// `org.apache.jsp` package prefix, [`DEFAULT_TARGET_VM`] target,
    /// `fail_fast` disabled, and `web.xml` fragment generation enabled.
    pub fn new(webapp_root: impl AsRef<Path>, output_dir: impl AsRef<Path>) -> PrecompileConfig {
        PrecompileConfig {
            webapp_root: webapp_root.as_ref().to_path_buf(),
            output_dir: output_dir.as_ref().to_path_buf(),
            package_prefix: DEFAULT_PACKAGE_PREFIX.to_string(),
            target_vm: DEFAULT_TARGET_VM.to_string(),
            fail_fast: false,
            generate_web_xml_fragment: true,
        }
    }

    /// Override the generated-servlet package prefix.
    pub fn with_package_prefix(mut self, prefix: impl Into<String>) -> PrecompileConfig {
        self.package_prefix = prefix.into();
        self
    }

    /// Override the JVM bytecode target.
    pub fn with_target_vm(mut self, target_vm: impl Into<String>) -> PrecompileConfig {
        self.target_vm = target_vm.into();
        self
    }

    /// Set whether the run aborts on the first compilation failure.
    pub fn with_fail_fast(mut self, fail_fast: bool) -> PrecompileConfig {
        self.fail_fast = fail_fast;
        self
    }

    /// Set whether a `web.xml` fragment is generated.
    pub fn with_web_xml_fragment(mut self, generate: bool) -> PrecompileConfig {
        self.generate_web_xml_fragment = generate;
        self
    }
}

/// One generated servlet: the JSP it came from and where its artefacts land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedServlet {
    /// The JSP's context-relative request path, always `/`-rooted and using
    /// `/` separators (e.g. `/WEB-INF/jsp/my-page.jsp`).
    pub jsp_path: String,
    /// The fully-qualified generated servlet class name, e.g.
    /// `org.apache.jsp.WEB_002dINF.jsp.my_002dpage_jsp`.
    pub class_name: String,
    /// Absolute path of the generated `.java` source under the output dir.
    pub java_path: PathBuf,
    /// Absolute path of the compiled `.class` file under the output dir.
    pub class_path: PathBuf,
}

/// The outcome of a [`PrecompileTask::run`].
#[derive(Debug, Clone)]
pub struct PrecompileReport {
    /// How many JSP source files were discovered.
    pub discovered: usize,
    /// Fully-qualified class names that were (or, offline, would be) compiled.
    pub compiled: Vec<String>,
    /// `Some(reason)` if compilation was skipped instead of run — for example
    /// because no Jasper classpath is configured. `None` means compilation was
    /// attempted (or, in offline mode, fully simulated).
    pub skipped: Option<String>,
    /// The generated-servlet plan: one entry per discovered JSP.
    pub generated: Vec<GeneratedServlet>,
}

impl PrecompileReport {
    /// Whether compilation was skipped rather than performed.
    pub fn was_skipped(&self) -> bool {
        self.skipped.is_some()
    }
}

/// An ahead-of-time JSP precompilation task for one web application.
///
/// Construct one with [`PrecompileTask::discover`], which walks the webapp,
/// records every JSP file, and computes the Jasper-mangled servlet class name
/// and output paths for each. [`PrecompileTask::run`] then drives compilation,
/// and [`PrecompileTask::web_xml_fragment`] produces the deployment descriptor
/// fragment.
#[derive(Debug, Clone)]
pub struct PrecompileTask {
    config: PrecompileConfig,
    generated: Vec<GeneratedServlet>,
}

impl PrecompileTask {
    /// Discover every JSP under `config.webapp_root` and compute the generated
    /// servlet plan.
    ///
    /// For each JSP the context-relative path is mangled into a fully-qualified
    /// servlet class name (see the [module docs](self)) and the corresponding
    /// `.java` / `.class` output paths under `config.output_dir` are derived.
    ///
    /// # Errors
    ///
    /// * [`Error::NotFound`] if `webapp_root` does not exist or is not a
    ///   directory.
    /// * [`Error::Io`] if the directory tree cannot be traversed.
    pub fn discover(config: PrecompileConfig) -> Result<PrecompileTask> {
        let jsp_files = find_jsp_files(&config.webapp_root)?;

        let mut generated = Vec::with_capacity(jsp_files.len());
        for abs in &jsp_files {
            let rel = context_relative_path(&config.webapp_root, abs);
            let class_name = mangle_class_name(&rel, &config.package_prefix);
            let (java_path, class_path) = output_paths(&config.output_dir, &class_name);
            generated.push(GeneratedServlet {
                jsp_path: rel,
                class_name,
                java_path,
                class_path,
            });
        }

        tracing::info!(
            webapp_root = %config.webapp_root.display(),
            output_dir = %config.output_dir.display(),
            jsp_files = generated.len(),
            "discovered JSP files and computed Jasper servlet class names"
        );

        Ok(PrecompileTask { config, generated })
    }

    /// The configuration this task was discovered with.
    pub fn config(&self) -> &PrecompileConfig {
        &self.config
    }

    /// The web application root this task was discovered from.
    pub fn webapp_root(&self) -> &Path {
        &self.config.webapp_root
    }

    /// The generated-servlet plan: one entry per discovered JSP, in
    /// deterministic order.
    pub fn generated(&self) -> &[GeneratedServlet] {
        &self.generated
    }

    /// Every JSP file discovered, as context-relative `/`-rooted paths.
    pub fn jsp_paths(&self) -> Vec<&str> {
        self.generated.iter().map(|g| g.jsp_path.as_str()).collect()
    }

    /// Whether the web application contains no JSP files at all.
    pub fn is_empty(&self) -> bool {
        self.generated.is_empty()
    }

    /// The number of JSP files discovered.
    pub fn len(&self) -> usize {
        self.generated.len()
    }

    /// Build the `org.apache.jasper.JspC` command-line arguments for this task
    /// (everything after the `org.apache.jasper.JspC` main class itself).
    ///
    /// This is the canonical translation of a [`PrecompileConfig`] into
    /// `JspC`'s own flags; it is used to construct the `java` invocation under
    /// the `jvm` feature and is exposed for inspection and testing.
    pub fn jspc_args(&self) -> Vec<String> {
        let mut args = vec![
            "-d".to_string(),
            self.config.output_dir.display().to_string(),
            "-p".to_string(),
            self.config.package_prefix.clone(),
            "-compileSourceVM".to_string(),
            self.config.target_vm.clone(),
            "-compileTargetVM".to_string(),
            self.config.target_vm.clone(),
            // Always generate .class files, not just .java sources.
            "-compile".to_string(),
        ];
        if self.config.fail_fast {
            args.push("-failFast".to_string());
        }
        if self.config.generate_web_xml_fragment {
            args.push("-webxmlencoding".to_string());
            args.push("UTF-8".to_string());
            args.push("-webinc".to_string());
            args.push(
                self.config
                    .output_dir
                    .join("generated_web.xml")
                    .display()
                    .to_string(),
            );
        }
        // `-webapp` makes JspC scan the directory tree itself.
        args.push("-webapp".to_string());
        args.push(self.config.webapp_root.display().to_string());
        args
    }

    /// Run the precompilation and produce a [`PrecompileReport`].
    ///
    /// Without the `jvm` feature this is a fully offline simulation: it returns
    /// a report describing exactly what would be compiled — every discovered
    /// JSP, its generated class name and intended output paths — with
    /// `skipped` set to a reason noting the `jvm` feature is disabled. Nothing
    /// is executed and no files are written.
    ///
    /// # Errors
    ///
    /// Never fails on the default (no-`jvm`) path.
    #[cfg(not(feature = "jvm"))]
    pub fn run(&self) -> Result<PrecompileReport> {
        tracing::info!(
            jsp_files = self.generated.len(),
            "precompile run in offline mode (jvm feature disabled): producing a \
             plan without invoking org.apache.jasper.JspC"
        );
        Ok(PrecompileReport {
            discovered: self.generated.len(),
            compiled: Vec::new(),
            skipped: Some(
                "jvm feature disabled: org.apache.jasper.JspC was not invoked; \
                 report describes the intended precompilation plan only"
                    .to_string(),
            ),
            generated: self.generated.clone(),
        })
    }

    /// Run the precompilation and produce a [`PrecompileReport`].
    ///
    /// With the `jvm` feature this builds the `org.apache.jasper.JspC` command
    /// line (see [`Self::jspc_args`]) and shells out to `java` — *if* a Jasper
    /// classpath is resolvable from the environment ([`JASPER_CLASSPATH_ENV`],
    /// or `<JASPER_HOME>/lib/*` via [`JASPER_HOME_ENV`]). If neither is set,
    /// the run is *skipped* gracefully: the returned report carries a clear
    /// `skipped` reason and the full generated plan, and the build is not
    /// failed. Wiring an in-process `JspC` (Jasper jars on a managed classpath)
    /// is a later increment.
    ///
    /// # Errors
    ///
    /// * [`Error::Other`] if the `java` process cannot be launched.
    /// * [`Error::Other`] if `JspC` exits non-zero (compilation failure).
    #[cfg(feature = "jvm")]
    pub fn run(&self) -> Result<PrecompileReport> {
        let classpath = match resolve_jasper_classpath() {
            Some(cp) => cp,
            None => {
                let reason = format!(
                    "no Jasper classpath configured: set ${JASPER_CLASSPATH_ENV} \
                     (or ${JASPER_HOME_ENV}) to the classpath hosting \
                     org.apache.jasper.JspC; skipping compilation of {} JSP file(s)",
                    self.generated.len()
                );
                tracing::warn!(reason, "precompile run skipped");
                return Ok(PrecompileReport {
                    discovered: self.generated.len(),
                    compiled: Vec::new(),
                    skipped: Some(reason),
                    generated: self.generated.clone(),
                });
            }
        };

        let mut cmd = std::process::Command::new("java");
        cmd.arg("-cp")
            .arg(&classpath)
            .arg("org.apache.jasper.JspC")
            .args(self.jspc_args());

        tracing::info!(
            classpath = %classpath,
            jsp_files = self.generated.len(),
            "invoking org.apache.jasper.JspC"
        );

        let status = cmd.status().map_err(|e| {
            Error::Other(format!(
                "failed to launch `java` for org.apache.jasper.JspC: {e}"
            ))
        })?;

        if !status.success() {
            return Err(Error::Other(format!(
                "org.apache.jasper.JspC exited with {status} while precompiling \
                 {} JSP file(s) under {}",
                self.generated.len(),
                self.config.webapp_root.display()
            )));
        }

        Ok(PrecompileReport {
            discovered: self.generated.len(),
            compiled: self
                .generated
                .iter()
                .map(|g| g.class_name.clone())
                .collect(),
            skipped: None,
            generated: self.generated.clone(),
        })
    }

    /// Generate the `web.xml` fragment mapping every discovered JSP path to its
    /// precompiled servlet class.
    ///
    /// The output is a `<servlet>` element (with `<servlet-name>` and
    /// `<servlet-class>`) plus a matching `<servlet-mapping>` (with the JSP's
    /// context-relative `<url-pattern>`) for each JSP — exactly the shape
    /// `org.apache.jasper.JspC -webxml` produces. The servlet name is the
    /// fully-qualified generated class name, which is unique per JSP.
    ///
    /// The returned string is an XML *fragment* (no document element); it is
    /// meant to be spliced into a webapp's `WEB-INF/web.xml`.
    pub fn web_xml_fragment(&self) -> String {
        let mut out = String::new();
        out.push_str(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!-- Generated by tomcatrs-jsp precompiler; equivalent to \
             `org.apache.jasper.JspC -webxml`. -->\n",
        );

        for g in &self.generated {
            out.push_str("<servlet>\n");
            out.push_str("    <servlet-name>");
            out.push_str(&xml_escape(&g.class_name));
            out.push_str("</servlet-name>\n");
            out.push_str("    <servlet-class>");
            out.push_str(&xml_escape(&g.class_name));
            out.push_str("</servlet-class>\n");
            out.push_str("</servlet>\n");
        }

        for g in &self.generated {
            out.push_str("<servlet-mapping>\n");
            out.push_str("    <servlet-name>");
            out.push_str(&xml_escape(&g.class_name));
            out.push_str("</servlet-name>\n");
            out.push_str("    <url-pattern>");
            out.push_str(&xml_escape(&g.jsp_path));
            out.push_str("</url-pattern>\n");
            out.push_str("</servlet-mapping>\n");
        }

        out
    }
}

/// Resolve the classpath hosting `org.apache.jasper.JspC`, if configured.
///
/// Prefers [`JASPER_CLASSPATH_ENV`] verbatim; otherwise, if [`JASPER_HOME_ENV`]
/// names an existing directory, returns `<JASPER_HOME>/lib/*` (the JVM wildcard
/// classpath form). Returns `None` if nothing is configured.
#[cfg(feature = "jvm")]
fn resolve_jasper_classpath() -> Option<String> {
    if let Ok(cp) = std::env::var(JASPER_CLASSPATH_ENV) {
        if !cp.trim().is_empty() {
            return Some(cp);
        }
    }
    if let Ok(home) = std::env::var(JASPER_HOME_ENV) {
        let lib = Path::new(&home).join("lib");
        if lib.is_dir() {
            return Some(lib.join("*").display().to_string());
        }
    }
    None
}

/// Recursively find every `*.jsp` / `*.jspx` file under `webapp_root`.
///
/// Results are sorted by path for deterministic ordering. Symlink loops are not
/// followed beyond the standard library's directory iteration semantics.
///
/// # Errors
///
/// * [`Error::NotFound`] if `webapp_root` does not exist or is not a directory.
/// * [`Error::Io`] if a directory in the tree cannot be read.
pub fn find_jsp_files(webapp_root: &Path) -> Result<Vec<PathBuf>> {
    if !webapp_root.exists() {
        return Err(Error::NotFound(format!(
            "webapp root {} does not exist",
            webapp_root.display()
        )));
    }
    if !webapp_root.is_dir() {
        return Err(Error::NotFound(format!(
            "webapp root {} is not a directory",
            webapp_root.display()
        )));
    }

    let mut found = Vec::new();
    let mut stack = vec![webapp_root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() && has_jsp_extension(&path) {
                found.push(path);
            }
        }
    }

    found.sort();
    Ok(found)
}

/// Whether `path` has a recognised JSP extension (case-insensitive).
fn has_jsp_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let lower = e.to_ascii_lowercase();
            JSP_EXTENSIONS.contains(&lower.as_str())
        })
        .unwrap_or(false)
}

/// Express `abs` as a context-relative, `/`-rooted path under `webapp_root`,
/// always using `/` separators (so results are stable across platforms).
fn context_relative_path(webapp_root: &Path, abs: &Path) -> String {
    let rel = abs.strip_prefix(webapp_root).unwrap_or(abs);
    let mut s = String::from("/");
    let parts: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(os) => Some(os.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    s.push_str(&parts.join("/"));
    s
}

/// Compute the fully-qualified Jasper servlet class name for a context-relative
/// JSP path.
///
/// `/index.jsp` with prefix `org.apache.jsp` becomes `org.apache.jsp.index_jsp`;
/// `/foo/bar.jsp` becomes `org.apache.jsp.foo.bar_jsp`. Each path segment is run
/// through [`mangle_identifier`], and the class name has `_jsp` appended (after
/// mangling the bare file name). This mirrors
/// `org.apache.jasper.compiler.JspCompilationContext.getServletPackageName` /
/// `getServletClassName`.
pub fn mangle_class_name(jsp_path: &str, package_prefix: &str) -> String {
    let trimmed = jsp_path.trim_start_matches('/');
    let mut segments: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();

    // The final segment is the file name; the rest form the sub-package.
    let file = segments.pop().unwrap_or("");
    let stem = match file.rfind('.') {
        Some(idx) => &file[..idx],
        None => file,
    };

    let mut parts: Vec<String> = Vec::new();
    if !package_prefix.is_empty() {
        // The prefix is already a valid dotted package; keep it verbatim.
        parts.push(package_prefix.to_string());
    }
    for seg in segments {
        parts.push(mangle_identifier(seg));
    }
    // Class name: char-mangled stem with the Jasper `_jsp` suffix. The
    // reserved-word check is intentionally *not* applied to the stem: the
    // `_jsp` suffix guarantees the resulting class name is never a reserved
    // word, which matches Jasper (`for.jsp` → `for_jsp`, not `for__jsp`).
    let class_simple = format!("{}_jsp", mangle_identifier_chars(stem));
    parts.push(class_simple);

    parts.join(".")
}

/// Derive the `.java` and `.class` output paths for a fully-qualified class
/// name under `output_dir` (dotted package → nested directories).
fn output_paths(output_dir: &Path, class_name: &str) -> (PathBuf, PathBuf) {
    let mut java = output_dir.to_path_buf();
    let segments: Vec<&str> = class_name.split('.').collect();
    for seg in &segments[..segments.len().saturating_sub(1)] {
        java.push(seg);
    }
    let simple = segments.last().copied().unwrap_or("");
    let class = java.join(format!("{simple}.class"));
    java.push(format!("{simple}.java"));
    (java, class)
}

/// Mangle one path segment into a valid Java identifier, the way Jasper does.
///
/// Rules (from `org.apache.jasper.compiler.JspUtil.makeJavaIdentifier`):
///
/// * An empty segment becomes `_`.
/// * If the first character is not a valid Java identifier *start*, the result
///   is prefixed with `_` — this covers leading digits (`9lives` → `_9lives`).
/// * Each character that is not a valid Java identifier *part* is replaced with
///   `_` followed by its code point as a zero-padded, lower-case 4-hex-digit
///   number (`-` → `_002d`, `.` → `_002e`, space → `_0020`).
/// * Java reserved words and literals (`class`, `for`, `true`, …) are suffixed
///   with `_` so they remain usable as identifiers (`for` → `for_`).
///
/// Note that `_` itself is a valid identifier part, so it is preserved as-is;
/// this is why a clean name like `index` is unchanged.
pub fn mangle_identifier(segment: &str) -> String {
    let out = mangle_identifier_chars(segment);
    if is_java_reserved(&out) {
        let mut out = out;
        out.push('_');
        out
    } else {
        out
    }
}

/// Mangle one path segment so every character is valid in a Java identifier,
/// *without* the reserved-word suffix step.
///
/// This is the character-level half of [`mangle_identifier`]: an empty segment
/// becomes `_`, a leading non-start (e.g. a digit) is prefixed with `_`, and
/// any non-identifier character is escaped via [`mangle_char`]. It is used
/// directly for the class-name stem, where the always-appended `_jsp` suffix
/// makes the reserved-word check moot.
fn mangle_identifier_chars(segment: &str) -> String {
    if segment.is_empty() {
        return "_".to_string();
    }

    let mut out = String::with_capacity(segment.len() + 2);
    for (i, ch) in segment.chars().enumerate() {
        let valid = if i == 0 {
            is_java_identifier_start(ch)
        } else {
            is_java_identifier_part(ch)
        };
        if valid {
            out.push(ch);
        } else if i == 0 && is_java_identifier_part(ch) {
            // Valid as a *part* but not a *start* (e.g. a leading digit):
            // Jasper prefixes the whole identifier with `_`.
            out.push('_');
            out.push(ch);
        } else {
            // Not a valid identifier character at all: escape it.
            out.push_str(&mangle_char(ch));
        }
    }
    out
}

/// Escape a single non-identifier character as `_` + 4-digit lower-case hex.
fn mangle_char(ch: char) -> String {
    format!("_{:04x}", ch as u32)
}

/// Whether `ch` may *start* a Java identifier.
///
/// Approximates `java.lang.Character.isJavaIdentifierStart`: ASCII letters,
/// `_`, `$`, and — for the wide range of webapp file names — any non-ASCII
/// alphabetic character.
fn is_java_identifier_start(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphabetic() || (!ch.is_ascii() && ch.is_alphabetic())
}

/// Whether `ch` may appear as a non-initial part of a Java identifier.
///
/// Approximates `java.lang.Character.isJavaIdentifierPart`: everything
/// [`is_java_identifier_start`] allows, plus ASCII digits and non-ASCII
/// digits.
fn is_java_identifier_part(ch: char) -> bool {
    is_java_identifier_start(ch) || ch.is_ascii_digit() || (!ch.is_ascii() && ch.is_numeric())
}

/// Java reserved words, plus the boolean/null literals, that cannot be used as
/// bare identifiers. Jasper suffixes any segment matching one of these with `_`.
const JAVA_RESERVED: &[&str] = &[
    "abstract",
    "assert",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "final",
    "finally",
    "float",
    "for",
    "goto",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "static",
    "strictfp",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "try",
    "void",
    "volatile",
    "while",
    "true",
    "false",
    "null",
];

/// Whether `word` is a Java reserved word or literal.
fn is_java_reserved(word: &str) -> bool {
    JAVA_RESERVED.contains(&word)
}

/// Minimal XML text escaping for content placed inside `web.xml` elements.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn unique_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tomcatrs-jsp-precompile-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    // --- name mangling ---------------------------------------------------

    #[test]
    fn mangle_simple_index() {
        assert_eq!(
            mangle_class_name("/index.jsp", DEFAULT_PACKAGE_PREFIX),
            "org.apache.jsp.index_jsp"
        );
    }

    #[test]
    fn mangle_nested_package() {
        assert_eq!(
            mangle_class_name("/foo/bar.jsp", DEFAULT_PACKAGE_PREFIX),
            "org.apache.jsp.foo.bar_jsp"
        );
    }

    #[test]
    fn mangle_jspx_extension() {
        assert_eq!(
            mangle_class_name("/pages/about.jspx", DEFAULT_PACKAGE_PREFIX),
            "org.apache.jsp.pages.about_jsp"
        );
    }

    #[test]
    fn mangle_hyphenated_segments() {
        // `-` is U+002D → `_002d`; applies to both directory and file segments.
        assert_eq!(
            mangle_class_name("/WEB-INF/jsp/my-page.jsp", DEFAULT_PACKAGE_PREFIX),
            "org.apache.jsp.WEB_002dINF.jsp.my_002dpage_jsp"
        );
    }

    #[test]
    fn mangle_leading_digit() {
        // A leading digit is valid as an identifier *part* but not *start*,
        // so the whole segment is prefixed with `_`.
        assert_eq!(mangle_identifier("9lives"), "_9lives");
        assert_eq!(
            mangle_class_name("/9lives.jsp", DEFAULT_PACKAGE_PREFIX),
            "org.apache.jsp._9lives_jsp"
        );
        assert_eq!(
            mangle_class_name("/2020/report.jsp", DEFAULT_PACKAGE_PREFIX),
            "org.apache.jsp._2020.report_jsp"
        );
    }

    #[test]
    fn mangle_reserved_word_segments() {
        // Reserved words get a trailing `_` so they stay valid identifiers.
        assert_eq!(mangle_identifier("class"), "class_");
        assert_eq!(mangle_identifier("for"), "for_");
        assert_eq!(
            mangle_class_name("/class/for.jsp", DEFAULT_PACKAGE_PREFIX),
            "org.apache.jsp.class_.for_jsp"
        );
    }

    #[test]
    fn mangle_misc_non_identifier_chars() {
        // Space U+0020, dot U+002E inside the stem.
        assert_eq!(mangle_identifier("a b"), "a_0020b");
        assert_eq!(mangle_identifier("v1.2"), "v1_002e2");
    }

    #[test]
    fn mangle_custom_prefix() {
        assert_eq!(
            mangle_class_name("/index.jsp", "com.example.gen"),
            "com.example.gen.index_jsp"
        );
        assert_eq!(mangle_class_name("/index.jsp", ""), "index_jsp");
    }

    #[test]
    fn output_paths_follow_package_dirs() {
        let (java, class) = output_paths(Path::new("/work/out"), "org.apache.jsp.foo.bar_jsp");
        assert_eq!(
            java,
            PathBuf::from("/work/out/org/apache/jsp/foo/bar_jsp.java")
        );
        assert_eq!(
            class,
            PathBuf::from("/work/out/org/apache/jsp/foo/bar_jsp.class")
        );
    }

    // --- discovery -------------------------------------------------------

    #[test]
    fn find_jsp_files_walks_recursively() {
        let root = unique_root("walk");
        fs::create_dir_all(root.join("WEB-INF/jsp")).unwrap();
        fs::create_dir_all(root.join("pages")).unwrap();
        fs::write(root.join("index.jsp"), b"<%-- --%>").unwrap();
        fs::write(root.join("pages/about.JSPX"), b"<jsp:root/>").unwrap();
        fs::write(root.join("WEB-INF/jsp/admin.jsp"), b"<%-- --%>").unwrap();
        fs::write(root.join("readme.txt"), b"not a jsp").unwrap();
        fs::write(root.join("style.css"), b"body{}").unwrap();

        let files = find_jsp_files(&root).unwrap();
        assert_eq!(files.len(), 3);
        assert!(files.iter().any(|p| p.ends_with("index.jsp")));
        assert!(files.iter().any(|p| p.ends_with("about.JSPX")));
        assert!(files.iter().any(|p| p.ends_with("admin.jsp")));

        let mut sorted = files.clone();
        sorted.sort();
        assert_eq!(files, sorted);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn discover_builds_task_with_generated_plan() {
        let root = unique_root("discover");
        fs::create_dir_all(root.join("WEB-INF/jsp")).unwrap();
        fs::write(root.join("index.jsp"), b"x").unwrap();
        fs::write(root.join("WEB-INF/jsp/my-page.jsp"), b"x").unwrap();
        let out = unique_root("discover-out");

        let cfg = PrecompileConfig::new(&root, &out);
        let task = PrecompileTask::discover(cfg).unwrap();

        assert!(!task.is_empty());
        assert_eq!(task.len(), 2);
        assert_eq!(task.webapp_root(), root.as_path());

        let by_path: std::collections::HashMap<_, _> = task
            .generated()
            .iter()
            .map(|g| (g.jsp_path.clone(), g))
            .collect();

        let index = by_path.get("/index.jsp").expect("index.jsp present");
        assert_eq!(index.class_name, "org.apache.jsp.index_jsp");
        assert_eq!(index.java_path, out.join("org/apache/jsp/index_jsp.java"));
        assert_eq!(index.class_path, out.join("org/apache/jsp/index_jsp.class"));

        let page = by_path
            .get("/WEB-INF/jsp/my-page.jsp")
            .expect("my-page.jsp present");
        assert_eq!(
            page.class_name,
            "org.apache.jsp.WEB_002dINF.jsp.my_002dpage_jsp"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_root_is_not_found() {
        let missing = unique_root("missing");
        let err = find_jsp_files(&missing).unwrap_err();
        assert!(matches!(err, Error::NotFound(_)));

        let cfg = PrecompileConfig::new(&missing, missing.join("out"));
        assert!(matches!(
            PrecompileTask::discover(cfg),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn empty_webapp_yields_empty_task() {
        let root = unique_root("empty");
        fs::create_dir_all(&root).unwrap();
        let cfg = PrecompileConfig::new(&root, root.join("out"));
        let task = PrecompileTask::discover(cfg).unwrap();
        assert!(task.is_empty());
        assert_eq!(task.len(), 0);
        fs::remove_dir_all(&root).ok();
    }

    // --- web.xml fragment ------------------------------------------------

    #[test]
    fn web_xml_fragment_has_servlet_and_mapping_per_jsp() {
        let root = unique_root("webxml");
        fs::create_dir_all(root.join("admin")).unwrap();
        fs::write(root.join("index.jsp"), b"x").unwrap();
        fs::write(root.join("admin/users.jsp"), b"x").unwrap();
        let out = unique_root("webxml-out");

        let task = PrecompileTask::discover(PrecompileConfig::new(&root, &out)).unwrap();
        let frag = task.web_xml_fragment();

        // One <servlet> and one <servlet-mapping> per JSP.
        assert_eq!(frag.matches("<servlet>").count(), 2);
        assert_eq!(frag.matches("<servlet-mapping>").count(), 2);

        assert!(frag.contains("<servlet-class>org.apache.jsp.index_jsp</servlet-class>"));
        assert!(frag.contains("<servlet-class>org.apache.jsp.admin.users_jsp</servlet-class>"));
        assert!(frag.contains("<url-pattern>/index.jsp</url-pattern>"));
        assert!(frag.contains("<url-pattern>/admin/users.jsp</url-pattern>"));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn web_xml_fragment_escapes_xml() {
        assert_eq!(xml_escape("a&b<c>\"d\""), "a&amp;b&lt;c&gt;&quot;d&quot;");
    }

    // --- jspc args -------------------------------------------------------

    #[test]
    fn jspc_args_translate_config() {
        let root = unique_root("args");
        fs::create_dir_all(&root).unwrap();
        let out = root.join("out");
        let cfg = PrecompileConfig::new(&root, &out)
            .with_package_prefix("com.example")
            .with_target_vm("21")
            .with_fail_fast(true);
        let task = PrecompileTask::discover(cfg).unwrap();
        let args = task.jspc_args();

        assert!(args.windows(2).any(|w| w == ["-p", "com.example"]));
        assert!(args.windows(2).any(|w| w == ["-compileTargetVM", "21"]));
        assert!(args.iter().any(|a| a == "-failFast"));
        assert!(args.iter().any(|a| a == "-webapp"));
        assert!(args.iter().any(|a| a == "-compile"));

        fs::remove_dir_all(&root).ok();
    }

    // --- run (offline path) ---------------------------------------------

    #[cfg(not(feature = "jvm"))]
    #[test]
    fn run_offline_produces_full_plan() {
        let root = unique_root("run");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("home.jsp"), b"x").unwrap();
        fs::write(root.join("sub/detail.jsp"), b"x").unwrap();
        let out = unique_root("run-out");

        let task = PrecompileTask::discover(PrecompileConfig::new(&root, &out)).unwrap();
        let report = task.run().unwrap();

        assert_eq!(report.discovered, 2);
        assert!(report.was_skipped());
        assert!(report.skipped.as_deref().unwrap().contains("jvm feature"));
        assert!(report.compiled.is_empty());
        assert_eq!(report.generated.len(), 2);

        let names: Vec<&str> = report
            .generated
            .iter()
            .map(|g| g.class_name.as_str())
            .collect();
        assert!(names.contains(&"org.apache.jsp.home_jsp"));
        assert!(names.contains(&"org.apache.jsp.sub.detail_jsp"));

        // Offline run writes nothing.
        assert!(!out.exists());

        fs::remove_dir_all(&root).ok();
    }
}
