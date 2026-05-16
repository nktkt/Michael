package jakarta.servlet;

import java.io.InputStream;
import java.net.MalformedURLException;
import java.net.URL;
import java.util.Enumeration;
import java.util.EventListener;
import java.util.Map;
import java.util.Set;

/**
 * STUB of {@code jakarta.servlet.ServletContext}.
 *
 * <p>Minimal compile-time stub declaring the surface the Tomcat-RS bridge
 * {@code TomcatRsServletContext} implements. Signatures match the real
 * Jakarta Servlet 6 interface so the bridge compiles against either this
 * stub or the production {@code jakarta.servlet-api} jar.
 *
 * <p>Surface intentionally trimmed to what {@code DispatcherServlet} and the
 * other framework bootstrappers exercised by Tomcat-RS actually need:
 * dynamic servlet/filter/listener registration, init-parameter and
 * attribute access, resource look-up, and logging. See
 * {@code jakarta.servlet.ServletRequest} for the broader stub rationale.
 */
public interface ServletContext {

    // --- Identity -----------------------------------------------------------

    String getContextPath();

    String getServletContextName();

    String getServerInfo();

    int getMajorVersion();

    int getMinorVersion();

    int getEffectiveMajorVersion();

    int getEffectiveMinorVersion();

    // --- Init parameters ----------------------------------------------------

    String getInitParameter(String name);

    Enumeration<String> getInitParameterNames();

    boolean setInitParameter(String name, String value);

    // --- Attributes ---------------------------------------------------------

    Object getAttribute(String name);

    Enumeration<String> getAttributeNames();

    void setAttribute(String name, Object value);

    void removeAttribute(String name);

    // --- Resources ----------------------------------------------------------

    String getRealPath(String path);

    Set<String> getResourcePaths(String path);

    URL getResource(String path) throws MalformedURLException;

    InputStream getResourceAsStream(String path);

    String getMimeType(String file);

    // --- Servlet / filter / listener registration --------------------------

    ServletRegistration.Dynamic addServlet(String servletName, String className);

    ServletRegistration.Dynamic addServlet(String servletName, Servlet servlet);

    ServletRegistration.Dynamic addServlet(
            String servletName, Class<? extends Servlet> servletClass);

    ServletRegistration getServletRegistration(String servletName);

    Map<String, ? extends ServletRegistration> getServletRegistrations();

    FilterRegistration.Dynamic addFilter(String filterName, String className);

    FilterRegistration.Dynamic addFilter(String filterName, Filter filter);

    FilterRegistration.Dynamic addFilter(
            String filterName, Class<? extends Filter> filterClass);

    FilterRegistration getFilterRegistration(String filterName);

    Map<String, ? extends FilterRegistration> getFilterRegistrations();

    void addListener(String className);

    <T extends EventListener> void addListener(T t);

    void addListener(Class<? extends EventListener> listenerClass);

    // --- Logging ------------------------------------------------------------

    void log(String msg);

    void log(String message, Throwable throwable);
}
