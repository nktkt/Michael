package jakarta.servlet;

import java.io.IOException;
import java.io.InputStream;

/**
 * Fixture-only stub of {@code jakarta.servlet.ServletInputStream}. See the
 * package comment in {@code Servlet.java}.
 */
public abstract class ServletInputStream extends InputStream {

    protected ServletInputStream() {
    }

    public abstract boolean isFinished();

    public abstract boolean isReady();

    public abstract void setReadListener(ReadListener readListener);

    @Override
    public int read() throws IOException {
        throw new IOException("ServletInputStream stub: read() not implemented");
    }
}
