package jakarta.servlet;

import java.util.EventObject;

/**
 * Fixture-only stub of {@code jakarta.servlet.ServletContextEvent}. See the
 * package comment in {@code Servlet.java}.
 */
public class ServletContextEvent extends EventObject {

    public ServletContextEvent(ServletContext source) {
        super(source);
    }

    public ServletContext getServletContext() {
        return (ServletContext) super.getSource();
    }
}
