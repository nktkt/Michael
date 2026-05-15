package jakarta.servlet;

import java.util.EventListener;

/**
 * Fixture-only stub of {@code jakarta.servlet.ServletContextListener}. See the
 * package comment in {@code Servlet.java}.
 */
public interface ServletContextListener extends EventListener {

    default void contextInitialized(ServletContextEvent sce) {
    }

    default void contextDestroyed(ServletContextEvent sce) {
    }
}
