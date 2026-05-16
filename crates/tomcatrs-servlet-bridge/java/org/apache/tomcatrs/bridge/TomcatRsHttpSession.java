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
 * <p><strong>Honest gap (v1.0.0): attribute values are String-typed.</strong>
 * The Rust {@code SessionData::attributes} is a
 * {@code HashMap<String, String>}, so {@link #setAttribute(String, Object)}
 * stringifies its argument before crossing JNI and {@link #getAttribute}
 * returns the stored {@code String}. Framework code that stores typed objects
 * (e.g. Spring Security's {@code SecurityContext}) will get a {@code String}
 * when reading back. A future release will introduce a typed attribute value
 * across the JNI boundary; for now, applications can opt into JSON / custom
 * serialisation on top of the string surface.
 *
 * <p><strong>Honest gap (v1.0.0):
 * {@code HttpSessionListener.sessionCreated} is not driven from this
 * facade.</strong> Listeners are recorded in the registration plan but not
 * invoked when a new session is created via the request facade's
 * {@code getSession(true)} path. Application code that relies on
 * session-creation listeners will need to add the bridge hook, or use the
 * Rust {@code SessionBinder} directly.
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

    /**
     * Convenience single-arg constructor: builds a facade with no
     * {@link ServletContext} attached. Used by the request-facade bridge
     * path, where the binding only needs the {@code nativeSessionId}; the
     * {@link #getServletContext()} accessor returns {@code null} on this
     * path (the Servlet spec permits but discourages a null context, and
     * the bridge does not currently route servlet-context references back
     * through the session facade).
     */
    public TomcatRsHttpSession(long nativeSessionId) {
        this(nativeSessionId, null);
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
