/*
 * Licensed under the Apache License, Version 2.0.
 *
 * SCI (ServletContainerInitializer) discovery + invocation helper invoked
 * over JNI by {@code tomcatrs_servlet_bridge::sci::run_sci}.
 *
 * <p>Lives next to {@link ServletDispatcher} (not nested in
 * {@link TomcatRsBridge}) so the JNI signature is the plain class name —
 * {@code org/apache/tomcatrs/bridge/ServletContainerInitializerInvoker}.
 */
package org.apache.tomcatrs.bridge;

import jakarta.servlet.ServletContainerInitializer;
import jakarta.servlet.ServletContext;
import jakarta.servlet.ServletException;
import jakarta.servlet.annotation.HandlesTypes;

import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.lang.reflect.InvocationTargetException;
import java.net.URL;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Enumeration;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Set;

/**
 * Container-side helper that drives the Servlet 6 SCI discovery + invocation
 * dance:
 *
 * <ol>
 *   <li>{@link #discoverServiceClasses(ClassLoader)} walks every
 *       {@code META-INF/services/jakarta.servlet.ServletContainerInitializer}
 *       resource visible to the supplied class loader (using
 *       {@code ClassLoader.getResources(...)}) and returns the
 *       fully-qualified SCI class names, deduplicated, in discovery order.
 *   <li>{@link #invoke(ClassLoader, String, Set, ServletContext)} instantiates
 *       one SCI via its public no-arg constructor and invokes
 *       {@code onStartup(handledTypes, ctx)} on it.
 *   <li>{@link #readHandlesTypes(Class)} extracts the {@code @HandlesTypes}
 *       value array if present, so the Rust side can decide what set of
 *       application classes to pass into {@code onStartup(...)}.
 * </ol>
 */
public final class ServletContainerInitializerInvoker {

    private ServletContainerInitializerInvoker() {
        // Static helper.
    }

    /**
     * Read every {@code META-INF/services/jakarta.servlet.ServletContainerInitializer}
     * resource visible through {@code loader} and return the listed class
     * names in discovery order, deduplicated.
     *
     * <p>Each resource is a UTF-8 text file with one fully-qualified class
     * name per line; blank lines and {@code #}-prefixed comments are skipped,
     * matching the {@link java.util.ServiceLoader} contract.
     *
     * <p>Failures while reading any single resource are logged to
     * {@link System#err} and the discovery walk continues — one bad jar must
     * not prevent the rest of the webapp's initialisers from running.
     *
     * @param loader the class loader to consult (typically the webapp's
     *               isolating loader). Must not be {@code null}.
     * @return the discovered SCI class names, in resource order, with
     *         duplicates removed.
     */
    public static List<String> discoverServiceClasses(ClassLoader loader) {
        if (loader == null) {
            throw new IllegalArgumentException(
                    "ServletContainerInitializerInvoker.discoverServiceClasses: loader is null");
        }
        final String resource =
                "META-INF/services/jakarta.servlet.ServletContainerInitializer";
        // LinkedHashSet preserves insertion order while deduplicating across
        // jars that ship the same SCI class name twice.
        LinkedHashSet<String> names = new LinkedHashSet<>();
        Enumeration<URL> urls;
        try {
            urls = loader.getResources(resource);
        } catch (IOException e) {
            System.err.println(
                    "[tomcatrs SCI] failed to enumerate " + resource + ": " + e);
            return new ArrayList<>();
        }
        while (urls.hasMoreElements()) {
            URL url = urls.nextElement();
            try (InputStream in = url.openStream();
                 BufferedReader reader = new BufferedReader(
                         new InputStreamReader(in, StandardCharsets.UTF_8))) {
                String line;
                while ((line = reader.readLine()) != null) {
                    // Strip comments and surrounding whitespace, ServiceLoader-style.
                    int hash = line.indexOf('#');
                    if (hash >= 0) {
                        line = line.substring(0, hash);
                    }
                    line = line.trim();
                    if (!line.isEmpty()) {
                        names.add(line);
                    }
                }
            } catch (IOException e) {
                System.err.println(
                        "[tomcatrs SCI] failed to read " + url + ": " + e);
                // continue with the next resource
            }
        }
        return new ArrayList<>(names);
    }

    /**
     * Instantiate the named SCI through {@code loader}, cast it to
     * {@link ServletContainerInitializer}, and invoke
     * {@code onStartup(handledTypes, ctx)} on it.
     *
     * <p>Wraps the various failure modes — {@code ClassNotFoundException},
     * the constructor-reflection family, {@link InvocationTargetException},
     * and {@link ServletException} — in a single {@link ServletException}
     * carrying helpful context (the SCI class name and the failing step).
     *
     * @param loader        the class loader to load the SCI from.
     * @param className     fully-qualified SCI class name.
     * @param handledTypes  the (possibly empty) set of types to pass as the
     *                      first argument to {@code onStartup}.
     * @param ctx           the {@link ServletContext} to pass as the second
     *                      argument.
     * @throws ServletException on any failure to load, construct, or invoke.
     */
    public static void invoke(ClassLoader loader,
                              String className,
                              Set<Class<?>> handledTypes,
                              ServletContext ctx)
            throws ServletException {
        if (loader == null) {
            throw new ServletException("SCI invoke: loader is null");
        }
        if (className == null || className.isEmpty()) {
            throw new ServletException("SCI invoke: className is null/empty");
        }
        Class<?> clazz;
        try {
            clazz = Class.forName(className, true, loader);
        } catch (ClassNotFoundException | LinkageError e) {
            throw new ServletException(
                    "SCI: cannot load class '" + className + "': " + e, e);
        }
        if (!ServletContainerInitializer.class.isAssignableFrom(clazz)) {
            throw new ServletException(
                    "SCI: '" + className + "' does not implement "
                            + "jakarta.servlet.ServletContainerInitializer");
        }
        Object instance;
        try {
            instance = clazz.getDeclaredConstructor().newInstance();
        } catch (NoSuchMethodException e) {
            throw new ServletException(
                    "SCI: '" + className + "' has no public no-arg constructor", e);
        } catch (InvocationTargetException e) {
            Throwable cause = e.getCause() != null ? e.getCause() : e;
            throw new ServletException(
                    "SCI: '" + className + "' constructor threw: " + cause, cause);
        } catch (ReflectiveOperationException e) {
            throw new ServletException(
                    "SCI: '" + className + "' could not be instantiated: " + e, e);
        }
        ServletContainerInitializer sci = (ServletContainerInitializer) instance;
        Set<Class<?>> arg = (handledTypes == null) ? Collections.emptySet() : handledTypes;
        try {
            sci.onStartup(arg, ctx);
        } catch (ServletException e) {
            throw new ServletException(
                    "SCI: '" + className + "'.onStartup threw: " + e.getMessage(), e);
        } catch (RuntimeException e) {
            throw new ServletException(
                    "SCI: '" + className + "'.onStartup threw: " + e, e);
        }
    }

    /**
     * Read the {@code @HandlesTypes} annotation off an SCI class, returning
     * its {@code value()} array if present and a zero-length array otherwise.
     *
     * <p>Used by the Rust side to drive the (still-degraded) types-of-interest
     * scan. Built against the bridge stub annotation; production builds with
     * the real {@code jakarta.servlet-api} jar pick up the real annotation
     * automatically because the simple name + package match.
     *
     * @param sciClass the SCI class (or any other class) to read.
     * @return the {@code value()} of the {@code @HandlesTypes} annotation, or
     *         a zero-length array if the annotation is absent.
     */
    public static Class<?>[] readHandlesTypes(Class<?> sciClass) {
        if (sciClass == null) {
            return new Class<?>[0];
        }
        HandlesTypes annotation = sciClass.getAnnotation(HandlesTypes.class);
        if (annotation == null) {
            return new Class<?>[0];
        }
        Class<?>[] value = annotation.value();
        return (value == null) ? new Class<?>[0] : value;
    }
}
