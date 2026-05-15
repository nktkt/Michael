package com.example.spring;

import java.io.IOException;

import jakarta.servlet.ServletConfig;
import jakarta.servlet.ServletException;
import jakarta.servlet.http.HttpServlet;
import jakarta.servlet.http.HttpServletRequest;
import jakarta.servlet.http.HttpServletResponse;

/**
 * Tiny JSON-style API servlet for the {@code spring-boot-style} fixture.
 * Demonstrates init-param plumbing ({@code version}) and reading the request's
 * path-info / query-string after path-mapping.
 */
public class ApiServlet extends HttpServlet {

    private String version = "v0";

    @Override
    public void init(ServletConfig config) throws ServletException {
        super.init(config);
        String v = config.getInitParameter("version");
        if (v != null && !v.isEmpty()) {
            this.version = v;
        }
    }

    /** Exposed for tests. */
    public String getVersion() {
        return version;
    }

    @Override
    protected void doGet(HttpServletRequest req, HttpServletResponse resp)
            throws ServletException, IOException {
        String pathInfo = req.getPathInfo();
        if (pathInfo == null) {
            pathInfo = "/";
        }
        resp.setStatus(HttpServletResponse.SC_OK);
        resp.setContentType("application/json; charset=utf-8");
        // Hand-rolled JSON: keeps the fixture free of external deps.
        String body = "{\"version\":\"" + jsonEscape(version)
                + "\",\"path\":\"" + jsonEscape(pathInfo) + "\"}";
        resp.getWriter().write(body);
    }

    @Override
    protected void doPost(HttpServletRequest req, HttpServletResponse resp)
            throws ServletException, IOException {
        // Symmetric POST: echoes the `payload` request parameter back. Lets
        // integration tests exercise the parameter-decoding pipeline without
        // pulling in a JSON parser.
        String payload = req.getParameter("payload");
        if (payload == null) {
            payload = "";
        }
        resp.setStatus(HttpServletResponse.SC_OK);
        resp.setContentType("application/json; charset=utf-8");
        resp.getWriter().write(
                "{\"version\":\"" + jsonEscape(version)
                        + "\",\"echo\":\"" + jsonEscape(payload) + "\"}");
    }

    private static String jsonEscape(String s) {
        StringBuilder out = new StringBuilder(s.length() + 8);
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            switch (c) {
                case '"':
                    out.append("\\\"");
                    break;
                case '\\':
                    out.append("\\\\");
                    break;
                case '\n':
                    out.append("\\n");
                    break;
                case '\r':
                    out.append("\\r");
                    break;
                case '\t':
                    out.append("\\t");
                    break;
                default:
                    if (c < 0x20) {
                        out.append(String.format("\\u%04x", (int) c));
                    } else {
                        out.append(c);
                    }
            }
        }
        return out.toString();
    }
}
