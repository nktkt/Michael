package com.example.spring;

import java.io.IOException;

import jakarta.servlet.ServletException;
import jakarta.servlet.http.HttpServlet;
import jakarta.servlet.http.HttpServletRequest;
import jakarta.servlet.http.HttpServletResponse;

/**
 * Landing-page servlet for the {@code spring-boot-style} fixture. Serves a
 * tiny HTML index from {@code GET /}.
 */
public class RootServlet extends HttpServlet {

    @Override
    protected void doGet(HttpServletRequest req, HttpServletResponse resp)
            throws ServletException, IOException {
        resp.setStatus(HttpServletResponse.SC_OK);
        resp.setContentType("text/html; charset=utf-8");
        resp.getWriter().write(
                "<!doctype html><html><body>"
                        + "<h1>spring-boot-style</h1>"
                        + "<p>Integration-test fixture for the Tomcat-RS JVM bridge.</p>"
                        + "</body></html>");
    }
}
