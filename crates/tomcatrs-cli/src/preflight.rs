//! `tomcatrs preflight` — production-readiness checks for a deployment's
//! `server.xml`.
//!
//! The preflight subcommand is meant to run **before** the runtime ever
//! binds a socket. It loads the configuration through the same parser that
//! `run` and `check-config` use (so we are validating the configuration the
//! server would actually see), then walks a checklist of hardening rules
//! described in [`SECURITY.md`] and prints one `[OK] / [WARN] / [FAIL]`
//! line per rule.
//!
//! ## Severity model
//!
//! Each check is recorded as a [`CheckResult`] with one of three statuses:
//!
//! * [`Status::Ok`]   — the check passed.
//! * [`Status::Warn`] — the check found something that is *suspicious* but
//!   not necessarily wrong. Treated as a soft failure under `--strict`.
//! * [`Status::Fail`] — the check found something that is *unsafe to ship*.
//!   Always fails the preflight, regardless of `--strict`.
//!
//! ## Exit codes
//!
//! * `0` — no `Fail`s, and either no `Warn`s or `--strict` was not given.
//! * `1` — at least one `Fail`, **or** at least one `Warn` under `--strict`.
//!
//! ## What we cannot see from the typed model
//!
//! A few of the checks (AJP `secret`, the Manager API's `allow_remote`
//! posture) reference attributes that the in-memory `ServerConfig` does
//! not currently track. Rather than introduce model surface for them
//! here, the preflight reads the raw XML once and inspects it for those
//! attributes textually. The intent is documented at the call site of
//! each "raw XML" helper so the next person to extend the model can
//! migrate the check over cleanly.
//!
//! [`SECURITY.md`]: ../../../SECURITY.md

use std::path::{Path, PathBuf};

use tomcatrs_config::{Protocol, RequestLimits, ServerConfig};

/// Status of a single preflight check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The check passed.
    Ok,
    /// The check is suspicious but not necessarily unsafe.
    Warn,
    /// The check is unsafe to ship.
    Fail,
}

impl Status {
    /// The bracketed label printed at the start of each line.
    fn label(self) -> &'static str {
        match self {
            Status::Ok => "[OK]",
            Status::Warn => "[WARN]",
            Status::Fail => "[FAIL]",
        }
    }
}

/// One row of the preflight checklist.
#[derive(Debug, Clone)]
pub struct CheckResult {
    /// Outcome of the check.
    pub status: Status,
    /// Human-readable description of what was checked and what was found.
    pub message: String,
}

impl CheckResult {
    fn ok(msg: impl Into<String>) -> Self {
        Self {
            status: Status::Ok,
            message: msg.into(),
        }
    }
    fn warn(msg: impl Into<String>) -> Self {
        Self {
            status: Status::Warn,
            message: msg.into(),
        }
    }
    fn fail(msg: impl Into<String>) -> Self {
        Self {
            status: Status::Fail,
            message: msg.into(),
        }
    }
}

/// Aggregated result of every check in the preflight run.
#[derive(Debug, Default, Clone)]
pub struct PreflightReport {
    /// Individual check rows, in the order they were recorded.
    pub results: Vec<CheckResult>,
}

impl PreflightReport {
    /// Number of `Fail` rows in the report.
    pub fn fail_count(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.status == Status::Fail)
            .count()
    }

    /// Number of `Warn` rows in the report.
    pub fn warn_count(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.status == Status::Warn)
            .count()
    }

    /// Return the exit code we should produce for this run.
    ///
    /// `0` when nothing failed (and either no warnings or `strict == false`),
    /// `1` otherwise.
    pub fn exit_code(&self, strict: bool) -> i32 {
        if self.fail_count() > 0 {
            return 1;
        }
        if strict && self.warn_count() > 0 {
            return 1;
        }
        0
    }

    fn push(&mut self, r: CheckResult) {
        self.results.push(r);
    }
}

/// Run the full preflight against the configuration at `config_path`.
///
/// Errors returned here are *exceptional* — e.g. the config file does not
/// exist on disk, or filesystem access blew up — and are *separate* from
/// the per-check `Fail` rows you see in the printed report. A failure to
/// parse the `server.xml` is the very first check, so a bad config does
/// not turn into an `Err` here either: it surfaces as a `Fail` row.
pub fn run_preflight(config_path: &Path, strict: bool) -> anyhow::Result<i32> {
    let report = collect_report(config_path);
    print_report(&report);
    print_summary(&report, strict);
    Ok(report.exit_code(strict))
}

/// Gather every check row without printing anything. Splitting the report
/// construction from the I/O keeps the unit tests deterministic.
pub(crate) fn collect_report(config_path: &Path) -> PreflightReport {
    let mut report = PreflightReport::default();

    // 1. server.xml parses. If this fails, the rest of the checklist can't
    //    run against a structured model — record the failure and bail out
    //    early. The XML-only checks (AJP secret, Manager API) still need
    //    the raw bytes, so they are queued after the parse check on the
    //    happy path.
    let raw_xml = match std::fs::read_to_string(config_path) {
        Ok(s) => s,
        Err(err) => {
            report.push(CheckResult::fail(format!(
                "could not read server.xml at {}: {err}",
                config_path.display()
            )));
            push_runtime_checks(&mut report);
            return report;
        }
    };

    let config = match ServerConfig::from_xml_str(&raw_xml) {
        Ok(cfg) => {
            report.push(CheckResult::ok(format!(
                "server.xml parses ({})",
                config_path.display()
            )));
            cfg
        }
        Err(err) => {
            report.push(CheckResult::fail(format!(
                "server.xml at {} failed to parse: {err}",
                config_path.display()
            )));
            push_runtime_checks(&mut report);
            return report;
        }
    };

    // 2. At least one HTTP/1.1 or HTTP/2 connector configured.
    check_http_connector_present(&config, &mut report);

    // 3. If TLS is configured anywhere, its cert/key paths must exist.
    check_tls_files_exist(&config, config_path, &mut report);

    // 4. RequestLimits non-default on at least one connector.
    check_request_limits_overridden(&config, &mut report);

    // 5. Manager API: allow_remote without a realm. The typed model does
    //    not yet track these, so we look at the raw XML.
    check_manager_api_exposure(&raw_xml, &mut report);

    // 6. AJP connectors must have a secret. Again, the typed model does
    //    not yet carry the `secret` attribute, so we walk the raw XML.
    check_ajp_secret(&config, &raw_xml, &mut report);

    // 7. Each Host.app_base directory exists and is readable.
    check_app_bases(&config, config_path, &mut report);

    push_runtime_checks(&mut report);

    report
}

/// Checks that are independent of the configuration file content: the
/// `jvm` feature flag and the effective uid. These always run, even when
/// the config failed to parse, because they describe the *binary* and
/// the *process*, not the deployment file.
fn push_runtime_checks(report: &mut PreflightReport) {
    // 8. `--features jvm` baked into this build.
    if cfg!(feature = "jvm") {
        report.push(CheckResult::ok(
            "the `jvm` feature is enabled in this build (servlets/JSPs can execute)",
        ));
    } else {
        report.push(CheckResult::warn(
            "the `jvm` feature is NOT enabled in this build; servlet/JSP invocations will return 501 \
             (rebuild with `--features jvm` for full compatibility)",
        ));
    }

    // 9. Running as root on Unix.
    if let Some(uid) = effective_uid() {
        if uid == 0 {
            report.push(CheckResult::warn(
                "process is running as root (euid=0); run as an unprivileged user instead",
            ));
        } else {
            report.push(CheckResult::ok(format!(
                "process is running as a non-root user (euid={uid})"
            )));
        }
    } else {
        // Non-Unix platforms: skip cleanly rather than emit a misleading row.
        report.push(CheckResult::ok("uid check skipped (non-Unix platform)"));
    }
}

/// Effective UID on Unix; `None` on Windows or other targets where the
/// concept does not apply.
#[cfg(unix)]
fn effective_uid() -> Option<u32> {
    // SAFETY: `geteuid` is a thread-safe, side-effect-free POSIX call that
    // takes no arguments and returns the caller's effective UID.
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    unsafe { Some(geteuid()) }
}

#[cfg(not(unix))]
fn effective_uid() -> Option<u32> {
    None
}

// ---- Individual check implementations ----------------------------------

fn check_http_connector_present(config: &ServerConfig, report: &mut PreflightReport) {
    let mut http11 = 0usize;
    let mut http2 = 0usize;
    let mut ajp = 0usize;
    for svc in &config.services {
        for c in &svc.connectors {
            match c.protocol {
                Protocol::Http11 => http11 += 1,
                Protocol::Http2 => http2 += 1,
                Protocol::Ajp => ajp += 1,
            }
        }
    }
    if http11 + http2 == 0 {
        report.push(CheckResult::fail(format!(
            "no HTTP/1.1 or HTTP/2 connector configured (found {ajp} AJP connector(s) only); \
             the server would accept no end-user traffic"
        )));
    } else {
        report.push(CheckResult::ok(format!(
            "at least one HTTP connector is configured (HTTP/1.1: {http11}, HTTP/2: {http2})"
        )));
    }
}

fn check_tls_files_exist(config: &ServerConfig, config_path: &Path, report: &mut PreflightReport) {
    let mut tls_connectors_seen = 0usize;
    let mut any_missing = false;

    for svc in &config.services {
        for c in &svc.connectors {
            if let Some(tls) = &c.tls {
                tls_connectors_seen += 1;
                for (kind, path) in [
                    ("certificate", &tls.cert_file),
                    ("private key", &tls.key_file),
                ] {
                    let resolved = resolve_app_base(path, config_path);
                    if path.as_os_str().is_empty() {
                        report.push(CheckResult::fail(format!(
                            "connector on port {}: TLS enabled but no {kind} path set",
                            c.port
                        )));
                        any_missing = true;
                    } else if !resolved.exists() {
                        report.push(CheckResult::fail(format!(
                            "connector on port {}: {kind} file `{}` does not exist",
                            c.port,
                            resolved.display()
                        )));
                        any_missing = true;
                    }
                }
            }
        }
    }

    if tls_connectors_seen == 0 {
        report.push(CheckResult::ok(
            "no TLS connectors configured (nothing to validate)",
        ));
    } else if !any_missing {
        report.push(CheckResult::ok(format!(
            "TLS material verified for {tls_connectors_seen} connector(s)"
        )));
    }
}

/// Returns `true` when *every* connector keeps the default request limits.
/// We treat that as the "no site override seen" trigger for the WARN row.
fn all_limits_are_default(config: &ServerConfig) -> bool {
    let defaults = RequestLimits::default();
    for svc in &config.services {
        for c in &svc.connectors {
            if !limits_equal(&c.limits, &defaults) {
                return false;
            }
        }
    }
    true
}

fn limits_equal(a: &RequestLimits, b: &RequestLimits) -> bool {
    a.max_header_count == b.max_header_count
        && a.max_header_size == b.max_header_size
        && a.max_parameter_count == b.max_parameter_count
        && a.max_post_size == b.max_post_size
        && a.max_part_count == b.max_part_count
        && a.max_uri_len == b.max_uri_len
        && a.request_timeout == b.request_timeout
        && a.keep_alive_timeout == b.keep_alive_timeout
}

fn check_request_limits_overridden(config: &ServerConfig, report: &mut PreflightReport) {
    let any_connector = config.services.iter().any(|s| !s.connectors.is_empty());
    if !any_connector {
        // Connector-count check will already have flagged this; stay quiet.
        return;
    }
    if all_limits_are_default(config) {
        report.push(CheckResult::warn(
            "every connector is using default RequestLimits — set realistic \
             maxHeaderCount / maxHttpHeaderSize / maxPostSize / connectionTimeout for your workload",
        ));
    } else {
        report.push(CheckResult::ok(
            "RequestLimits overridden from defaults on at least one connector",
        ));
    }
}

/// Inspect the raw XML for `<Manager ... allowRemote="true" ...>` without a
/// sibling `<Realm>`. The model doesn't carry the Manager element, so we
/// pattern-match the start tag and look for both attributes textually.
fn check_manager_api_exposure(raw_xml: &str, report: &mut PreflightReport) {
    // Mention any `<Manager ...>` start tag found in the configuration.
    let manager_tags = find_start_tags(raw_xml, "Manager");
    if manager_tags.is_empty() {
        report.push(CheckResult::ok(
            "Manager API uses defaults (loopback-only, auth required)",
        ));
        return;
    }

    // `allowRemote="true"` (case-insensitively) on any Manager element is
    // the trigger. A configured realm is identified by *any* `<Realm ...>`
    // tag anywhere in the document — that's a deliberate over-approximation:
    // we'd rather under-warn than miss a real `allowRemote=true, no realm`
    // combination.
    let allow_remote = manager_tags
        .iter()
        .any(|t| attr_eq_ignore_ascii_case(t, "allowRemote", "true"));
    let has_realm = !find_start_tags(raw_xml, "Realm").is_empty();

    if allow_remote && !has_realm {
        report.push(CheckResult::fail(
            "Manager API has allowRemote=\"true\" without any <Realm> configured; \
             this exposes deploy/undeploy/start/stop to the network with no authentication",
        ));
    } else if allow_remote {
        report.push(CheckResult::warn(
            "Manager API has allowRemote=\"true\"; make sure the configured <Realm> \
             grants the manager-script/manager-gui role correctly",
        ));
    } else {
        report.push(CheckResult::ok(
            "Manager API stays loopback-only (allowRemote not enabled)",
        ));
    }
}

fn check_ajp_secret(config: &ServerConfig, raw_xml: &str, report: &mut PreflightReport) {
    let ajp_count = config
        .services
        .iter()
        .flat_map(|s| s.connectors.iter())
        .filter(|c| c.protocol == Protocol::Ajp)
        .count();

    if ajp_count == 0 {
        report.push(CheckResult::ok("no AJP connectors configured"));
        return;
    }

    // For every <Connector ...> whose protocol attribute looks AJP-shaped,
    // require either `secret="..."` or `requiredSecret="..."` to be set to
    // a non-empty value.
    let mut ajp_with_secret = 0usize;
    let mut ajp_without_secret = 0usize;
    for tag in find_start_tags(raw_xml, "Connector") {
        let protocol = attr_value(&tag, "protocol").unwrap_or_default();
        if !protocol.to_ascii_lowercase().contains("ajp") {
            continue;
        }
        let secret = attr_value(&tag, "secret")
            .or_else(|| attr_value(&tag, "requiredSecret"))
            .unwrap_or_default();
        if secret.is_empty() {
            ajp_without_secret += 1;
        } else {
            ajp_with_secret += 1;
        }
    }

    if ajp_without_secret > 0 {
        report.push(CheckResult::fail(format!(
            "{ajp_without_secret} AJP connector(s) without a `secret=\"...\"` attribute; \
             AJP without a shared secret is the Ghostcat class of bug — set one or remove the connector"
        )));
    } else {
        report.push(CheckResult::ok(format!(
            "all {ajp_with_secret} AJP connector(s) have a secret configured"
        )));
    }
}

fn check_app_bases(config: &ServerConfig, config_path: &Path, report: &mut PreflightReport) {
    let mut any_bad = false;
    for svc in &config.services {
        for host in &svc.engine.hosts {
            let resolved = resolve_app_base(&host.app_base, config_path);
            match std::fs::metadata(&resolved) {
                Err(err) => {
                    report.push(CheckResult::fail(format!(
                        "host `{}` appBase `{}` is not readable: {err}",
                        host.name,
                        resolved.display()
                    )));
                    any_bad = true;
                }
                Ok(meta) if !meta.is_dir() => {
                    report.push(CheckResult::fail(format!(
                        "host `{}` appBase `{}` is not a directory",
                        host.name,
                        resolved.display()
                    )));
                    any_bad = true;
                }
                Ok(_) => {}
            }
        }
    }
    if !any_bad {
        report.push(CheckResult::ok(
            "every Host appBase exists and is a readable directory",
        ));
    }
}

// ---- Output ------------------------------------------------------------

fn print_report(report: &PreflightReport) {
    for row in &report.results {
        println!("{}: {}", row.status.label(), row.message);
    }
}

fn print_summary(report: &PreflightReport, strict: bool) {
    let fails = report.fail_count();
    let warns = report.warn_count();
    let oks = report
        .results
        .iter()
        .filter(|r| r.status == Status::Ok)
        .count();
    println!(
        "preflight: {oks} ok, {warns} warning(s), {fails} failure(s){}",
        if strict { " [strict]" } else { "" }
    );
}

// ---- XML-attribute helpers --------------------------------------------
//
// These are deliberately small string-scanning helpers, not a full XML
// parser. The structured parts of the config already went through the
// real parser (`ServerConfig::from_xml_str`) — these helpers only run
// for attributes we couldn't read off the typed model, and the goal is
// to be conservative: false positives on edge syntax are acceptable
// because they produce loud warnings rather than silent passes.

/// Find every `<Tag ...>` (or `<Tag ... />`) substring in `xml`. Returns
/// the *contents between the angle brackets* — i.e. the attribute string,
/// including the leading tag name — so callers can run [`attr_value`] on
/// the result.
fn find_start_tags(xml: &str, tag: &str) -> Vec<String> {
    let mut out = Vec::new();
    let needle = format!("<{tag}");
    let bytes = xml.as_bytes();
    let mut i = 0;
    while let Some(rel) = xml[i..].find(&needle) {
        let abs = i + rel;
        let after = abs + needle.len();
        // The next char must be whitespace, `>`, or `/` — otherwise this is
        // a longer tag name (e.g. `<ManagerThing>` when looking for `Manager`).
        let next = bytes.get(after).copied();
        if !matches!(
            next,
            Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') | Some(b'>') | Some(b'/')
        ) {
            i = after;
            continue;
        }
        // Find the matching closing `>` (ignoring `>` inside attribute values).
        let mut j = after;
        let mut in_quote: Option<u8> = None;
        while j < bytes.len() {
            let b = bytes[j];
            match in_quote {
                Some(q) if b == q => in_quote = None,
                None if b == b'"' || b == b'\'' => in_quote = Some(b),
                None if b == b'>' => break,
                _ => {}
            }
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }
        // Capture the inner attribute string (drops the leading `<` and the
        // trailing `>`/`/>`).
        let mut inner = &xml[abs + 1..j];
        if let Some(stripped) = inner.strip_suffix('/') {
            inner = stripped;
        }
        out.push(inner.trim().to_string());
        i = j + 1;
    }
    out
}

/// Extract the value of `attr` from a tag's attribute string, honouring
/// both `"..."` and `'...'` quoting. Returns `None` if the attribute is
/// absent or unquoted.
fn attr_value(tag: &str, attr: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = attr.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let tag_bytes = tag.as_bytes();
    let mut search_from = 0usize;
    while let Some(rel) = lower[search_from..].find(&needle) {
        let start = search_from + rel;
        // Must be preceded by whitespace or the start of the tag name
        // boundary, so we don't match `secretReq` when looking for `secret`.
        let prev_ok = start == 0
            || matches!(
                bytes.get(start - 1).copied(),
                Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n')
            );
        let after = start + needle.len();
        // Skip past optional whitespace then expect `=`.
        let mut k = after;
        while k < bytes.len() && matches!(bytes[k], b' ' | b'\t') {
            k += 1;
        }
        if !prev_ok || bytes.get(k).copied() != Some(b'=') {
            search_from = after;
            continue;
        }
        k += 1;
        while k < bytes.len() && matches!(bytes[k], b' ' | b'\t') {
            k += 1;
        }
        let quote = bytes.get(k).copied();
        if !matches!(quote, Some(b'"') | Some(b'\'')) {
            return None;
        }
        let q = quote.unwrap();
        k += 1;
        let value_start = k;
        while k < bytes.len() && bytes[k] != q {
            k += 1;
        }
        if k > bytes.len() {
            return None;
        }
        // Take the slice off the *original* (case-preserving) tag string.
        return Some(
            tag_bytes[value_start..k]
                .iter()
                .map(|&b| b as char)
                .collect(),
        );
    }
    None
}

/// Case-insensitive attribute-value equality check.
fn attr_eq_ignore_ascii_case(tag: &str, attr: &str, value: &str) -> bool {
    attr_value(tag, attr)
        .map(|v| v.eq_ignore_ascii_case(value))
        .unwrap_or(false)
}

/// Resolve `appBase` / TLS-material paths. Tomcat's convention is that
/// `server.xml` lives at `${catalina.base}/conf/server.xml`, and relative
/// paths inside it are resolved against `${catalina.base}` — i.e. the
/// **parent** of the config directory. We try that location first; if it
/// doesn't yield an existing path we fall back to the config dir itself
/// and finally the current working directory, so an operator running
/// `preflight` against a config in a non-standard layout still gets a
/// useful answer.
fn resolve_app_base(p: &Path, config_path: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let catalina_base = config_dir.parent().unwrap_or(config_dir);
    for candidate in [catalina_base.join(p), config_dir.join(p), PathBuf::from(p)] {
        if candidate.exists() {
            return candidate;
        }
    }
    // Nothing exists; report the catalina-base shape so the error message
    // points at the most likely intended location.
    catalina_base.join(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_start_tags_picks_up_simple_and_self_closing() {
        let xml = r#"<Server><Connector port="8080" />
            <Connector port="8443"/>
            <Manager allowRemote="true">
            </Manager></Server>"#;
        let connectors = find_start_tags(xml, "Connector");
        assert_eq!(connectors.len(), 2);
        let managers = find_start_tags(xml, "Manager");
        assert_eq!(managers.len(), 1);
    }

    #[test]
    fn attr_value_handles_both_quote_styles() {
        assert_eq!(
            attr_value(r#"Connector port="8080" protocol='AJP/1.3'"#, "protocol"),
            Some("AJP/1.3".to_string())
        );
        assert_eq!(
            attr_value("Connector port=\"8080\"", "port"),
            Some("8080".to_string())
        );
        assert!(attr_value("Connector port=\"8080\"", "secret").is_none());
    }

    #[test]
    fn limits_equal_treats_defaults_as_equal() {
        assert!(limits_equal(
            &RequestLimits::default(),
            &RequestLimits::default()
        ));
        let mut tweaked = RequestLimits::default();
        tweaked.max_header_count += 1;
        assert!(!limits_equal(&RequestLimits::default(), &tweaked));
    }

    #[test]
    fn exit_code_respects_strict() {
        let mut r = PreflightReport::default();
        r.push(CheckResult::warn("w"));
        assert_eq!(r.exit_code(false), 0);
        assert_eq!(r.exit_code(true), 1);
        r.push(CheckResult::fail("f"));
        assert_eq!(r.exit_code(false), 1);
        assert_eq!(r.exit_code(true), 1);
    }
}
