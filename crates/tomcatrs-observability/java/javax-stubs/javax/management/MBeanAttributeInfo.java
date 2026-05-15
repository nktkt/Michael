package javax.management;

/**
 * STUB of {@code javax.management.MBeanAttributeInfo}.
 */
public class MBeanAttributeInfo {

    private final String name;
    private final String type;
    private final String description;
    private final boolean isReadable;
    private final boolean isWritable;
    private final boolean isIs;

    public MBeanAttributeInfo(
            String name,
            String type,
            String description,
            boolean isReadable,
            boolean isWritable,
            boolean isIs) {
        this.name = name;
        this.type = type;
        this.description = description;
        this.isReadable = isReadable;
        this.isWritable = isWritable;
        this.isIs = isIs;
    }

    public String getName() {
        return name;
    }

    public String getType() {
        return type;
    }

    public String getDescription() {
        return description;
    }

    public boolean isReadable() {
        return isReadable;
    }

    public boolean isWritable() {
        return isWritable;
    }

    public boolean isIs() {
        return isIs;
    }
}
