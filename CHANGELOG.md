# Changelog

All notable changes to the Tomcat-RS Compatibility Runtime are documented in
this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0] - 2026-05-15

First stable release of the Tomcat-RS Compatibility Runtime. v1.0.0
collapses the planned 0.2 – 0.9 minor versions into one cut once their work
had landed and stabilised. The major additions, grouped by the originally
planned milestone, are below.

### Added — Milestone 2: HTTP/1.1 + control plane

- `HostDeployer` and `DeploymentWatcher` wired into every live `Host`, so
  `appBase` is scanned at startup and re-scanned periodically; the auto-deploy
  loop is cancel-safe.
- Engine / Host / Context valve pipeline executed on every request.
- `CatalinaAdapter` so a connector dispatches through the mapper.
- Chunked transfer encode/decode (`tomcatrs_coyote::chunked`), fuzzed.

### Added — Milestone 3: JVM servlet bridge

- Embedded JVM behind the `--features jvm` flag, with one `JavaVM` per
  runtime and a JNI-attached worker pool.
- Bootstrap → System → Common → Webapp classloader hierarchy.
- `tomcatrs-bridge.jar` facade classes (`TomcatRsRequestFacade`,
  `TomcatRsResponseFacade`, `TomcatRsServletContext`).
- Lazy header / attribute materialisation across the FFI boundary.
- End-to-end `HttpServlet` invocation streaming the response back to Rust.

### Added — Milestone 4: WAR deploy + web.xml + servlet mapping

- Exploded-WAR and packed-`.war` deployment.
- `web.xml` → servlet / filter / listener registration on the JVM side.
- Annotation scanning for `@WebServlet`, `@WebFilter`, `@WebListener`.
- `load-on-startup` ordering, init params, `ServletContext` attributes.

### Added — Milestone 5: Sessions, cookies, DefaultServlet

- `SessionManager` with pluggable stores: memory, file, Redis
  (`--features redis`), JDBC via the `JdbcExecutor` trait, and clustered
  (`DeltaManager` all-to-all and `BackupManager` primary-backup) backed by a
  pluggable `ClusterTransport`.
- `HttpSession` bridged into the JVM and backed by the Rust store.
- `AsyncContext` support — request state held until Java calls `complete()`.
- `DefaultServlet`: conditional GET, ETags, `Last-Modified`, single and
  multipart byte ranges, welcome files, optional directory listings, MIME
  typing. `PUT`/`DELETE` return `501` when `read_only` is `false` (full
  semantics post-1.0).

### Added — Milestone 6: Jasper bridge + Jakarta EL

- `JasperBridge` registering `org.apache.jasper.servlet.JspServlet` per
  context; `*.jsp` / `*.jspx` routed through it.
- Precompile path with `JspC`-mangled servlet class names, `web.xml` fragment
  generation, scratch-directory management.
- Self-contained Rust Jakarta EL evaluator (`tomcatrs_jsp::el`) for plain
  `${...}` / `#{...}` expressions and template interpolation, with the full
  set of EL coercions.

### Added — Milestone 7: HTTP/2, TLS, WebSocket transport

- HTTP/2 connector: full RFC 9113 framing, multiplexed streams, flow control,
  `GOAWAY` / `RST_STREAM` semantics.
- HPACK (RFC 7541) codec with static and dynamic table, Huffman, and
  validation of the connection-error conditions from RFC 9113.
- TLS termination via `rustls` (using the `ring` provider) with
  ALPN-negotiated `h2` and `http/1.1`, gated by the default-on `tls` feature.
- WebSocket transport: handshake, frame codec, message reassembly with
  interleaved control frames, automatic pong, close handshake, and
  `permessage-deflate` negotiation.

### Added — Milestone 8: Security hardening + AJP + fuzzing

- AJP/1.3 connector (server side): `Forward Request` decoding,
  `Send Headers` / `Send Body Chunk` / `End Response` encoding, with the
  documented Ghostcat-aware "trusted network only, secret required" posture.
- `BASIC`, `DIGEST` (keyed nonces, freshness window, replay defence), and
  `FORM` (`j_security_check`) authenticators.
- Realm backends: `InMemoryRealm`, `FileRealm` (`tomcat-users.xml`),
  `CombinedRealm`, `LockOutRealm`; JDBC via the executor trait.
- `<security-constraint>` registry with the Servlet-spec aggregation rules.
- Security valves: `SecurityHeadersValve` (HSTS, CSP, frame-options,
  MIME-sniff guard, referrer policy) and `HttpMethodFilterValve`.
- CSRF token support.
- `cargo-fuzz` targets for HTTP/1.1, chunked decode, cookie parsing, URI
  normalize, HPACK decode, HTTP/2 frames, AJP `Forward Request`, and the
  access-control matcher (`fuzz/fuzz_targets/`).

### Added — Milestone 9: Manager API + observability

- `ManagerService` — text/JSON Manager API mounted as a Coyote `Adapter`:
  `/manager/text/list`, `serverinfo`, `sessions`, `reload`, `stop`, `start`,
  `deploy`, `undeploy`.
- Hot redeploy via the `DeploymentWatcher` (and the Manager `reload`
  endpoint).
- `HealthAdapter` — IETF "health-check"-shaped `/health`, `/health/live`,
  and `/health/ready`.
- `JmxBridge` — Rust metrics surfaced as JVM MBeans for JConsole / VisualVM /
  APM tooling.
- `OtelMetricsExporter` — OTLP/HTTP renderer plus a built-in `POST` helper
  for shipping the metrics registry to a collector.
- `ContainerBackgroundProcessor` — the periodic tick over the component
  tree, driving session expiry, reloadable-context detection, and
  auto-deployment scans.

### Added — Milestone 10: Compatibility test suite

- `tomcatrs-compat-tests` — differential test harness that runs the same WAR
  on stock Tomcat and Tomcat-RS and diffs the responses.
- Protocol conformance suites for HTTP/1.1, HTTP/2, AJP, and TLS under
  `tests/protocol/`.
- Sample WAR corpus under `tests/fixtures/` covering plain Servlet/JSP apps
  and `web.xml`-based apps.
- Documented compatibility guarantees in [`docs/compatibility.md`](docs/compatibility.md).
- Public-facing release-notes page at [`docs/release-notes-1.0.0.md`](docs/release-notes-1.0.0.md).

### Changed

- README, ROADMAP, and `docs/*.md` rewritten to describe v1.0.0 reality
  instead of v0.1.0 scaffold status.
- `tomcatrs-servlet-bridge` `NoopServletInvoker` now responds `501 Not
  Implemented` instead of "bridge not available" when the `jvm` feature is
  off, matching the rest of the runtime's behaviour.

### Known limitations

- `DefaultServlet` `PUT` / `DELETE` answer `501` when `read_only` is
  `false`; full write semantics with `If-Match` preconditions are post-1.0.
- Jakarta WebSocket Jakarta-API dispatch through the JVM bridge is
  intentionally shallow — the Rust transport is complete, but rich
  `@ServerEndpoint` lifecycles defer to a future release.
- JSP runtime compile-on-demand uses embedded Jasper; there is no
  Rust-native JSP compiler. The precompile-first workflow is the recommended
  deployment model.
- LDAP realm backend is not yet implemented.
- A Manager / Host Manager HTML console is not shipped; the text/JSON API
  is.

## [0.1.0] - 2026-05-14

Initial early-MVP scaffold release.

### Added

- Initial Cargo workspace and project structure.
- Twelve workspace crates under `crates/`:
  - `tomcatrs-core` — shared types, error model, lifecycle traits, component model.
  - `tomcatrs-config` — `server.xml`, `web.xml`, and `catalina.properties` parsing.
  - `tomcatrs-coyote` — connector layer (HTTP/1.1 working; HTTP/2 and AJP scaffolded).
  - `tomcatrs-catalina` — container engine, valve pipeline, and request mapper.
  - `tomcatrs-webapp` — webapp model, deployment, and static resource serving.
  - `tomcatrs-servlet-bridge` — JNI bridge scaffold to an embedded JVM.
  - `tomcatrs-jsp` — JSP/Jasper orchestration glue.
  - `tomcatrs-session` — session manager and storage backends.
  - `tomcatrs-security` — request limits and URI hardening.
  - `tomcatrs-websocket` — WebSocket handshake and frame codec.
  - `tomcatrs-observability` — access logs, metrics, and tracing hooks.
  - `tomcatrs-cli` — the `tomcatrs` command-line binary.
- Working HTTP/1.1 connector with request-line, header, chunked-body, and
  keep-alive support.
- `server.xml` parser producing a validated Server/Service/Connector/Engine/
  Host/Context component tree.
- Lifecycle model: the `New → Initialized → Starting → Started → Stopping →
  Stopped → Destroyed` state machine (plus `Failed`) with ordered startup and
  shutdown of the component tree.
- Host / Context / Wrapper mapper for routing request URIs to webapps.
- Session manager with in-memory and file-backed session stores.
- Security hardening: URI normalization, path-traversal and encoded-separator
  rejection, and header/body size and count limits.
- WebSocket frame codec (encode/decode) and the upgrade handshake.
- Observability crate: access logging, basic metrics, and tracing integration.
- JVM servlet bridge scaffold, gated behind the optional `jvm` Cargo feature.
- `tomcatrs` CLI with `run`, `check-config`, and `version` subcommands.
- Project documentation: README, architecture, compatibility, spec-target,
  `server.xml` support matrix, and migration guide.
- Sample configuration: `conf/server.xml`, `conf/web.xml`,
  `conf/catalina.properties`, and a default `webapps/ROOT/index.html`.

[Unreleased]: https://github.com/nktkt/Michael/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/nktkt/Michael/releases/tag/v1.0.0
[0.1.0]: https://github.com/nktkt/Michael/releases/tag/v0.1.0
