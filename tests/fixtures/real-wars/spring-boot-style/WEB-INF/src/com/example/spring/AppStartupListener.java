package com.example.spring;

import jakarta.servlet.ServletContext;
import jakarta.servlet.ServletContextEvent;
import jakarta.servlet.ServletContextListener;

/**
 * Boot/shutdown listener for the {@code spring-boot-style} fixture.
 *
 * <p>On {@code contextInitialized} it reads the {@code app.name} and
 * {@code app.profile} context-params and stashes a {@code "boot.summary"}
 * context attribute that integration tests can read back to confirm the
 * listener actually fired.
 */
public class AppStartupListener implements ServletContextListener {

    /** The context attribute key the listener writes its summary into. */
    public static final String BOOT_SUMMARY_ATTR = "boot.summary";

    @Override
    public void contextInitialized(ServletContextEvent sce) {
        ServletContext ctx = sce.getServletContext();
        String name = orDefault(ctx.getInitParameter("app.name"), "unknown");
        String profile = orDefault(ctx.getInitParameter("app.profile"), "default");
        ctx.setAttribute(BOOT_SUMMARY_ATTR, name + ":" + profile);
        ctx.log("AppStartupListener: started " + name + " (" + profile + ")");
    }

    @Override
    public void contextDestroyed(ServletContextEvent sce) {
        // Clear the summary attribute so a re-deploy starts from a clean slate.
        sce.getServletContext().setAttribute(BOOT_SUMMARY_ATTR, null);
    }

    private static String orDefault(String value, String fallback) {
        return (value == null || value.isEmpty()) ? fallback : value;
    }
}
