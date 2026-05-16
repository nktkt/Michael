package org.apache.tomcatrs.bridge;

/**
 * Package-private holder for the {@code ServletContext}-side {@code native}
 * method declarations of the Tomcat-RS servlet bridge.
 *
 * <p>Every method takes the opaque {@code nativeContextId} owned by a
 * {@link TomcatRsServletContext} and returns the one value asked for from the
 * Rust-side context registry. The natives are <em>registered</em> by the
 * embedding Rust process via {@code JNIEnv::register_native_methods} at JVM
 * start-up, not loaded from a shared library — the {@code static} block below
 * therefore tolerates a missing library.
 *
 * <p>The canonical signatures live in the Rust crate in {@code src/jni.rs}
 * ({@code NATIVE_SERVLET_CONTEXT_METHODS}) and must be kept in sync with this
 * file.
 */
final class NativeServletContext {

    private NativeServletContext() {
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
     * Resolve {@code contextPath} to its registered {@code nativeContextId},
     * or {@code 0} if no Rust-side context entry has been registered for that
     * path. Used by the legacy {@code TomcatRsServletContext(String)}
     * constructor so SCI-style call sites that only know the context path
     * still find a real backing entry.
     */
    static native long nativeLookupContextId(String contextPath);

    static native String nativeGetRealPath(long nativeContextId, String path);

    /** Returns an empty array when the path resolves to no resources. */
    static native String[] nativeGetResourcePaths(long nativeContextId, String path);

    /** Returns {@code null} when the resource does not exist. */
    static native byte[] nativeOpenResource(long nativeContextId, String path);

    static native String nativeGetServerInfo(long nativeContextId);

    static native String nativeGetContextPath(long nativeContextId);

    static native String nativeGetServletContextName(long nativeContextId);

    static native String nativeGetInitParameter(long nativeContextId, String name);

    static native String[] nativeGetInitParameterNames(long nativeContextId);

    static native void nativeLog(long nativeContextId, String msg);

    /**
     * Promote a {@link jakarta.servlet.Servlet} instance dynamically
     * registered via {@code TomcatRsServletContext.addServlet(...)} into the
     * Rust-side {@code WebappRuntime}'s servlet registry, so the connector
     * mapper can route requests to it. Called once per successful
     * {@code addServlet} call.
     */
    static native void nativeRegisterServlet(
            long nativeContextId,
            String servletName,
            String className,
            jakarta.servlet.Servlet instance);
}
