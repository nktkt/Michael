package org.apache.tomcatrs.bridge;

import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStreamReader;
import java.net.URLDecoder;
import java.nio.charset.Charset;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Enumeration;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

import jakarta.servlet.ReadListener;
import jakarta.servlet.ServletInputStream;
import jakarta.servlet.http.HttpServletRequest;

/**
 * Thin {@link HttpServletRequest} facade backed by a Rust-side
 * {@code RequestHandle}.
 *
 * <p><strong>Scaffold.</strong> Only the methods that demonstrate the bridge
 * design are fleshed out; the remaining {@code HttpServletRequest} members are
 * omitted for brevity and would delegate to {@link NativeRequest} in the same
 * style. This file is not compiled by cargo — see {@code java/README.md}.
 *
 * <p>The facade holds a single opaque {@code long}, {@link #nativeRequestId}.
 * That id is the only value that crosses JNI; every accessor pulls just the
 * value it needs from Rust, lazily.
 */
public final class TomcatRsRequestFacade implements HttpServletRequest {

    /** Opaque handle into the Rust-side request registry. */
    private final long nativeRequestId;

    /** Lazily-created streaming view over the Rust request body. */
    private ServletInputStream inputStream;

    /** Lazily-parsed query-string parameter map (RFC 3986 + form-urlencoded). */
    private Map<String, String[]> parameterCache;

    /** Per-request character encoding override (Servlet API). */
    private String characterEncoding;

    /**
     * Lazily-parsed cookie array, materialised on the first call to
     * {@link #getCookies()}. A non-null sentinel value (possibly an empty
     * array) signals that parsing has already run for this request, so a
     * subsequent call cannot pay the cost a second time.
     *
     * <p>The Servlet API contract is that {@code getCookies()} returns
     * {@code null} when the request carried no {@code Cookie} header at all
     * — see {@link #getCookies()}; the {@link #cookiesParsed} flag
     * disambiguates "not parsed yet" from "parsed, but the request had no
     * cookies" when {@link #cookies} is {@code null}.
     */
    private jakarta.servlet.http.Cookie[] cookies;
    private boolean cookiesParsed;

    /**
     * Opaque handle to the Rust-side webapp context entry, used by the
     * session-resolution natives to find this webapp's
     * {@code SessionManager}. {@code 0} means "no context attached" — the
     * facade still works but {@link #getSession(boolean)} returns
     * {@code null}.
     */
    private final long nativeContextId;

    /**
     * Opaque handle to the Rust-side response sink for this request, used by
     * {@link #getSession(boolean)} to emit the
     * {@code Set-Cookie: JSESSIONID=...} header on a freshly-created session.
     * {@code 0} means "no response attached" — getSession still resolves a
     * session but cannot drive the cookie back to the client (the test
     * harness reads it off the registered handle instead).
     */
    private final long nativeResponseId;

    /**
     * Cached {@code HttpSession} bound to this request — set lazily on the
     * first {@link #getSession(boolean)} call so the session is resolved at
     * most once per request, matching the Servlet spec.
     */
    private jakarta.servlet.http.HttpSession sessionCache;

    /**
     * Legacy single-arg constructor: builds a facade without a context or
     * response handle. Sessions resolve to {@code null} on this path — the
     * facade is functional for non-session servlet tests and for callers
     * that have not yet been updated to the 3-arg form.
     */
    public TomcatRsRequestFacade(long nativeRequestId) {
        this(nativeRequestId, 0L, 0L);
    }

    /**
     * Full constructor: attaches the request to its webapp context (so
     * session resolution can find the per-context {@code SessionManager})
     * and to its response handle (so {@code getSession(true)} on a freshly
     * created session can emit a {@code Set-Cookie} header).
     */
    public TomcatRsRequestFacade(long nativeRequestId, long nativeContextId, long nativeResponseId) {
        this.nativeRequestId = nativeRequestId;
        this.nativeContextId = nativeContextId;
        this.nativeResponseId = nativeResponseId;
    }

    /** Exposes the opaque id (e.g. for the {@code AsyncContext} bridge). */
    public long nativeRequestId() {
        return nativeRequestId;
    }

    /** Exposes the context id (used by session-resolution wiring). */
    public long nativeContextId() {
        return nativeContextId;
    }

    /** Exposes the response id (used by session-cookie emission). */
    public long nativeResponseId() {
        return nativeResponseId;
    }

    // --- Lazy metadata accessors: one JNI call each --------------------------

    @Override
    public String getMethod() {
        return NativeRequest.nativeGetMethod(nativeRequestId);
    }

    @Override
    public String getRequestURI() {
        return NativeRequest.nativeGetRequestUri(nativeRequestId);
    }

    @Override
    public String getQueryString() {
        return NativeRequest.nativeGetQueryString(nativeRequestId);
    }

    @Override
    public String getProtocol() {
        return NativeRequest.nativeGetProtocol(nativeRequestId);
    }

    @Override
    public String getScheme() {
        return NativeRequest.nativeGetScheme(nativeRequestId);
    }

    @Override
    public String getRemoteAddr() {
        return NativeRequest.nativeGetRemoteAddr(nativeRequestId);
    }

    @Override
    public String getHeader(String name) {
        // The canonical lazy-materialization path: one header, one JNI call.
        return NativeRequest.nativeGetHeader(nativeRequestId, name);
    }

    @Override
    public Enumeration<String> getHeaderNames() {
        String[] names = NativeRequest.nativeGetHeaderNames(nativeRequestId);
        return Collections.enumeration(java.util.Arrays.asList(names));
    }

    /**
     * Per-request attribute map kept on the Java side so callers can store
     * arbitrary {@code Object} values (Spring's {@code RequestPath},
     * caching helpers, etc.). The Rust attribute store is String-only and
     * targets cross-component data, not per-request bookkeeping — Servlet
     * spec attributes live here.
     */
    private final java.util.concurrent.ConcurrentHashMap<String, Object> attributes =
            new java.util.concurrent.ConcurrentHashMap<>();

    @Override
    public Object getAttribute(String name) {
        // Only return the Java-side per-request map. The Rust attribute
        // store is String-typed and would break framework code that
        // stores typed objects (e.g. Spring's RequestPath, internal Set
        // markers) — `String != Set` blows up at the cast site otherwise.
        return attributes.get(name);
    }

    @Override
    public void setAttribute(String name, Object value) {
        if (value == null) {
            attributes.remove(name);
        } else {
            attributes.put(name, value);
        }
        // Also mirror string-typed values into the Rust attribute store so
        // observers on the Rust side (logging, valves) see them. Non-string
        // values stay Java-side only — the bridge cannot ferry arbitrary
        // Object types across JNI today.
        if (value == null || value instanceof CharSequence) {
            NativeRequest.nativeSetAttribute(
                    nativeRequestId, name, value == null ? null : value.toString());
        }
    }

    // --- Streaming body ------------------------------------------------------

    @Override
    public ServletInputStream getInputStream() {
        if (inputStream == null) {
            inputStream = new NativeServletInputStream(nativeRequestId);
        }
        return inputStream;
    }

    @Override
    public BufferedReader getReader() throws IOException {
        return new BufferedReader(new InputStreamReader(getInputStream()));
    }

    // --- Parameter handling (query string + form-urlencoded body) -----------

    @Override
    public String getParameter(String name) {
        String[] vals = ensureParameterMap().get(name);
        return vals == null || vals.length == 0 ? null : vals[0];
    }

    @Override
    public Enumeration<String> getParameterNames() {
        return Collections.enumeration(ensureParameterMap().keySet());
    }

    @Override
    public String[] getParameterValues(String name) {
        return ensureParameterMap().get(name);
    }

    @Override
    public Map<String, String[]> getParameterMap() {
        return Collections.unmodifiableMap(ensureParameterMap());
    }

    private Map<String, String[]> ensureParameterMap() {
        Map<String, String[]> cached = parameterCache;
        if (cached != null) {
            return cached;
        }
        Map<String, List<String>> work = new LinkedHashMap<>();
        decodeInto(work, NativeRequest.nativeGetQueryString(nativeRequestId));
        Map<String, String[]> built = new LinkedHashMap<>();
        for (Map.Entry<String, List<String>> e : work.entrySet()) {
            built.put(e.getKey(), e.getValue().toArray(new String[0]));
        }
        parameterCache = built;
        return built;
    }

    private void decodeInto(Map<String, List<String>> out, String raw) {
        if (raw == null || raw.isEmpty()) {
            return;
        }
        Charset cs = characterEncoding != null
                ? Charset.forName(characterEncoding)
                : StandardCharsets.UTF_8;
        for (String pair : raw.split("&")) {
            if (pair.isEmpty()) continue;
            int eq = pair.indexOf('=');
            String key = eq < 0 ? pair : pair.substring(0, eq);
            String val = eq < 0 ? "" : pair.substring(eq + 1);
            try {
                key = URLDecoder.decode(key, cs);
                val = URLDecoder.decode(val, cs);
            } catch (Exception ignore) {
                // Keep the raw value if decoding fails.
            }
            out.computeIfAbsent(key, k -> new ArrayList<>()).add(val);
        }
    }

    // --- Encoding + content-length + server info ----------------------------

    @Override
    public String getCharacterEncoding() {
        return characterEncoding;
    }

    @Override
    public void setCharacterEncoding(String encoding) {
        this.characterEncoding = encoding;
    }

    @Override
    public int getContentLength() {
        long n = getContentLengthLong();
        return n > Integer.MAX_VALUE ? Integer.MAX_VALUE : (int) n;
    }

    @Override
    public long getContentLengthLong() {
        return NativeRequest.nativeGetContentLength(nativeRequestId);
    }

    @Override
    public String getContentType() {
        return getHeader("Content-Type");
    }

    @Override
    public String getServerName() {
        String host = getHeader("Host");
        if (host == null) return "localhost";
        int colon = host.lastIndexOf(':');
        // Strip port for IPv4 / hostname; leave IPv6 (`[::1]`) alone.
        if (colon > 0 && !host.startsWith("[")) {
            return host.substring(0, colon);
        }
        return host;
    }

    @Override
    public int getServerPort() {
        String host = getHeader("Host");
        if (host != null && !host.startsWith("[")) {
            int colon = host.lastIndexOf(':');
            if (colon > 0) {
                try {
                    return Integer.parseInt(host.substring(colon + 1));
                } catch (NumberFormatException ignore) {
                    // fall through
                }
            }
        }
        return "https".equalsIgnoreCase(getScheme()) ? 443 : 80;
    }

    @Override
    public String getRemoteHost() {
        return getRemoteAddr();
    }

    @Override
    public boolean isSecure() {
        return "https".equalsIgnoreCase(getScheme());
    }

    // -----------------------------------------------------------------------
    // Additional HttpServletRequest surface implemented against the real
    // Jakarta Servlet API jar (loaded at runtime alongside the bridge JAR).
    //
    // The bridge stubs only declare the subset above; at runtime the real
    // jar's HttpServletRequest is what `instanceof` / dispatch see, so any
    // abstract method we do not implement throws `AbstractMethodError`
    // during framework processing (Spring MVC, security filters, etc.).
    // The methods below cover the surface Spring's request-processing
    // pipeline exercises during a typical dispatch — none of them
    // currently delegate to the Rust side; they return safe defaults that
    // let the framework run without crashing. Frameworks that genuinely
    // need (for example) real session integration should treat these as
    // the integration seam to push behaviour back into Rust as needed.
    // -----------------------------------------------------------------------

    public String getRequestURL_String() { return null; } // sentinel for stub compile

    /** Spring needs a non-null request URL for log lines + abs-link building. */
    public StringBuffer getRequestURL() {
        StringBuffer sb = new StringBuffer();
        sb.append(getScheme()).append("://")
          .append(getServerName()).append(':').append(getServerPort())
          .append(getRequestURI());
        return sb;
    }

    /**
     * Parse the {@code Cookie:} request header(s) into an array of
     * {@link jakarta.servlet.http.Cookie} pairs. Lazy: the parse runs at most
     * once per request; the result is cached in {@link #cookies}.
     *
     * <p>Servlet API contract: returns {@code null} when the request carried
     * no {@code Cookie} header at all (callers — Spring CSRF, session-id
     * extraction, etc. — special-case the {@code null}-vs-empty distinction).
     * Returns an empty array if a header was present but every pair in it was
     * malformed and skipped.
     *
     * <p>The bridge currently surfaces only the <em>first</em>
     * {@code Cookie} header via {@link NativeRequest#nativeGetHeader}; in
     * practice browsers and HTTP clients concatenate cookies into a single
     * header value, so this is sufficient for real-world traffic. A
     * multi-header bridge would require a {@code nativeGetHeaders} shim — see
     * the documented gap in {@link #getHeaders(String)}.
     *
     * <p>Inbound cookies have no attributes by the HTTP spec — {@code Path},
     * {@code Secure}, {@code HttpOnly}, {@code Domain}, {@code Max-Age} only
     * travel on outbound {@code Set-Cookie} response headers (RFC 6265 §4.2)
     * — so the returned cookies expose only {@code name} and {@code value}.
     */
    public jakarta.servlet.http.Cookie[] getCookies() {
        if (!cookiesParsed) {
            String header = NativeRequest.nativeGetHeader(nativeRequestId, "Cookie");
            cookies = header == null ? null : parseCookieHeader(header);
            cookiesParsed = true;
        }
        return cookies;
    }

    /**
     * Parse a single {@code Cookie:} header value into an array of
     * {@link jakarta.servlet.http.Cookie} name/value pairs.
     *
     * <p>Implementation follows lenient [RFC 6265] §5.4 parsing, matching the
     * pure-Rust {@code tomcatrs_coyote::cookies::parse_cookie_header} core but
     * kept Java-side so the bridge facade stays self-contained (one JNI call
     * per request to fetch the header, then pure-Java parsing — no second
     * round trip and no Rust-side {@code String[]} marshalling).
     *
     * <p>Rules:
     * <ul>
     *   <li>Pairs are separated by {@code ;}.</li>
     *   <li>Surrounding whitespace around each pair, the name, and the value
     *       is trimmed.</li>
     *   <li>A value wholly wrapped in double quotes has those quotes stripped
     *       (RFC 2616 quoted-string).</li>
     *   <li>RFC 2965 reserved attributes ({@code $Version}, {@code $Path},
     *       {@code $Domain}) and any other pair whose name starts with
     *       {@code $} are silently dropped — those are leftover request-cookie
     *       metadata, not application cookies.</li>
     *   <li>Pairs without {@code =}, with empty names, or with names that
     *       contain RFC 2616 separator characters / control characters are
     *       silently skipped — the parse never throws.</li>
     * </ul>
     *
     * <p>Always returns a non-{@code null} array. Returns
     * {@code new Cookie[0]} when every pair is malformed.
     */
    static jakarta.servlet.http.Cookie[] parseCookieHeader(String headerValue) {
        if (headerValue == null || headerValue.isEmpty()) {
            return new jakarta.servlet.http.Cookie[0];
        }
        java.util.ArrayList<jakarta.servlet.http.Cookie> out = new java.util.ArrayList<>();
        int len = headerValue.length();
        int i = 0;
        while (i < len) {
            int semi = headerValue.indexOf(';', i);
            int end = semi < 0 ? len : semi;
            // Skip surrounding ASCII whitespace within this pair slice.
            int start = i;
            while (start < end && isSpace(headerValue.charAt(start))) {
                start++;
            }
            int stop = end;
            while (stop > start && isSpace(headerValue.charAt(stop - 1))) {
                stop--;
            }
            if (start < stop) {
                int eq = headerValue.indexOf('=', start);
                if (eq >= 0 && eq < stop) {
                    String name = trimAscii(headerValue, start, eq);
                    String value = trimAscii(headerValue, eq + 1, stop);
                    // Strip a single layer of surrounding double quotes.
                    if (value.length() >= 2
                            && value.charAt(0) == '"'
                            && value.charAt(value.length() - 1) == '"') {
                        value = value.substring(1, value.length() - 1);
                    }
                    // Skip RFC 2965 reserved leftovers ($Version, $Path, $Domain).
                    if (!name.isEmpty() && name.charAt(0) != '$' && isValidCookieName(name)) {
                        try {
                            out.add(new jakarta.servlet.http.Cookie(name, value));
                        } catch (IllegalArgumentException ignore) {
                            // The real jakarta.servlet.Cookie constructor
                            // enforces RFC 2109 token rules and throws for
                            // invalid names; the bridge stub does not, but
                            // isValidCookieName above guards the common case
                            // either way. Skip silently per the contract.
                        }
                    }
                }
            }
            i = (semi < 0) ? len : semi + 1;
        }
        return out.toArray(new jakarta.servlet.http.Cookie[0]);
    }

    /** ASCII whitespace test — RFC 6265 OWS (`SP` / `HTAB`). */
    private static boolean isSpace(char c) {
        return c == ' ' || c == '\t';
    }

    /** Substring + trim, allocating only the final {@link String}. */
    private static String trimAscii(String s, int from, int to) {
        while (from < to && isSpace(s.charAt(from))) {
            from++;
        }
        while (to > from && isSpace(s.charAt(to - 1))) {
            to--;
        }
        return s.substring(from, to);
    }

    /**
     * Reject obviously-invalid cookie names so the real
     * {@code jakarta.servlet.http.Cookie} constructor's RFC 2109 token check
     * does not throw on a malformed pair. The full RFC 2616 separator set is
     * checked plus control characters; anything else is allowed (the bridge
     * stub permits any non-empty name).
     */
    private static boolean isValidCookieName(String name) {
        for (int i = 0; i < name.length(); i++) {
            char c = name.charAt(i);
            if (c <= 0x20 || c >= 0x7F) {
                return false;
            }
            switch (c) {
                case '(': case ')': case '<': case '>': case '@':
                case ',': case ';': case ':': case '\\': case '"':
                case '/': case '[': case ']': case '?': case '=':
                case '{': case '}':
                    return false;
                default:
                    // valid token char
            }
        }
        return true;
    }

    /** Context path = the webapp's mount point. Best-effort empty default. */
    public String getContextPath() { return ""; }

    /**
     * Spring uses this to compute the request URL with the dispatcher prefix.
     * Best-effort: report the full request URI (Spring tolerates this and
     * routes correctly on a single-DispatcherServlet app).
     */
    public String getServletPath() { return getRequestURI() == null ? "" : getRequestURI(); }

    /** Best-effort: no path-info routing layered on top of the servlet path. */
    public String getPathInfo() { return null; }

    /** Best-effort: same as {@link #getPathInfo()}. */
    public String getPathTranslated() { return null; }

    /** Best-effort: no authentication has been performed. */
    public String getRemoteUser() { return null; }

    /** Best-effort: no authenticated principal. */
    public java.security.Principal getUserPrincipal() { return null; }

    /** Best-effort: role-membership not yet wired to the realm. */
    public boolean isUserInRole(String role) { return false; }

    /** Best-effort: no Servlet-spec auth scheme applied. */
    public String getAuthType() { return null; }

    /** Returns -1 when the header is missing / not a date (spec). */
    public long getDateHeader(String name) { return -1L; }

    /** Returns -1 when the header is missing / not an int (spec). */
    public int getIntHeader(String name) {
        String v = getHeader(name);
        if (v == null) return -1;
        try { return Integer.parseInt(v); } catch (NumberFormatException e) { return -1; }
    }

    /**
     * The full header (single value): the bridge does not expose multiple
     * values per header today, so the enumeration contains at most one
     * element (or is empty when the header is absent).
     */
    public Enumeration<String> getHeaders(String name) {
        String v = getHeader(name);
        if (v == null) return Collections.emptyEnumeration();
        return Collections.enumeration(Collections.singletonList(v));
    }

    /** Best-effort: no `;jsessionid` extraction yet. */
    public String getRequestedSessionId() { return null; }

    public boolean isRequestedSessionIdValid() { return false; }
    public boolean isRequestedSessionIdFromCookie() { return false; }
    public boolean isRequestedSessionIdFromURL() { return false; }
    @Deprecated public boolean isRequestedSessionIdFromUrl() { return isRequestedSessionIdFromURL(); }

    /**
     * Resolve (or create) the {@code HttpSession} bound to this request.
     *
     * <p>End-to-end flow:
     * <ol>
     *   <li>If a session has already been resolved for this request, return
     *       the cached one — the Servlet spec guarantees at most one
     *       resolution per request.</li>
     *   <li>Otherwise, ask
     *       {@link NativeRequest#nativeResolveOrCreateSession} to scan the
     *       request's {@code Cookie:} headers for {@code JSESSIONID} and
     *       resolve / create through the per-context
     *       {@code SessionManager}.</li>
     *   <li>If the resolver freshly created the session (i.e.
     *       {@link NativeRequest#nativeIsNewSession} returns {@code true}),
     *       emit a {@code Set-Cookie: JSESSIONID=...} header on the response
     *       so the client adopts the cookie. Requires
     *       {@link #nativeResponseId} {@code != 0}; the legacy single-arg
     *       constructor leaves it as {@code 0}, which the bridge accepts
     *       but logs the cookie-drop on the Rust side.</li>
     * </ol>
     *
     * <p><strong>Honest gap (v1):</strong> session attribute values are
     * String-typed end-to-end. {@link TomcatRsHttpSession#setAttribute}
     * stringifies its argument; framework code that stores typed objects
     * (Spring Security's {@code SecurityContext}, etc.) will get a
     * {@code String} when reading back. The Java side
     * {@code TomcatRsHttpSession} doc-comment notes this; future releases
     * will introduce a typed attribute value across the JNI boundary.
     *
     * <p><strong>Honest gap (v1):</strong>
     * {@code HttpSessionListener.sessionCreated} is not driven from this
     * facade; webapps relying on session-creation listeners will not have
     * those listeners called.
     */
    public jakarta.servlet.http.HttpSession getSession(boolean create) {
        if (sessionCache != null) {
            return sessionCache;
        }
        long sid = NativeRequest.nativeResolveOrCreateSession(
                nativeRequestId, nativeContextId, create);
        if (sid == 0L) {
            return null;
        }
        // If the resolver freshly created the session, ensure the client
        // adopts the cookie via a single Set-Cookie header on the response.
        if (nativeResponseId != 0L && NativeRequest.nativeIsNewSession(sid)) {
            String cookieValue = NativeRequest.nativeNewSessionCookie(
                    nativeContextId, sid);
            if (cookieValue != null && !cookieValue.isEmpty()) {
                NativeResponse.nativeAddHeader(
                        nativeResponseId, "Set-Cookie", cookieValue);
            }
        }
        sessionCache = new TomcatRsHttpSession(sid, null);
        return sessionCache;
    }

    public jakarta.servlet.http.HttpSession getSession() { return getSession(true); }

    public String changeSessionId() { return null; }

    /** Multipart not wired yet. */
    public java.util.Collection<jakarta.servlet.http.Part> getParts() { return Collections.emptyList(); }
    public jakarta.servlet.http.Part getPart(String name) { return null; }

    /** Async not wired yet. */
    public boolean isAsyncStarted() { return false; }
    public boolean isAsyncSupported() { return false; }
    public jakarta.servlet.AsyncContext getAsyncContext() { throw new IllegalStateException("async not started"); }
    public jakarta.servlet.AsyncContext startAsync() { throw new IllegalStateException("async not supported in bridge v1"); }
    public jakarta.servlet.AsyncContext startAsync(jakarta.servlet.ServletRequest req, jakarta.servlet.ServletResponse res) { return startAsync(); }

    /** Best-effort: default request locale. */
    public java.util.Locale getLocale() { return java.util.Locale.getDefault(); }
    public Enumeration<java.util.Locale> getLocales() {
        return Collections.enumeration(Collections.singletonList(java.util.Locale.getDefault()));
    }

    /** Best-effort: no Servlet-spec dispatcher type other than REQUEST. */
    public jakarta.servlet.DispatcherType getDispatcherType() {
        try {
            return (jakarta.servlet.DispatcherType) jakarta.servlet.DispatcherType.class
                    .getField("REQUEST").get(null);
        } catch (ReflectiveOperationException e) {
            return null;
        }
    }

    /** Best-effort identity strings — Servlet 6 additions. */
    public String getRequestId() { return Long.toString(nativeRequestId); }
    public String getProtocolRequestId() { return ""; }
    public jakarta.servlet.ServletConnection getServletConnection() { return null; }
    public jakarta.servlet.RequestDispatcher getRequestDispatcher(String path) { return null; }

    /** Best-effort: no peer port surfaced. */
    public int getRemotePort() { return 0; }
    public String getLocalName() { return "localhost"; }
    public String getLocalAddr() { return "127.0.0.1"; }
    public int getLocalPort() { return getServerPort(); }

    /** Authentication helpers — not wired yet. */
    public boolean authenticate(jakarta.servlet.http.HttpServletResponse response) { return false; }
    public void login(String username, String password) {}
    public void logout() {}

    /** Upgrade (WebSocket etc.) handled by the connector path, not here. */
    public <T extends jakarta.servlet.http.HttpUpgradeHandler> T upgrade(Class<T> handlerClass) { return null; }

    /** Remove a per-request attribute from the Java-side map. */
    public void removeAttribute(String name) {
        attributes.remove(name);
        NativeRequest.nativeSetAttribute(nativeRequestId, name, null);
    }

    /** Iterate the names of every per-request attribute currently set. */
    public Enumeration<String> getAttributeNames() {
        return Collections.enumeration(attributes.keySet());
    }

    /**
     * {@link ServletInputStream} that pulls bytes from the Rust request body
     * on demand via {@link NativeRequest#nativeReadBody}. No whole-body buffer
     * is materialised on the Java heap.
     */
    private static final class NativeServletInputStream extends ServletInputStream {
        private final long nativeRequestId;
        private final byte[] one = new byte[1];

        NativeServletInputStream(long nativeRequestId) {
            this.nativeRequestId = nativeRequestId;
        }

        @Override
        public int read() throws IOException {
            int n = read(one, 0, 1);
            return n == -1 ? -1 : (one[0] & 0xFF);
        }

        @Override
        public int read(byte[] b, int off, int len) throws IOException {
            return NativeRequest.nativeReadBody(nativeRequestId, b, off, len);
        }

        @Override
        public boolean isFinished() {
            return NativeRequest.nativeBodyRemaining(nativeRequestId) == 0;
        }

        @Override
        public boolean isReady() {
            return true;
        }

        @Override
        public void setReadListener(ReadListener readListener) {
            // Non-blocking I/O listeners are not wired up in the scaffold.
            throw new UnsupportedOperationException("setReadListener not implemented in scaffold");
        }
    }

    // --- Remaining HttpServletRequest members omitted in the scaffold --------
    // Each would delegate to a `NativeRequest.native*` method in the same
    // style as the accessors above.
}
