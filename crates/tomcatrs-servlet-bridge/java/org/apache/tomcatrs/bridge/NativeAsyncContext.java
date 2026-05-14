package org.apache.tomcatrs.bridge;

/**
 * Package-private holder for the {@code AsyncContext}-side {@code native}
 * method declarations of the Tomcat-RS servlet bridge.
 *
 * <p>Every method takes the opaque {@code nativeRequestId} a
 * {@link TomcatRsAsyncContext} carries and drives the Rust-side
 * {@code AsyncContextState} state machine directly. The Rust side keeps the
 * pinned request/response handles alive until the async request settles
 * (complete, dispatch, or timeout).
 *
 * <p>As with {@link NativeRequest} / {@link NativeResponse}, these natives are
 * <em>registered</em> by the embedding Rust process at JVM start-up via
 * {@code JNIEnv::register_native_methods} rather than loaded from a shared
 * library. The canonical Rust implementations live in the crate in
 * {@code src/async_servlet.rs}
 * ({@code Java_org_apache_tomcatrs_bridge_NativeAsyncContext_*}).
 */
final class NativeAsyncContext {

    private NativeAsyncContext() {
        // Static-only.
    }

    static {
        try {
            System.loadLibrary("tomcatrs_servlet_bridge");
        } catch (UnsatisfiedLinkError expectedWhenEmbedded) {
            // Natives are registered directly by the Rust host process.
        }
    }

    /**
     * Enters async mode for the request/response pair: drives the Rust state
     * machine {@code Dispatched -> Starting -> Started} and arms the timeout.
     * Returns {@code true} on success.
     */
    static native boolean nativeStartAsync(long nativeRequestId, long nativeResponseId);

    /**
     * Completes the async request: drives {@code Started -> Completing ->
     * Completed} and fires the Rust-side completion signal so the connector
     * serializes the response.
     */
    static native void nativeComplete(long nativeRequestId);

    /**
     * Records a re-dispatch target and moves the Rust state machine toward
     * {@code Dispatching}, firing the completion signal.
     */
    static native void nativeDispatch(long nativeRequestId, String path);

    /** Sets the async timeout in milliseconds ({@code 0} disables it). */
    static native void nativeSetTimeout(long nativeRequestId, long timeoutMillis);

    /** Returns the configured async timeout in milliseconds ({@code 0} = off). */
    static native long nativeGetTimeout(long nativeRequestId);

    /** Whether {@code startAsync()} has been called and not yet settled. */
    static native boolean nativeIsAsyncStarted(long nativeRequestId);
}
