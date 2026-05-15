package jakarta.servlet.http;

import java.io.IOException;

import jakarta.servlet.Servlet;
import jakarta.servlet.ServletConfig;
import jakarta.servlet.ServletException;
import jakarta.servlet.ServletRequest;
import jakarta.servlet.ServletResponse;

/**
 * STUB of {@code jakarta.servlet.http.HttpServlet}.
 *
 * <p>Minimal compile-time stub bundled into {@code tomcatrs-bridge.jar} so that
 * user servlets in deployed webapps can {@code extend HttpServlet} and resolve
 * at JVM class-load time when the bridge jar is the only thing supplying the
 * Jakarta Servlet API surface (e.g. in tests and minimal embeddings).
 *
 * <p>In a real production deployment the genuine {@code jakarta.servlet-api}
 * jar replaces this stub with the full {@code HttpServlet} implementation; the
 * surface declared here is intentionally a subset so it can be transparently
 * superseded.
 *
 * <p>This file mirrors the fixture stub at
 * {@code tests/fixtures/real-wars/_stubs/jakarta/servlet/http/HttpServlet.java}
 * so test webapps that link against that stub at compile time resolve cleanly
 * at runtime against this bundled stub.
 */
public abstract class HttpServlet implements Servlet {

    private transient ServletConfig config;

    public HttpServlet() {
    }

    @Override
    public void init(ServletConfig config) throws ServletException {
        this.config = config;
        init();
    }

    public void init() throws ServletException {
    }

    @Override
    public ServletConfig getServletConfig() {
        return config;
    }

    public String getInitParameter(String name) {
        ServletConfig c = getServletConfig();
        return c == null ? null : c.getInitParameter(name);
    }

    @Override
    public String getServletInfo() {
        return "";
    }

    @Override
    public void destroy() {
    }

    protected void doGet(HttpServletRequest req, HttpServletResponse resp)
            throws ServletException, IOException {
        resp.sendError(405, "GET not supported");
    }

    protected void doPost(HttpServletRequest req, HttpServletResponse resp)
            throws ServletException, IOException {
        resp.sendError(405, "POST not supported");
    }

    protected void doPut(HttpServletRequest req, HttpServletResponse resp)
            throws ServletException, IOException {
        resp.sendError(405, "PUT not supported");
    }

    protected void doDelete(HttpServletRequest req, HttpServletResponse resp)
            throws ServletException, IOException {
        resp.sendError(405, "DELETE not supported");
    }

    protected void service(HttpServletRequest req, HttpServletResponse resp)
            throws ServletException, IOException {
        String method = req.getMethod();
        if ("GET".equals(method)) {
            doGet(req, resp);
        } else if ("POST".equals(method)) {
            doPost(req, resp);
        } else if ("PUT".equals(method)) {
            doPut(req, resp);
        } else if ("DELETE".equals(method)) {
            doDelete(req, resp);
        } else {
            resp.sendError(501, "Not Implemented: " + method);
        }
    }

    @Override
    public void service(ServletRequest req, ServletResponse res)
            throws ServletException, IOException {
        if (req instanceof HttpServletRequest && res instanceof HttpServletResponse) {
            service((HttpServletRequest) req, (HttpServletResponse) res);
        } else {
            throw new ServletException("non-HTTP request or response");
        }
    }
}
