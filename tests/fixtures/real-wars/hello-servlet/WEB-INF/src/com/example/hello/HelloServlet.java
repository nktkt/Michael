package com.example.hello;

import java.io.IOException;

import jakarta.servlet.ServletConfig;
import jakarta.servlet.ServletException;
import jakarta.servlet.http.HttpServlet;
import jakarta.servlet.http.HttpServletRequest;
import jakarta.servlet.http.HttpServletResponse;

/**
 * Real, compilable {@code HttpServlet} used by the Tomcat-RS JVM-bridge
 * integration tests.
 *
 * <p>Behaviour:
 * <ul>
 *   <li>{@link #init(ServletConfig)} captures the {@code greeting} init-param
 *       (default {@code "Hello"}) so callers can verify init-param plumbing
 *       end-to-end.</li>
 *   <li>{@link #doGet} writes {@code <greeting>, <name>!} as
 *       {@code text/plain; charset=utf-8}, where {@code <name>} comes from the
 *       {@code name} query parameter (default {@code "World"}).</li>
 * </ul>
 *
 * <p>Compiles against the fixture-only stubs at
 * {@code tests/fixtures/real-wars/_stubs/} and against the real
 * {@code jakarta.servlet-api} jar without source changes.
 */
public class HelloServlet extends HttpServlet {

    /** Default greeting if the {@code greeting} init-param is absent. */
    static final String DEFAULT_GREETING = "Hello";

    /** Default {@code name} if the request parameter is absent or blank. */
    static final String DEFAULT_NAME = "World";

    /** Captured from {@code <init-param>greeting</init-param>} at init time. */
    private String greeting = DEFAULT_GREETING;

    @Override
    public void init(ServletConfig config) throws ServletException {
        super.init(config);
        String configured = config.getInitParameter("greeting");
        if (configured != null && !configured.isEmpty()) {
            this.greeting = configured;
        }
    }

    /** Exposed for tests; never null. */
    public String getGreeting() {
        return greeting;
    }

    @Override
    protected void doGet(HttpServletRequest req, HttpServletResponse resp)
            throws ServletException, IOException {
        String name = req.getParameter("name");
        if (name == null || name.isEmpty()) {
            name = DEFAULT_NAME;
        }
        resp.setStatus(HttpServletResponse.SC_OK);
        resp.setContentType("text/plain; charset=utf-8");
        resp.getWriter().write(greeting + ", " + name + "!");
    }
}
