//! Scaffold: exposing Rust-side metrics to the JVM as JMX MBeans.
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
//! tooling working, the JVM side will register a thin "proxy" MBean for each
//! Rust metric whose attribute reads call back across JNI into the registry.
//!
//! This module defines the *description* of that mapping. The actual JNI
//! registration (`MBeanServer.registerMBean`, `ObjectName` construction, the
//! native attribute-getter callbacks) is **future work** — every method here
//! is documented as such and performs no JVM calls.
//!
//! ```text
//!   Rust MetricsRegistry ──describe──▶ MBeanDescriptor
//!                                          │
//!                                  (future: JNI registration)
//!                                          ▼
//!                                  javax.management.MBeanServer
//! ```

use crate::metrics::MetricsRegistry;

/// The JMX type of a single MBean attribute, mirroring the Rust metric kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MBeanAttributeKind {
    /// A monotonically increasing counter — maps to a `long` attribute.
    Counter,
    /// A gauge — maps to a `long` attribute that may rise and fall.
    Gauge,
}

/// Description of one attribute exposed on an MBean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MBeanAttribute {
    /// The attribute name as it will appear in JConsole, e.g.
    /// `requestCount`.
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
///
/// In its current scaffold form the bridge only *collects* descriptors;
/// [`JmxBridge::register_with_jvm`] is a documented placeholder for the JNI
/// wiring that will come with the `tomcatrs-servlet-bridge` integration.
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

    /// Register all collected descriptors with the JVM `MBeanServer`.
    ///
    /// **Not yet implemented.** This is the JNI seam: a future revision will,
    /// for each descriptor, construct an `ObjectName`, build a dynamic MBean
    /// whose attribute getters invoke native callbacks into [`registry`], and
    /// call `MBeanServer.registerMBean`. Until the `tomcatrs-servlet-bridge`
    /// JNI layer lands, this returns the number of descriptors that *would* be
    /// registered, so callers and tests can exercise the surface.
    ///
    /// [`registry`]: JmxBridge::registry
    pub fn register_with_jvm(&self) -> usize {
        // Future work: JNI registration. For now, report the queued count.
        self.descriptors.len()
    }
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
    }

    #[test]
    fn bridge_collects_descriptors() {
        let reg = MetricsRegistry::new();
        let mut bridge = JmxBridge::new(&reg);
        bridge.describe(MBeanDescriptor::new("TomcatRS:type=Server"));
        bridge.describe(MBeanDescriptor::new("TomcatRS:type=Engine"));
        assert_eq!(bridge.descriptors().len(), 2);
        assert_eq!(bridge.register_with_jvm(), 2);
    }
}
