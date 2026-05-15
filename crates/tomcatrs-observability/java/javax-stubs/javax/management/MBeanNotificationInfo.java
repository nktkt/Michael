package javax.management;

/**
 * STUB of {@code javax.management.MBeanNotificationInfo}.
 */
public class MBeanNotificationInfo {
    private final String[] types;
    private final String name;
    private final String description;

    public MBeanNotificationInfo(String[] types, String name, String description) {
        this.types = types;
        this.name = name;
        this.description = description;
    }

    public String[] getNotifTypes() {
        return types;
    }

    public String getName() {
        return name;
    }

    public String getDescription() {
        return description;
    }
}
