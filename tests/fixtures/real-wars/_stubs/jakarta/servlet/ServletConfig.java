package jakarta.servlet;

import java.util.Enumeration;

/**
 * Fixture-only stub of {@code jakarta.servlet.ServletConfig}. See the package
 * comment in {@code Servlet.java}.
 */
public interface ServletConfig {

    String getServletName();

    ServletContext getServletContext();

    String getInitParameter(String name);

    Enumeration<String> getInitParameterNames();
}
