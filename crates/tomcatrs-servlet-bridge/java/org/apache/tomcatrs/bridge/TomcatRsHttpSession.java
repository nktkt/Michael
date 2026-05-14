package org.apache.tomcatrs.bridge;

import java.util.Arrays;
import java.util.Collections;
import java.util.Enumeration;

import jakarta.servlet.ServletContext;
import jakarta.servlet.http.HttpSession;

/**
 * Thin {@link HttpSession} facade backed by a Rust-side {@code SessionHandle}.
 *
 * <p><strong>Scaffold.</strong> The facade holds a single opaque {@code long},
 * {@link #nativeSessionId}. That id is the only value that crosses JNI; every
 * accessor delegates to a {@link NativeSession} {@code native} method, which
 * drives the Rust-side {@code tomcatrs_session::SessionManager}. No session
 * state lives on the Java heap.
 *
 * <p>In v1.0.0 session attribute values are string-valued: {@code setAttribute}
 * stringifies its argument before crossing JNI and {@code getAttribute} returns
 * the stored {@code String}. A future release introduces a typed attribute
 * value.
 *
 * <p>Operating on an invalidated or expired session causes the underlying
 * {@code native} call to throw {@link IllegalStateException}, exactly as the
 * Servlet spec requires.
 */
public final class TomcatRsHttpSession implements HttpSession {

    /** Opaque handle into the Rust-side session-handle registry. */
    private final long nativeSessionId;

    /** The owning servlet context, supplied by the bridge at bind time. */
    private final ServletContext servletContext;

    public TomcatRsHttpSession(long nativeSessionId, ServletContext servletContext) {
        this.nativeSessionId = nativeSessionId;
        this.servletContext = servletContext;
    }

    /** Exposes the opaque id (e.g. for the request-facade {@code getSession} bridge). */
    public long nativeSessionId() {
        return nativeSessionId;
    }

    @Override
    public String getId() {
        return NativeSession.nativeGetId(nativeSessionId);
    }

    @Override
    public long getCreationTime() {
        return NativeSession.nativeGetCreationTime(nativeSessionId);
    }

    @Override
    public long getLastAccessedTime() {
        return NativeSession.nativeGetLastAccessedTime(nativeSessionId);
    }

    @Override
    public int getMaxInactiveInterval() {
        return NativeSession.nativeGetMaxInactiveInterval(nativeSessionId);
    }

    @Override
    public void setMaxInactiveInterval(int interval) {
        NativeSession.nativeSetMaxInactiveInterval(nativeSessionId, interval);
    }

    @Override
    public Object getAttribute(String name) {
        // v1.0.0: attribute values are string-valued.
        return NativeSession.nativeGetAttribute(nativeSessionId, name);
    }

    @Override
    public Enumeration<String> getAttributeNames() {
        String[] names = NativeSession.nativeGetAttributeNames(nativeSessionId);
        if (names == null) {
            names = new String[0];
        }
        return Collections.enumeration(Arrays.asList(names));
    }

    @Override
    public void setAttribute(String name, Object value) {
        if (value == null) {
            // The Servlet API treats setAttribute(name, null) as removal.
            NativeSession.nativeRemoveAttribute(nativeSessionId, name);
        } else {
            NativeSession.nativeSetAttribute(nativeSessionId, name, value.toString());
        }
    }

    @Override
    public void removeAttribute(String name) {
        NativeSession.nativeRemoveAttribute(nativeSessionId, name);
    }

    @Override
    public void invalidate() {
        NativeSession.nativeInvalidate(nativeSessionId);
    }

    @Override
    public boolean isNew() {
        return NativeSession.nativeIsNew(nativeSessionId);
    }

    @Override
    public ServletContext getServletContext() {
        return servletContext;
    }
}
