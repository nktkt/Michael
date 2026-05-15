package javax.management;

/**
 * STUB of {@code javax.management.DynamicMBean}.
 *
 * <p>Minimal compile-time stub mirroring just the surface
 * {@code org.apache.tomcatrs.jmx.TomcatRsMetricsMBean} needs to implement, so
 * that the metrics MBean compiles standalone without the real
 * {@code java.management} module on the classpath. In production the real
 * JDK type replaces this stub.
 */
public interface DynamicMBean {

    Object getAttribute(String attribute)
            throws AttributeNotFoundException, MBeanException, ReflectionException;

    void setAttribute(Attribute attribute)
            throws AttributeNotFoundException, InvalidAttributeValueException,
                    MBeanException, ReflectionException;

    AttributeList getAttributes(String[] attributes);

    AttributeList setAttributes(AttributeList attributes);

    Object invoke(String actionName, Object[] params, String[] signature)
            throws MBeanException, ReflectionException;

    MBeanInfo getMBeanInfo();
}
