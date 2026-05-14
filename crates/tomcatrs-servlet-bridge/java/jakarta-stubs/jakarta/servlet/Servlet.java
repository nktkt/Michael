package jakarta.servlet;

import java.io.IOException;

/**
 * STUB of {@code jakarta.servlet.Servlet}.
 *
 * <p>Minimal compile-time stub declaring the full {@code Servlet} lifecycle
 * surface the Tomcat-RS bridge dispatcher relies on. See
 * {@code jakarta.servlet.ServletRequest} for the rationale. In production the
 * real {@code jakarta.servlet-api} jar replaces these stubs.
 */
public interface Servlet {

    void init(ServletConfig config) throws ServletException;

    ServletConfig getServletConfig();

    void service(ServletRequest req, ServletResponse res)
            throws ServletException, IOException;

    String getServletInfo();

    void destroy();
}
