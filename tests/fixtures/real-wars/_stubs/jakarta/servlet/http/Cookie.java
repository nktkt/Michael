package jakarta.servlet.http;

/**
 * Fixture-only stub of {@code jakarta.servlet.http.Cookie}. See the package
 * comment in {@code tests/fixtures/real-wars/_stubs/jakarta/servlet/Servlet.java}.
 *
 * <p>Only the {@code name} + {@code value} accessors are surfaced — the
 * cookies-fixture servlet writes those back as the response body, and the
 * integration test asserts on the strings. Attributes ({@code path},
 * {@code domain}, {@code secure}, {@code httpOnly}, {@code maxAge}) only
 * travel on outbound {@code Set-Cookie} headers per the HTTP spec; the inbound
 * facade has no reason to surface them.
 */
public class Cookie {
    private final String name;
    private String value;

    public Cookie(String name, String value) {
        this.name = name;
        this.value = value;
    }

    public String getName() {
        return name;
    }

    public String getValue() {
        return value;
    }

    public void setValue(String value) {
        this.value = value;
    }
}
