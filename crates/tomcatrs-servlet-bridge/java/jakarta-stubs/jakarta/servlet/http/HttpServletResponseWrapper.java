package jakarta.servlet.http;

import java.io.IOException;

import jakarta.servlet.ServletResponseWrapper;

/**
 * STUB of {@code jakarta.servlet.http.HttpServletResponseWrapper}.
 *
 * <p>Decorator base used by Spring Boot's {@code ErrorPageFilter},
 * compression filters, security filters, and many gateway adapters.
 * Real Jakarta jar replaces this in production.
 */
public class HttpServletResponseWrapper extends ServletResponseWrapper implements HttpServletResponse {

    public HttpServletResponseWrapper(HttpServletResponse response) {
        super(response);
    }

    private HttpServletResponse http() {
        return (HttpServletResponse) getResponse();
    }

    @Override
    public void setStatus(int sc) { http().setStatus(sc); }

    @Override
    public void setHeader(String name, String value) { http().setHeader(name, value); }

    @Override
    public void addHeader(String name, String value) { http().addHeader(name, value); }

    @Override
    public void sendError(int sc, String msg) throws IOException { http().sendError(sc, msg); }

    @Override
    public void sendError(int sc) throws IOException { http().sendError(sc); }
}
