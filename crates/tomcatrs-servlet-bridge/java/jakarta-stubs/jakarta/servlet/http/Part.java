package jakarta.servlet.http;

import java.io.IOException;
import java.io.InputStream;
import java.util.Collection;

/** STUB of jakarta.servlet.http.Part — multipart upload part. */
public interface Part {
    InputStream getInputStream() throws IOException;
    String getContentType();
    String getName();
    String getSubmittedFileName();
    long getSize();
    void write(String fileName) throws IOException;
    void delete() throws IOException;
    String getHeader(String name);
    Collection<String> getHeaders(String name);
    Collection<String> getHeaderNames();
}
