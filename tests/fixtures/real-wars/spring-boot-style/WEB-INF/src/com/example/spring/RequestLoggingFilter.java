package com.example.spring;

import java.io.IOException;

import jakarta.servlet.Filter;
import jakarta.servlet.FilterChain;
import jakarta.servlet.FilterConfig;
import jakarta.servlet.ServletException;
import jakarta.servlet.ServletRequest;
import jakarta.servlet.ServletResponse;
import jakarta.servlet.http.HttpServletRequest;

/**
 * Global request-logging filter for the {@code spring-boot-style} fixture.
 * Mapped to {@code /*}. Demonstrates {@code <init-param>} plumbing on a filter
 * and the wrap-and-delegate {@link FilterChain} pattern.
 */
public class RequestLoggingFilter implements Filter {

    private String prefix = "[filter]";
    private long count = 0L;

    @Override
    public void init(FilterConfig filterConfig) throws ServletException {
        String p = filterConfig.getInitParameter("prefix");
        if (p != null && !p.isEmpty()) {
            this.prefix = p;
        }
    }

    /** Exposed for tests. */
    public String getPrefix() {
        return prefix;
    }

    /** Exposed for tests. */
    public synchronized long getCount() {
        return count;
    }

    @Override
    public void doFilter(ServletRequest request, ServletResponse response, FilterChain chain)
            throws ServletException, IOException {
        synchronized (this) {
            count++;
        }
        if (request instanceof HttpServletRequest) {
            HttpServletRequest http = (HttpServletRequest) request;
            // Stash a marker attribute so downstream servlets can verify the
            // filter actually ran in front of them.
            request.setAttribute("x-logging-filter", prefix + " " + http.getMethod()
                    + " " + http.getRequestURI());
        }
        chain.doFilter(request, response);
    }

    @Override
    public void destroy() {
        // Nothing to release; counter is in-memory only.
    }
}
