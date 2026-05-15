package jakarta.servlet;

import java.util.Set;

/**
 * STUB of {@code jakarta.servlet.ServletContainerInitializer}.
 *
 * <p>Minimal compile-time stub of the Servlet 6 SCI interface. The Servlet
 * specification's {@code ServiceLoader}-style discovery mechanism for
 * framework "bootstrapper" code: implementations are declared in
 * {@code META-INF/services/jakarta.servlet.ServletContainerInitializer}
 * inside a webapp's classes/jars; the container instantiates each one and
 * invokes {@link #onStartup(Set, ServletContext)} during context
 * initialisation, before any servlet {@code init()} fires. This is the entry
 * point Spring Boot and most modern frameworks use to bootstrap without a
 * {@code web.xml}.
 *
 * <p>See {@code jakarta.servlet.ServletRequest} for the broader stub
 * rationale. In production the real {@code jakarta.servlet-api} jar replaces
 * these stubs.
 */
public interface ServletContainerInitializer {

    /**
     * Receives notification during startup of a web application's
     * initialisation.
     *
     * @param c the (possibly {@code null}) set of classes that the
     *          container has discovered to satisfy this initialiser's
     *          {@code @HandlesTypes} annotation, if any.
     * @param ctx the {@link ServletContext} of the web application being
     *            initialised.
     * @throws ServletException if an error occurs during processing.
     */
    void onStartup(Set<Class<?>> c, ServletContext ctx) throws ServletException;
}
