package org.apache.tomcatrs.bridge;

/**
 * Package-private holder for the request-side {@code native} method
 * declarations of the Tomcat-RS servlet bridge.
 *
 * <p>Every method takes the opaque {@code nativeRequestId} owned by a
 * {@link TomcatRsRequestFacade} and returns exactly the one value asked for —
 * this is the "lazy materialization" contract: a header the servlet never
 * reads never crosses the JNI boundary.
 *
 * <p>The natives are <em>registered</em> by the embedding Rust process via
 * {@code JNIEnv::register_native_methods} at JVM start-up, not loaded from a
 * shared library. The {@code static} block below therefore tolerates a missing
 * library: when Tomcat-RS is the launcher there is no {@code .so} to load.
 *
 * <p>The canonical signatures live in the Rust crate in {@code src/jni.rs}
 * ({@code NATIVE_REQUEST_METHODS}) and must be kept in sync with this file.
 */
final class NativeRequest {

    private NativeRequest() {
        // Static-only.
    }

    static {
        try {
            System.loadLibrary("tomcatrs_servlet_bridge");
        } catch (UnsatisfiedLinkError expectedWhenEmbedded) {
            // Natives are registered directly by the Rust host process.
        }
    }

    static native String nativeGetMethod(long nativeRequestId);

    static native String nativeGetRequestUri(long nativeRequestId);

    static native String nativeGetQueryString(long nativeRequestId);

    static native String nativeGetProtocol(long nativeRequestId);

    static native String nativeGetScheme(long nativeRequestId);

    static native String nativeGetRemoteAddr(long nativeRequestId);

    static native String nativeGetHeader(long nativeRequestId, String name);

    static native String[] nativeGetHeaderNames(long nativeRequestId);

    /**
     * Parsed {@code Content-Length} header value, or {@code -1} when absent or
     * malformed — matching {@code HttpServletRequest.getContentLengthLong()}.
     */
    static native long nativeGetContentLength(long nativeRequestId);

    static native String nativeGetAttribute(long nativeRequestId, String name);

    static native void nativeSetAttribute(long nativeRequestId, String name, String value);

    /**
     * Streams the request body. Copies up to {@code len} bytes into
     * {@code buffer} at {@code off}; returns the number of bytes copied, or
     * {@code -1} at end-of-stream.
     */
    static native int nativeReadBody(long nativeRequestId, byte[] buffer, int off, int len);

    static native int nativeBodyRemaining(long nativeRequestId);

    // --- Session resolution -------------------------------------------------
    //
    // These three natives bind a {@code TomcatRsHttpSession} to a request. The
    // surface lives on {@code NativeRequest} (rather than {@code NativeSession})
    // because session resolution starts from the request — scanning its
    // {@code Cookie:} headers for {@code JSESSIONID} and consulting the
    // per-context {@code SessionManager} — and ends with a fresh
    // {@code nativeSessionId} the request facade hands to
    // {@code TomcatRsHttpSession}. The session-side operations
    // ({@code getAttribute}, {@code invalidate}, ...) stay on
    // {@link NativeSession}.

    /**
     * Resolve (or create) the {@code HttpSession} bound to a request.
     *
     * <p>Scans the request's {@code Cookie:} header for {@code JSESSIONID};
     * if a valid one is present, returns a fresh {@code nativeSessionId}
     * against the reused {@link org.apache.tomcatrs.bridge.NativeSession}
     * handle. Otherwise, when {@code create} is {@code true}, creates a brand
     * new session and returns its id; when {@code create} is {@code false},
     * returns {@code 0} (matching the Servlet spec contract for
     * {@code HttpServletRequest.getSession(false)}).
     *
     * @return a {@code nativeSessionId} suitable for
     *         {@code new TomcatRsHttpSession(...)}, or {@code 0} when no
     *         session was bound.
     */
    static native long nativeResolveOrCreateSession(
            long nativeRequestId, long nativeContextId, boolean create);

    /**
     * Whether {@code nativeResolveOrCreateSession} freshly created the
     * session in this request. Used by the request facade to decide whether
     * to emit a {@code Set-Cookie: JSESSIONID=...} on the response.
     * Returns {@code false} on an unknown id rather than throwing — a
     * stale id should never error during cookie-decision logic.
     */
    static native boolean nativeIsNewSession(long nativeSessionId);

    /**
     * Build the {@code Set-Cookie} header <strong>value</strong> binding
     * {@code JSESSIONID} to a freshly created session id, via the per-context
     * {@code CookieProcessor}. Returns the empty string on any failure
     * (unknown context, unknown session) so the caller treats that as
     * "no Set-Cookie".
     */
    static native String nativeNewSessionCookie(
            long nativeContextId, long nativeSessionId);
}
