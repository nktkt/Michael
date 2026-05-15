package org.apache.tomcatrs.bridge;

import java.io.IOException;
import java.io.PrintWriter;

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
