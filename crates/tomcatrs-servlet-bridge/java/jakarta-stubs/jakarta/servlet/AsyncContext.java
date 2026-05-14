package jakarta.servlet;

/**
 * STUB of {@code jakarta.servlet.AsyncContext}.
 *
 * <p>This is <strong>not</strong> the real Jakarta Servlet API. It is a minimal
 * compile-time stub declaring only the members the Tomcat-RS bridge
 * {@code TomcatRsAsyncContext} implements, so
 * {@code java/org/apache/tomcatrs/bridge/*.java} can be compiled standalone
 * without vendoring the full {@code jakarta.servlet-api} jar. In production the
 * real {@code jakarta.servlet-api} jar replaces these stubs and provides the
 * complete interface (including {@code getRequest()}, {@code getResponse()},
 * {@code start(Runnable)}, the {@code AsyncListener} registration methods, and
 * the no-argument {@code dispatch()} overloads).
 *
 * <p>Servlet 3.0+ asynchronous processing: a servlet calls
 * {@code ServletRequest.startAsync()}, the request thread returns while the
 * response stays open, and later — from any thread — code calls
 * {@link #complete()} or {@link #dispatch(String)}, or the timeout fires.
 */
public interface AsyncContext {

    /**
     * Default async timeout, in milliseconds, when none is set explicitly.
     * Matches Tomcat's historical {@code 30_000} ms default.
     */
    long ASYNC_CONTEXT_PATH = 0L;

    /**
     * Completes the asynchronous operation: the response is finalised and the
     * container may flush it to the wire and recycle the request/response.
     */
    void complete();

    /**
     * Re-dispatches the request and response to the given path within the same
     * web application, to be serviced on a container thread.
     */
    void dispatch(String path);

    /**
     * Sets the timeout (in milliseconds) for this async operation. A value of
     * {@code 0} or less disables the timeout. Must be called before the
     * container-initiated dispatch returns.
     */
    void setTimeout(long timeout);

    /**
     * Returns the timeout (in milliseconds) for this async operation; {@code 0}
     * means the timeout is disabled.
     */
    long getTimeout();
}
