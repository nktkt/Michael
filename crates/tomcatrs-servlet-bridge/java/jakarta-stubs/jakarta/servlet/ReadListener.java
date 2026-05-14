package jakarta.servlet;

/**
 * STUB of {@code jakarta.servlet.ReadListener}.
 *
 * <p>This is <strong>not</strong> the real Jakarta Servlet API. It is a minimal
 * compile-time stub that declares only the members the Tomcat-RS bridge facades
 * touch, so the {@code java/} sources can be compiled standalone without
 * vendoring the full {@code jakarta.servlet-api} jar. In production the real
 * Jakarta Servlet API jar is on the classpath instead of these stubs.
 */
public interface ReadListener {
    void onDataAvailable() throws java.io.IOException;

    void onAllDataRead() throws java.io.IOException;

    void onError(Throwable t);
}
