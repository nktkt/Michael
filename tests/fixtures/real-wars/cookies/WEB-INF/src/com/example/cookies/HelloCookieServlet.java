package com.example.cookies;

import java.io.IOException;

import jakarta.servlet.ServletException;
import jakarta.servlet.http.Cookie;
import jakarta.servlet.http.HttpServlet;
import jakarta.servlet.http.HttpServletRequest;
import jakarta.servlet.http.HttpServletResponse;

/**
 * Real, compilable {@code HttpServlet} that round-trips inbound cookies back
 * into the response body so the Tomcat-RS JVM-bridge integration test can
 * assert that {@code TomcatRsRequestFacade.getCookies()} surfaces the values
 * the connector parsed from the {@code Cookie:} request header.
 *
 * <p>Response format: each parsed cookie is written as
 * {@code <name>=<value>} on its own line, in the order
 * {@code HttpServletRequest.getCookies()} returns them. When the request
 * carries no {@code Cookie} header the body is the literal string
 * {@code NO_COOKIES} so the test can distinguish "header absent" from "header
 * present but every pair malformed".
 */
public class HelloCookieServlet extends HttpServlet {

    @Override
    protected void doGet(HttpServletRequest req, HttpServletResponse resp)
            throws ServletException, IOException {
        Cookie[] cookies = req.getCookies();
        resp.setStatus(HttpServletResponse.SC_OK);
        resp.setContentType("text/plain; charset=utf-8");
        if (cookies == null) {
            resp.getWriter().write("NO_COOKIES");
            return;
        }
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < cookies.length; i++) {
            if (i > 0) {
                sb.append('\n');
            }
            sb.append(cookies[i].getName()).append('=').append(cookies[i].getValue());
        }
        resp.getWriter().write(sb.toString());
    }
}
