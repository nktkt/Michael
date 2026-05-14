package jakarta.servlet;

import java.util.Enumeration;

/**
 * STUB of {@code jakarta.servlet.FilterConfig}.
 *
 * <p>Minimal compile-time stub — see {@code jakarta.servlet.ServletRequest} for
 * the rationale. In production the real {@code jakarta.servlet-api} jar replaces
 * these stubs.
 */
public interface FilterConfig {

    String getFilterName();

    ServletContext getServletContext();

    String getInitParameter(String name);

    Enumeration<String> getInitParameterNames();
}
