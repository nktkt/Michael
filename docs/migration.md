# Migrating from Apache Tomcat

This is a short guide for someone who already runs Apache Tomcat and wants to
try the same application on Tomcat-RS. As of **v1.0.0** the compatibility
surface is broad enough that a standard Tomcat 11.0.x WAR — Servlets, filters,
listeners, JSPs (precompile-first preferred), sessions, `BASIC`/`DIGEST`/`FORM`
auth, `<security-constraint>`s — should run unmodified.

## The basic idea

Tomcat-RS reuses Tomcat's configuration shape on purpose. In the common case
you should be able to:

1. **Copy your `server.xml`** into `conf/server.xml`.
2. **Point `appBase`** at your existing `webapps/` directory (or copy your
   webapps under the project's `webapps/`).
3. **Build with the `jvm` feature** if your apps need servlet/JSP execution
   (i.e. anything beyond static content):

   ```sh
   cargo build --release -p tomcatrs-servlet-bridge --features jvm
   ```

4. **Run** `cargo run -p tomcatrs-cli -- run`.

Before starting the server, validate the config:

```sh
cargo run -p tomcatrs-cli -- check-config conf/server.xml
```

This prints exactly which elements and attributes were Supported, Partial,
Ignored-with-warning, or Planned — see [`serverxml-support.md`](serverxml-support.md)
for the full matrix.

## What works in v1.0.0

- HTTP/1.1, HTTP/2 (with HPACK), and AJP/1.3 connectors, plus TLS termination
  via `rustls` with ALPN (`h2` / `http/1.1`).
- `server.xml`, `web.xml`, `context.xml`, `catalina.properties` parsing for
  the full component tree.
- The lifecycle and component model — your component tree starts and stops in
  the right order, with the background-processing tick driving periodic work.
- Auto-deploy + hot redeploy via `HostDeployer` / `DeploymentWatcher` and the
  Manager API (`/manager/text/reload`).
- Host / Context routing from the request URI, with Servlet-spec URL-pattern
  precedence.
- `DefaultServlet`: static file serving with conditional GET, ETags, byte
  ranges, welcome files, and optional directory listings.
- Servlets, filters, listeners, and JSPs running on the embedded JVM via
  `tomcatrs-servlet-bridge` (`--features jvm`), with `AsyncContext`,
  `web.xml` wiring, and `@WebServlet`/`@WebFilter`/`@WebListener` annotations.
- Sessions across memory, file, Redis, JDBC, and clustered
  (`DeltaManager` / `BackupManager`) backends; `HttpSession` bridged into the
  JVM.
- Cookies: `Cookie` parsing, `JSESSIONID` extraction, `Set-Cookie` building.
- Security: `BASIC`, `DIGEST`, and `FORM` authenticators against in-memory /
  `tomcat-users.xml` / combined / lock-out realm backends, plus JDBC via the
  pluggable executor. `<security-constraint>` evaluation is wired.
- Hardening valves: response-header hardening (HSTS / CSP / frame-options /
  MIME-sniff / referrer) and HTTP-method allow-list.
- WebSocket transport (handshake, frame codec, reassembly, close,
  `permessage-deflate`).
- Manager API (text/JSON), health endpoint (`/health[/live|/ready]`), JMX
  bridge, OTLP/HTTP export, Prometheus metrics, access logs, `tracing`.
- URI hardening and request limits.
- Fuzzed parsers for HTTP/1.1, HTTP/2, AJP, HPACK, chunked, cookies, URI
  normalize, and access control.

## What to expect (and not expect) in v1.0.0

- **`DefaultServlet` `PUT`/`DELETE`** answer `501` when `read_only` is
  `false`; full write semantics with `If-Match` preconditions are post-1.0.
- **Jakarta WebSocket Jakarta-API JVM dispatch** is shallow: the Rust
  transport works end to end and Rust-native WebSocket adapters are first
  class, but rich `@ServerEndpoint` lifecycles defer to a future release.
- **JSP compile-on-demand** still goes through embedded Jasper on the JVM
  side — there is no Rust-native JSP compiler yet. For predictable
  deploy-time errors and no Java compiler in the production image, use the
  precompile path (`PrecompileTask` in `tomcatrs-jsp`).
- **LDAP realm** is not implemented (planned post-1.0).
- **Manager / Host Manager HTML UI** is not shipped; the text/JSON API is.
- Unknown `server.xml` content is skipped with a warning rather than failing —
  expect warnings for features that are not yet implemented.

Cross-reference the relevant modules when something does not behave as
expected:

- Security: `tomcatrs-security::{auth_basic, auth_digest, auth_form, realm,
  realm_backends, constraints}` and `tomcatrs-catalina::security_valve`.
- Manager API: `tomcatrs-catalina::manager`.
- Health / JMX / OTel: `tomcatrs-observability::{health, jmx_bridge, otel}`.
- Hot redeploy: `tomcatrs-catalina::{deployer, background}`.

## Comparison-testing approach

The recommended way to evaluate compatibility — and the methodology
`tomcatrs-compat-tests` is built around — is **differential testing against
stock Tomcat**:

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
