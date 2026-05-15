package jakarta.servlet;

import java.io.IOException;
import java.util.EventListener;

/**
 * Fixture-only stub of {@code jakarta.servlet.WriteListener}. See the package
 * comment in {@code Servlet.java}.
 */
public interface WriteListener extends EventListener {

    void onWritePossible() throws IOException;

    void onError(Throwable t);
}
