package jakarta.servlet;

/**
 * STUB of {@code jakarta.servlet.MultipartConfigElement}.
 *
 * <p>Minimal compile-time stub matching Servlet 6 — Spring Boot's
 * {@code RegistrationBean.onStartup} reflectively invokes
 * {@code ServletRegistration.Dynamic.setMultipartConfig(MultipartConfigElement)}
 * on every servlet registration, so the bridge facade must compile against
 * a class with this exact name + package even when the real
 * {@code jakarta.servlet-api} jar is providing the runtime definition.
 */
public class MultipartConfigElement {

    private final String location;
    private final long maxFileSize;
    private final long maxRequestSize;
    private final int fileSizeThreshold;

    public MultipartConfigElement(String location) {
        this(location, -1L, -1L, 0);
    }

    public MultipartConfigElement(
            String location,
            long maxFileSize,
            long maxRequestSize,
            int fileSizeThreshold) {
        this.location = location == null ? "" : location;
        this.maxFileSize = maxFileSize;
        this.maxRequestSize = maxRequestSize;
        this.fileSizeThreshold = Math.max(0, fileSizeThreshold);
    }

    public String getLocation() { return location; }
    public long getMaxFileSize() { return maxFileSize; }
    public long getMaxRequestSize() { return maxRequestSize; }
    public int getFileSizeThreshold() { return fileSizeThreshold; }
}
