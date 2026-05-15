package com.example.hello;

import jakarta.servlet.ServletException;
import jakarta.servlet.http.HttpServlet;
import jakarta.servlet.http.HttpServletRequest;
import jakarta.servlet.http.HttpServletResponse;

import java.io.IOException;

/**
 * Minimal documentation-only servlet source for the `hello/` fixture.
 *
 * The Tomcat-RS test harness does not compile this file; it exists so a human
 * reading the fixture knows exactly what `HelloServlet` is expected to do
 * when the JVM bridge is wired up.
 */
public class HelloServlet extends HttpServlet {
    @Override
    protected void doGet(HttpServletRequest req, HttpServletResponse resp) throws ServletException, IOException {
        resp.setContentType("text/plain; charset=utf-8");
        resp.getWriter().write("hello");
    }
}
