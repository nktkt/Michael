package com.example.listener;

import jakarta.servlet.ServletContextEvent;
import jakarta.servlet.ServletContextListener;

/**
 * Documentation-only ServletContextListener source for the `listener/`
 * fixture. Logs context startup and shutdown events.
 */
public class BootListener implements ServletContextListener {
    @Override
    public void contextInitialized(ServletContextEvent sce) {
        sce.getServletContext().log("listener fixture: context initialized");
    }

    @Override
    public void contextDestroyed(ServletContextEvent sce) {
        sce.getServletContext().log("listener fixture: context destroyed");
    }
}
