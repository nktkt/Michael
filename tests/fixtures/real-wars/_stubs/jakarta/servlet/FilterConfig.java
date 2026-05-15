package jakarta.servlet;

import java.util.Enumeration;

/**
 * Fixture-only stub of {@code jakarta.servlet.FilterConfig}. See the package
 * comment in {@code Servlet.java}.
 */
public interface FilterConfig {

    String getFilterName();

    ServletContext getServletContext();

    String getInitParameter(String name);

    Enumeration<String> getInitParameterNames();
}
