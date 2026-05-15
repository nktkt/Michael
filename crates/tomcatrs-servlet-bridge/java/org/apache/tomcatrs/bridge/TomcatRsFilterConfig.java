/*
 * Licensed under the Apache License, Version 2.0.
 *
 * Bridge-side FilterConfig implementation. Built on the Rust side by
 * {@code tomcatrs_servlet_bridge::registration::build_filter_config} during
 * webapp registration and passed to {@code Filter.init(FilterConfig)}.
 *
 * Carries the filter name and its {@code <init-param>} map.
 */
package org.apache.tomcatrs.bridge;

import jakarta.servlet.FilterConfig;
import jakarta.servlet.ServletContext;

import java.util.Collections;
import java.util.Enumeration;
import java.util.LinkedHashMap;
import java.util.Map;

public final class TomcatRsFilterConfig implements FilterConfig {

    private final String filterName;
    private final Map<String, String> initParams;
    private final ServletContext servletContext;

    /** Constructed from Rust over JNI. The map is copied for safety. */
    public TomcatRsFilterConfig(String filterName, Map<String, String> initParams) {
        this(filterName, initParams, new TomcatRsServletContext(""));
    }

    public TomcatRsFilterConfig(
            String filterName,
            Map<String, String> initParams,
            ServletContext servletContext) {
        this.filterName = filterName == null ? "" : filterName;
        this.initParams = initParams == null
                ? Collections.emptyMap()
                : new LinkedHashMap<>(initParams);
        this.servletContext = servletContext;
    }

    @Override
    public String getFilterName() {
        return filterName;
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
