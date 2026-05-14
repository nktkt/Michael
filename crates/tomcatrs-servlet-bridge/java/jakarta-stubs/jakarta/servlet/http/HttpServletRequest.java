package jakarta.servlet.http;

import java.util.Enumeration;

import jakarta.servlet.ServletRequest;

/**
 * STUB of {@code jakarta.servlet.http.HttpServletRequest}.
 *
 * <p>Minimal compile-time stub declaring only the members the Tomcat-RS bridge
 * {@code TomcatRsRequestFacade} implements. See
 * {@code jakarta.servlet.ServletRequest} for the rationale. In production the
 * real {@code jakarta.servlet-api} jar replaces these stubs.
 */
public interface HttpServletRequest extends ServletRequest {

    String getMethod();

    String getRequestURI();

    String getQueryString();

    String getHeader(String name);

    Enumeration<String> getHeaderNames();
}
