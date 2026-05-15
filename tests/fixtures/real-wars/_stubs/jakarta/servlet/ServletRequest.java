package jakarta.servlet;

import java.io.BufferedReader;
import java.io.IOException;
import java.util.Enumeration;
import java.util.Map;

/**
 * Fixture-only stub of {@code jakarta.servlet.ServletRequest}. See the package
 * comment in {@code Servlet.java}.
 */
public interface ServletRequest {

    Object getAttribute(String name);

    void setAttribute(String name, Object value);

    String getProtocol();

    String getScheme();

    String getRemoteAddr();

    String getCharacterEncoding();

    void setCharacterEncoding(String env) throws java.io.UnsupportedEncodingException;

    String getParameter(String name);

    String[] getParameterValues(String name);

    Enumeration<String> getParameterNames();

    Map<String, String[]> getParameterMap();

    ServletInputStream getInputStream() throws IOException;

    BufferedReader getReader() throws IOException;
}
