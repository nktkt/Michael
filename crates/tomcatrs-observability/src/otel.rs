//! OpenTelemetry export for the Tomcat-RS metrics registry.
//!
//! This module renders the contents of a [`crate::metrics::MetricsRegistry`]
//! into the [OTLP/HTTP] protobuf-JSON shape:
//!
//! ```json
//! {
//!   "resourceMetrics": [{
//!     "resource": { "attributes": [...] },
//!     "scopeMetrics": [{
//!       "scope": { "name": "tomcatrs-observability" },
//!       "metrics": [
//!         { "name": "...", "sum":   { "dataPoints": [...] } },
//!         { "name": "...", "gauge": { "dataPoints": [...] } }
//!       ]
//!     }]
//!   }]
//! }
//! ```
//!
//! [`OtelMetricsExporter::to_otlp_json`] produces that document; sending it on
//! to a collector is the caller's job. As a convenience,
//! [`OtelMetricsExporter::post_to_collector`] performs a hand-rolled HTTP/1.1
//! `POST` over a `tokio::net::TcpStream` so no extra HTTP client crate is
//! required. When the configured endpoint is `None` the document is logged via
//! `tracing` instead, which is useful in tests and during local development.
//!
//! [OTLP/HTTP]: https://opentelemetry.io/docs/specs/otlp/#otlphttp

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::interval;

use crate::metrics::MetricsRegistry;

/// Static-from-construction configuration for the OTLP exporter.
#[derive(Debug, Clone)]
pub struct OtelConfig {
    /// Value of the `service.name` resource attribute.
    pub service_name: String,
    /// OTLP/HTTP collector endpoint, e.g. `http://localhost:4318/v1/metrics`.
    /// When `None` the exporter renders to JSON and logs via `tracing` rather
    /// than performing any network I/O.
    pub endpoint: Option<String>,
    /// Additional `Resource` attributes — appended to the synthesized
    /// `service.name` attribute.
    pub resource_attributes: Vec<(String, String)>,
}

impl OtelConfig {
    /// Convenience constructor that names the service and leaves the endpoint
    /// unset (i.e. log-to-tracing mode).
    pub fn new(service_name: impl Into<String>) -> Self {
        OtelConfig {
            service_name: service_name.into(),
            endpoint: None,
            resource_attributes: Vec::new(),
        }
    }

    /// Set the OTLP/HTTP collector endpoint.
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Append a single resource attribute.
    pub fn with_attribute(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.resource_attributes.push((key.into(), value.into()));
        self
    }
}

/// Renders a [`MetricsRegistry`] into the OTLP/HTTP JSON shape and (optionally)
/// flushes it to a collector.
pub struct OtelMetricsExporter {
    config: OtelConfig,
    registry: Arc<MetricsRegistry>,
}

impl OtelMetricsExporter {
    /// Build a new exporter binding `config` to `registry`.
    pub fn new(config: OtelConfig, registry: Arc<MetricsRegistry>) -> Self {
        OtelMetricsExporter { config, registry }
    }

    /// The configuration this exporter was created with.
    pub fn config(&self) -> &OtelConfig {
        &self.config
    }

    /// Render the registry to the OTLP/HTTP protobuf-JSON document.
    pub fn to_otlp_json(&self) -> String {
        let now_ns: u64 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);

        let mut attributes: Vec<Value> = Vec::new();
        attributes.push(attr("service.name", &self.config.service_name));
        for (k, v) in &self.config.resource_attributes {
            attributes.push(attr(k, v));
        }

        // Collect a sorted snapshot of the registry so the output is
        // deterministic. We piggy-back on the existing Prometheus renderer's
        // structure: it groups `# TYPE` lines with their samples, and we just
        // parse those into OTLP shapes. This avoids exposing the private
        // `Metric` enum.
        let prom = self.registry.render_prometheus();
        let mut metrics_json: Vec<Value> = Vec::new();
        let mut lines = prom.lines();
        while let Some(type_line) = lines.next() {
            // Lines come in pairs: "# TYPE <name> <kind>" then "<name> <value>".
            let Some(type_tail) = type_line.strip_prefix("# TYPE ") else {
                continue;
            };
            let mut parts = type_tail.split_whitespace();
            let name = parts.next().unwrap_or("").to_string();
            let kind = parts.next().unwrap_or("").to_string();
            let Some(sample_line) = lines.next() else {
                continue;
            };
            let mut sample_parts = sample_line.splitn(2, ' ');
            let _name_again = sample_parts.next();
            let value_str = sample_parts.next().unwrap_or("0");

            let data_point = match kind.as_str() {
                "counter" => {
                    let v: u64 = value_str.parse().unwrap_or(0);
                    json!({
                        "startTimeUnixNano": now_ns.to_string(),
                        "timeUnixNano": now_ns.to_string(),
                        "asInt": v.to_string(),
                    })
                }
                "gauge" => {
                    let v: i64 = value_str.parse().unwrap_or(0);
                    json!({
                        "timeUnixNano": now_ns.to_string(),
                        "asInt": v.to_string(),
                    })
                }
                _ => continue,
            };

            let metric_obj = match kind.as_str() {
                "counter" => json!({
                    "name": name,
                    "sum": {
                        "dataPoints": [data_point],
                        // OTel `Sum.aggregationTemporality`: 2 = CUMULATIVE.
                        "aggregationTemporality": 2,
                        "isMonotonic": true,
                    }
                }),
                "gauge" => json!({
                    "name": name,
                    "gauge": {
                        "dataPoints": [data_point],
                    }
                }),
                _ => continue,
            };
            metrics_json.push(metric_obj);
        }

        let doc = json!({
            "resourceMetrics": [{
                "resource": { "attributes": attributes },
                "scopeMetrics": [{
                    "scope": { "name": "tomcatrs-observability" },
                    "metrics": metrics_json,
                }],
            }]
        });
        serde_json::to_string(&doc).unwrap_or_else(|_| "{}".to_string())
    }

    /// Render and dispatch one batch. When no endpoint is configured the JSON
    /// is logged via `tracing::debug!` and `Ok(())` is returned.
    pub async fn export_once(&self) -> std::io::Result<()> {
        let body = self.to_otlp_json();
        match &self.config.endpoint {
            Some(ep) => self.post_to_collector(ep, &body).await,
            None => {
                tracing::debug!(target: "tomcatrs::otel", payload = %body, "otel export (stdout)");
                Ok(())
            }
        }
    }

    /// Hand-rolled HTTP/1.1 `POST` of `body` to `endpoint`. Only `http://`
    /// URLs are supported; for `https://` you would normally swap in `rustls`.
    pub async fn post_to_collector(&self, endpoint: &str, body: &str) -> std::io::Result<()> {
        let (host, port, path) = parse_http_url(endpoint)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let mut stream = TcpStream::connect((host.as_str(), port)).await?;
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
             Content-Length: {len}\r\nConnection: close\r\n\r\n{body}",
            len = body.len()
        );
        stream.write_all(req.as_bytes()).await?;
        stream.flush().await?;
        // Drain the response so the server can close cleanly; we don't parse
        // it because OTLP collectors return small acks we don't act on.
        let mut sink = Vec::with_capacity(256);
        let _ = stream.read_to_end(&mut sink).await;
        Ok(())
    }
}

/// Spawn a background task that flushes `exporter` at `flush_interval`.
///
/// Returns the [`OtelMetricsExporter`] (wrapped in `Arc`) the caller installed
/// — the spawned task only borrows it.
pub fn init_otel(
    config: OtelConfig,
    registry: Arc<MetricsRegistry>,
    flush_interval: Duration,
) -> Arc<OtelMetricsExporter> {
    let exporter = Arc::new(OtelMetricsExporter::new(config, registry));
    let bg = exporter.clone();
    tokio::spawn(async move {
        let mut ticker = interval(flush_interval);
        // The first tick fires immediately; skip it so we don't double-export
        // on startup.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(err) = bg.export_once().await {
                tracing::warn!(target: "tomcatrs::otel", %err, "otel export failed");
            }
        }
    });
    exporter
}

/// Build a single OTel `KeyValue` JSON object.
fn attr(key: &str, value: &str) -> Value {
    json!({
        "key": key,
        "value": { "stringValue": value },
    })
}

/// Parse an `http://host[:port]/path` URL into its components. Returns
/// `(host, port, path)` with sensible defaults (`port=80`, `path="/"`).
fn parse_http_url(url: &str) -> Result<(String, u16, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("only http:// endpoints are supported, got {url:?}"))?;
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| format!("invalid port in {url:?}"))?,
        ),
        None => (authority.to_string(), 80u16),
    };
    if host.is_empty() {
        return Err(format!("missing host in {url:?}"));
    }
    Ok((host, port, path.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_builders_compose() {
        let cfg = OtelConfig::new("svc")
            .with_endpoint("http://localhost:4318/v1/metrics")
            .with_attribute("deployment.environment", "test");
        assert_eq!(cfg.service_name, "svc");
        assert_eq!(
            cfg.endpoint.as_deref(),
            Some("http://localhost:4318/v1/metrics")
        );
        assert_eq!(cfg.resource_attributes.len(), 1);
    }

    #[test]
    fn to_otlp_json_renders_counter_and_gauge_values() {
        let reg = Arc::new(MetricsRegistry::new());
        reg.counter("http_requests_total").add(42);
        reg.gauge("active_connections").set(7);

        let exporter = OtelMetricsExporter::new(
            OtelConfig::new("test-service").with_attribute("env", "ci"),
            reg.clone(),
        );
        let json_text = exporter.to_otlp_json();
        let parsed: Value = serde_json::from_str(&json_text).expect("OTLP JSON should parse");

        // Resource attributes carry service.name + extras.
        let attrs = &parsed["resourceMetrics"][0]["resource"]["attributes"];
        let attr_arr = attrs.as_array().expect("attributes array");
        let has_service = attr_arr
            .iter()
            .any(|a| a["key"] == "service.name" && a["value"]["stringValue"] == "test-service");
        let has_env = attr_arr
            .iter()
            .any(|a| a["key"] == "env" && a["value"]["stringValue"] == "ci");
        assert!(has_service && has_env, "got {attrs}");

        // Locate the counter and gauge entries.
        let metrics = parsed["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .expect("metrics array");
        let counter = metrics
            .iter()
            .find(|m| m["name"] == "http_requests_total")
            .expect("counter present");
        assert_eq!(
            counter["sum"]["dataPoints"][0]["asInt"], "42",
            "counter value should be 42, got {counter}"
        );
        assert_eq!(counter["sum"]["isMonotonic"], true);

        let gauge = metrics
            .iter()
            .find(|m| m["name"] == "active_connections")
            .expect("gauge present");
        assert_eq!(
            gauge["gauge"]["dataPoints"][0]["asInt"], "7",
            "gauge value should be 7, got {gauge}"
        );
    }

    #[test]
    fn to_otlp_json_handles_empty_registry() {
        let reg = Arc::new(MetricsRegistry::new());
        let exporter = OtelMetricsExporter::new(OtelConfig::new("svc"), reg);
        let parsed: Value = serde_json::from_str(&exporter.to_otlp_json()).unwrap();
        let metrics = parsed["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .unwrap();
        assert!(metrics.is_empty());
    }

    #[test]
    fn parse_http_url_extracts_components() {
        assert_eq!(
            parse_http_url("http://localhost:4318/v1/metrics").unwrap(),
            ("localhost".to_string(), 4318, "/v1/metrics".to_string())
        );
        assert_eq!(
            parse_http_url("http://collector").unwrap(),
            ("collector".to_string(), 80, "/".to_string())
        );
        assert!(parse_http_url("https://x").is_err());
        assert!(parse_http_url("http://:80/x").is_err());
    }

    #[tokio::test]
    async fn export_once_without_endpoint_is_a_noop() {
        let reg = Arc::new(MetricsRegistry::new());
        reg.counter("c").inc();
        let exporter = OtelMetricsExporter::new(OtelConfig::new("svc"), reg);
        exporter.export_once().await.expect("noop export");
    }
}
