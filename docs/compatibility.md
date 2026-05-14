# Compatibility

Tomcat-RS aims to host **existing, unmodified Java web applications**. This
document describes what that means in practice.

## Target platform

- **Apache Tomcat branch:** `main` / the 11.0.x line. The component model,
  `server.xml` schema, and default behaviors are taken from Tomcat 11.0.x.
- **Jakarta EE level:** Jakarta Servlet semantics as shipped with Tomcat 11
  (the `jakarta.*` namespace, not the legacy `javax.*` namespace).
- **Java runtime:** Java 17 or newer for the embedded JVM.

## What a deployable WAR looks like

Tomcat-RS targets standard WAR layout. A deployed application — whether an
expanded directory or a `.war` archive under a Host's `appBase` — is expected
to contain:

- `WEB-INF/web.xml` — the deployment descriptor (optional for annotation-only
  apps, but parsed when present).
- `WEB-INF/classes/` — the application's compiled classes.
- `WEB-INF/lib/*.jar` — the application's bundled dependencies.
- Static content (HTML, CSS, JS, images) at any path outside `WEB-INF`.
- JSP files at any path outside `WEB-INF`.

The following Servlet-spec concepts are in scope as compatibility targets:

| Feature | Notes |
| --- | --- |
| Servlets | Declared in `web.xml` or via annotations; invoked on the JVM. |
| Filters | `web.xml`/annotation filter chains; run on the JVM. |
| Listeners | Context, session, and request listeners; run on the JVM. |
| Sessions | `HttpSession` semantics; storage backends are Rust-side. |
| Cookies | Parsing/serialization in Rust; application access via the JVM. |
| JSP | Compiled and executed by the embedded Jasper via the bridge. |
| Expression Language | Jakarta EL, evaluated on the JVM. |
| WebSocket | Handshake and framing in Rust; endpoint logic on the JVM. |
| Static resources | Served directly by the Rust static resource handler. |

## Responsibility split: Rust side vs JVM side

The dividing line is: **transport, parsing, routing, and the control plane are
Rust; application code execution is JVM.**

### Rust side

- TLS and TCP accept loops.
- The Coyote connectors and their HTTP/1.1, HTTP/2, and AJP codecs.
- Request-line, header, and cookie parsing and normalization.
- URI hardening — path-traversal defense, encoded-separator handling,
  canonicalization.
- The Host/Context/Wrapper mapper.
- `server.xml`, `web.xml`, and `catalina.properties` parsing.
- The lifecycle orchestrator and component model.
- Deployment watching and auto-deploy.
- Static resource serving.
- Access logging, metrics, and tracing.
- Session storage backends (memory, file, cluster).
- The clustering / replication transport.

### JVM side

- Servlet API execution — `service()`, `doGet()`, `doPost()`, and so on.
- Filter and Listener invocation (the *chain* is driven from Rust; the
  application code runs on the JVM).
- Jasper: JSP translation, compilation, and runtime.
- Jakarta Expression Language evaluation.
- Per-webapp classloader isolation and Java classloading semantics.
- WebSocket endpoint dispatch (the application's annotated/programmatic
  endpoints).

## Honest status note

Compatibility is the **goal**, not yet the **guarantee**. In v0.1.0 the JVM
bridge is a scaffold: servlet and JSP invocation are not yet wired end to end.
The Rust-side pieces above (connector, mapper, config, sessions, security,
WebSocket codec) are implemented and tested. See the README for the full
"works vs scaffolded" breakdown and the roadmap.
