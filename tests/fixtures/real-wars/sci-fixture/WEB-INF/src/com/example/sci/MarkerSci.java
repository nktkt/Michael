package com.example.sci;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.nio.file.StandardOpenOption;
import java.util.Set;
import java.util.concurrent.atomic.AtomicInteger;

import jakarta.servlet.ServletContainerInitializer;
import jakarta.servlet.ServletContext;
import jakarta.servlet.ServletException;

/**
 * Tiny test SCI used by {@code tomcatrs-servlet-bridge}'s
 * {@code sci_test.rs}.
 *
 * <p>On invocation:
 *
 * <ol>
 *   <li>increments a static counter so the Rust test can assert "called
 *       exactly once" within a JVM lifetime;</li>
 *   <li>appends a marker line to the path named by the
 *       {@code tomcatrs.sci.marker} system property, when set — the Rust
 *       test sets this to a temp file path before booting the JVM, and reads
 *       the file back afterwards to verify the SCI executed.</li>
 * </ol>
 *
 * <p>Deliberately has no {@code @HandlesTypes} annotation so the Rust side
 * can drive this SCI today even without the (out-of-scope) classpath scan;
 * see {@code sci.rs} for the "honest gap" note.
 */
public class MarkerSci implements ServletContainerInitializer {

    /** Process-lifetime count of successful invocations. */
    public static final AtomicInteger INVOCATIONS = new AtomicInteger();

    @Override
    public void onStartup(Set<Class<?>> c, ServletContext ctx) throws ServletException {
        INVOCATIONS.incrementAndGet();

        String marker = System.getProperty("tomcatrs.sci.marker");
        if (marker == null || marker.isEmpty()) {
            // No marker requested; the counter alone is enough for in-JVM
            // assertions.
            return;
        }
        try {
            Path target = Paths.get(marker);
            Path parent = target.getParent();
            if (parent != null && !Files.isDirectory(parent)) {
                Files.createDirectories(parent);
            }
            String line = "MarkerSci.onStartup called, context="
                    + (ctx == null ? "<null>" : ctx.getContextPath())
                    + ", handledTypes="
                    + (c == null ? -1 : c.size())
                    + System.lineSeparator();
            Files.write(
                    target,
                    line.getBytes(StandardCharsets.UTF_8),
                    StandardOpenOption.CREATE,
                    StandardOpenOption.APPEND);
        } catch (IOException e) {
            throw new ServletException(
                    "MarkerSci: failed to write marker file: " + e, e);
        }
    }
}
