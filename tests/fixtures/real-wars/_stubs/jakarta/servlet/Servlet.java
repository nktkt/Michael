package jakarta.servlet;

import java.io.IOException;

/**
 * Fixture-only stub of {@code jakarta.servlet.Servlet}.
 *
 * <p>Lives under {@code tests/fixtures/real-wars/_stubs/} alongside the rest of
 * the fixture-specific Jakarta Servlet surface. It is NOT the bridge stub at
 * {@code crates/tomcatrs-servlet-bridge/java/jakarta-stubs/} — the two paths
 * exist independently so fixtures may rely on a richer compile-time surface
 * than the bridge facades themselves expose. In production the real
 * {@code jakarta.servlet-api} jar replaces both.
 */
public interface Servlet {

    void init(ServletConfig config) throws ServletException;

    ServletConfig getServletConfig();

    void service(ServletRequest req, ServletResponse res)
            throws ServletException, IOException;

    String getServletInfo();

    void destroy();
}
