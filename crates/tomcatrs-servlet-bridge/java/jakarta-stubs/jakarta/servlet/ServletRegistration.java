package jakarta.servlet;

import java.util.Collection;
import java.util.Map;
import java.util.Set;

/**
 * STUB of {@code jakarta.servlet.ServletRegistration}.
 *
 * <p>Minimal compile-time stub of the Servlet 6
 * {@link jakarta.servlet.ServletContext#getServletRegistration ServletRegistration}
 * and its nested {@link Dynamic} returned by the {@code addServlet} family.
 *
 * <p>Signatures match the real Jakarta Servlet 6 interface; production builds
 * with the real {@code jakarta.servlet-api} jar replace these stubs. See
 * {@code jakarta.servlet.ServletRequest} for the broader rationale.
 */
public interface ServletRegistration extends Registration {

    /**
     * Add the URL mappings this servlet is reachable under.
     *
     * @return the set of patterns that could not be mapped (because they
     *         conflicted with an existing mapping). The real container returns
     *         the failing patterns; this stub mirrors that contract.
     */
    Set<String> addMapping(String... urlPatterns);

    /** The URL mappings currently registered for this servlet. */
    Collection<String> getMappings();

    /** The {@code <run-as>} role name, or {@code null} if none. */
    String getRunAsRole();

    /**
     * Servlet 3.0+ dynamic-registration interface returned by
     * {@code ServletContext.addServlet(...)}.
     */
    interface Dynamic extends ServletRegistration, Registration.Dynamic {

        void setLoadOnStartup(int loadOnStartup);

        void setMultipartConfig(MultipartConfigElement multipartConfig);

        void setRunAsRole(String roleName);

        Set<String> setServletSecurity(Object constraint);
    }

    /**
     * Base of the {@link ServletRegistration} / {@link FilterRegistration}
     * hierarchy — held inline to keep the stub file count down. The real
     * jakarta-servlet-api jar defines it in its own file
     * ({@code jakarta.servlet.Registration}); the simple name + package match
     * so the bridge compiles against either.
     */
    // NOTE: Registration lives in its own file (see Registration.java) — this
    // comment is a pointer for human readers, no code here.
}
