package javax.management;

/**
 * STUB of {@code javax.management.MBeanInfo}.
 *
 * <p>Captures only the fields {@code TomcatRsMetricsMBean} needs to build a
 * descriptor. In production the real {@code java.management} module supplies
 * the canonical class.
 */
public class MBeanInfo {

    private final String className;
    private final String description;
    private final MBeanAttributeInfo[] attributes;
    private final MBeanConstructorInfo[] constructors;
    private final MBeanOperationInfo[] operations;
    private final MBeanNotificationInfo[] notifications;

    public MBeanInfo(
            String className,
            String description,
            MBeanAttributeInfo[] attributes,
            MBeanConstructorInfo[] constructors,
            MBeanOperationInfo[] operations,
            MBeanNotificationInfo[] notifications) {
        this.className = className;
        this.description = description;
        this.attributes = attributes;
        this.constructors = constructors;
        this.operations = operations;
        this.notifications = notifications;
    }

    public String getClassName() {
        return className;
    }

    public String getDescription() {
        return description;
    }

    public MBeanAttributeInfo[] getAttributes() {
        return attributes;
    }

    public MBeanConstructorInfo[] getConstructors() {
        return constructors;
    }

    public MBeanOperationInfo[] getOperations() {
        return operations;
    }

    public MBeanNotificationInfo[] getNotifications() {
        return notifications;
    }
}
