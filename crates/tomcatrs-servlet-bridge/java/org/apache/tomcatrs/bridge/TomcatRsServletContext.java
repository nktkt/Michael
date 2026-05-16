/*
 * Licensed under the Apache License, Version 2.0.
 *
 * Bridge-side ServletContext implementation. One per deployed web application;
 * constructed from Rust over JNI during webapp registration with a
 * {@code nativeContextId} that points at a Rust-side {@code ContextEntry}
 * holding the immutable context-scoped state (context path, doc base, server
 * info, web.xml &lt;context-param&gt; map, ...).
 */
package org.apache.tomcatrs.bridge;

import jakarta.servlet.Filter;
import jakarta.servlet.FilterRegistration;
import jakarta.servlet.MultipartConfigElement;
import jakarta.servlet.Registration;
import jakarta.servlet.Servlet;
import jakarta.servlet.ServletContext;
import jakarta.servlet.ServletException;
import jakarta.servlet.ServletRegistration;

import java.io.ByteArrayInputStream;
import java.io.File;
import java.io.InputStream;
import java.net.MalformedURLException;
import java.net.URL;
import java.util.Arrays;
import java.util.Collection;
import java.util.Collections;
import java.util.Enumeration;
import java.util.EnumSet;
import java.util.EventListener;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;

/**
 * {@link ServletContext} implementation backed by a Rust-side
 * {@code ContextEntry}.
 *
 * <p><strong>What works today.</strong> The methods Spring's
 * {@code SpringServletContainerInitializer.onStartup} chain needs are all
 * functional:
 *
 * <ul>
 *   <li>Identity: {@link #getContextPath()}, {@link #getServletContextName()},
 *       {@link #getServerInfo()} — backed by the Rust context entry.</li>
 *   <li>Init parameters: {@link #getInitParameter(String)} /
 *       {@link #getInitParameterNames()} — read from the web.xml
 *       {@code <context-param>} map on the Rust side. The dynamic
 *       {@link #setInitParameter(String, String)} mutator is intentionally a
 *       no-op (returns {@code false}) — see the gap list below.</li>
 *   <li>Attributes: {@link #setAttribute(String, Object)} /
 *       {@link #getAttribute(String)} / {@link #getAttributeNames()} /
 *       {@link #removeAttribute(String)} — held entirely on the Java side in
 *       a {@link ConcurrentHashMap} keyed by name. This is the right scope
 *       (per-context) and the right lifetime (until the webapp restarts).</li>
 *   <li>Resources: {@link #getRealPath(String)},
 *       {@link #getResourcePaths(String)}, {@link #getResource(String)},
 *       {@link #getResourceAsStream(String)} — resolved under the Rust-side
 *       {@code doc_base}. Returns {@code null}/empty for missing resources,
 *       matching the spec.</li>
 *   <li>Logging: {@link #log(String)} /
 *       {@link #log(String, Throwable)} — bridged to the Rust tracing layer
 *       via {@code nativeLog}.</li>
 *   <li>Dynamic registration: {@link #addServlet(String, String)} and the two
 *       overloads, mirrored for {@code addFilter}, plus
 *       {@link #addListener(String)} and friends — instantiate the named
 *       class through the context class loader and <em>record</em> the
 *       registration in a Java-side map.</li>
 * </ul>
 *
 * <p><strong>Honest gap — dynamic mountings do not serve traffic.</strong>
 * Recording a {@code ServletRegistration.Dynamic} (with its mappings and
 * init-params) is enough for {@code DispatcherServlet}'s
 * {@code WebApplicationInitializer.onStartup} to return successfully — the
 * spec contract for {@code addServlet} is to <em>register</em>, not to
 * dispatch. But the Tomcat-RS connector's mapper today consults the
 * {@code web.xml}-derived {@code ServletMappingTable} built in
 * {@code tomcatrs_servlet_bridge::registration}; servlets added through this
 * API at runtime are <strong>not yet</strong> consulted by the mapper, so a
 * request to e.g. {@code /myapp/*} will not actually be routed to a
 * dynamically-registered {@code DispatcherServlet} yet. The plumbing for that
 * "merge runtime registrations into the mapper" step is the natural next
 * follow-up — the registration record produced here is exactly what it will
 * consume.
 *
 * <p>Listener invocation is similarly recorded-but-not-yet-driven: the
 * {@link #addListener(String)} family stores the listener instance but the
 * bridge does not yet fire {@code ServletContextListener.contextInitialized}
 * et al. against these. See {@code TomcatRsServletContext#listeners()} for the
 * recorded list a future driver can iterate.
 */
public final class TomcatRsServletContext implements ServletContext {

    /** Opaque id of the backing Rust {@code ContextEntry}. {@code 0} means "no entry". */
    private final long nativeContextId;

    /** Cached context path read out of the Rust entry once. */
    private final String contextPath;

    // --- Java-side state ----------------------------------------------------

    /** Java-side per-context attributes — {@code ServletContext} attribute scope. */
    private final ConcurrentHashMap<String, Object> attributes = new ConcurrentHashMap<>();

    /** {@code servletName -> RegisteredServletEntry}, insertion order preserved. */
    private final Map<String, RegisteredServletEntry> servletRegistry =
            Collections.synchronizedMap(new LinkedHashMap<>());

    /** {@code filterName -> RegisteredFilterEntry}, insertion order preserved. */
    private final Map<String, RegisteredFilterEntry> filterRegistry =
            Collections.synchronizedMap(new LinkedHashMap<>());

    /** Lifecycle listener instances added via {@link #addListener}, in order. */
    private final java.util.List<EventListener> listeners =
            Collections.synchronizedList(new java.util.ArrayList<>());

    // ------------------------------------------------------------------------
    // Construction
    // ------------------------------------------------------------------------

    /**
     * Modern constructor used by {@code registration.rs}: the Rust side has
     * already allocated a {@code ContextEntry} and is passing its id.
     */
    public TomcatRsServletContext(long nativeContextId) {
        this.nativeContextId = nativeContextId;
        String resolved = nativeContextId == 0L
                ? ""
                : NativeServletContext.nativeGetContextPath(nativeContextId);
        this.contextPath = resolved == null ? "" : resolved;
    }

    /**
     * Legacy constructor kept for the SCI driver in {@code sci.rs}, which
     * only knows the context path. Looks the {@code nativeContextId} up via
     * the Rust-side path index; falls back to id {@code 0} (degraded mode:
     * only attributes work, every Rust-backed accessor returns its empty
     * default) when no entry has been registered for the path yet.
     */
    public TomcatRsServletContext(String contextPath) {
        long id = 0L;
        if (contextPath != null) {
            try {
                id = NativeServletContext.nativeLookupContextId(contextPath);
            } catch (Throwable ignore) {
                // Native not registered (no JVM bridge): stay in degraded mode.
            }
        }
        this.nativeContextId = id;
        this.contextPath = contextPath == null ? "" : contextPath;
    }

    /** The Rust-side {@code ContextEntry} id this facade is bound to. */
    public long nativeContextId() {
        return nativeContextId;
    }

    /**
     * The registered servlet entries, in insertion order. Visible to the
     * bridge so future mapping/dispatch wiring can consume them — the
     * dynamic-registration "honest gap" called out in the class docs.
     */
    public Collection<RegisteredServletEntry> registeredServlets() {
        synchronized (servletRegistry) {
            return new java.util.ArrayList<>(servletRegistry.values());
        }
    }

    /** The registered filter entries, in insertion order. */
    public Collection<RegisteredFilterEntry> registeredFilters() {
        synchronized (filterRegistry) {
            return new java.util.ArrayList<>(filterRegistry.values());
        }
    }

    /** The lifecycle listener instances added via {@link #addListener}. */
    public java.util.List<EventListener> listeners() {
        synchronized (listeners) {
            return new java.util.ArrayList<>(listeners);
        }
    }

    // ------------------------------------------------------------------------
    // Identity
    // ------------------------------------------------------------------------

    @Override
    public String getContextPath() {
        return contextPath;
    }

    @Override
    public String getServletContextName() {
        if (nativeContextId == 0L) {
            return "";
        }
        String n = NativeServletContext.nativeGetServletContextName(nativeContextId);
        return n == null ? "" : n;
    }

    @Override
    public String getServerInfo() {
        if (nativeContextId == 0L) {
            return "Tomcat-RS Compatibility Runtime";
        }
        String s = NativeServletContext.nativeGetServerInfo(nativeContextId);
        return s == null ? "Tomcat-RS Compatibility Runtime" : s;
    }

    @Override
    public int getMajorVersion() {
        return 6;
    }

    @Override
    public int getMinorVersion() {
        return 0;
    }

    @Override
    public int getEffectiveMajorVersion() {
        return 6;
    }

    @Override
    public int getEffectiveMinorVersion() {
        return 0;
    }

    // ------------------------------------------------------------------------
    // Init parameters
    // ------------------------------------------------------------------------

    @Override
    public String getInitParameter(String name) {
        if (nativeContextId == 0L || name == null) {
            return null;
        }
        return NativeServletContext.nativeGetInitParameter(nativeContextId, name);
    }

    @Override
    public Enumeration<String> getInitParameterNames() {
        String[] names = nativeContextId == 0L
                ? new String[0]
                : NativeServletContext.nativeGetInitParameterNames(nativeContextId);
        if (names == null) {
            names = new String[0];
        }
        return Collections.enumeration(Arrays.asList(names));
    }

    @Override
    public boolean setInitParameter(String name, String value) {
        // Spec contract: returns {@code true} if the parameter was set, {@code
        // false} if it was already present. Tomcat-RS treats the
        // web.xml-derived init-params as immutable on the Rust side, so
        // every call returns {@code false} ("already set" semantics).
        // DispatcherServlet does not depend on this returning {@code true}.
        return false;
    }

    // ------------------------------------------------------------------------
    // Attributes (Java-side ConcurrentHashMap)
    // ------------------------------------------------------------------------

    @Override
    public Object getAttribute(String name) {
        return name == null ? null : attributes.get(name);
    }

    @Override
    public Enumeration<String> getAttributeNames() {
        return Collections.enumeration(new LinkedHashSet<>(attributes.keySet()));
    }

    @Override
    public void setAttribute(String name, Object value) {
        if (name == null) {
            return;
        }
        if (value == null) {
            attributes.remove(name);
        } else {
            attributes.put(name, value);
        }
    }

    @Override
    public void removeAttribute(String name) {
        if (name != null) {
            attributes.remove(name);
        }
    }

    // ------------------------------------------------------------------------
    // Resources
    // ------------------------------------------------------------------------

    @Override
    public String getRealPath(String path) {
        if (nativeContextId == 0L) {
            return null;
        }
        return NativeServletContext.nativeGetRealPath(nativeContextId, path == null ? "" : path);
    }

    @Override
    public Set<String> getResourcePaths(String path) {
        if (nativeContextId == 0L) {
            return null;
        }
        String[] entries = NativeServletContext.nativeGetResourcePaths(
                nativeContextId, path == null ? "/" : path);
        if (entries == null || entries.length == 0) {
            return null;
        }
        // Spec: paths must be returned in a Set whose iteration order is
        // implementation-defined; LinkedHashSet preserves the Rust-side order.
        return new LinkedHashSet<>(Arrays.asList(entries));
    }

    @Override
    public URL getResource(String path) throws MalformedURLException {
        String real = getRealPath(path);
        if (real == null) {
            return null;
        }
        File f = new File(real);
        if (!f.exists()) {
            return null;
        }
        return f.toURI().toURL();
    }

    @Override
    public InputStream getResourceAsStream(String path) {
        if (nativeContextId == 0L) {
            return null;
        }
        byte[] body = NativeServletContext.nativeOpenResource(
                nativeContextId, path == null ? "" : path);
        return body == null ? null : new ByteArrayInputStream(body);
    }

    @Override
    public String getMimeType(String file) {
        // The bridge does not implement a MIME type registry yet. Returning
        // {@code null} matches the spec contract for "unknown".
        return null;
    }

    // ------------------------------------------------------------------------
    // Dynamic servlet registration
    // ------------------------------------------------------------------------

    @Override
    public ServletRegistration.Dynamic addServlet(String servletName, String className) {
        if (servletName == null || servletName.isEmpty() || className == null) {
            return null;
        }
        Servlet instance = instantiate(className, Servlet.class);
        if (instance == null) {
            return null;
        }
        return registerServlet(servletName, className, instance);
    }

    @Override
    public ServletRegistration.Dynamic addServlet(String servletName, Servlet servlet) {
        if (servletName == null || servletName.isEmpty() || servlet == null) {
            return null;
        }
        return registerServlet(servletName, servlet.getClass().getName(), servlet);
    }

    @Override
    public ServletRegistration.Dynamic addServlet(
            String servletName, Class<? extends Servlet> servletClass) {
        if (servletName == null || servletName.isEmpty() || servletClass == null) {
            return null;
        }
        Servlet instance;
        try {
            instance = servletClass.getDeclaredConstructor().newInstance();
        } catch (ReflectiveOperationException e) {
            log("addServlet: cannot instantiate " + servletClass.getName(), e);
            return null;
        }
        return registerServlet(servletName, servletClass.getName(), instance);
    }

    private ServletRegistration.Dynamic registerServlet(
            String name, String className, Servlet instance) {
        synchronized (servletRegistry) {
            if (servletRegistry.containsKey(name)) {
                // Spec: returning {@code null} signals that a registration of
                // that name already exists.
                return null;
            }
            RegisteredServletEntry entry =
                    new RegisteredServletEntry(name, className, instance);
            servletRegistry.put(name, entry);
            // Promote the registration into the Rust-side WebappRuntime so
            // the connector mapper can route real requests to this servlet
            // (without this step Spring's DispatcherServlet would be visible
            // only inside the Java facade and unreachable from outside).
            // The native may be a no-op in unit-test contexts where no
            // WebappRuntime is attached for this contextId; that's fine.
            try {
                NativeServletContext.nativeRegisterServlet(
                        nativeContextId, name, className, instance);
            } catch (Throwable t) {
                log("addServlet: nativeRegisterServlet failed for '" + name + "'", t);
            }
            return entry;
        }
    }

    @Override
    public ServletRegistration getServletRegistration(String servletName) {
        return servletName == null ? null : servletRegistry.get(servletName);
    }

    @Override
    public Map<String, ? extends ServletRegistration> getServletRegistrations() {
        synchronized (servletRegistry) {
            return Collections.unmodifiableMap(new LinkedHashMap<>(servletRegistry));
        }
    }

    /**
     * Initialise every dynamically-registered servlet whose
     * {@code load-on-startup} is non-negative (Servlet 6 §10.3): the
     * container is responsible for driving {@code Servlet.init(ServletConfig)}
     * after all SCIs and listeners have finished registering things.
     *
     * <p>Idempotent — each entry tracks whether it has already been
     * initialised and skips a second pass. Init failures are logged but do
     * not block other servlets from initialising. Returns the number of
     * servlets actually initialised in this pass.
     *
     * <p>Called from the Rust side via the bridge after
     * {@code tomcatrs_servlet_bridge::sci::run_sci} completes, so Spring's
     * {@code DispatcherServlet} (registered with {@code loadOnStartup=1} by
     * {@code SpringServletContainerInitializer}) is wired up before the
     * connector starts dispatching real requests to it.
     */
    public int initLoadOnStartupServlets() {
        int initialised = 0;
        // Eagerly init every dynamically-registered servlet with a backing
        // instance. The Servlet spec permits the container to init lazily on
        // first request when load-on-startup is negative, but Spring Boot
        // defaults `DispatcherServlet`'s load-on-startup to -1 yet still
        // expects `getServletConfig()` to be populated by the time a
        // request lands — the eager pass keeps that contract.
        //
        // Servlets with a non-negative load-on-startup go first, in
        // ascending value order, matching the spec's ordering requirement
        // for explicit eager initialisation.
        java.util.List<RegisteredServletEntry> eligible;
        synchronized (servletRegistry) {
            eligible = new java.util.ArrayList<>();
            for (RegisteredServletEntry e : servletRegistry.values()) {
                if (!e.initialised && e.instance != null) {
                    eligible.add(e);
                }
            }
        }
        eligible.sort((a, b) -> {
            int al = a.loadOnStartup < 0 ? Integer.MAX_VALUE : a.loadOnStartup;
            int bl = b.loadOnStartup < 0 ? Integer.MAX_VALUE : b.loadOnStartup;
            return Integer.compare(al, bl);
        });
        for (RegisteredServletEntry entry : eligible) {
            try {
                TomcatRsServletConfig config =
                        new TomcatRsServletConfig(
                                entry.getName(),
                                new LinkedHashMap<>(entry.getInitParameters()),
                                this);
                entry.instance.init(config);
                entry.initialised = true;
                initialised++;
            } catch (Throwable t) {
                log("initLoadOnStartupServlets: init('" + entry.getName() + "') failed", t);
            }
        }
        return initialised;
    }

    // ------------------------------------------------------------------------
    // Dynamic filter registration
    // ------------------------------------------------------------------------

    @Override
    public FilterRegistration.Dynamic addFilter(String filterName, String className) {
        if (filterName == null || filterName.isEmpty() || className == null) {
            return null;
        }
        Filter instance = instantiate(className, Filter.class);
        if (instance == null) {
            return null;
        }
        return registerFilter(filterName, className, instance);
    }

    @Override
    public FilterRegistration.Dynamic addFilter(String filterName, Filter filter) {
        if (filterName == null || filterName.isEmpty() || filter == null) {
            return null;
        }
        return registerFilter(filterName, filter.getClass().getName(), filter);
    }

    @Override
    public FilterRegistration.Dynamic addFilter(
            String filterName, Class<? extends Filter> filterClass) {
        if (filterName == null || filterName.isEmpty() || filterClass == null) {
            return null;
        }
        Filter instance;
        try {
            instance = filterClass.getDeclaredConstructor().newInstance();
        } catch (ReflectiveOperationException e) {
            log("addFilter: cannot instantiate " + filterClass.getName(), e);
            return null;
        }
        return registerFilter(filterName, filterClass.getName(), instance);
    }

    private FilterRegistration.Dynamic registerFilter(
            String name, String className, Filter instance) {
        synchronized (filterRegistry) {
            if (filterRegistry.containsKey(name)) {
                return null;
            }
            RegisteredFilterEntry entry =
                    new RegisteredFilterEntry(name, className, instance);
            filterRegistry.put(name, entry);
            return entry;
        }
    }

    @Override
    public FilterRegistration getFilterRegistration(String filterName) {
        return filterName == null ? null : filterRegistry.get(filterName);
    }

    @Override
    public Map<String, ? extends FilterRegistration> getFilterRegistrations() {
        synchronized (filterRegistry) {
            return Collections.unmodifiableMap(new LinkedHashMap<>(filterRegistry));
        }
    }

    // ------------------------------------------------------------------------
    // Listeners
    // ------------------------------------------------------------------------

    @Override
    public void addListener(String className) {
        if (className == null || className.isEmpty()) {
            return;
        }
        EventListener instance = instantiate(className, EventListener.class);
        if (instance != null) {
            listeners.add(instance);
        }
    }

    @Override
    public <T extends EventListener> void addListener(T t) {
        if (t != null) {
            listeners.add(t);
        }
    }

    @Override
    public void addListener(Class<? extends EventListener> listenerClass) {
        if (listenerClass == null) {
            return;
        }
        try {
            listeners.add(listenerClass.getDeclaredConstructor().newInstance());
        } catch (ReflectiveOperationException e) {
            log("addListener: cannot instantiate " + listenerClass.getName(), e);
        }
    }

    // ------------------------------------------------------------------------
    // Logging
    // ------------------------------------------------------------------------

    @Override
    public void log(String msg) {
        if (nativeContextId != 0L) {
            try {
                NativeServletContext.nativeLog(nativeContextId, msg == null ? "" : msg);
                return;
            } catch (Throwable ignore) {
                // Native not wired (degraded mode): fall through to stderr.
            }
        }
        System.err.println("[tomcatrs ctx " + contextPath + "] " + msg);
    }

    @Override
    public void log(String message, Throwable throwable) {
        log(message);
        if (throwable != null) {
            throwable.printStackTrace(System.err);
        }
    }

    // ------------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------------

    /**
     * Resolve {@code className} through the current thread's context class
     * loader, instantiate it via its public no-arg constructor, and cast to
     * {@code expected}. Logs and returns {@code null} on failure rather than
     * throwing — matches what the Servlet 6 spec calls for at the
     * {@code addServlet}/{@code addFilter} boundary.
     */
    private <T> T instantiate(String className, Class<T> expected) {
        ClassLoader ctxLoader = Thread.currentThread().getContextClassLoader();
        if (ctxLoader == null) {
            ctxLoader = getClass().getClassLoader();
        }
        try {
            Class<?> c = Class.forName(className, true, ctxLoader);
            if (!expected.isAssignableFrom(c)) {
                log("instantiate: '" + className + "' is not a "
                        + expected.getName());
                return null;
            }
            Object o = c.getDeclaredConstructor().newInstance();
            return expected.cast(o);
        } catch (ReflectiveOperationException | LinkageError e) {
            log("instantiate: cannot load/build '" + className + "': " + e);
            return null;
        }
    }

    // ------------------------------------------------------------------------
    // Registration record types
    // ------------------------------------------------------------------------

    /**
     * One dynamically-registered servlet's full record: name, class name, the
     * live instance, its URL mappings (insertion order), its init parameters,
     * and the dynamic-only flags ({@code load-on-startup},
     * {@code async-supported}). The {@code ServletRegistration.Dynamic}
     * implementation Spring's bootstrappers see.
     */
    public static final class RegisteredServletEntry implements ServletRegistration.Dynamic {

        private final String name;
        private final String className;
        private final Servlet instance;
        private final LinkedHashSet<String> mappings = new LinkedHashSet<>();
        private final LinkedHashMap<String, String> initParams = new LinkedHashMap<>();
        private int loadOnStartup = -1;
        private boolean asyncSupported = false;
        private String runAsRole;
        private MultipartConfigElement multipartConfig;
        /** True once {@link Servlet#init(jakarta.servlet.ServletConfig)} has been driven. */
        boolean initialised;

        RegisteredServletEntry(String name, String className, Servlet instance) {
            this.name = name;
            this.className = className;
            this.instance = instance;
        }

        @Override
        public String getName() {
            return name;
        }

        @Override
        public String getClassName() {
            return className;
        }

        /** The live servlet instance. */
        public Servlet getInstance() {
            return instance;
        }

        @Override
        public Set<String> addMapping(String... urlPatterns) {
            if (urlPatterns == null || urlPatterns.length == 0) {
                throw new IllegalArgumentException(
                        "addMapping: urlPatterns must not be empty");
            }
            Set<String> conflicts = new HashSet<>();
            synchronized (mappings) {
                for (String p : urlPatterns) {
                    if (p == null || p.isEmpty()) {
                        throw new IllegalArgumentException(
                                "addMapping: pattern must not be null/empty");
                    }
                    if (!mappings.add(p)) {
                        conflicts.add(p);
                    }
                }
            }
            return conflicts;
        }

        @Override
        public Collection<String> getMappings() {
            synchronized (mappings) {
                return new LinkedHashSet<>(mappings);
            }
        }

        @Override
        public String getRunAsRole() {
            return runAsRole;
        }

        @Override
        public boolean setInitParameter(String key, String value) {
            if (key == null || value == null) {
                throw new IllegalArgumentException(
                        "setInitParameter: name/value must be non-null");
            }
            synchronized (initParams) {
                if (initParams.containsKey(key)) {
                    return false;
                }
                initParams.put(key, value);
                return true;
            }
        }

        @Override
        public String getInitParameter(String key) {
            synchronized (initParams) {
                return initParams.get(key);
            }
        }

        @Override
        public Set<String> setInitParameters(Map<String, String> params) {
            if (params == null) {
                throw new IllegalArgumentException(
                        "setInitParameters: map must be non-null");
            }
            Set<String> conflicts = new HashSet<>();
            synchronized (initParams) {
                for (Map.Entry<String, String> e : params.entrySet()) {
                    if (e.getKey() == null || e.getValue() == null) {
                        throw new IllegalArgumentException(
                                "setInitParameters: name/value must be non-null");
                    }
                    if (initParams.containsKey(e.getKey())) {
                        conflicts.add(e.getKey());
                    } else {
                        initParams.put(e.getKey(), e.getValue());
                    }
                }
            }
            return conflicts;
        }

        @Override
        public Map<String, String> getInitParameters() {
            synchronized (initParams) {
                return new LinkedHashMap<>(initParams);
            }
        }

        @Override
        public void setLoadOnStartup(int los) {
            this.loadOnStartup = los;
        }

        /** Effective {@code <load-on-startup>}; defaults to {@code -1} (lazy). */
        public int getLoadOnStartup() {
            return loadOnStartup;
        }

        @Override
        public void setMultipartConfig(MultipartConfigElement multipartConfig) {
            this.multipartConfig = multipartConfig;
        }

        /** The multipart config recorded for this servlet, or {@code null}. */
        public MultipartConfigElement getMultipartConfig() {
            return multipartConfig;
        }

        @Override
        public void setRunAsRole(String roleName) {
            this.runAsRole = roleName;
        }

        @Override
        public Set<String> setServletSecurity(Object constraint) {
            // No security model in v1.
            return Collections.emptySet();
        }

        @Override
        public void setAsyncSupported(boolean isAsyncSupported) {
            this.asyncSupported = isAsyncSupported;
        }

        /** Whether the servlet was registered as async-supported. */
        public boolean isAsyncSupported() {
            return asyncSupported;
        }
    }

    /**
     * One dynamically-registered filter's full record. Tracks URL-pattern and
     * servlet-name mappings separately, matching the Servlet 6 API.
     */
    public static final class RegisteredFilterEntry implements FilterRegistration.Dynamic {

        private final String name;
        private final String className;
        private final Filter instance;
        private final LinkedHashSet<String> urlPatternMappings = new LinkedHashSet<>();
        private final LinkedHashSet<String> servletNameMappings = new LinkedHashSet<>();
        private final LinkedHashMap<String, String> initParams = new LinkedHashMap<>();
        private boolean asyncSupported = false;

        RegisteredFilterEntry(String name, String className, Filter instance) {
            this.name = name;
            this.className = className;
            this.instance = instance;
        }

        @Override
        public String getName() {
            return name;
        }

        @Override
        public String getClassName() {
            return className;
        }

        /** The live filter instance. */
        public Filter getInstance() {
            return instance;
        }

        @Override
        public void addMappingForServletNames(
                EnumSet<?> dispatcherTypes,
                boolean isMatchAfter,
                String... servletNames) {
            if (servletNames == null || servletNames.length == 0) {
                throw new IllegalArgumentException(
                        "addMappingForServletNames: servletNames must not be empty");
            }
            synchronized (servletNameMappings) {
                for (String n : servletNames) {
                    if (n == null || n.isEmpty()) {
                        throw new IllegalArgumentException(
                                "addMappingForServletNames: name must not be null/empty");
                    }
                    servletNameMappings.add(n);
                }
            }
        }

        @Override
        public Collection<String> getServletNameMappings() {
            synchronized (servletNameMappings) {
                return new LinkedHashSet<>(servletNameMappings);
            }
        }

        @Override
        public void addMappingForUrlPatterns(
                EnumSet<?> dispatcherTypes,
                boolean isMatchAfter,
                String... urlPatterns) {
            if (urlPatterns == null || urlPatterns.length == 0) {
                throw new IllegalArgumentException(
                        "addMappingForUrlPatterns: urlPatterns must not be empty");
            }
            synchronized (urlPatternMappings) {
                for (String p : urlPatterns) {
                    if (p == null || p.isEmpty()) {
                        throw new IllegalArgumentException(
                                "addMappingForUrlPatterns: pattern must not be null/empty");
                    }
                    urlPatternMappings.add(p);
                }
            }
        }

        @Override
        public Collection<String> getUrlPatternMappings() {
            synchronized (urlPatternMappings) {
                return new LinkedHashSet<>(urlPatternMappings);
            }
        }

        @Override
        public boolean setInitParameter(String key, String value) {
            if (key == null || value == null) {
                throw new IllegalArgumentException(
                        "setInitParameter: name/value must be non-null");
            }
            synchronized (initParams) {
                if (initParams.containsKey(key)) {
                    return false;
                }
                initParams.put(key, value);
                return true;
            }
        }

        @Override
        public String getInitParameter(String key) {
            synchronized (initParams) {
                return initParams.get(key);
            }
        }

        @Override
        public Set<String> setInitParameters(Map<String, String> params) {
            if (params == null) {
                throw new IllegalArgumentException(
                        "setInitParameters: map must be non-null");
            }
            Set<String> conflicts = new HashSet<>();
            synchronized (initParams) {
                for (Map.Entry<String, String> e : params.entrySet()) {
                    if (e.getKey() == null || e.getValue() == null) {
                        throw new IllegalArgumentException(
                                "setInitParameters: name/value must be non-null");
                    }
                    if (initParams.containsKey(e.getKey())) {
                        conflicts.add(e.getKey());
                    } else {
                        initParams.put(e.getKey(), e.getValue());
                    }
                }
            }
            return conflicts;
        }

        @Override
        public Map<String, String> getInitParameters() {
            synchronized (initParams) {
                return new LinkedHashMap<>(initParams);
            }
        }

        @Override
        public void setAsyncSupported(boolean isAsyncSupported) {
            this.asyncSupported = isAsyncSupported;
        }

        /** Whether the filter was registered as async-supported. */
        public boolean isAsyncSupported() {
            return asyncSupported;
        }
    }
}
