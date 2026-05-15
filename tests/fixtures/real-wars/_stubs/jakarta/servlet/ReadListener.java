package jakarta.servlet;

import java.io.IOException;
import java.util.EventListener;

/**
 * Fixture-only stub of {@code jakarta.servlet.ReadListener}. See the package
 * comment in {@code Servlet.java}.
 */
public interface ReadListener extends EventListener {

    void onDataAvailable() throws IOException;

    void onAllDataRead() throws IOException;

    void onError(Throwable t);
}
