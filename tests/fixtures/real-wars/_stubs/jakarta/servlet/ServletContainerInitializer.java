package jakarta.servlet;

import java.util.Set;

/**
 * Fixture-only stub of {@code jakarta.servlet.ServletContainerInitializer}.
 *
 * <p>Used to compile real-WAR fixtures under {@code tests/fixtures/real-wars/}
 * without depending on the production {@code jakarta.servlet-api} jar at
 * fixture-build time. The fixture-side classes never ship in
 * {@code WEB-INF/classes/}; the per-fixture build directs {@code javac} to
 * write its outputs into a temp directory and only the application classes
 * end up in {@code WEB-INF/classes/}.
 */
public interface ServletContainerInitializer {
    void onStartup(Set<Class<?>> c, ServletContext ctx) throws ServletException;
}
