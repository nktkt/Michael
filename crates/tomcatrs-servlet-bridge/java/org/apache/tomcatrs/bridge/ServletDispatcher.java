/*
 * Licensed under the Apache License, Version 2.0.
 *
 * Top-level dispatcher invoked over JNI by
 * {@code tomcatrs_servlet_bridge::invoker::JvmServletInvoker}.
 *
 * The Rust side resolves which {@link Servlet} should handle the request,
 * registers the request/response handles with the Rust-side
 * {@code HANDLE_REGISTRY}, and then calls
 * {@code ServletDispatcher.dispatch(servlet, req, res)} so the unmodified
 * user servlet runs against Tomcat-RS facade objects whose accessors cross
 * back into Rust over the registered native methods.
 *
 * Kept as a top-level class (rather than nested in {@link TomcatRsBridge})
 * so that the JNI signature is the plain
 * {@code org/apache/tomcatrs/bridge/ServletDispatcher} — no {@code $}
 * mangling — making the Rust JNI {@code call_static_method} call simple.
 */
package org.apache.tomcatrs.bridge;

import jakarta.servlet.Filter;
import jakarta.servlet.FilterChain;
import jakarta.servlet.Servlet;
import jakarta.servlet.ServletException;
import jakarta.servlet.ServletRequest;
import jakarta.servlet.ServletResponse;

import java.io.IOException;

public final class ServletDispatcher {

    private ServletDispatcher() {
        // Static helper.
    }

    /**
     * Invokes {@code servlet.service(req, res)}. Called from Rust over JNI
     * with the base {@link ServletRequest} / {@link ServletResponse} types
     * so the JNI signature is stable regardless of the concrete facade
     * implementation. The arguments are {@link TomcatRsRequestFacade} /
     * {@link TomcatRsResponseFacade} in practice; this method does not
     * down-cast — the servlet sees them through the standard interfaces.
     */
    public static void dispatch(Servlet servlet,
                                ServletRequest request,
                                ServletResponse response)
            throws ServletException, IOException {
        if (servlet == null) {
            throw new ServletException("no servlet bound for this request");
        }
        try {
            servlet.service(request, response);
        } finally {
            // The Servlet spec requires the container to flush any buffered
            // writer / output stream when the service() call returns. Without
            // this the PrintWriter chunks the user's `out.write(...)` calls
            // remain on the JVM heap and the Rust side sees an empty body.
            if (response instanceof TomcatRsResponseFacade) {
                ((TomcatRsResponseFacade) response).flushAll();
            } else {
                response.flushBuffer();
            }
        }
    }

    /**
     * Convenience overload that materialises the facades from opaque native
     * ids before dispatching. Useful from Java callers (such as the
     * AsyncContext bridge) that hold the ids rather than facade objects.
     */
    public static void dispatch(Servlet servlet,
                                long nativeRequestId,
                                long nativeResponseId)
            throws ServletException, IOException {
        TomcatRsRequestFacade request = new TomcatRsRequestFacade(nativeRequestId);
        TomcatRsResponseFacade response = new TomcatRsResponseFacade(nativeResponseId);
        dispatch(servlet, request, response);
    }

    /**
     * Runs a single {@link Filter} against the facades, terminating the
     * chain with the supplied {@code servlet}. The Rust connector composes
     * longer chains by nesting these.
     */
    public static void dispatch(Filter filter,
                                Servlet servlet,
                                ServletRequest request,
                                ServletResponse response)
            throws ServletException, IOException {
        if (filter == null) {
            dispatch(servlet, request, response);
            return;
        }
        FilterChain terminator = new FilterChain() {
            @Override
            public void doFilter(ServletRequest req, ServletResponse res)
                    throws IOException, ServletException {
                dispatch(servlet, req, res);
            }
        };
        filter.doFilter(request, response, terminator);
    }
}
