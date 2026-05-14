package jakarta.servlet;

import java.io.BufferedReader;
import java.io.IOException;

/**
 * STUB of {@code jakarta.servlet.ServletRequest}.
 *
 * <p>This is <strong>not</strong> the real Jakarta Servlet API. It is a minimal
 * compile-time stub declaring only the members the Tomcat-RS bridge facades
 * implement, so {@code java/org/apache/tomcatrs/bridge/*.java} can be compiled
 * standalone without vendoring the full {@code jakarta.servlet-api} jar. In
 * production the real Jakarta Servlet API jar replaces these stubs and provides
 * the complete interface.
 */
public interface ServletRequest {

    Object getAttribute(String name);

    void setAttribute(String name, Object value);

    String getProtocol();

    String getScheme();

    String getRemoteAddr();

    ServletInputStream getInputStream() throws IOException;

    BufferedReader getReader() throws IOException;
}
