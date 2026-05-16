package jakarta.servlet;

import java.io.IOException;

/** STUB of jakarta.servlet.RequestDispatcher. */
public interface RequestDispatcher {
    void forward(ServletRequest request, ServletResponse response) throws ServletException, IOException;
    void include(ServletRequest request, ServletResponse response) throws ServletException, IOException;
}
