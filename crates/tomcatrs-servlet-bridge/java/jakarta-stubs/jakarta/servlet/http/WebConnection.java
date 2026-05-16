package jakarta.servlet.http;

import java.io.IOException;

import jakarta.servlet.ServletInputStream;
import jakarta.servlet.ServletOutputStream;

/** STUB of jakarta.servlet.http.WebConnection. */
public interface WebConnection extends AutoCloseable {
    ServletInputStream getInputStream() throws IOException;
    ServletOutputStream getOutputStream() throws IOException;
}
