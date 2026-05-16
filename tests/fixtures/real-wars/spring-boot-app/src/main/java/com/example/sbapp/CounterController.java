/*
 * Tomcat-RS — Spring Boot 3.x WAR fixture: a stateful counter endpoint.
 *
 * The companion `HelloController` proves the **stateless** Spring MVC path
 * works end-to-end through the JVM bridge. This controller proves the
 * **stateful** path: that a single client, presenting the same `JSESSIONID`
 * cookie across two requests, observes a monotonically incrementing counter
 * stored in their `HttpSession`.
 *
 * Endpoint: GET /counter  →  {"count": <n>, "sessionId": "<JSESSIONID>"}
 *
 *   First hit  with no JSESSIONID  →  count=1, server emits Set-Cookie.
 *   Second hit with that JSESSIONID  →  count=2, no Set-Cookie (session reused).
 *
 * Note on attribute typing (bridge v1):
 *   The Tomcat-RS bridge stores session attributes as **String** values today
 *   — `HttpSession.setAttribute(String, Object)` round-trips through the Rust
 *   `SessionManager` which models attributes as strings. So the natural
 *   `(Integer) session.getAttribute("n")` cast would fail with a ClassCastException
 *   (or be silently null) the moment the second request retrieves the stored
 *   value. We work around this by serialising the counter as a decimal
 *   string and parsing it on read. Frameworks/apps that genuinely need
 *   object-typed session attributes should either coerce to string-safe types
 *   (as we do here) or wait for object-typed session attributes to land in
 *   the bridge.
 */
package com.example.sbapp;

import java.util.LinkedHashMap;
import java.util.Map;

import jakarta.servlet.http.HttpSession;

import org.springframework.web.bind.annotation.GetMapping;
import org.springframework.web.bind.annotation.RequestMapping;
import org.springframework.web.bind.annotation.RestController;

@RestController
@RequestMapping("/counter")
public class CounterController {

    /**
     * Increment the per-session counter, then return it together with the
     * `JSESSIONID` so the integration test can assert that two successive
     * requests with the same cookie land on the same session.
     *
     * `session.getAttribute("n")` is read as a `String` (see the class-level
     * note about bridge v1's string-typed session attributes), `null` on the
     * first hit. We parse → increment → re-serialise.
     */
    @GetMapping
    public Map<String, Object> incrementAndGet(HttpSession session) {
        // Bridge v1: session attributes are stored as String. The cast to
        // Integer would NPE / ClassCastException on the second request, so
        // we coerce explicitly through string parsing.
        String s = (String) session.getAttribute("n");
        int n = s == null ? 0 : Integer.parseInt(s);
        n += 1;
        session.setAttribute("n", String.valueOf(n));

        // LinkedHashMap preserves insertion order in the serialised JSON, so
        // the response body reads `{"count":...,"sessionId":"..."}` rather
        // than the unspecified ordering Map.of() would yield. Pure cosmetic
        // — the test asserts on substrings, not field order.
        Map<String, Object> body = new LinkedHashMap<>();
        body.put("count", n);
        body.put("sessionId", session.getId());
        return body;
    }
}
