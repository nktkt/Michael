package jakarta.servlet;

import java.io.IOException;

/**
 * STUB of {@code jakarta.servlet.FilterChain}.
 *
 * <p>Minimal compile-time stub — see {@code jakarta.servlet.ServletRequest} for
 * the rationale. In production the real {@code jakarta.servlet-api} jar replaces
 * these stubs.
 */
public interface FilterChain {

    void doFilter(ServletRequest request, ServletResponse response)
            throws ServletException, IOException;
}
