//! `tomcatrs-observability` — logging, access logs, and metrics for the
//! **Tomcat-RS Compatibility Runtime**.
//!
//! Apache Tomcat scatters its observability across `AccessLogValve`,
//! `juli` logging, and JMX MBeans. This crate gathers the Rust-side
//! equivalents into one place:
//!
//! * [`access_log`] — Apache/Tomcat-style request access logging
//!   (`Common` and `Combined` log formats).
//! * [`metrics`] — a lightweight, atomic-backed counter/gauge registry that
//!   renders to the Prometheus text exposition format.
//! * [`tracing`] — an idempotent initializer for the `tracing` ecosystem.
//! * [`jmx_bridge`] — a documented scaffold for surfacing Rust metrics to the
//!   JVM as JMX MBeans.

pub mod access_log;
pub mod health;
pub mod jmx_bridge;
pub mod metrics;
pub mod otel;
pub mod tracing;

pub use access_log::{AccessLog, AccessLogEntry, AccessLogFormat};
pub use health::{HealthAdapter, HealthCheck, HealthRegistry, HealthReport, HealthStatus};
pub use jmx_bridge::{JmxBridge, MBeanAttribute, MBeanDescriptor};
pub use metrics::{Counter, Gauge, MetricsRegistry};
pub use otel::{OtelConfig, OtelMetricsExporter};
pub use tracing::init_tracing;
