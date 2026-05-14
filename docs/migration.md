# Migrating from Apache Tomcat

This is a short guide for someone who already runs Apache Tomcat and wants to
try the same application on Tomcat-RS. It is honest about the current state:
v0.1.0 is an early MVP, so treat this as "how to experiment," not "how to
migrate production."

## The basic idea

Tomcat-RS reuses Tomcat's configuration shape on purpose. In the common case
you should be able to:

1. **Copy your `server.xml`** into `conf/server.xml`.
2. **Point `appBase`** at your existing `webapps/` directory (or copy your
   webapps under the project's `webapps/`).
3. **Run** `cargo run -p tomcatrs-cli -- run`.

Before starting the server, validate the config:

```sh
cargo run -p tomcatrs-cli -- check-config conf/server.xml
```

This prints exactly which elements and attributes were Supported, Partial,
Ignored-with-warning, or Planned — see `docs/serverxml-support.md` for the full
matrix.

## What works today

- HTTP/1.1 listening on the `<Connector>` port.
- `server.xml` parsing for Server / Service / Connector / Engine / Host /
  Context.
- The lifecycle and component model — your component tree starts and stops in
  the right order.
- Host / Context routing from the request URI.
- Static file serving from the webapp directory.
- Sessions (in-memory or file-backed) and cookie handling.
- Access logs and metrics.
- URI hardening and request limits.
- The WebSocket handshake and frame codec.

## What to expect (and not expect) in v0.1.0

- **Servlets and JSPs do not execute yet.** The JVM bridge is a scaffold. A
  WAR will deploy and its static content will serve, but requests that need
  servlet or JSP execution will return a "bridge not available" result until
  the `jvm` feature and the remaining JNI work land.
- **HTTP/2, AJP, and TLS connectors are scaffolded.** Configure only an
  HTTP/1.1 `<Connector>` for now.
- **Clustering and the Manager UI are not available.**
- Unknown `server.xml` content is skipped with a warning rather than failing —
  so don't be surprised to see warnings for features not yet implemented.

In short: today Tomcat-RS is useful for exercising the Rust runtime layers
(connector, routing, config, static content, sessions, security). Full
application compatibility arrives as the roadmap milestones land.

## Comparison-testing approach

The recommended way to evaluate compatibility — and the methodology the test
suite is built around — is **differential testing against stock Tomcat**:

1. Pick a WAR.
2. Deploy it on a stock Apache Tomcat 11.0.x instance and on Tomcat-RS, with
   equivalent `server.xml` and `web.xml`.
3. Send the same set of requests to both.
4. **Diff the observable outputs**: response status codes, response headers,
   response bodies, `Set-Cookie` headers and cookie behavior, and the access
   log lines.
5. Any divergence is either a bug to fix or a documented, intentional
   difference.

See `tests/README.md` for how this is organized into the test tree.
