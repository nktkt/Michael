package com.example.filtered;

import jakarta.servlet.Filter;
import jakarta.servlet.FilterChain;
import jakarta.servlet.FilterConfig;
import jakarta.servlet.ServletException;
import jakarta.servlet.ServletRequest;
import jakarta.servlet.ServletResponse;

import java.io.IOException;

/**
 * Documentation-only filter source for the `filtered/` fixture.
 * Forces a request character encoding before the chain runs.
 */
public class EncodingFilter implements Filter {
    private String charset = "UTF-8";

    @Override
    public void init(FilterConfig config) {
        String configured = config.getInitParameter("charset");
        if (configured != null && !configured.isEmpty()) {
            this.charset = configured;
        }
    }

    @Override
    public void doFilter(ServletRequest request, ServletResponse response, FilterChain chain) throws IOException, ServletException {
        request.setCharacterEncoding(charset);
        chain.doFilter(request, response);
    }
}
