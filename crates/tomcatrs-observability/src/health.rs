//! Health checks and an HTTP adapter that exposes them.
//!
//! The shape follows the [IETF "health-check" draft][rfc-draft]:
//!
//! ```json
//! {
//!   "status": "pass",
//!   "checks": {
//!     "uptime": { "status": "pass" },
//!     "db":     { "status": "warn", "output": "slow" }
//!   }
//! }
//! ```
//!
//! Aggregation is the obvious worst-wins reduction: any `Fail` makes the
//! whole report `Fail`, otherwise any `Warn` makes it `Warn`, otherwise `Pass`.
//!
//! [`HealthAdapter`] wraps a [`HealthRegistry`] and implements
//! [`tomcatrs_coyote::Adapter`] so the same connector code that serves an
//! application can serve `/health`, `/health/live`, and `/health/ready`.
//!
//! [rfc-draft]: https://datatracker.ietf.org/doc/html/draft-inadarei-api-health-check

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use serde_json::{json, Value};
use tomcatrs_core::runtime::Runtime;
use tomcatrs_coyote::{Adapter, Request, Response};

/// The three states a single check (or aggregate) can be in.
///
/// `Warn` and `Fail` carry a free-text reason that is surfaced as `"output"`
/// in the rendered JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthStatus {
    /// All good.
    Pass,
    /// Degraded but still functional; includes an explanation.
    Warn(String),
    /// Broken; includes an explanation.
    Fail(String),
}

impl HealthStatus {
    /// The IETF-spec status string (`"pass" | "warn" | "fail"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            HealthStatus::Pass => "pass",
            HealthStatus::Warn(_) => "warn",
            HealthStatus::Fail(_) => "fail",
        }
    }

    /// The optional free-text reason, if any.
    pub fn output(&self) -> Option<&str> {
        match self {
            HealthStatus::Pass => None,
            HealthStatus::Warn(s) | HealthStatus::Fail(s) => Some(s.as_str()),
        }
    }

    fn to_json(&self) -> Value {
        match self.output() {
            Some(out) => json!({ "status": self.as_str(), "output": out }),
            None => json!({ "status": self.as_str() }),
        }
    }
}

/// A single named check.
#[async_trait]
pub trait HealthCheck: Send + Sync {
    /// Run the check. The future should be cheap; expensive checks should
    /// timebox themselves internally.
    async fn check(&self) -> HealthStatus;
}

/// One entry in the rendered report — a check's name paired with its status.
#[derive(Debug, Clone)]
pub struct NamedStatus {
    /// The check's registration name.
    pub name: String,
    /// The result of the most recent invocation.
    pub status: HealthStatus,
}

/// The aggregated outcome of every registered check.
#[derive(Debug, Clone)]
pub struct HealthReport {
    /// Worst-wins aggregate over `checks`.
    pub status: HealthStatus,
    /// All checks, in registration order.
    pub checks: Vec<NamedStatus>,
}

impl HealthReport {
    /// Render the report into IETF-style JSON.
    pub fn to_json(&self) -> String {
        let mut checks_map = serde_json::Map::new();
        for c in &self.checks {
            checks_map.insert(c.name.clone(), c.status.to_json());
        }
        let doc = json!({
            "status": self.status.as_str(),
            "checks": Value::Object(checks_map),
        });
        // Include the aggregate output if any so degraded reports have a
        // human-readable hint at the top level.
        let mut doc_obj = doc.as_object().cloned().unwrap_or_default();
        if let Some(out) = self.status.output() {
            doc_obj.insert("output".to_string(), Value::String(out.to_string()));
        }
        serde_json::to_string(&Value::Object(doc_obj)).unwrap_or_else(|_| "{}".to_string())
    }
}

/// A list of named health checks.
#[derive(Default, Clone)]
pub struct HealthRegistry {
    checks: Vec<(String, Arc<dyn HealthCheck>)>,
}

impl std::fmt::Debug for HealthRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HealthRegistry")
            .field(
                "checks",
                &self
                    .checks
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl HealthRegistry {
    /// Construct an empty registry.
    pub fn new() -> Self {
        HealthRegistry { checks: Vec::new() }
    }

    /// Append a check.
    pub fn register(&mut self, name: impl Into<String>, check: Arc<dyn HealthCheck>) -> &mut Self {
        self.checks.push((name.into(), check));
        self
    }

    /// Number of registered checks.
    pub fn len(&self) -> usize {
        self.checks.len()
    }

    /// `true` if no checks are registered.
    pub fn is_empty(&self) -> bool {
        self.checks.is_empty()
    }

    /// Run every check and aggregate the results.
    ///
    /// Aggregation rules: any `Fail` ⇒ `Fail`, else any `Warn` ⇒ `Warn`,
    /// else `Pass`. With no registered checks the aggregate is `Pass`.
    pub async fn aggregate(&self) -> HealthReport {
        let mut results: Vec<NamedStatus> = Vec::with_capacity(self.checks.len());
        for (name, check) in &self.checks {
            let status = check.check().await;
            results.push(NamedStatus {
                name: name.clone(),
                status,
            });
        }

        let mut overall = HealthStatus::Pass;
        let mut fail_msgs: Vec<String> = Vec::new();
        let mut warn_msgs: Vec<String> = Vec::new();
        for r in &results {
            match &r.status {
                HealthStatus::Fail(m) => {
                    fail_msgs.push(format!("{}: {}", r.name, m));
                }
                HealthStatus::Warn(m) => {
                    warn_msgs.push(format!("{}: {}", r.name, m));
                }
                HealthStatus::Pass => {}
            }
        }
        if !fail_msgs.is_empty() {
            overall = HealthStatus::Fail(fail_msgs.join("; "));
        } else if !warn_msgs.is_empty() {
            overall = HealthStatus::Warn(warn_msgs.join("; "));
        }

        HealthReport {
            status: overall,
            checks: results,
        }
    }
}

/// Adapter that serves `/health`, `/health/live`, and `/health/ready`.
///
/// * `GET /health` and `GET /health/ready` run the registry and produce a JSON
///   body. Status code is `200` for `Pass`/`Warn` and `503` for `Fail`.
/// * `GET /health/live` always returns `200` with `{"status":"pass"}`.
/// * Anything else is a `404`.
pub struct HealthAdapter {
    registry: Arc<HealthRegistry>,
}

impl HealthAdapter {
    /// Wrap an [`Arc`]-shared registry.
    pub fn new(registry: Arc<HealthRegistry>) -> Self {
        HealthAdapter { registry }
    }

    fn json_response(status_code: u16, body: String) -> Response {
        let mut resp = Response::with_body(status_code, Bytes::from(body));
        resp.set_header("Content-Type", "application/json");
        resp
    }
}

#[async_trait]
impl Adapter for HealthAdapter {
    async fn service(&self, req: Request) -> Response {
        if req.method != "GET" {
            return Self::json_response(
                404,
                r#"{"status":"fail","output":"not found"}"#.to_string(),
            );
        }
        match req.path.as_str() {
            "/health" | "/health/ready" => {
                let report = self.registry.aggregate().await;
                let code = match report.status {
                    HealthStatus::Fail(_) => 503,
                    HealthStatus::Pass | HealthStatus::Warn(_) => 200,
                };
                Self::json_response(code, report.to_json())
            }
            "/health/live" => Self::json_response(200, r#"{"status":"pass"}"#.to_string()),
            _ => Self::json_response(404, r#"{"status":"fail","output":"not found"}"#.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Built-in checks.
// ---------------------------------------------------------------------------

/// A check that always returns `Pass`. Handy in tests and as a sanity probe.
#[derive(Debug, Default, Clone, Copy)]
pub struct AlwaysPassCheck;

#[async_trait]
impl HealthCheck for AlwaysPassCheck {
    async fn check(&self) -> HealthStatus {
        HealthStatus::Pass
    }
}

/// Passes if the runtime reports a non-zero uptime — i.e. the process is
/// actually past startup.
#[derive(Clone)]
pub struct UptimeCheck {
    runtime: Runtime,
}

impl UptimeCheck {
    /// Build the check around `runtime`.
    pub fn new(runtime: Runtime) -> Self {
        UptimeCheck { runtime }
    }
}

#[async_trait]
impl HealthCheck for UptimeCheck {
    async fn check(&self) -> HealthStatus {
        if self.runtime.uptime() > std::time::Duration::ZERO {
            HealthStatus::Pass
        } else {
            HealthStatus::Warn("runtime just started".to_string())
        }
    }
}

/// Placeholder memory-health check.
///
/// A production implementation would inspect `/proc/self/status` (Linux) or
/// `task_info` (macOS); here we just return `Pass` since the runtime has no
/// portable RSS source. The struct exists so callers can wire it up today and
/// gain real behavior in a later release without changing their code.
#[derive(Debug, Default, Clone, Copy)]
pub struct MemoryHealthCheck;

#[async_trait]
impl HealthCheck for MemoryHealthCheck {
    async fn check(&self) -> HealthStatus {
        HealthStatus::Pass
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct WarnCheck(&'static str);
    #[async_trait]
    impl HealthCheck for WarnCheck {
        async fn check(&self) -> HealthStatus {
            HealthStatus::Warn(self.0.to_string())
        }
    }

    struct FailCheck(&'static str);
    #[async_trait]
    impl HealthCheck for FailCheck {
        async fn check(&self) -> HealthStatus {
            HealthStatus::Fail(self.0.to_string())
        }
    }

    fn req(method: &str, path: &str) -> Request {
        Request {
            method: method.to_string(),
            uri: path.to_string(),
            path: path.to_string(),
            query: None,
            version: "HTTP/1.1".to_string(),
            headers: Vec::new(),
            body: Bytes::new(),
            peer_addr: "127.0.0.1:0".parse().unwrap(),
        }
    }

    #[tokio::test]
    async fn aggregate_pass_when_all_pass() {
        let mut reg = HealthRegistry::new();
        reg.register("a", Arc::new(AlwaysPassCheck))
            .register("b", Arc::new(AlwaysPassCheck));
        let report = reg.aggregate().await;
        assert_eq!(report.status, HealthStatus::Pass);
        assert_eq!(report.checks.len(), 2);
    }

    #[tokio::test]
    async fn aggregate_warn_when_any_warn_but_no_fail() {
        let mut reg = HealthRegistry::new();
        reg.register("ok", Arc::new(AlwaysPassCheck))
            .register("slow", Arc::new(WarnCheck("latency high")));
        let report = reg.aggregate().await;
        assert!(matches!(report.status, HealthStatus::Warn(_)));
        let body = report.to_json();
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["status"], "warn");
        assert_eq!(parsed["checks"]["slow"]["status"], "warn");
        assert_eq!(parsed["checks"]["slow"]["output"], "latency high");
        assert_eq!(parsed["checks"]["ok"]["status"], "pass");
    }

    #[tokio::test]
    async fn aggregate_fail_wins_over_warn() {
        let mut reg = HealthRegistry::new();
        reg.register("ok", Arc::new(AlwaysPassCheck))
            .register("slow", Arc::new(WarnCheck("warn")))
            .register("broken", Arc::new(FailCheck("kaboom")));
        let report = reg.aggregate().await;
        assert!(matches!(report.status, HealthStatus::Fail(_)));
        let parsed: Value = serde_json::from_str(&report.to_json()).unwrap();
        assert_eq!(parsed["status"], "fail");
        assert_eq!(parsed["checks"]["broken"]["status"], "fail");
        assert_eq!(parsed["checks"]["broken"]["output"], "kaboom");
        assert!(parsed["output"].as_str().unwrap().contains("kaboom"));
    }

    #[tokio::test]
    async fn aggregate_empty_registry_is_pass() {
        let reg = HealthRegistry::new();
        let report = reg.aggregate().await;
        assert_eq!(report.status, HealthStatus::Pass);
        let parsed: Value = serde_json::from_str(&report.to_json()).unwrap();
        assert_eq!(parsed["status"], "pass");
        assert!(parsed["checks"].as_object().unwrap().is_empty());
    }

    #[tokio::test]
    async fn uptime_check_passes_after_construction() {
        let rt = Runtime::new();
        // Give the runtime a moment so `uptime()` is strictly positive on all
        // platforms with low-resolution clocks.
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        let status = UptimeCheck::new(rt).check().await;
        assert_eq!(status, HealthStatus::Pass);
    }

    #[tokio::test]
    async fn memory_check_passes() {
        assert_eq!(MemoryHealthCheck.check().await, HealthStatus::Pass);
    }

    #[tokio::test]
    async fn adapter_returns_200_when_pass() {
        let mut reg = HealthRegistry::new();
        reg.register("ok", Arc::new(AlwaysPassCheck));
        let adapter = HealthAdapter::new(Arc::new(reg));
        let resp = adapter.service(req("GET", "/health")).await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.header("Content-Type"), Some("application/json"));
        let parsed: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(parsed["status"], "pass");
    }

    #[tokio::test]
    async fn adapter_returns_503_when_any_fail() {
        let mut reg = HealthRegistry::new();
        reg.register("ok", Arc::new(AlwaysPassCheck))
            .register("broken", Arc::new(FailCheck("boom")));
        let adapter = HealthAdapter::new(Arc::new(reg));
        let resp = adapter.service(req("GET", "/health")).await;
        assert_eq!(resp.status, 503);
        let parsed: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(parsed["status"], "fail");
        assert_eq!(parsed["checks"]["broken"]["status"], "fail");
    }

    #[tokio::test]
    async fn adapter_live_always_200() {
        // Even when the registry would fail, `/health/live` ignores it.
        let mut reg = HealthRegistry::new();
        reg.register("broken", Arc::new(FailCheck("x")));
        let adapter = HealthAdapter::new(Arc::new(reg));
        let resp = adapter.service(req("GET", "/health/live")).await;
        assert_eq!(resp.status, 200);
        let parsed: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(parsed["status"], "pass");
    }

    #[tokio::test]
    async fn adapter_ready_mirrors_aggregate() {
        let mut reg = HealthRegistry::new();
        reg.register("ok", Arc::new(AlwaysPassCheck));
        let adapter = HealthAdapter::new(Arc::new(reg));
        let resp = adapter.service(req("GET", "/health/ready")).await;
        assert_eq!(resp.status, 200);
    }

    #[tokio::test]
    async fn adapter_returns_404_for_unknown_path() {
        let reg = HealthRegistry::new();
        let adapter = HealthAdapter::new(Arc::new(reg));
        let resp = adapter.service(req("GET", "/nope")).await;
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn adapter_returns_404_for_non_get() {
        let reg = HealthRegistry::new();
        let adapter = HealthAdapter::new(Arc::new(reg));
        let resp = adapter.service(req("POST", "/health")).await;
        assert_eq!(resp.status, 404);
    }
}
