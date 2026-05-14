package jakarta.servlet.http;

import java.util.Enumeration;

import jakarta.servlet.ServletContext;

/**
 * STUB of {@code jakarta.servlet.http.HttpSession}.
 *
 * <p>Minimal compile-time stub declaring only the members the Tomcat-RS bridge
 * {@code TomcatRsHttpSession} implements. See
 * {@code jakarta.servlet.ServletRequest} for the rationale. In production the
 * real {@code jakarta.servlet-api} jar replaces these stubs.
 */
public interface HttpSession {

    String getId();

    long getCreationTime();

    long getLastAccessedTime();

    void setMaxInactiveInterval(int interval);

    int getMaxInactiveInterval();

    Object getAttribute(String name);

    Enumeration<String> getAttributeNames();

    void setAttribute(String name, Object value);

    void removeAttribute(String name);

    void invalidate();

    boolean isNew();

    ServletContext getServletContext();
}
