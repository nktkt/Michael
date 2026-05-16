package jakarta.servlet;

/** STUB of jakarta.servlet.ServletConnection (Servlet 6). */
public interface ServletConnection {
    String getConnectionId();
    String getProtocol();
    String getProtocolConnectionId();
    boolean isSecure();
}
