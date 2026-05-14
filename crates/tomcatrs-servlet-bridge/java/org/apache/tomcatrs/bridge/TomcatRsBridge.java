package org.apache.tomcatrs.bridge;

import java.io.IOException;

import jakarta.servlet.Filter;
import jakarta.servlet.FilterChain;
import jakarta.servlet.Servlet;
import jakarta.servlet.ServletException;

/**
 * Entry point of the Tomcat-RS Java bridge.
 *
 * <p>This class holds the JVM-side lifecycle hooks the embedding Rust process
 * ({@code JvmRuntime}) calls at start-up / shutdown, plus the
 * {@link ServletDispatcher} helper the per-request worker threads use to invoke
 * unmodified user {@link Servlet}s against the Tomcat-RS request/response
 * facades.
 *
 * <p>As with {@link NativeRequest} / {@link NativeResponse}, the {@code native}
 * methods here are <em>registered</em> by the Rust host via
 * {@code JNIEnv::register_native_methods} at JVM start-up rather than loaded
 * from a shared library; the {@code static} block tolerates a missing library
 * accordingly.
 */
public final class TomcatRsBridge {

    private TomcatRsBridge() {
        // Static-only entry point.
    }

    static {
        try {
            System.loadLibrary("tomcatrs_servlet_bridge");
        } catch (UnsatisfiedLinkError expectedWhenEmbedded) {
            // Natives are registered directly by the Rust host process.
        }
    }

    // --- Lifecycle hooks called by the Rust host ----------------------------

    /**
     * Called once by {@code JvmRuntime} after the JVM is up and the native
     * methods have been registered. Returns the bridge protocol version so the
     * Rust side can verify the jar on the classpath matches its expectations.
     */
    public static native int nativeOnStart();

    /** Called once by {@code JvmRuntime} during orderly shutdown. */
    public static native void nativeOnShutdown();

    /**
     * Reports a fatal bridge-side error back to the Rust host (e.g. a servlet
     * threw during {@code init}). The host decides whether to fail the
     * deployment or serve a 500.
     */
    public static native void nativeReportError(String contextPath, String message);

    /**
     * Bridge protocol version. Bumped whenever the native method tables in
     * {@code src/jni.rs} change shape; checked against {@link #nativeOnStart}.
     */
    public static final int PROTOCOL_VERSION = 1;

    // --- Servlet dispatch ----------------------------------------------------

    /**
     * Drives a user {@link Servlet} (or {@link Filter} chain) against a pair of
     * Tomcat-RS facades. Worker threads build the facades from the opaque
     * native ids handed over JNI, then call into here.
     */
    public static final class ServletDispatcher {

        private ServletDispatcher() {
            // Static helper.
        }

        /**
         * Invokes {@code servlet.service(req, res)} against freshly-built
         * facades for the given opaque native ids.
         *
         * @param servlet          the unmodified user servlet instance
         * @param nativeRequestId  opaque id of the Rust-side request handle
         * @param nativeResponseId opaque id of the Rust-side response handle
         */
        public static void dispatch(Servlet servlet, long nativeRequestId, long nativeResponseId)
                throws ServletException, IOException {
            TomcatRsRequestFacade request = new TomcatRsRequestFacade(nativeRequestId);
            TomcatRsResponseFacade response = new TomcatRsResponseFacade(nativeResponseId);
            dispatch(servlet, request, response);
        }

        /**
         * Invokes {@code servlet.service(req, res)} against pre-built facades.
         * Kept separate so callers that already hold the facades (e.g. the
         * {@code AsyncContext} bridge) can reuse them.
         */
        public static void dispatch(Servlet servlet,
                                    TomcatRsRequestFacade request,
                                    TomcatRsResponseFacade response)
                throws ServletException, IOException {
            if (servlet == null) {
                throw new ServletException("no servlet bound for this request");
            }
            servlet.service(request, response);
        }

        /**
         * Runs a single {@link Filter} against the facades, terminating the
         * chain with the supplied {@code servlet}. Mirrors the shape of
         * Tomcat's {@code ApplicationFilterChain} but for exactly one filter —
         * the Rust connector composes longer chains by nesting these.
         */
        public static void dispatch(Filter filter,
                                    Servlet servlet,
                                    long nativeRequestId,
                                    long nativeResponseId)
                throws ServletException, IOException {
            TomcatRsRequestFacade request = new TomcatRsRequestFacade(nativeRequestId);
            TomcatRsResponseFacade response = new TomcatRsResponseFacade(nativeResponseId);
            FilterChain terminal = (req, res) -> {
                if (servlet == null) {
                    throw new ServletException("filter chain has no terminal servlet");
                }
                servlet.service(req, res);
            };
            filter.doFilter(request, response, terminal);
        }
    }
}
