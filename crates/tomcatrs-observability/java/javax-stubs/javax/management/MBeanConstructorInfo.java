package javax.management;

/**
 * STUB of {@code javax.management.MBeanConstructorInfo}.
 */
public class MBeanConstructorInfo {
    private final String name;
    private final String description;

    public MBeanConstructorInfo(String name, String description) {
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
