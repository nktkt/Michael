package jakarta.servlet;

import java.io.BufferedReader;
import java.io.IOException;
import java.util.Enumeration;
import java.util.Map;

/**
 * STUB of {@code jakarta.servlet.ServletRequestWrapper} — the decorator base
 * Spring's {@code RequestFacade}, error-page filters, and most security
 * filters extend.
 *
 * <p>Real Jakarta jar replaces this in production. The stub here exists so
 * the bridge can compile standalone and so reflection scans (e.g.
 * {@code ReflectionUtils.getDeclaredMethods}) succeed when the framework
 * walks the class hierarchy.
 */
public class ServletRequestWrapper implements ServletRequest {

    private ServletRequest request;

    public ServletRequestWrapper(ServletRequest request) {
        if (request == null) {
            throw new IllegalArgumentException("ServletRequest cannot be null");
        }
        this.request = request;
    }

    public ServletRequest getRequest() {
        return request;
    }

    public void setRequest(ServletRequest request) {
        if (request == null) {
            throw new IllegalArgumentException("ServletRequest cannot be null");
        }
        this.request = request;
    }

    @Override
    public Object getAttribute(String name) { return request.getAttribute(name); }

    @Override
    public void setAttribute(String name, Object value) { request.setAttribute(name, value); }

    @Override
    public String getProtocol() { return request.getProtocol(); }

    @Override
    public String getScheme() { return request.getScheme(); }

    @Override
    public String getRemoteAddr() { return request.getRemoteAddr(); }

    @Override
    public ServletInputStream getInputStream() throws IOException { return request.getInputStream(); }

    @Override
    public BufferedReader getReader() throws IOException { return request.getReader(); }

    @Override
    public String getParameter(String name) { return request.getParameter(name); }

    @Override
    public Enumeration<String> getParameterNames() { return request.getParameterNames(); }

    @Override
    public String[] getParameterValues(String name) { return request.getParameterValues(name); }

    @Override
    public Map<String, String[]> getParameterMap() { return request.getParameterMap(); }

    @Override
    public String getCharacterEncoding() { return request.getCharacterEncoding(); }

    @Override
    public void setCharacterEncoding(String encoding) { request.setCharacterEncoding(encoding); }

    @Override
    public int getContentLength() { return request.getContentLength(); }

    @Override
    public long getContentLengthLong() { return request.getContentLengthLong(); }

    @Override
    public String getContentType() { return request.getContentType(); }

    @Override
    public String getServerName() { return request.getServerName(); }

    @Override
    public int getServerPort() { return request.getServerPort(); }

    @Override
    public String getRemoteHost() { return request.getRemoteHost(); }

    @Override
    public boolean isSecure() { return request.isSecure(); }
}
