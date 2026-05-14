package org.apache.tomcatrs.bridge;

/**
 * Package-private holder for the session-side {@code native} method
 * declarations of the Tomcat-RS servlet bridge.
 *
 * <p>Every method takes the opaque {@code nativeSessionId} owned by a
 * {@link TomcatRsHttpSession} and drives the Rust-side
 * {@code tomcatrs_session::SessionManager} through a load &rarr; mutate &rarr;
 * save cycle — the JVM side keeps no session state of its own.
 *
 * <p>The natives are <em>registered</em> by the embedding Rust process via
 * {@code JNIEnv::register_native_methods} at JVM start-up, not loaded from a
 * shared library. The {@code static} block below therefore tolerates a missing
 * library: when Tomcat-RS is the launcher there is no {@code .so} to load.
 *
 * <p>Operating on an invalidated or expired session throws
 * {@link IllegalStateException}, matching the Servlet spec contract for
 * {@code HttpSession}.
 *
 * <p>The canonical signatures live in the Rust crate in
 * {@code src/session_bridge.rs} ({@code NATIVE_SESSION_METHODS}) and must be
 * kept in sync with this file.
 */
final class NativeSession {

    private NativeSession() {
        // Static-only.
    }

    static {
        try {
            System.loadLibrary("tomcatrs_servlet_bridge");
        } catch (UnsatisfiedLinkError expectedWhenEmbedded) {
            // Natives are registered directly by the Rust host process.
        }
    }

    static native String nativeGetId(long nativeSessionId);

    static native String nativeGetAttribute(long nativeSessionId, String name);

    static native void nativeSetAttribute(long nativeSessionId, String name, String value);

    static native void nativeRemoveAttribute(long nativeSessionId, String name);

    static native String[] nativeGetAttributeNames(long nativeSessionId);

    /** Session creation time, as Unix-epoch milliseconds. */
    static native long nativeGetCreationTime(long nativeSessionId);

    /** Session last-accessed time, as Unix-epoch milliseconds. */
    static native long nativeGetLastAccessedTime(long nativeSessionId);

    /** The {@code max-inactive-interval}, in whole seconds. */
    static native int nativeGetMaxInactiveInterval(long nativeSessionId);

    /** Sets the {@code max-inactive-interval}; a non-positive value means "never expires". */
    static native void nativeSetMaxInactiveInterval(long nativeSessionId, int seconds);

    static native void nativeInvalidate(long nativeSessionId);

    static native boolean nativeIsNew(long nativeSessionId);
}
