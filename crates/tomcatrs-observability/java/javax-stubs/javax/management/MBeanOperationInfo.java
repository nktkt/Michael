package javax.management;

/**
 * STUB of {@code javax.management.MBeanOperationInfo}.
 */
public class MBeanOperationInfo {
    private final String name;
    private final String description;

    public MBeanOperationInfo(String name, String description) {
        this.name = name;
        this.description = description;
    }

    public String getName() {
        return name;
    }

    public String getDescription() {
        return description;
    }
}
