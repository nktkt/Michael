package jakarta.servlet;

import java.io.IOException;

/**
 * Fixture-only stub of {@code jakarta.servlet.FilterChain}. See the package
 * comment in {@code Servlet.java}.
 */
public interface FilterChain {

    void doFilter(ServletRequest request, ServletResponse response)
            throws ServletException, IOException;
}
