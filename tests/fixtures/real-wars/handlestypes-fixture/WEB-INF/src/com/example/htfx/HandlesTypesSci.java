package com.example.htfx;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.nio.file.StandardOpenOption;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.Set;

import jakarta.servlet.ServletContainerInitializer;
import jakarta.servlet.ServletContext;
import jakarta.servlet.ServletException;
import jakarta.servlet.annotation.HandlesTypes;

/**
 * Test SCI used by {@code tomcatrs-servlet-bridge}'s
 * {@code sci_test.rs} to exercise the Servlet 6 {@code @HandlesTypes}
 * classpath scan.
 *
 * <p>Annotated with {@code @HandlesTypes(Marker.class)}. The container
 * is required to scan the webapp's classpath for every class that
 * extends, implements, or is annotated by {@link Marker}, and pass
 * those classes as the first argument to {@link #onStartup}.
 *
 * <p>On invocation, writes one line per discovered handled-type class
 * to the file named by the {@code tomcatrs.handlestypes.marker}
 * system property. The Rust test reads the file back and asserts that
 * both {@link AlphaImpl} and {@link BetaImpl} are listed.
 */
@HandlesTypes(Marker.class)
public class HandlesTypesSci implements ServletContainerInitializer {

    @Override
    public void onStartup(Set<Class<?>> c, ServletContext ctx) throws ServletException {
        String marker = System.getProperty("tomcatrs.handlestypes.marker");
        if (marker == null || marker.isEmpty()) {
            return;
        }
        try {
            Path target = Paths.get(marker);
            Path parent = target.getParent();
            if (parent != null && !Files.isDirectory(parent)) {
                Files.createDirectories(parent);
            }
            // Sort for deterministic output regardless of HashSet iteration
            // order — the Rust test asserts by exact lines.
            List<String> names = new ArrayList<>();
            if (c != null) {
                for (Class<?> klass : c) {
                    names.add(klass.getName());
                }
            }
            Collections.sort(names);
            StringBuilder sb = new StringBuilder();
            sb.append("HandlesTypesSci.onStartup called, context=")
                    .append(ctx == null ? "<null>" : ctx.getContextPath())
                    .append(", handledTypes=")
                    .append(names.size())
                    .append(System.lineSeparator());
            for (String name : names) {
                sb.append("  handled: ").append(name).append(System.lineSeparator());
            }
            Files.write(
                    target,
                    sb.toString().getBytes(StandardCharsets.UTF_8),
                    StandardOpenOption.CREATE,
                    StandardOpenOption.APPEND);
        } catch (IOException e) {
            throw new ServletException(
                    "HandlesTypesSci: failed to write marker file: " + e, e);
        }
    }
}
