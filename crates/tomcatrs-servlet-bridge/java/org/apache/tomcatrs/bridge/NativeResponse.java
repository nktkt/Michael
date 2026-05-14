package org.apache.tomcatrs.bridge;

/**
 * Package-private holder for the response-side {@code native} method
 * declarations of the Tomcat-RS servlet bridge.
 *
 * <p>Every method takes the opaque {@code nativeResponseId} owned by a
 * {@link TomcatRsResponseFacade} and mutates the Rust-side response sink
 * directly. Status and headers become immutable once {@link #nativeCommit}
 * has been called.
 *
 * <p>As with {@link NativeRequest}, these natives are <em>registered</em> by
 * the embedding Rust process at JVM start-up rather than loaded from a shared
 * library. The canonical signatures live in the Rust crate in
 * {@code src/jni.rs} ({@code NATIVE_RESPONSE_METHODS}).
 */
final class NativeResponse {

    private NativeResponse() {
        // Static-only.
    }

    static {
        try {
            System.loadLibrary("tomcatrs_servlet_bridge");
        } catch (UnsatisfiedLinkError expectedWhenEmbedded) {
            // Natives are registered directly by the Rust host process.
        }
    }

    static native void nativeSetStatus(long nativeResponseId, int status);

    static native void nativeSetHeader(long nativeResponseId, String name, String value);

    static native void nativeAddHeader(long nativeResponseId, String name, String value);

    /**
     * Drains a chunk of the {@code ServletOutputStream} into the Rust sink:
     * writes {@code len} bytes from {@code buffer} starting at {@code off}.
     */
    static native void nativeWriteBody(long nativeResponseId, byte[] buffer, int off, int len);

    /** Commits the response (freezes status line and headers). */
    static native boolean nativeCommit(long nativeResponseId);

    static native boolean nativeIsCommitted(long nativeResponseId);
}
