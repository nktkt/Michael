package jakarta.servlet.http;

import java.util.Enumeration;

import jakarta.servlet.ServletRequestWrapper;

/**
 * STUB of {@code jakarta.servlet.http.HttpServletRequestWrapper}.
 *
 * <p>Decorator base used by security filters, CORS filters, gateway
 * adapters, and Spring's request facades. Real Jakarta jar replaces this
 * in production.
 */
public class HttpServletRequestWrapper extends ServletRequestWrapper implements HttpServletRequest {

    public HttpServletRequestWrapper(HttpServletRequest request) {
        super(request);
    }

    private HttpServletRequest http() {
        return (HttpServletRequest) getRequest();
    }

    @Override
    public String getMethod() { return http().getMethod(); }

    @Override
    public String getRequestURI() { return http().getRequestURI(); }

    @Override
    public String getQueryString() { return http().getQueryString(); }

    @Override
    public String getHeader(String name) { return http().getHeader(name); }

    @Override
    public Enumeration<String> getHeaderNames() { return http().getHeaderNames(); }
}
