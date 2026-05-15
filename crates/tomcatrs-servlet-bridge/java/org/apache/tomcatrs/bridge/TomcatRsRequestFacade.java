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

    @Override
    public Object getAttribute(String name) {
        return NativeRequest.nativeGetAttribute(nativeRequestId, name);
    }

    @Override
    public void setAttribute(String name, Object value) {
        NativeRequest.nativeSetAttribute(
                nativeRequestId, name, value == null ? null : value.toString());
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
