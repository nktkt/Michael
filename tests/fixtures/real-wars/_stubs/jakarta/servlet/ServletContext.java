package jakarta.servlet;

import java.util.Enumeration;

/**
 * Fixture-only stub of {@code jakarta.servlet.ServletContext}. See the package
 * comment in {@code Servlet.java}.
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

    void log(String msg);
}
