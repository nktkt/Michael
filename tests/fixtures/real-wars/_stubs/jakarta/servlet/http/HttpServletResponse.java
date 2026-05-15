package jakarta.servlet.http;

import java.io.IOException;

import jakarta.servlet.ServletResponse;

/**
 * Fixture-only stub of {@code jakarta.servlet.http.HttpServletResponse}. See
 * the package comment in
 * {@code tests/fixtures/real-wars/_stubs/jakarta/servlet/Servlet.java}.
 */
public interface HttpServletResponse extends ServletResponse {

    int SC_OK = 200;
    int SC_NOT_FOUND = 404;
    int SC_METHOD_NOT_ALLOWED = 405;
    int SC_INTERNAL_SERVER_ERROR = 500;
    int SC_NOT_IMPLEMENTED = 501;

    void setStatus(int sc);

    int getStatus();

    void setHeader(String name, String value);

    void addHeader(String name, String value);

    void sendError(int sc, String msg) throws IOException;

    void sendError(int sc) throws IOException;

    void sendRedirect(String location) throws IOException;
}
