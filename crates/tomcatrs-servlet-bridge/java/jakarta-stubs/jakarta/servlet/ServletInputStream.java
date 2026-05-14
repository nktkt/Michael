package jakarta.servlet;

import java.io.IOException;
import java.io.InputStream;

/**
 * STUB of {@code jakarta.servlet.ServletInputStream}.
 *
 * <p>Minimal compile-time stub — see {@code ReadListener} for the rationale.
 * Only the abstract surface the Tomcat-RS bridge facades override is declared.
 * In production the real {@code jakarta.servlet-api} jar replaces these stubs.
 */
public abstract class ServletInputStream extends InputStream {

    protected ServletInputStream() {
    }

    public abstract boolean isFinished();

    public abstract boolean isReady();

    public abstract void setReadListener(ReadListener readListener);

    @Override
    public abstract int read() throws IOException;
}
