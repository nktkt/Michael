package jakarta.servlet;

import java.io.IOException;
import java.io.OutputStream;

/**
 * Fixture-only stub of {@code jakarta.servlet.ServletOutputStream}. See the
 * package comment in {@code Servlet.java}.
 */
public abstract class ServletOutputStream extends OutputStream {

    protected ServletOutputStream() {
    }

    public abstract boolean isReady();

    public abstract void setWriteListener(WriteListener writeListener);

    public void print(String s) throws IOException {
        if (s != null) {
            write(s.getBytes());
        }
    }

    public void println(String s) throws IOException {
        print(s == null ? "null" : s);
        write('\n');
    }
}
