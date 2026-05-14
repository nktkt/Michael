package jakarta.servlet;

import java.io.IOException;

/**
 * STUB of {@code jakarta.servlet.Filter}.
 *
 * <p>Minimal compile-time stub — see {@code jakarta.servlet.ServletRequest} for
 * the rationale. The {@code init} and {@code destroy} methods are declared
 * {@code default} (matching the real Jakarta Servlet 6 API) so the dispatcher
 * can drive bare {@code Filter} implementations. In production the real
 * {@code jakarta.servlet-api} jar replaces these stubs.
 */
public interface Filter {

    default void init(FilterConfig filterConfig) throws ServletException {
    }

    void doFilter(ServletRequest request, ServletResponse response, FilterChain chain)
            throws ServletException, IOException;

    default void destroy() {
    }
}
