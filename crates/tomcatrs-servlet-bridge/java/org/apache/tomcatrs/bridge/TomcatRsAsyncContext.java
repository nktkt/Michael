package org.apache.tomcatrs.bridge;

import jakarta.servlet.AsyncContext;

/**
 * Thin {@link AsyncContext} facade backed by a Rust-side
 * {@code AsyncContextState}.
 *
 * <p>Servlet 3.0+ asynchronous processing: a servlet calls
 * {@code ServletRequest.startAsync()}, which (in the bridge) registers an
 * {@code AsyncContextState} in the Rust process keyed by the request's opaque
 * {@code nativeRequestId} and hands back one of these facades. The request
 * worker thread then returns, but the Rust side keeps the pinned
 * request/response handles alive until the application calls {@link #complete()}
 * or {@link #dispatch(String)} from any thread — or the timeout fires.
 *
 * <p>Like {@link TomcatRsRequestFacade} / {@link TomcatRsResponseFacade}, this
 * facade holds only opaque {@code long} ids; every method is a single JNI call
 * into {@link NativeAsyncContext} that drives the Rust-side state machine. No
 * async state is duplicated on the Java heap.
 *
 * <p><strong>Scaffold.</strong> Only the methods that demonstrate the bridge
 * design are fleshed out; the remaining {@code AsyncContext} members
 * ({@code getRequest()}, {@code start(Runnable)}, the {@code AsyncListener}
 * registration methods, …) are omitted for brevity and would delegate to
 * {@link NativeAsyncContext} in the same style.
 */
public final class TomcatRsAsyncContext implements AsyncContext {

    /** Opaque handle into the Rust-side request registry (the registry key). */
    private final long nativeRequestId;

    /** Opaque handle into the Rust-side response registry. */
    private final long nativeResponseId;

    public TomcatRsAsyncContext(long nativeRequestId, long nativeResponseId) {
        this.nativeRequestId = nativeRequestId;
        this.nativeResponseId = nativeResponseId;
    }

    /**
     * Convenience constructor binding this async context to a pair of
     * already-built bridge facades. This is the call {@code startAsync()}
     * makes once it has the request/response facades in hand.
     */
    public TomcatRsAsyncContext(TomcatRsRequestFacade request,
                                TomcatRsResponseFacade response) {
        this(request.nativeRequestId(), response.nativeResponseId());
    }

    /** Exposes the opaque request id (e.g. for diagnostics). */
    public long nativeRequestId() {
        return nativeRequestId;
    }

    /** Exposes the opaque response id. */
    public long nativeResponseId() {
        return nativeResponseId;
    }

    /**
     * Enters async mode: drives the Rust state machine
     * {@code Dispatched -> Starting -> Started} and arms the timeout. Called by
     * the bridge when a servlet invokes {@code ServletRequest.startAsync()}.
     *
     * @return {@code true} if the request entered async mode
     */
    public boolean startAsync() {
        return NativeAsyncContext.nativeStartAsync(nativeRequestId, nativeResponseId);
    }

    @Override
    public void complete() {
        NativeAsyncContext.nativeComplete(nativeRequestId);
    }

    @Override
    public void dispatch(String path) {
        NativeAsyncContext.nativeDispatch(nativeRequestId, path);
    }

    @Override
    public void setTimeout(long timeout) {
        NativeAsyncContext.nativeSetTimeout(nativeRequestId, timeout);
    }

    @Override
    public long getTimeout() {
        return NativeAsyncContext.nativeGetTimeout(nativeRequestId);
    }

    /**
     * Whether {@code startAsync()} has been called and the request has not yet
     * settled (completed, dispatched, or timed out). Backs
     * {@code ServletRequest.isAsyncStarted()}.
     */
    public boolean isAsyncStarted() {
        return NativeAsyncContext.nativeIsAsyncStarted(nativeRequestId);
    }

    // --- Remaining AsyncContext members omitted in the scaffold -------------
    // Each would delegate to a `NativeAsyncContext.native*` method in the same
    // style as the methods above.
}
