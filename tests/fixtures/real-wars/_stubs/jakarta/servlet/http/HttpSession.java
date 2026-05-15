package jakarta.servlet.http;

/**
 * Fixture-only stub of {@code jakarta.servlet.http.HttpSession}. See the
 * package comment in
 * {@code tests/fixtures/real-wars/_stubs/jakarta/servlet/Servlet.java}.
 */
public interface HttpSession {

    String getId();

    Object getAttribute(String name);

    void setAttribute(String name, Object value);

    void invalidate();
}
