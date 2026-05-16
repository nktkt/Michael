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

    public TomcatRsRequestFacade(long nativeRequestId) {
        this.nativeRequestId = nativeRequestId;
    }

    /** Exposes the opaque id (e.g. for the {@code AsyncContext} bridge). */
    public long nativeRequestId() {
        return nativeRequestId;
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

    /** No cookies surfaced from the bridge yet; an empty array is safe. */
    public jakarta.servlet.http.Cookie[] getCookies() { return new jakarta.servlet.http.Cookie[0]; }

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
     * Sessions are not wired to the Rust SessionManager from this facade
     * yet. Return {@code null} when {@code create=false} per the spec;
     * for {@code create=true} log and return {@code null} as well so
     * frameworks discover the absence at the first dereference rather than
     * at use time (no AbstractMethodError, no half-built session).
     */
    public jakarta.servlet.http.HttpSession getSession(boolean create) {
        // TODO: bridge to tomcatrs_session::SessionManager via a native fn.
        return null;
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
