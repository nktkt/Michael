/*
 * Tomcat-RS — Spring Boot 3.x WAR fixture.
 *
 * This is the WAR's entry point in two senses:
 *
 *   1. When deployed into a Servlet 6 container (Tomcat 10/11 or Tomcat-RS),
 *      the container scans `META-INF/services/jakarta.servlet.ServletContainerInitializer`
 *      and finds Spring's `SpringServletContainerInitializer`. Because this
 *      class extends `SpringBootServletInitializer`, the SCI hands control
 *      here via `WebApplicationInitializer.onStartup(...)`.
 *
 *   2. When run from `java -jar ...` (not how Tomcat-RS uses it; included
 *      because Spring Boot's archetype expects a `main()`), the inherited
 *      `SpringApplication.run(...)` boots the embedded Tomcat.
 *
 * Tomcat-RS path (1) is what the integration test exercises: SCI discovery
 * + `WebApplicationInitializer.onStartup` to wire the Spring DispatcherServlet
 * + the user `@RestController`.
 */
package com.example.sbapp;

import org.springframework.boot.SpringApplication;
import org.springframework.boot.autoconfigure.SpringBootApplication;
import org.springframework.boot.builder.SpringApplicationBuilder;
import org.springframework.boot.web.servlet.support.SpringBootServletInitializer;

@SpringBootApplication
public class SbApplication extends SpringBootServletInitializer {

    /**
     * Standalone entry point. NOT used when the WAR is deployed into
     * Tomcat-RS — included so `mvn spring-boot:run` and the canonical
     * `java -jar` flow keep working unchanged.
     */
    public static void main(String[] args) {
        SpringApplication.run(SbApplication.class, args);
    }

    /**
     * Container-deployment entry point. Invoked by the
     * `SpringServletContainerInitializer` SCI when the WAR is dropped into
     * a Servlet 6 container. Returns the same application class so Spring
     * builds the application context normally.
     */
    @Override
    protected SpringApplicationBuilder configure(SpringApplicationBuilder builder) {
        return builder.sources(SbApplication.class);
    }
}
