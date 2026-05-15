/*
 * Licensed under the Apache License, Version 2.0.
 *
 * Bridge-side ServletConfig implementation. Built on the Rust side by
 * {@code tomcatrs_servlet_bridge::registration::build_servlet_config} during
 * webapp registration and passed to {@code Servlet.init(ServletConfig)}.
 *
 * Carries the servlet name and a snapshot of the {@code <init-param>} map
 * from {@code web.xml} (or the corresponding annotation). The bridge does
 * not expose a real {@link jakarta.servlet.ServletContext} yet — see
 * {@link TomcatRsServletContext} — so {@link #getServletContext()} returns
 * a minimal facade.
 */
package org.apache.tomcatrs.bridge;

import jakarta.servlet.ServletConfig;
import jakarta.servlet.ServletContext;

import java.util.Collections;
import java.util.Enumeration;
import java.util.LinkedHashMap;
import java.util.Map;

public final class TomcatRsServletConfig implements ServletConfig {

    private final String servletName;
    private final Map<String, String> initParams;
    private final ServletContext servletContext;

    /** Constructed from Rust over JNI. The map is copied for safety. */
    public TomcatRsServletConfig(String servletName, Map<String, String> initParams) {
        this(servletName, initParams, new TomcatRsServletContext(""));
    }

    public TomcatRsServletConfig(
            String servletName,
            Map<String, String> initParams,
            ServletContext servletContext) {
        this.servletName = servletName == null ? "" : servletName;
        this.initParams = initParams == null
                ? Collections.emptyMap()
                : new LinkedHashMap<>(initParams);
        this.servletContext = servletContext;
    }

    @Override
    public String getServletName() {
        return servletName;
    }

    @Override
    public ServletContext getServletContext() {
        return servletContext;
    }

    @Override
    public String getInitParameter(String name) {
        return initParams.get(name);
    }

    @Override
    public Enumeration<String> getInitParameterNames() {
        return Collections.enumeration(initParams.keySet());
    }
}
