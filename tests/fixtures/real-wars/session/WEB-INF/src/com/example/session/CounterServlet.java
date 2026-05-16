package com.example.session;

import java.io.IOException;

import jakarta.servlet.ServletException;
import jakarta.servlet.http.HttpServlet;
import jakarta.servlet.http.HttpServletRequest;
import jakarta.servlet.http.HttpServletResponse;
import jakarta.servlet.http.HttpSession;

/**
 * Real, compilable {@code HttpServlet} exercising the bridge's
 * {@link HttpSession} surface end-to-end.
 *
 * <p>On every {@code GET /session}:
 * <ol>
 *   <li>{@code req.getSession()} → resolves an existing session via the
 *       request's {@code JSESSIONID} cookie, or creates a fresh one (and the
 *       bridge emits a single {@code Set-Cookie: JSESSIONID=...} on the
 *       response).</li>
 *   <li>Reads the {@code count} attribute as a {@code String} (the bridge
 *       stores session attribute values as strings in v1), increments it,
 *       writes it back via {@code setAttribute}.</li>
 *   <li>Responds {@code text/plain} with body {@code count=<n>}.</li>
 * </ol>
 *
 * <p>The test harness drives this twice — the first call with no
 * {@code JSESSIONID} cookie, the second with the cookie returned by the
 * first — and asserts both responses share the same id and observe the
 * incremented counter.
 */
public class CounterServlet extends HttpServlet {

    @Override
    protected void doGet(HttpServletRequest req, HttpServletResponse resp)
            throws ServletException, IOException {
        HttpSession session = req.getSession();
        Object raw = session.getAttribute("count");
        int n;
        if (raw == null) {
            n = 0;
        } else {
            try {
                n = Integer.parseInt(raw.toString());
            } catch (NumberFormatException e) {
                n = 0;
            }
        }
        n += 1;
        session.setAttribute("count", Integer.toString(n));

        resp.setStatus(HttpServletResponse.SC_OK);
        resp.setContentType("text/plain; charset=utf-8");
        resp.getWriter().write("count=" + n);
    }
}
