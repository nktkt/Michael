# Release notes — Tomcat-RS Compatibility Runtime 1.0.0

*Released 2026-05-15.*

This is the first stable release of the Tomcat-RS Compatibility Runtime.
Going from the v0.1.0 scaffold to v1.0.0 collapsed the planned 0.2 – 0.9
minor versions into a single cut once their work had landed and stabilised.
The result is a runtime that can host a standard Tomcat 11.0.x WAR — with
unmodified Servlets, filters, listeners, and JSPs — over HTTP/1.1, HTTP/2,
AJP, or TLS, with sessions, security, and the operations surface (Manager
API, health, JMX, OpenTelemetry) all wired and exercised by tests.

The full per-milestone list of additions lives in [`../CHANGELOG.md`](../CHANGELOG.md);
this page is the short tour.

## What's new vs v0.1.0

### Transports and connectors

- **HTTP/2** — RFC 9113 framing, HPACK (RFC 7541) with static + dynamic
  table and Huffman, multiplexed streams, flow control, `GOAWAY` /
  `RST_STREAM`. Fuzzed.
- **AJP/1.3 (server side)** — `Forward Request` decode and
  `Send Headers` / `Send Body Chunk` / `End Response` encode, with the
  documented Ghostcat-aware "trusted network only, secret required"
  deployment posture.
- **TLS termination** — via `rustls` (using the `ring` provider) with
  ALPN-negotiated `h2` and `http/1.1`. Behind the default-on `tls` feature.

### Application execution

- **JVM servlet bridge** *(with `--features jvm`)* — embedded JVM with a
  per-process worker pool, Bootstrap → System → Common → Webapp
  classloaders, lazy header/attribute materialisation across the FFI,
  end-to-end `HttpServlet` invocation, and `AsyncContext` support.
- **WAR deployment** — `web.xml` wiring plus `@WebServlet` / `@WebFilter` /
  `@WebListener` annotation scanning, `load-on-startup` ordering, init
  params, and `ServletContext` attributes.
- **Jasper bridge** — `*.jsp` / `*.jspx` routed through
  `org.apache.jasper.servlet.JspServlet`; a precompile path that produces
  `JspC`-mangled servlet class names and a matching `web.xml` fragment.
- **Jakarta EL** — a self-contained Rust evaluator for plain `${...}` /
  `#{...}` expressions and template interpolation, on top of the JVM-side
  EL that backs JSP/JSF runtime.
- **`DefaultServlet`** — real static-resource semantics: conditional GET,
  ETags, single and multipart byte ranges, welcome files, optional
  directory listings, MIME typing.

### Sessions

- Memory, file, Redis (`--features redis`), JDBC (via the `JdbcExecutor`
  trait), and clustered (`DeltaManager` all-to-all, `BackupManager`
  primary-backup) over a pluggable `ClusterTransport`.
- `HttpSession` bridged into the JVM and backed by the Rust store.

### Security

- `BASIC` (RFC 7617), `DIGEST` (RFC 2617/7616, keyed nonces with replay
  defence), and `FORM` (`j_security_check`) authenticators.
- In-memory, `tomcat-users.xml` file, combined, and lock-out realm
  backends; JDBC via the executor trait.
- `<security-constraint>` evaluation with the Servlet-spec aggregation
  rules.
- `RemoteAddrValve`, `SecurityHeadersValve` (HSTS / CSP / frame-options /
  MIME-sniff / referrer), and `HttpMethodFilterValve`.
- CSRF token helpers.

### WebSocket

- Full Rust transport: handshake, frame codec, message reassembly with
  interleaved control frames, automatic pong, the close handshake, and
  `permessage-deflate` negotiation.

### Operations and observability

- **Manager API** (text/JSON): `/manager/text/list`, `serverinfo`,
  `sessions`, `reload`, `stop`, `start`, `deploy`, `undeploy`, served as a
  Coyote `Adapter`.
- **Hot redeploy** via `HostDeployer` / `DeploymentWatcher` and the
  Manager API.
- **Health endpoint**: `/health`, `/health/live`, `/health/ready` in the
  shape of the IETF "health-check" draft.
- **JMX bridge** — Rust metrics surfaced as JVM MBeans so JConsole /
  VisualVM / APM tooling keeps working.
- **OpenTelemetry export** — OTLP/HTTP renderer with a built-in `POST`
  helper.
- Common / Combined access logs, Prometheus metrics, `tracing`
  integration.

### Testing and hardening

- `cargo-fuzz` targets for HTTP/1.1, chunked decode, cookie parsing, URI
  normalize, HPACK decode, HTTP/2 frames, AJP `Forward Request`, and the
  access-control matcher.
- A differential test harness (`tomcatrs-compat-tests`) that runs the same
  WAR on stock Tomcat and Tomcat-RS and diffs the responses.

## Try it

Build everything (the `jvm` feature requires a JDK 17+):

```sh
# Rust-only build (no JDK required)
cargo build --release

# Full build with the embedded JVM
cargo build --release -p tomcatrs-servlet-bridge --features jvm
```

Run the test suite:

```sh
cargo test
```

Start the server with the sample configuration:

```sh
cargo run -p tomcatrs-cli -- run --port 8080 --app-base ./webapps
```

Validate a `server.xml` without booting the server:

```sh
cargo run -p tomcatrs-cli -- check-config conf/server.xml
```

Hit the default webapp, the health endpoint, and the Manager API:

```sh
# the default ROOT webapp
curl -i http://localhost:8080/

# health probes
curl -i http://localhost:8080/health
curl -i http://localhost:8080/health/live
curl -i http://localhost:8080/health/ready

# manager: list deployed contexts (JSON)
curl -i http://localhost:8080/manager/text/list

# manager: best-effort reload of a context
curl -i -X POST 'http://localhost:8080/manager/text/reload?context=/'
```

Try HTTP/2 over TLS (assumes a connector with `SSLEnabled="true"` and a
matching cert in `conf/server.xml`):

```sh
curl --http2 -ki https://localhost:8443/
```

## Known limitations

These are intentionally partial in 1.0.0 and tracked under
[`../ROADMAP.md`](../ROADMAP.md)'s "Future" section:

- **`DefaultServlet` `PUT` / `DELETE`** answer `501` when `read_only` is
  `false`; full write semantics with `If-Match` preconditions are post-1.0.
- **Jakarta WebSocket Jakarta-API JVM dispatch** is shallow. The Rust
  transport is complete and Rust-native WebSocket adapters work; rich
  `@ServerEndpoint` lifecycle wiring through the JVM bridge is future
  work.
- **JSP compile-on-demand** uses embedded Jasper. There is no Rust-native
  JSP compiler yet — use the precompile-first workflow for predictable
  deploy-time errors and Java-compiler-free production images.
- **LDAP realm backend** is not yet implemented.
- **Manager / Host Manager HTML UI** is not shipped; the text/JSON API is.
- **Tribes-compatible cluster transport** is not bundled; the clustering
  code is transport-agnostic and ships with an in-memory channel transport
  for testing and single-process multi-node setups.

## Acknowledgements

Thank you to everyone who reported issues, reviewed PRs, ran their WARs
against pre-release builds, and pushed back on incomplete designs during
the 0.x cycle — and to the Apache Software Foundation for the decades of
work on Apache Tomcat that this project takes as its compatibility target.
Tomcat-RS is an independent, experimental project and is not affiliated
with, sponsored by, or endorsed by the ASF.
