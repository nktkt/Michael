package jakarta.servlet;

import java.util.Map;
import java.util.Set;

/**
 * STUB of {@code jakarta.servlet.Registration}.
 *
 * <p>Common base of {@link ServletRegistration} and {@link FilterRegistration}.
 * Signatures match Jakarta Servlet 6; production builds replace this stub via
 * the real {@code jakarta.servlet-api} jar.
 */
public interface Registration {

    String getName();

    String getClassName();

    boolean setInitParameter(String name, String value);

    String getInitParameter(String name);

    Set<String> setInitParameters(Map<String, String> initParameters);

    Map<String, String> getInitParameters();

    /**
     * Servlet 3.0+ dynamic-registration extension: the methods callers can
     * only set on a registration that came from a runtime
     * {@code addServlet}/{@code addFilter} call (vs. one parsed from
     * {@code web.xml} at deploy time).
     */
    interface Dynamic extends Registration {
        void setAsyncSupported(boolean isAsyncSupported);
    }
}
