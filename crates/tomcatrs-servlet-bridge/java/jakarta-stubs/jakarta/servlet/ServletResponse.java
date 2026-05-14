package jakarta.servlet;

import java.io.IOException;
import java.io.PrintWriter;

/**
 * STUB of {@code jakarta.servlet.ServletResponse}.
 *
 * <p>Minimal compile-time stub declaring only the members the Tomcat-RS bridge
 * {@code TomcatRsResponseFacade} implements. See
 * {@code jakarta.servlet.ServletRequest} for the rationale. In production the
 * real {@code jakarta.servlet-api} jar replaces these stubs.
 */
public interface ServletResponse {

    void setContentType(String type);

    void setContentLength(int len);

    boolean isCommitted();

    void flushBuffer() throws IOException;

    ServletOutputStream getOutputStream() throws IOException;

    PrintWriter getWriter() throws IOException;
}
