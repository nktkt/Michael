package org.apache.tomcatrs.bridge;

import java.util.Collections;
import java.util.Enumeration;

import jakarta.servlet.ServletContext;

/**
 * Thin {@link ServletContext} facade bridging context-scoped lookups to the
 * Rust-side webapp registry owned by {@code JvmRuntime}.
 *
 * <p><strong>Scaffold.</strong> Only the methods that demonstrate the bridge
 * design are fleshed out; the remaining {@code ServletContext} members are
 * omitted for brevity. This file is not compiled by cargo — see
 * {@code java/README.md}.
 *
 * <p>Unlike the request/response facades there is one of these per deployed
 * web application, so it is keyed by the {@code contextPath} string (the Rust
 * {@code ContextId}) rather than a per-request {@code long}.
 */
public final class TomcatRsServletContext implements ServletContext {

    /** The web application's context path, e.g. {@code "/myapp"} — the Rust {@code ContextId}. */
    private final String contextPath;

    public TomcatRsServletContext(String contextPath) {
        this.contextPath = contextPath;
    }

    @Override
    public String getContextPath() {
        return contextPath;
    }

    @Override
    public String getServerInfo() {
        return "Tomcat-RS Compatibility Runtime";
    }

    @Override
    public String getRealPath(String path) {
        // Delegates to the Rust webapp registry, which knows the WAR's
        // exploded location on disk for this context.
        return nativeGetRealPath(contextPath, path);
    }

    @Override
    public String getInitParameter(String name) {
        return nativeGetInitParameter(contextPath, name);
    }

    @Override
    public Enumeration<String> getInitParameterNames() {
        return Collections.enumeration(
                java.util.Arrays.asList(nativeGetInitParameterNames(contextPath)));
    }

    @Override
    public Object getAttribute(String name) {
        return nativeGetAttribute(contextPath, name);
    }

    @Override
    public void setAttribute(String name, Object value) {
        nativeSetAttribute(contextPath, name, value == null ? null : value.toString());
    }

    @Override
    public int getMajorVersion() {
        return 6;
    }

    @Override
    public int getMinorVersion() {
        return 0;
    }

    // --- Native bridge methods ----------------------------------------------
    // Registered by the Rust host process, like the request/response natives.

    private static native String nativeGetRealPath(String contextPath, String path);

    private static native String nativeGetInitParameter(String contextPath, String name);

    private static native String[] nativeGetInitParameterNames(String contextPath);

    private static native String nativeGetAttribute(String contextPath, String name);

    private static native void nativeSetAttribute(String contextPath, String name, String value);

    // --- Remaining ServletContext members omitted in the scaffold -----------
}
