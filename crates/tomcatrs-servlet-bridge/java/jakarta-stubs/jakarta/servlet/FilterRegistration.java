package jakarta.servlet;

import java.util.Collection;
import java.util.EnumSet;

/**
 * STUB of {@code jakarta.servlet.FilterRegistration}.
 *
 * <p>Minimal compile-time stub of the Servlet 6 {@code FilterRegistration}
 * interface and its nested {@link Dynamic} returned by
 * {@code ServletContext.addFilter(...)}. Signatures match the real Jakarta
 * Servlet 6 API. See {@code jakarta.servlet.ServletRequest} for the broader
 * rationale.
 */
public interface FilterRegistration extends Registration {

    /**
     * Add a mapping by servlet name(s).
     *
     * <p>The {@code dispatcherTypes} argument is the real Servlet 6
     * {@code EnumSet<DispatcherType>}; in the stub it is loosely typed
     * because {@code DispatcherType} is not part of the bridge's compile
     * surface yet. Production builds against the real jar see the real
     * enum.
     */
    void addMappingForServletNames(
            EnumSet<?> dispatcherTypes,
            boolean isMatchAfter,
            String... servletNames);

    Collection<String> getServletNameMappings();

    void addMappingForUrlPatterns(
            EnumSet<?> dispatcherTypes,
            boolean isMatchAfter,
            String... urlPatterns);

    Collection<String> getUrlPatternMappings();

    /**
     * Servlet 3.0+ dynamic-registration interface returned by
     * {@code ServletContext.addFilter(...)}.
     */
    interface Dynamic extends FilterRegistration, Registration.Dynamic {
    }
}
