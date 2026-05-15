package jakarta.servlet;

import java.io.IOException;

/**
 * Fixture-only stub of {@code jakarta.servlet.Filter}. See the package comment
 * in {@code Servlet.java}.
 */
public interface Filter {

    default void init(FilterConfig filterConfig) throws ServletException {
    }

    void doFilter(ServletRequest request, ServletResponse response, FilterChain chain)
            throws ServletException, IOException;

    default void destroy() {
    }
}
