package jakarta.servlet;

import java.io.IOException;
import java.io.PrintWriter;

/**
 * STUB of {@code jakarta.servlet.ServletResponseWrapper} — the decorator base
 * Spring's `ErrorPageFilter` and other response-rewriting filters extend.
 *
 * <p>Real Jakarta jar replaces this in production.
 */
public class ServletResponseWrapper implements ServletResponse {

    private ServletResponse response;

    public ServletResponseWrapper(ServletResponse response) {
        if (response == null) {
            throw new IllegalArgumentException("ServletResponse cannot be null");
        }
        this.response = response;
    }

    public ServletResponse getResponse() {
        return response;
    }

    public void setResponse(ServletResponse response) {
        if (response == null) {
            throw new IllegalArgumentException("ServletResponse cannot be null");
        }
        this.response = response;
    }

    @Override
    public void setContentType(String type) { response.setContentType(type); }

    @Override
    public void setContentLength(int len) { response.setContentLength(len); }

    @Override
    public boolean isCommitted() { return response.isCommitted(); }

    @Override
    public void flushBuffer() throws IOException { response.flushBuffer(); }

    @Override
    public ServletOutputStream getOutputStream() throws IOException { return response.getOutputStream(); }

    @Override
    public PrintWriter getWriter() throws IOException { return response.getWriter(); }
}
