package org.apache.tomcatrs.bridge;

import java.io.IOException;
import java.io.PrintWriter;
import java.util.Map;

import jakarta.servlet.ServletOutputStream;
import jakarta.servlet.WriteListener;
import jakarta.servlet.http.HttpServletResponse;

/**
 * Thin {@link HttpServletResponse} facade backed by a Rust-side
 * {@code ResponseHandle}.
 *
 * <p><strong>Scaffold.</strong> Only the methods that demonstrate the bridge
 * design are fleshed out; the remaining {@code HttpServletResponse} members are
 * omitted for brevity and would delegate to {@link NativeResponse} in the same
 * style. This file is not compiled by cargo — see {@code java/README.md}.
 *
 * <p>The facade holds a single opaque {@code long}, {@link #nativeResponseId}.
 * Status and headers are {@code native} mutations of the Rust response sink;
 * the {@link ServletOutputStream} drains directly into that sink without
 * buffering the whole response on the Java heap.
 */
public final class TomcatRsResponseFacade implements HttpServletResponse {

    /** Opaque handle into the Rust-side response registry. */
    private final long nativeResponseId;

    private ServletOutputStream outputStream;
    private PrintWriter writer;

    public TomcatRsResponseFacade(long nativeResponseId) {
        this.nativeResponseId = nativeResponseId;
    }

    /** Exposes the opaque id (e.g. for the {@code AsyncContext} bridge). */
    public long nativeResponseId() {
        return nativeResponseId;
    }

    // --- Status & headers: native mutations of the Rust sink -----------------

    @Override
    public void setStatus(int sc) {
        NativeResponse.nativeSetStatus(nativeResponseId, sc);
        this.statusCode = sc;
    }

    @Override
    public void setHeader(String name, String value) {
        NativeResponse.nativeSetHeader(nativeResponseId, name, value);
    }

    @Override
    public void addHeader(String name, String value) {
        NativeResponse.nativeAddHeader(nativeResponseId, name, value);
    }

    @Override
    public void setContentType(String type) {
        NativeResponse.nativeSetHeader(nativeResponseId, "Content-Type", type);
    }

    @Override
    public void setContentLength(int len) {
        NativeResponse.nativeSetHeader(
                nativeResponseId, "Content-Length", Integer.toString(len));
    }

    @Override
    public boolean isCommitted() {
        return NativeResponse.nativeIsCommitted(nativeResponseId);
    }

    @Override
    public void flushBuffer() throws IOException {
        NativeResponse.nativeCommit(nativeResponseId);
    }

    @Override
    public void sendError(int sc, String msg) throws IOException {
        NativeResponse.nativeSetStatus(nativeResponseId, sc);
        if (msg != null) {
            getOutputStream().write(msg.getBytes());
        }
        NativeResponse.nativeCommit(nativeResponseId);
    }

    @Override
    public void sendError(int sc) throws IOException {
        sendError(sc, null);
    }

    // --- Streaming body ------------------------------------------------------

    @Override
    public ServletOutputStream getOutputStream() {
        if (outputStream == null) {
            outputStream = new NativeServletOutputStream(nativeResponseId);
        }
        return outputStream;
    }

    @Override
    public PrintWriter getWriter() {
        if (writer == null) {
            writer = new PrintWriter(getOutputStream());
        }
        return writer;
    }

    /**
     * Flushes any buffered writer + the underlying native sink. The Rust
     * side calls this through {@code ServletDispatcher} once the servlet
     * returns, so the standard "container flushes for you" Servlet contract
     * holds even though the user code did not explicitly call flush.
     */
    public void flushAll() throws IOException {
        if (writer != null) {
            writer.flush();
        }
        if (outputStream != null) {
            outputStream.flush();
        }
        flushBuffer();
    }

    // -----------------------------------------------------------------------
    // Additional HttpServletResponse surface needed when the real Jakarta
    // Servlet API jar is on the runtime classpath (Spring MVC, security
    // filters, etc. exercise the full interface). Safe-default
    // implementations — keep the JVM dispatch happy and let frameworks run
    // to completion. Real wiring to Rust-side state lands as needed.
    // -----------------------------------------------------------------------

    /** Tracked locally so {@code getStatus()} returns the same value
     *  callers set via {@code setStatus()}/{@code sendError()}. */
    private int statusCode = 200;
    private String characterEncoding = "UTF-8";
    private java.util.Locale locale = java.util.Locale.getDefault();
    private long contentLengthLong = -1L;
    private int bufferSize = 8192;

    public int getStatus() { return statusCode; }
    public String getHeader(String name) { return null; }
    public java.util.Collection<String> getHeaders(String name) { return java.util.Collections.emptyList(); }
    public java.util.Collection<String> getHeaderNames() { return java.util.Collections.emptyList(); }
    public boolean containsHeader(String name) { return false; }
    public void setIntHeader(String name, int value) { setHeader(name, Integer.toString(value)); }
    public void addIntHeader(String name, int value) { addHeader(name, Integer.toString(value)); }
    public void setDateHeader(String name, long date) { setHeader(name, Long.toString(date)); }
    public void addDateHeader(String name, long date) { addHeader(name, Long.toString(date)); }
    public void addCookie(jakarta.servlet.http.Cookie cookie) {
        if (cookie == null) return;
        StringBuilder sb = new StringBuilder();
        sb.append(cookie.getName()).append('=').append(cookie.getValue() == null ? "" : cookie.getValue());
        if (cookie.getPath() != null) sb.append("; Path=").append(cookie.getPath());
        if (cookie.getDomain() != null) sb.append("; Domain=").append(cookie.getDomain());
        if (cookie.getMaxAge() >= 0) sb.append("; Max-Age=").append(cookie.getMaxAge());
        if (cookie.getSecure()) sb.append("; Secure");
        if (cookie.isHttpOnly()) sb.append("; HttpOnly");
        addHeader("Set-Cookie", sb.toString());
    }
    public String encodeURL(String url) { return url; }
    public String encodeRedirectURL(String url) { return url; }
    @Deprecated public String encodeUrl(String url) { return encodeURL(url); }
    @Deprecated public String encodeRedirectUrl(String url) { return encodeRedirectURL(url); }
    public void sendRedirect(String location) throws IOException { sendRedirect(location, 302, true); }
    public void sendRedirect(String location, int sc) throws IOException { sendRedirect(location, sc, true); }
    public void sendRedirect(String location, boolean clearBuffer) throws IOException { sendRedirect(location, 302, clearBuffer); }
    public void sendRedirect(String location, int sc, boolean clearBuffer) throws IOException {
        setStatus(sc);
        setHeader("Location", location == null ? "" : location);
        flushBuffer();
    }
    public String getCharacterEncoding() { return characterEncoding; }
    public void setCharacterEncoding(String charset) { this.characterEncoding = charset; }
    public String getContentType() { return getHeader("Content-Type"); }
    public void setContentLengthLong(long len) {
        this.contentLengthLong = len;
        setHeader("Content-Length", Long.toString(len));
    }
    public int getBufferSize() { return bufferSize; }
    public void setBufferSize(int size) { this.bufferSize = size; }
    public void resetBuffer() { /* best-effort: no buffered body retained */ }
    public void reset() { resetBuffer(); statusCode = 200; }
    public java.util.Locale getLocale() { return locale; }
    public void setLocale(java.util.Locale locale) { if (locale != null) this.locale = locale; }
    @Deprecated public void setStatus(int sc, String sm) { setStatus(sc); }
    public Map<String, String> getTrailerFields() { return java.util.Collections.emptyMap(); }
    public void setTrailerFields(java.util.function.Supplier<Map<String, String>> supplier) {}

    /**
     * {@link ServletOutputStream} that forwards every write straight into the
     * Rust response sink via {@link NativeResponse#nativeWriteBody}.
     */
    private static final class NativeServletOutputStream extends ServletOutputStream {
        private final long nativeResponseId;
        private final byte[] one = new byte[1];

        NativeServletOutputStream(long nativeResponseId) {
            this.nativeResponseId = nativeResponseId;
        }

        @Override
        public void write(int b) throws IOException {
            one[0] = (byte) b;
            write(one, 0, 1);
        }

        @Override
        public void write(byte[] b, int off, int len) throws IOException {
            NativeResponse.nativeWriteBody(nativeResponseId, b, off, len);
        }

        @Override
        public boolean isReady() {
            return true;
        }

        @Override
        public void setWriteListener(WriteListener writeListener) {
            // Non-blocking I/O listeners are not wired up in the scaffold.
            throw new UnsupportedOperationException("setWriteListener not implemented in scaffold");
        }
    }

    // --- Remaining HttpServletResponse members omitted in the scaffold -------
    // Each would delegate to a `NativeResponse.native*` method in the same
    // style as the mutators above.
}
