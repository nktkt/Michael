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
}
