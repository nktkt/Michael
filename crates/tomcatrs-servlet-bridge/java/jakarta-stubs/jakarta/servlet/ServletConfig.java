package jakarta.servlet;

import java.util.Enumeration;

/**
 * STUB of {@code jakarta.servlet.ServletConfig}.
 *
 * <p>Minimal compile-time stub — see {@code jakarta.servlet.ServletRequest} for
 * the rationale. In production the real {@code jakarta.servlet-api} jar replaces
 * these stubs.
 */
public interface ServletConfig {

    String getServletName();

    ServletContext getServletContext();

    String getInitParameter(String name);

    Enumeration<String> getInitParameterNames();
}
