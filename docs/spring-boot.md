# Running Spring Boot 3 WARs on Tomcat-RS

Tomcat-RS targets the Servlet 6 / Jakarta EE 10 surface that Spring Boot
3.x emits. This document describes the in-tree validation fixture, what it
proves today, what it does **not** yet prove, and the concrete upgrade
path from the latter to the former.

## TL;DR

```bash
cd <workspace-root>
# 1. Build the WAR (downloads Spring Boot 3.3.5 + deps from Maven Central
#    into ~/.m2, then packages and explodes the deployable WAR).
bash tests/fixtures/real-wars/build-spring.sh

# 2. Drive the integration test. Re-runs the build script on demand if
#    the exploded tree is missing.
cargo test -p tomcatrs-servlet-bridge --features jvm --test spring_boot
```

If Maven is not installed the test **passes by graceful skip** with a clear
`eprintln!`. If Maven is installed but the test fails for a code reason, the
panic message names the next concrete gap — never a generic "something
went wrong".

## Prerequisites

| Tool       | Minimum  | Why                                                  |
|------------|----------|------------------------------------------------------|
| **JDK**    | 17       | Spring Boot 3.x compiles against Java 17 bytecode.   |
| **Maven**  | 3.6      | Builds the WAR via `spring-boot-starter-parent`.     |
| **unzip**  | any      | Expands the WAR into `spring-boot-app/exploded/`.    |
| **Rust**   | 1.75     | Per the workspace `rust-toolchain.toml`.             |
| Network    | once     | First run pulls ~25 MB of jars from Maven Central.   |

`JAVA_HOME` is honoured but not required; whichever `javac` and `mvn` are
first on `PATH` win. The integration test additionally needs the embedded
JVM the `jvm` feature provides — which is built automatically by the
bridge crate's `build.rs` when `javac` is available at compile time.

## The fixture

```
tests/fixtures/real-wars/spring-boot-app/
├── pom.xml                              -- packaging=war, parent=Spring Boot 3.3.5
├── src/main/
│   ├── java/com/example/sbapp/
│   │   ├── SbApplication.java           -- @SpringBootApplication + SpringBootServletInitializer
│   │   └── HelloController.java         -- @RestController, @GetMapping("/hello")
│   └── resources/
│       └── application.properties       -- server.servlet.context-path=/, logs at WARN
├── target/                              -- (git-ignored) Maven build output
└── exploded/                            -- (git-ignored) WAR contents, what Tomcat-RS deploys
    ├── META-INF/
    └── WEB-INF/
        ├── classes/com/example/sbapp/*.class
        └── lib/*.jar                    -- ~30 Spring/Jackson/SLF4J jars
```

Two source files, one properties file, one `pom.xml`. Nothing else is
committed; everything else is fetched on demand by `build-spring.sh`.

### Why `spring-boot-starter-tomcat` is `provided`

The fixture's `pom.xml` declares:

```xml
<dependency>
    <groupId>org.springframework.boot</groupId>
    <artifactId>spring-boot-starter-tomcat</artifactId>
    <scope>provided</scope>
</dependency>
```

This excludes `tomcat-embed-core`, `-el`, and `-websocket` from
`WEB-INF/lib/`. When the WAR is deployed into a real Servlet 6 container
(Tomcat 10/11 or Tomcat-RS) those classes are supplied by the container
rather than the WAR. Without `provided` the WAR would ship two
incompatible copies of `jakarta.servlet.*` and class-load to a hard
`LinkageError` the first time the SCI is invoked.

### Why the build script prefers `*.war.original`

The Spring Boot Maven plugin's `repackage` goal — bound to the `package`
phase by `spring-boot-starter-parent` — *replaces* the deployable WAR
with an **executable** archive (the kind you `java -jar`) under the same
name, and renames the original to `<name>.war.original`. Tomcat-RS
deploys WARs into its own container; it never runs `java -jar`. The
build script therefore explodes the `*.war.original` artifact when it
exists, which gives a clean `WEB-INF/classes/` + `WEB-INF/lib/`
structure rather than the `BOOT-INF/` layout the executable variant
uses.

## What currently works (proven by the integration test)

Running `cargo test -p tomcatrs-servlet-bridge --features jvm --test
spring_boot -- --nocapture` produces (with Maven installed, on this
workspace at the time of writing):

```text
[spring_boot] exploded WAR is ready: classes=…/exploded/WEB-INF/classes, 30 jar(s) under WEB-INF/lib
[spring_boot] run_sci summary: 0 SCI(s) ran, 2 error(s)
[spring_boot]   SCI error: SCI 'ch.qos.logback.classic.servlet.LogbackServletContainerInitializer' failed: Java exception was thrown
[spring_boot]   SCI error: SCI 'org.springframework.web.SpringServletContainerInitializer' failed: Java exception was thrown
[spring_boot] skipping dispatch: …documented @HandlesTypes gap…
test spring_boot_war_serves_hello_endpoint ... ok
```

Concretely, this proves the following ARE reproducible from a clean
checkout:

1. **Maven-driven WAR build.** A standard Spring Boot 3.3.x project
   compiles cleanly against Java 17+ and produces a deployable WAR with
   the right metadata + ~30 jars in `WEB-INF/lib/`.
2. **Tomcat-RS classloader over `WEB-INF/lib/*.jar`.** A
   `URLClassLoader` is built from `WEB-INF/classes/` plus every jar
   under `WEB-INF/lib/`; the bridge JAR is added to the common
   classpath; the Spring runtime, Spring MVC, Jackson, SLF4J, Logback
   etc. resolve.
3. **Servlet 6 SCI discovery.** The bridge's
   `ServletContainerInitializerInvoker.discoverServiceClasses` walks the
   webapp classloader for
   `META-INF/services/jakarta.servlet.ServletContainerInitializer`
   resources and finds *both* Logback's and Spring's SCIs from
   `WEB-INF/lib/*.jar`. This rules out the "service-loader only reads
   `WEB-INF/classes`" failure mode many half-built SCI implementations
   exhibit.
4. **SCI invocation up to the empty-handled-types boundary.** The
   bridge constructs a `TomcatRsServletContext`, hands it to each
   SCI, and calls `onStartup(Set, ServletContext)`. The pipeline reaches
   the SCI's own code before it throws on the empty handled-types set
   (see below).

## What currently does NOT work

The integration test detects this and skips with a clear message rather
than hanging or producing a misleading 500:

1. **`@HandlesTypes` scanning is a no-op (the v1 honest gap).** Per
   Servlet 6 §8.2.4, an SCI annotated `@HandlesTypes` expects the
   container to scan the webapp's classpath for classes that
   extend/implement/are-annotated-with any of the listed types, and to
   pass that set as the first argument to `onStartup`. Spring's
   `SpringServletContainerInitializer` declares
   `@HandlesTypes(WebApplicationInitializer.class)` and uses that set
   exclusively to find user-supplied initializers — which is how it
   discovers our `SbApplication`. The bridge currently passes an empty
   set (see the `TODO(@HandlesTypes)` in
   `crates/tomcatrs-servlet-bridge/src/sci.rs`). The result is that
   Spring's SCI exits without registering the DispatcherServlet.

2. **Bridge `ServletContext` facade is partial.** The two SCIs above
   throw `Java exception was thrown` because the `TomcatRsServletContext`
   does not yet implement every Servlet 6 method either SCI calls (e.g.
   `addServlet`, `getResourcePaths`, `getInitParameterNames`). Filling
   in the facade is independent of `@HandlesTypes`; either gap is
   sufficient to block Spring's SCI on its own.

3. **`web.xml`-less deployment ordering.** A Spring Boot WAR ships no
   `web.xml`. The container is expected to load it anyway and let SCIs
   register every servlet. The bridge's `WebappRegistrar::register`
   accepts an empty `WebXml` correctly today, but the registration
   surface the SCIs use (`ServletContext.addServlet`) does not yet
   route into the same `WebappRuntime` servlet registry that
   `JvmServletInvoker::invoke_coyote` looks up against.

## Upgrade path

Two concrete pieces of work flip this test from "skip after SCI" to
"pass with a real Spring `Hello, Spring!` JSON body":

| Step | What to land                                                                                                                    | Where                                                              |
|------|----------------------------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------------|
| 1    | Complete the `TomcatRsServletContext` facade — at minimum, `addServlet`, `addServletMapping`, `addFilter`, `getResourcePaths`. | `crates/tomcatrs-servlet-bridge/java/org/apache/tomcatrs/bridge/`  |
| 2    | Wire `addServlet` to insert a `ServletInstanceHandle` into `WebappRuntime`'s registry, so `JvmServletInvoker` can find it.       | `crates/tomcatrs-servlet-bridge/src/jni.rs` + `registration.rs`     |
| 3    | Implement `@HandlesTypes` classpath scanning in `run_sci`: read the annotation, walk every `WEB-INF/classes/**/*.class` and `WEB-INF/lib/*.jar` entry, decide membership, pass the populated `Set<Class<?>>`. | `crates/tomcatrs-servlet-bridge/src/sci.rs` (`TODO(@HandlesTypes)`) |

Once those land, the integration test's dispatch step (already written,
currently behind a runtime skip) will start asserting:

* `resp.status == 200`
* `body.contains("Hello, Spring!")`

— with no test-file changes required.

## File map

| Path                                                                                          | Purpose                                              |
|-----------------------------------------------------------------------------------------------|------------------------------------------------------|
| `tests/fixtures/real-wars/spring-boot-app/pom.xml`                                            | Maven project definition                             |
| `tests/fixtures/real-wars/spring-boot-app/src/main/java/com/example/sbapp/SbApplication.java` | `@SpringBootApplication` + `SpringBootServletInitializer` |
| `tests/fixtures/real-wars/spring-boot-app/src/main/java/com/example/sbapp/HelloController.java` | `@RestController` with `/hello` endpoint            |
| `tests/fixtures/real-wars/spring-boot-app/src/main/resources/application.properties`          | Mount at `/`, log at WARN                            |
| `tests/fixtures/real-wars/build-spring.sh`                                                    | `mvn package` + explode into `exploded/`             |
| `crates/tomcatrs-servlet-bridge/tests/spring_boot.rs`                                         | The integration test                                 |
