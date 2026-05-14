package jakarta.servlet;

/**
 * STUB of {@code jakarta.servlet.WriteListener}.
 *
 * <p>Minimal compile-time stub — see {@code ReadListener} for the rationale.
 * In production the real {@code jakarta.servlet-api} jar replaces these stubs.
 */
public interface WriteListener {
    void onWritePossible() throws java.io.IOException;

    void onError(Throwable t);
}
