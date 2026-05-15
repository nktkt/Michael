# Roadmap — Tomcat-RS Compatibility Runtime

This document tracks where the project is and where it is going. It is a
living plan: dates are aspirational, scope is negotiable, and the ordering
reflects *risk* and *dependency*, not just preference.

The guiding principle does not change: **move the dangerous, I/O-heavy,
parser-heavy, control-plane layers into Rust first; keep Servlet / JSP / EL
execution on the embedded JVM until a Rust replacement is genuinely safe.**

## Status legend

| Mark | Meaning |
|------|---------|
| ✅ | Done and exercised by tests |
| 🟡 | Partially implemented / scaffolded |
| ⬜ | Not started |

## Release overview

| Version | Theme | State |
|---------|-------|-------|
| **0.1.0** | Workspace skeleton, HTTP/1.1 connector, config + lifecycle, static serving | ✅ Released — 2026-05-14 |
| **0.2.0** | Rust control plane: deployment watcher, valve/filter pipeline, full mapper integration | ✅ Landed in 1.0.0 |
| **0.3.0** | JVM servlet bridge — boot a JVM, load a WAR, invoke a single Servlet end to end | ✅ Landed in 1.0.0 |
| **0.4.0** | WAR deployment: `web.xml` wiring, servlet mappings, filters, listeners | ✅ Landed in 1.0.0 |
| **0.5.0** | Sessions & cookies across the bridge; `DefaultServlet`, range requests, sendfile | ✅ Landed in 1.0.0 (PUT/DELETE still partial) |
| **0.6.0** | Jasper bridge: JSP execution (precompile-first) and EL on the JVM side | ✅ Landed in 1.0.0 |
| **0.7.0** | HTTP/2 + TLS termination; WebSocket transport in Rust | ✅ Landed in 1.0.0 |
| **0.8.0** | Security hardening pass, fuzzing, AJP connector | ✅ Landed in 1.0.0 |
| **0.9.0** | Manager/admin API, clustering & session replication, full observability | ✅ Landed in 1.0.0 |
| **1.0.0** | Production compatibility test suite green; documented compatibility guarantees | ✅ Released — 2026-05-15 |

The pre-1.0 minor versions were collapsed into a single `1.0.0` cut once the
work for each had landed and stabilised; see [`CHANGELOG.md`](CHANGELOG.md)
for the per-milestone list of what shipped.

---

## Milestone 1 — Repo skeleton + config + lifecycle  ·  v0.1.0  ·  ✅ Done

The foundation. Shipped in v0.1.0.

- ✅ 12-crate Cargo workspace, zero-warning build, ~210 tests passing
- ✅ `tomcatrs-core`: error model, `Lifecycle` trait, `LifecycleState` machine, `Runtime`
- ✅ `tomcatrs-config`: `server.xml`, `web.xml`, `context.xml`, `catalina.properties` parsers
- ✅ `tomcatrs-catalina`: `Server → Service → Engine → Host → Context → Wrapper` model
- ✅ Host / Context / Wrapper `Mapper` with correct servlet url-pattern precedence
- ✅ `tomcatrs-cli`: `run`, `check-config`, `version`

## Milestone 2 — HTTP/1.1 connector + static response  ·  v1.0.0  ·  ✅ Done

- ✅ Working HTTP/1.1 accept loop, request-line + header parsing, keep-alive
- ✅ `RequestLimits` enforcement (header count/size, URI length, body size, timeouts)
- ✅ URI normalization: dot-segment collapse, traversal / encoded-slash / backslash rejection
- ✅ `StaticAdapter`: static file serving with content-type guessing
- ✅ `DeploymentScanner` + `HostDeployer` + `DeploymentWatcher` driving live auto-deploy
- ✅ Engine / Host / Context valve pipeline executed on every request
- ✅ `Mapper` result drives `CatalinaAdapter` dispatch in the CLI
- ✅ Chunked transfer decode/encode (`tomcatrs_coyote::chunked`), fuzzed

## Milestone 3 — JVM boot + classloader + single servlet invocation  ·  v1.0.0  ·  ✅ Done

The highest-risk milestone — the architectural bet of the whole project.

- ✅ Embed a JVM in-process via the `jvm` feature; one `JavaVM` per runtime
- ✅ Build the Bootstrap → System → Common → Webapp classloader hierarchy
- ✅ Compile and ship the `tomcatrs-bridge.jar` facade classes
- ✅ Per-worker JNI attach (thread pool attached once, not per request)
- ✅ Invoke `HttpServlet` end to end and stream the response back to Rust
- ✅ Lazy header/attribute materialization across the FFI boundary (`request_facade`)

## Milestone 4 — WAR deploy + web.xml + servlet mapping  ·  v1.0.0  ·  ✅ Done

- ✅ Exploded-WAR deployment through the bridge; packed `.war` discovery and unpacking
- ✅ `web.xml` → servlet / filter / listener registration on the JVM side
- ✅ Annotation scanning (`@WebServlet`, `@WebFilter`, `@WebListener`)
- ✅ `load-on-startup` ordering, init params, `ServletContext` attributes
- ✅ Filter chain coordination between the Rust pipeline and JVM filters

## Milestone 5 — Filters + sessions + cookies  ·  v1.0.0  ·  🟡 Mostly done

- ✅ `SessionManager` with memory, file, Redis, JDBC, and clustered backends
- ✅ `CookieProcessor`: `Cookie` parsing, `JSESSIONID` extraction, `Set-Cookie` building
- ✅ Session bridged into the JVM (`HttpSession` backed by the Rust store)
- ✅ `AsyncContext` support — Rust request state held until Java `complete()`s
- ✅ `DefaultServlet`: range requests, conditional GET, ETags, welcome files, listings
- 🟡 `DefaultServlet` `PUT`/`DELETE` answer `501` when `read_only` is `false`; full
      write semantics (with `If-Match` preconditions) are post-1.0

## Milestone 6 — Jasper bridge  ·  v1.0.0  ·  ✅ Done

- ✅ `JasperBridge` registers `org.apache.jasper.servlet.JspServlet` per context
- ✅ `*.jsp` / `*.jspx` routed through `JspServlet`
- ✅ Precompile path: `JspC`-mangled servlet class names, `web.xml` fragment generation
- ✅ Scratch-directory lifecycle management per context
- ✅ Jakarta EL evaluator on the Rust side (plus the JVM side for JSP/JSF runtime)

## Milestone 7 — HTTP/2 + WebSocket  ·  v1.0.0  ·  🟡 Transport complete, Jakarta-API thin

- ✅ HTTP/2 connector — RFC 9113 framing, HPACK, multiplexed streams, flow control
- ✅ TLS termination via `rustls`, ALPN for `h2` and `http/1.1`
- ✅ WebSocket RFC 6455 transport: handshake, frame codec, reassembly, close handshake
- ✅ `permessage-deflate` negotiation
- 🟡 Jakarta WebSocket Jakarta-API JVM dispatch — the JNI plumbing for rich
      `@ServerEndpoint` lifecycles is intentionally shallow in 1.0; Rust-native
      WebSocket adapters work end to end

## Milestone 8 — Security hardening + fuzzing  ·  v1.0.0  ·  ✅ Done

- ✅ URI normalization, path-traversal / `WEB-INF` / `META-INF` rejection
- ✅ Request-limit enforcement, CSRF tokens
- ✅ Continuous fuzzing via `cargo-fuzz`: HTTP/1.1, chunked decode, HPACK decode,
      HTTP/2 frames, AJP `Forward Request`, cookies, URI normalize, access control
- ✅ AJP/1.3 connector (clear-text, trusted-network only, secret required)
- ✅ `BASIC`, `DIGEST` (keyed nonces, replay defence), and `FORM` (`j_security_check`)
      authenticators completed
- ✅ Realm backends: in-memory, `tomcat-users.xml` file, combined, lock-out;
      JDBC via the pluggable executor trait
- ✅ Security valves: response-header hardening (HSTS/CSP/frame/MIME/referrer)
      and HTTP-method allow-list
- ✅ `<security-constraint>` evaluation with Servlet-spec aggregation
- ⬜ LDAP realm backend (planned post-1.0)

## Milestone 9 — Manager/API + observability  ·  v1.0.0  ·  ✅ Done

- ✅ Access logs (Common / Combined), Prometheus metrics, tracing init
- ✅ Rust admin API (`ManagerService`) — `/manager/text/list`, `serverinfo`,
      `sessions`, `reload`, `stop`, `start`, `deploy`, `undeploy`
- ✅ Hot redeploy via the `DeploymentWatcher` and reloadable contexts
- ✅ Clustering: `all-to-all` (`DeltaManager`) and `primary-backup`
      (`BackupManager`) session replication backends
- ✅ JMX bridge — Rust metrics surfaced to the JVM as MBeans
- ✅ Health endpoint (`/health`, `/health/live`, `/health/ready`) and an
      OpenTelemetry OTLP/HTTP exporter
- ⬜ Manager / Host Manager compatible HTML UI — the text/JSON API ships; the
      HTML console is intentionally deferred (planned post-1.0)

## Milestone 10 — Production compatibility test suite  ·  v1.0.0  ·  ✅ Done

- ✅ Differential test harness (`tomcatrs-compat-tests`): run the same WAR on
      stock Tomcat and Tomcat-RS, diff status / headers / body / cookies /
      session behaviour / logs
- ✅ Sample WAR corpus: plain Servlet/JSP apps and a `web.xml` app
- ✅ Protocol conformance suites for HTTP/1.1, HTTP/2, AJP, TLS
- ✅ Documented compatibility guarantees in `docs/compatibility.md`
- ✅ 1.0.0: stable public APIs across the `tomcatrs-*` crates
- 🟡 A full Spring Boot WAR is included in the corpus; published performance
      baselines against stock Tomcat are tracked in CI but not yet in `docs/`

---

## Future — post-1.0

Things the project will tackle after 1.0, in no fixed order:

- **A Rust-native JSP compiler.** v1.0 keeps Jasper on the JVM. A Rust JSP →
  servlet translator would let the runtime serve JSP-heavy apps without
  embedding Jasper at all; the precompile-first workflow we already ship is
  the stepping stone.
- **Full Jakarta WebSocket (`jakarta.websocket`) integration.** The Rust
  transport is complete; making `@ServerEndpoint` dispatch and the
  `Session` / `RemoteEndpoint` Java APIs first-class through the bridge is
  the remaining work.
- **`DefaultServlet` `PUT`/`DELETE`.** Including `If-Match` preconditions,
  atomic writes, and `WEB-INF`-aware path safety.
- **A Manager / Host Manager HTML UI.** The text/JSON API ships in 1.0; an
  HTML console layered on top is straightforward but deferred.
- **LDAP realm backend.** Filling out the realm matrix on top of the
  existing `Realm` trait.
- **Kubernetes operator and Helm chart.** First-class deployment paths for
  containerised environments, including session-replication discovery,
  Manager-API exposure, and health-probe wiring.
- **Tribes-compatible cluster transport.** The session-replication logic is
  transport-agnostic; a TCP + UDP multicast transport matching Tomcat's
  Tribes stack is the obvious next backend.
- **Published performance baselines.** A documented benchmark methodology
  versus stock Tomcat, with reproducible numbers in `docs/`.
- **Native-image / static-binary builds.** Producing a single `tomcatrs`
  binary that runs the Rust subset without a JDK at all (for apps that
  don't need the servlet bridge).

---

## Cross-cutting, always-on work

These are not milestones; they run continuously across every release.

- **Testing** — every crate keeps unit tests; integration and differential
  tests grow with each release. CI must stay green and warning-free.
- **Documentation** — `docs/` (architecture, compatibility, spec-target,
  `server.xml` support matrix, migration, release notes) tracks reality, not
  intent.
- **`server.xml` fidelity** — unknown elements/attributes are warned-and-skipped;
  the support matrix in `docs/serverxml-support.md` is expanded as attributes
  become real.
- **FFI discipline** — keep the Rust ↔ JVM boundary thin: request handles, lazy
  materialization, buffered streaming, no per-header / per-byte JNI calls.

## How to contribute to the roadmap

Pick a 🟡 item or anything in the **Future** section, open an issue to claim
it, and send a PR. The highest-leverage post-1.0 help right now is
**a Rust-native JSP compiler** and **full Jakarta WebSocket integration** —
see [`CONTRIBUTING.md`](CONTRIBUTING.md).
