package org.apache.tomcatrs.bridge;

import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStreamReader;
import java.util.Collections;
import java.util.Enumeration;

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
