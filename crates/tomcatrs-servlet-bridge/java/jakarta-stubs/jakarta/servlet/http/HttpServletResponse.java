package jakarta.servlet.http;

import java.io.IOException;

import jakarta.servlet.ServletResponse;

/**
 * STUB of {@code jakarta.servlet.http.HttpServletResponse}.
 *
 * <p>Minimal compile-time stub declaring only the members the Tomcat-RS bridge
 * {@code TomcatRsResponseFacade} implements. See
 * {@code jakarta.servlet.ServletRequest} for the rationale. In production the
 * real {@code jakarta.servlet-api} jar replaces these stubs.
 */
public interface HttpServletResponse extends ServletResponse {

    void setStatus(int sc);

    void setHeader(String name, String value);

    void addHeader(String name, String value);

    void sendError(int sc, String msg) throws IOException;

    void sendError(int sc) throws IOException;
}
