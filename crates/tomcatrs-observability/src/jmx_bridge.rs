//! Surfacing Rust-side metrics to the JVM as JMX MBeans.
//!
//! # Why a bridge?
//!
//! Apache Tomcat publishes virtually all of its runtime state through **JMX**
//! MBeans (`Catalina:type=ThreadPool,name=...`, `java.lang:type=Memory`, …),
//! and existing monitoring tooling — JConsole, VisualVM, the Tomcat Manager
//! webapp, countless APM agents — speaks JMX fluently.
//!
//! As subsystems migrate to Rust, their metrics live in a
//! [`crate::MetricsRegistry`] instead of a Java object. To keep the existing
//! tooling working, the JVM side registers a thin "proxy" MBean per Rust
//! metric whose attribute reads call back across JNI into the registry.
//!
//! ```text
//!   Rust MetricsRegistry ──snapshot──▶ MBeanDescriptor list
//!                                          │
//!                              JmxBridge::register_with_jvm
//!                                          ▼
//!                                  javax.management.MBeanServer
//! ```
//!
//! # Layers
//!
//! Two pieces live in this module:
//!
//! * The **data path** — [`MBeanDescriptor`], [`MBeanAttribute`] and
//!   [`JmxBridge::mbean_snapshot`] — describes what *would* be exposed and is
//!   fully testable without a JVM.
//! * The **JNI path** — [`JmxBridge::register_with_jvm`] — actually talks to
//!   `javax.management.ManagementFactory.getPlatformMBeanServer()` and
//!   registers one `DynamicMBean` per metric. This path is gated behind the
//!   `jvm` cargo feature so the crate continues to build on hosts without a
//!   JDK / `libjvm`. With the feature off, [`register_with_jvm`] returns a
//!   descriptive `tomcatrs_core::Error::Other` rather than silently doing
//!   nothing.
//!
//! [`register_with_jvm`]: JmxBridge::register_with_jvm

use crate::metrics::MetricsRegistry;

/// The JMX type of a single MBean attribute, mirroring the Rust metric kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MBeanAttributeKind {
    /// A monotonically increasing counter — maps to a `long` attribute.
    Counter,
    /// A gauge — maps to a `long` attribute that may rise and fall.
    Gauge,
}

impl MBeanAttributeKind {
    /// The JMX/Java type name reported for this attribute. JMX uses Java
    /// type strings (`"long"`, `"double"`, …); both metric kinds surface as
    /// `long` because their underlying atomics are integer-typed.
    pub fn java_type(self) -> &'static str {
        match self {
            MBeanAttributeKind::Counter | MBeanAttributeKind::Gauge => "long",
        }
    }
}

/// Description of one attribute exposed on an MBean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MBeanAttribute {
    /// The attribute name as it will appear in JConsole, e.g. `requestCount`.
    pub name: String,
    /// Whether the attribute is counter- or gauge-typed.
    pub kind: MBeanAttributeKind,
    /// Human-readable description, surfaced as the JMX attribute description.
    pub description: String,
}

/// Description of a single MBean to be registered on the JVM `MBeanServer`.
///
/// A descriptor is a plain, JNI-friendly data structure: the JVM side reads it
/// once at registration time to build a `javax.management.ObjectName` and a
/// dynamic MBean whose getters call back into Rust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MBeanDescriptor {
    /// The JMX `ObjectName` string, e.g.
    /// `TomcatRS:type=Connector,name="http-nio-8080"`.
    pub object_name: String,
    /// The attributes this MBean exposes.
    pub attributes: Vec<MBeanAttribute>,
}

impl MBeanDescriptor {
    /// Begin a descriptor for the given JMX `ObjectName`, with no attributes.
    pub fn new(object_name: impl Into<String>) -> Self {
        MBeanDescriptor {
            object_name: object_name.into(),
            attributes: Vec::new(),
        }
    }

    /// Add an attribute description, returning `self` for chaining.
    pub fn with_attribute(
        mut self,
        name: impl Into<String>,
        kind: MBeanAttributeKind,
        description: impl Into<String>,
    ) -> Self {
        self.attributes.push(MBeanAttribute {
            name: name.into(),
            kind,
            description: description.into(),
        });
        self
    }
}

/// The Rust end of the JMX bridge.
///
/// Holds the set of [`MBeanDescriptor`]s that *should* be exposed on the JVM
/// `MBeanServer`, alongside a reference to the [`MetricsRegistry`] their
/// attributes ultimately read from.
pub struct JmxBridge<'reg> {
    registry: &'reg MetricsRegistry,
    descriptors: Vec<MBeanDescriptor>,
}

impl<'reg> JmxBridge<'reg> {
    /// Create a bridge over the given metrics registry, with no MBeans yet
    /// described.
    pub fn new(registry: &'reg MetricsRegistry) -> Self {
        JmxBridge {
            registry,
            descriptors: Vec::new(),
        }
    }

    /// Queue an MBean descriptor for eventual registration.
    pub fn describe(&mut self, descriptor: MBeanDescriptor) {
        self.descriptors.push(descriptor);
    }

    /// The MBean descriptors collected so far.
    pub fn descriptors(&self) -> &[MBeanDescriptor] {
        &self.descriptors
    }

    /// The metrics registry whose values back these MBeans' attributes.
    pub fn registry(&self) -> &MetricsRegistry {
        self.registry
    }

    /// A flat snapshot of every metric the JVM-side `DynamicMBean` would read.
    ///
    /// The result is `(metric_name, value_as_f64)`. Counters and gauges both
    /// project losslessly into `f64` for the metric magnitudes in scope —
    /// session counts, connection counts, request counts in the billions — so
    /// a single signature handles both kinds.
    ///
    /// The snapshot is taken from [`MetricsRegistry::render_prometheus`] so
    /// its semantics stay in lock-step with the metrics module without
    /// reaching into its internals. Output is sorted by metric name (inherited
    /// from `render_prometheus`).
    pub fn mbean_snapshot(&self) -> Vec<(String, f64)> {
        parse_prometheus_snapshot(&self.registry.render_prometheus())
    }

    /// Register the metrics surface on the JVM platform `MBeanServer`.
    ///
    /// With the `jvm` cargo feature **off** this is a clear, actionable error
    /// rather than a silent no-op — callers can branch on it to fall back to
    /// the pure-Rust Prometheus exposition.
    #[cfg(not(feature = "jvm"))]
    pub fn register_with_jvm(&self) -> tomcatrs_core::Result<()> {
        Err(tomcatrs_core::Error::Other(
            "JMX bridge requires the `jvm` cargo feature".to_string(),
        ))
    }

    /// Register the metrics surface on the JVM platform `MBeanServer`.
    ///
    /// For each metric in the registry, this registers exactly one
    /// `org.apache.tomcatrs.jmx.TomcatRsMetricsMBean` instance under the
    /// object name `TomcatRS:type=Metric,name=<metric>`. The Java side is a
    /// `DynamicMBean` whose attribute getters invoke a native method back
    /// into Rust which reads from this bridge's [`MetricsRegistry`].
    ///
    /// # JNI contract
    ///
    /// * Class: `org.apache.tomcatrs.jmx.TomcatRsMetricsMBean`
    ///   * `<init>(Ljava/lang/String;)V` — metric name as a Java `String`.
    /// * `javax.management.ManagementFactory.getPlatformMBeanServer()`
    /// * `javax.management.MBeanServer.registerMBean(Object, ObjectName)`
    ///
    /// The Rust-side native callback (responsible for answering
    /// `getAttribute`) reads the metric value via [`mbean_snapshot`] and is
    /// registered separately by the embedding application.
    ///
    /// [`mbean_snapshot`]: JmxBridge::mbean_snapshot
    #[cfg(feature = "jvm")]
    pub fn register_with_jvm(&self, env: &mut jni::JNIEnv) -> tomcatrs_core::Result<()> {
        use jni::objects::JValue;

        let map_err = |e: jni::errors::Error| {
            tomcatrs_core::Error::Other(format!("jmx bridge: jni error: {e}"))
        };

        // 1) MBeanServer mbs = ManagementFactory.getPlatformMBeanServer();
        let mgmt_factory = env
            .find_class("java/lang/management/ManagementFactory")
            .map_err(map_err)?;
        let mbean_server = env
            .call_static_method(
                &mgmt_factory,
                "getPlatformMBeanServer",
                "()Ljavax/management/MBeanServer;",
                &[],
            )
            .map_err(map_err)?
            .l()
            .map_err(map_err)?;

        // 2) For each metric, build a TomcatRsMetricsMBean(name) and an
        //    ObjectName, then mbs.registerMBean(obj, name).
        let mbean_class = env
            .find_class("org/apache/tomcatrs/jmx/TomcatRsMetricsMBean")
            .map_err(map_err)?;
        let object_name_class = env
            .find_class("javax/management/ObjectName")
            .map_err(map_err)?;

        for (metric_name, _value) in self.mbean_snapshot() {
            let j_metric = env.new_string(&metric_name).map_err(map_err)?;
            let mbean_obj = env
                .new_object(
                    &mbean_class,
                    "(Ljava/lang/String;)V",
                    &[JValue::Object(&j_metric.into())],
                )
                .map_err(map_err)?;

            let object_name_str = format!("TomcatRS:type=Metric,name={metric_name}");
            let j_obj_name_str = env.new_string(&object_name_str).map_err(map_err)?;
            let object_name = env
                .new_object(
                    &object_name_class,
                    "(Ljava/lang/String;)V",
                    &[JValue::Object(&j_obj_name_str.into())],
                )
                .map_err(map_err)?;

            env.call_method(
                &mbean_server,
                "registerMBean",
                "(Ljava/lang/Object;Ljavax/management/ObjectName;)Ljavax/management/ObjectInstance;",
                &[
                    JValue::Object(&mbean_obj),
                    JValue::Object(&object_name),
                ],
            )
            .map_err(map_err)?;
        }

        Ok(())
    }
}

/// Parse the `# TYPE … <name> <value>\n` Prometheus exposition format produced
/// by [`MetricsRegistry::render_prometheus`] into `(name, value)` pairs.
///
/// We only consume the subset our own registry emits — `# TYPE` lines are
/// skipped, every other non-blank line is `"<name> <value>"`. A value that
/// does not parse as `f64` is silently dropped; the format guarantees this
/// cannot happen for a healthy registry, but a guard keeps a corrupted line
/// from poisoning the entire snapshot.
fn parse_prometheus_snapshot(text: &str) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(2, ' ');
        let (Some(name), Some(value)) = (parts.next(), parts.next()) else {
            continue;
        };
        if let Ok(v) = value.parse::<f64>() {
            out.push((name.to_string(), v));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_builder_collects_attributes() {
        let d = MBeanDescriptor::new("TomcatRS:type=Connector,name=http-8080")
            .with_attribute(
                "requestCount",
                MBeanAttributeKind::Counter,
                "Total requests served",
            )
            .with_attribute(
                "activeConnections",
                MBeanAttributeKind::Gauge,
                "Currently open connections",
            );
        assert_eq!(d.attributes.len(), 2);
        assert_eq!(d.attributes[0].kind, MBeanAttributeKind::Counter);
        assert_eq!(d.attributes[0].kind.java_type(), "long");
    }

    #[test]
    fn bridge_collects_descriptors() {
        let reg = MetricsRegistry::new();
        let mut bridge = JmxBridge::new(&reg);
        bridge.describe(MBeanDescriptor::new("TomcatRS:type=Server"));
        bridge.describe(MBeanDescriptor::new("TomcatRS:type=Engine"));
        assert_eq!(bridge.descriptors().len(), 2);
    }

    #[test]
    fn mbean_snapshot_reports_registered_counters_and_gauges() {
        let reg = MetricsRegistry::new();
        reg.counter("http_requests_total").add(42);
        reg.gauge("active_connections").set(3);
        reg.counter("zzz_last").add(1);

        let bridge = JmxBridge::new(&reg);
        let snap = bridge.mbean_snapshot();

        // Result is sorted (inherited from render_prometheus) and includes
        // every metric we registered.
        let names: Vec<&str> = snap.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["active_connections", "http_requests_total", "zzz_last"]
        );

        let by_name: std::collections::HashMap<&str, f64> =
            snap.iter().map(|(n, v)| (n.as_str(), *v)).collect();
        assert_eq!(by_name["http_requests_total"], 42.0);
        assert_eq!(by_name["active_connections"], 3.0);
        assert_eq!(by_name["zzz_last"], 1.0);
    }

    #[test]
    fn empty_registry_snapshots_to_empty() {
        let reg = MetricsRegistry::new();
        let bridge = JmxBridge::new(&reg);
        assert!(bridge.mbean_snapshot().is_empty());
    }

    #[cfg(not(feature = "jvm"))]
    #[test]
    fn register_without_feature_returns_error() {
        let reg = MetricsRegistry::new();
        let bridge = JmxBridge::new(&reg);
        let err = bridge.register_with_jvm().unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("`jvm` cargo feature"),
            "unexpected error message: {msg}"
        );
    }
}
