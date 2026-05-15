package org.apache.tomcatrs.jmx;

import javax.management.Attribute;
import javax.management.AttributeList;
import javax.management.AttributeNotFoundException;
import javax.management.DynamicMBean;
import javax.management.InvalidAttributeValueException;
import javax.management.MBeanAttributeInfo;
import javax.management.MBeanConstructorInfo;
import javax.management.MBeanException;
import javax.management.MBeanInfo;
import javax.management.MBeanNotificationInfo;
import javax.management.MBeanOperationInfo;
import javax.management.ReflectionException;

/**
 * Companion {@link DynamicMBean} for a single Rust-side metric.
 *
 * <p>One instance is registered per metric on the platform {@code MBeanServer}
 * by {@code JmxBridge::register_with_jvm}. The MBean exposes a fixed,
 * single-attribute surface — {@code "Value"} — whose getter calls back into
 * Rust through the {@link #nativeGetMetricValue(String)} native method.
 *
 * <p>The native method is <em>registered</em> by the embedding Rust process
 * via {@code JNIEnv::register_native_methods} at JVM start-up rather than
 * loaded from a shared library, mirroring the
 * {@code org.apache.tomcatrs.bridge.Native*} classes elsewhere in the project.
 * The {@code static} block tolerates a missing native library so the class
 * still loads when Tomcat-RS is the launcher and there is no {@code .so} to
 * find.
 *
 * <p>This class only implements the read-only path of {@code DynamicMBean}:
 * a metric value is fundamentally a sample, not configuration, so
 * {@code setAttribute} / {@code invoke} return a typed exception.
 */
public final class TomcatRsMetricsMBean implements DynamicMBean {

    static {
        try {
            System.loadLibrary("tomcatrs_observability");
        } catch (UnsatisfiedLinkError expectedWhenEmbedded) {
            // Natives are registered directly by the Rust host process.
        }
    }

    /**
     * Returns the current value of the metric with the given name. Implemented
     * in Rust against the host process's {@code MetricsRegistry}; returns
     * {@code 0L} when the metric is unknown (the JMX surface is best-effort).
     */
    static native long nativeGetMetricValue(String metricName);

    /** The single attribute every metric MBean exposes. */
    private static final String ATTR_VALUE = "Value";

    /** The metric name this MBean wraps; supplied at construction. */
    private final String metricName;

    /** Cached MBean descriptor — every instance of this class has the same shape. */
    private final MBeanInfo mbeanInfo;

    public TomcatRsMetricsMBean(String metricName) {
        this.metricName = metricName;
        this.mbeanInfo = buildMBeanInfo(metricName);
    }

    public String getMetricName() {
        return metricName;
    }

    private static MBeanInfo buildMBeanInfo(String metricName) {
        MBeanAttributeInfo[] attrs = new MBeanAttributeInfo[] {
                new MBeanAttributeInfo(
                        ATTR_VALUE,
                        "long",
                        "Current value of metric '" + metricName + "'",
                        /* isReadable= */ true,
                        /* isWritable= */ false,
                        /* isIs= */ false)
        };
        return new MBeanInfo(
                TomcatRsMetricsMBean.class.getName(),
                "Tomcat-RS metric proxy for '" + metricName + "'",
                attrs,
                new MBeanConstructorInfo[0],
                new MBeanOperationInfo[0],
                new MBeanNotificationInfo[0]);
    }

    @Override
    public Object getAttribute(String attribute)
            throws AttributeNotFoundException, MBeanException, ReflectionException {
        if (ATTR_VALUE.equals(attribute)) {
            return Long.valueOf(nativeGetMetricValue(metricName));
        }
        throw new AttributeNotFoundException(
                "unknown attribute '" + attribute + "' on metric '" + metricName + "'");
    }

    @Override
    public void setAttribute(Attribute attribute)
            throws AttributeNotFoundException, InvalidAttributeValueException,
                    MBeanException, ReflectionException {
        throw new MBeanException(
                new UnsupportedOperationException("metric attributes are read-only"));
    }

    @Override
    public AttributeList getAttributes(String[] attributes) {
        AttributeList out = new AttributeList(attributes.length);
        for (String name : attributes) {
            try {
                Object v = getAttribute(name);
                out.add(new Attribute(name, v));
            } catch (Exception ignored) {
                // Per spec, getAttributes silently skips unreadable names.
            }
        }
        return out;
    }

    @Override
    public AttributeList setAttributes(AttributeList attributes) {
        // Read-only surface: nothing was set, return an empty list.
        return new AttributeList();
    }

    @Override
    public Object invoke(String actionName, Object[] params, String[] signature)
            throws MBeanException, ReflectionException {
        throw new MBeanException(
                new UnsupportedOperationException(
                        "metric MBean exposes no operations (called '" + actionName + "')"));
    }

    @Override
    public MBeanInfo getMBeanInfo() {
        return mbeanInfo;
    }
}
