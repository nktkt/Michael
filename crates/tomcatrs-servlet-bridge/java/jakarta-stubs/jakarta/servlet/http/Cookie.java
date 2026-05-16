package jakarta.servlet.http;

/** STUB of jakarta.servlet.http.Cookie. Real Jakarta jar replaces at runtime. */
public class Cookie implements Cloneable {
    private String name;
    private String value;
    private String path;
    private String domain;
    private int maxAge = -1;
    private boolean secure;
    private boolean httpOnly;
    public Cookie(String name, String value) { this.name = name; this.value = value; }
    public String getName() { return name; }
    public String getValue() { return value; }
    public void setValue(String value) { this.value = value; }
    public String getPath() { return path; }
    public void setPath(String path) { this.path = path; }
    public String getDomain() { return domain; }
    public void setDomain(String domain) { this.domain = domain; }
    public int getMaxAge() { return maxAge; }
    public void setMaxAge(int maxAge) { this.maxAge = maxAge; }
    public boolean getSecure() { return secure; }
    public void setSecure(boolean secure) { this.secure = secure; }
    public boolean isHttpOnly() { return httpOnly; }
    public void setHttpOnly(boolean httpOnly) { this.httpOnly = httpOnly; }
    @Override public Object clone() { try { return super.clone(); } catch (CloneNotSupportedException e) { throw new InternalError(e); } }
}
