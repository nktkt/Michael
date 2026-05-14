package jakarta.servlet;

import java.util.Enumeration;

/**
 * STUB of {@code jakarta.servlet.ServletContext}.
 *
 * <p>Minimal compile-time stub declaring only the members the Tomcat-RS bridge
 * {@code TomcatRsServletContext} implements. See
 * {@code jakarta.servlet.ServletRequest} for the rationale. In production the
 * real {@code jakarta.servlet-api} jar replaces these stubs.
 */
public interface ServletContext {

    String getContextPath();

    String getServerInfo();

    String getRealPath(String path);

    String getInitParameter(String name);

    Enumeration<String> getInitParameterNames();

    Object getAttribute(String name);

    void setAttribute(String name, Object value);

    int getMajorVersion();

    int getMinorVersion();
}
