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

- TLS termination (`rustls`, ALPN) and TCP accept loops.
- The Coyote connectors and their HTTP/1.1, HTTP/2 (RFC 9113 + HPACK), and
  AJP/1.3 codecs.
- Request-line, header, and cookie parsing and normalization.
- URI hardening — path-traversal defense, encoded-separator handling,
  canonicalization.
- The Host/Context/Wrapper mapper.
- `server.xml`, `web.xml`, `context.xml`, and `catalina.properties` parsing.
- The lifecycle orchestrator, component model, and the periodic
  background-processing tick.
- Deployment watching and auto-deploy (`HostDeployer`, `DeploymentWatcher`),
  including hot redeploy through the Manager API.
- Static resource serving via `DefaultServlet` — conditional GET, ETags,
  byte ranges, welcome files.
- Access logging (Common / Combined), Prometheus metrics, `tracing`,
  the OTLP/HTTP exporter, the health adapter, and the JMX bridge.
- Session storage backends: memory, file, Redis, JDBC (via the
  `JdbcExecutor` trait), and clustered (`DeltaManager` / `BackupManager`)
  over a pluggable `ClusterTransport`.
- Security: `BASIC`/`DIGEST`/`FORM` authenticators, in-memory / file
  (`tomcat-users.xml`) / combined / lock-out realm backends,
  `<security-constraint>` aggregation, `RemoteAddrValve`,
  `SecurityHeadersValve`, and `HttpMethodFilterValve`.
- The WebSocket transport: handshake, frame codec, reassembly, control-frame
  handling, close handshake, and `permessage-deflate` negotiation.
- The Manager service (`/manager/text/*`).

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

As of **v1.0.0**, compatibility is the **explicit guarantee for the
documented surface**: with `--features jvm` enabled, unmodified Servlets,
filters, listeners, and JSPs from a standard Tomcat 11.0.x WAR execute end
to end against the embedded JVM. Sessions, cookies, `DefaultServlet`,
auth (`BASIC`/`DIGEST`/`FORM`), `<security-constraint>` evaluation, the
Manager text/JSON API, the health endpoint, JMX, and OTel export are all
wired and tested. The HTTP/1.1, HTTP/2, AJP, TLS, and WebSocket transports
are implemented in Rust and exercised by both unit tests and a fuzz suite.

The honest caveats — what is intentionally partial in 1.0.0 — are listed in
the README's "What's still partial in 1.0.0" section and in
[`release-notes-1.0.0.md`](release-notes-1.0.0.md). The short version:
Jakarta WebSocket Jakarta-API JVM dispatch is shallow, `DefaultServlet`
`PUT`/`DELETE` answer `501` until full write semantics land, JSP
compile-on-demand still uses embedded Jasper (no Rust-native JSP compiler
yet), there is no LDAP realm, and the Manager HTML UI is not shipped.

See [`release-notes-1.0.0.md`](release-notes-1.0.0.md) for the new-in-1.0.0
tour, and the roadmap for what comes after.
