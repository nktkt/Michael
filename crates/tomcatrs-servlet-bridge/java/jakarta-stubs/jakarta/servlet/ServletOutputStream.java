package jakarta.servlet;

import java.io.IOException;
import java.io.OutputStream;

/**
 * STUB of {@code jakarta.servlet.ServletOutputStream}.
 *
 * <p>Minimal compile-time stub — see {@code ReadListener} for the rationale.
 * Only the abstract surface the Tomcat-RS bridge facades override is declared.
 * In production the real {@code jakarta.servlet-api} jar replaces these stubs.
 */
public abstract class ServletOutputStream extends OutputStream {

    protected ServletOutputStream() {
    }

    public abstract boolean isReady();

    public abstract void setWriteListener(WriteListener writeListener);

    @Override
    public abstract void write(int b) throws IOException;
}
