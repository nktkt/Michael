package com.example.filtered;

import jakarta.servlet.ServletException;
import jakarta.servlet.http.HttpServlet;
import jakarta.servlet.http.HttpServletRequest;
import jakarta.servlet.http.HttpServletResponse;

import java.io.IOException;

/**
 * Documentation-only servlet source for the `filtered/` fixture.
 * Echoes a query parameter back to the client.
 */
public class EchoServlet extends HttpServlet {
    @Override
    protected void doGet(HttpServletRequest req, HttpServletResponse resp) throws ServletException, IOException {
        String value = req.getParameter("q");
        resp.setContentType("text/plain; charset=" + req.getCharacterEncoding());
        resp.getWriter().write(value == null ? "" : value);
    }
}
