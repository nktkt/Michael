package jakarta.servlet.http;

import java.util.Enumeration;

import jakarta.servlet.ServletRequest;

/**
 * Fixture-only stub of {@code jakarta.servlet.http.HttpServletRequest}. See
 * the package comment in
 * {@code tests/fixtures/real-wars/_stubs/jakarta/servlet/Servlet.java}.
 */
public interface HttpServletRequest extends ServletRequest {

    String getMethod();

    String getRequestURI();

    String getContextPath();

    String getServletPath();

    String getPathInfo();

    String getQueryString();

    Cookie[] getCookies();

    String getHeader(String name);

    Enumeration<String> getHeaderNames();

    HttpSession getSession();

    HttpSession getSession(boolean create);
}
