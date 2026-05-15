package jakarta.servlet;

import java.io.IOException;
import java.io.PrintWriter;

/**
 * Fixture-only stub of {@code jakarta.servlet.ServletResponse}. See the package
 * comment in {@code Servlet.java}.
 */
public interface ServletResponse {

    void setContentType(String type);

    void setContentLength(int len);

    void setCharacterEncoding(String charset);

    String getCharacterEncoding();

    boolean isCommitted();

    void flushBuffer() throws IOException;

    ServletOutputStream getOutputStream() throws IOException;

    PrintWriter getWriter() throws IOException;
}
