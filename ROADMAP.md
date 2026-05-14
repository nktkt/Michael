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
| **0.2.0** | Rust control plane: deployment watcher, valve/filter pipeline, full mapper integration | ⬜ Planned |
| **0.3.0** | JVM servlet bridge — boot a JVM, load a WAR, invoke a single Servlet end to end | ⬜ Planned |
| **0.4.0** | WAR deployment: `web.xml` wiring, servlet mappings, filters, listeners | ⬜ Planned |
| **0.5.0** | Sessions & cookies across the bridge; `DefaultServlet`, range requests, sendfile | ⬜ Planned |
| **0.6.0** | Jasper bridge: JSP execution (precompile-first) and EL on the JVM side | ⬜ Planned |
| **0.7.0** | HTTP/2 + TLS termination; WebSocket transport in Rust | ⬜ Planned |
| **0.8.0** | Security hardening pass, fuzzing, AJP connector | ⬜ Planned |
| **0.9.0** | Manager/admin API, clustering & session replication, full observability | ⬜ Planned |
| **1.0.0** | Production compatibility test suite green; documented compatibility guarantees | ⬜ Planned |

---

## Milestone 1 — Repo skeleton + config + lifecycle  ·  v0.1.0  ·  ✅ Done

The foundation. Shipped in v0.1.0.

- ✅ 12-crate Cargo workspace, zero-warning build, ~210 tests passing
- ✅ `tomcatrs-core`: error model, `Lifecycle` trait, `LifecycleState` machine, `Runtime`
- ✅ `tomcatrs-config`: `server.xml`, `web.xml`, `context.xml`, `catalina.properties` parsers
- ✅ `tomcatrs-catalina`: `Server → Service → Engine → Host → Context → Wrapper` model
- ✅ Host / Context / Wrapper `Mapper` with correct servlet url-pattern precedence
- ✅ `tomcatrs-cli`: `run`, `check-config`, `version`

## Milestone 2 — HTTP/1.1 connector + static response  ·  v0.1.0 / v0.2.0  ·  🟡 In progress

- ✅ Working HTTP/1.1 accept loop, request-line + header parsing, keep-alive
- ✅ `RequestLimits` enforcement (header count/size, URI length, body size, timeouts)
- ✅ URI normalization: dot-segment collapse, traversal / encoded-slash / backslash rejection
- ✅ `StaticAdapter`: static file serving with content-type guessing
- ⬜ `DeploymentScanner` wired into a live host so `webapps/` auto-deploys at startup *(→ v0.2.0)*
- ⬜ Valve / Filter pipeline executed for every request *(→ v0.2.0)*
- ⬜ `Mapper` result actually driving request dispatch in the CLI *(→ v0.2.0)*
- ⬜ Chunked transfer-decoding (currently rejected with `411`) *(→ v0.2.0)*

## Milestone 3 — JVM boot + classloader + single servlet invocation  ·  v0.3.0  ·  ⬜

The highest-risk milestone — the architectural bet of the whole project.

- ⬜ Embed a JVM in-process via the `jvm` feature; one `JavaVM` per runtime
- ⬜ Build the Bootstrap → System → Common → Webapp classloader hierarchy
- ⬜ Compile and ship `tomcatrs-bridge.jar` (the Java facade classes)
- ⬜ Per-worker JNI attach (thread pool attached once, not per request)
- ⬜ Invoke one trivial `HttpServlet` end to end and stream the response back to Rust
- ⬜ Lazy header/attribute materialization across the FFI boundary

## Milestone 4 — WAR deploy + web.xml + servlet mapping  ·  v0.4.0  ·  ⬜

- ⬜ Exploded-WAR and packed-`.war` deployment through the bridge
- ⬜ `web.xml` → servlet / filter / listener registration on the JVM side
- ⬜ Annotation scanning (`@WebServlet`, `@WebFilter`, `@WebListener`)
- ⬜ `load-on-startup` ordering, init params, `ServletContext` attributes
- ⬜ Filter chain coordination between Rust pipeline and Java filters

## Milestone 5 — Filters + sessions + cookies  ·  v0.5.0  ·  🟡 Partially scaffolded

- ✅ `SessionManager` with in-memory and file-backed stores *(v0.1.0)*
- ✅ `CookieProcessor`: `Cookie` parsing, `JSESSIONID` extraction, `Set-Cookie` building *(v0.1.0)*
- ⬜ Session bridged into the JVM (`HttpSession` backed by the Rust store)
- ⬜ `AsyncContext` support — hold Rust request state until Java calls `complete()`
- ⬜ `DefaultServlet`: directory listing policy, range requests, `sendfile`
- ⬜ Redis-backed session store promoted from feature-gated scaffold to supported

## Milestone 6 — Jasper bridge  ·  v0.6.0  ·  🟡 Scaffolded

- 🟡 `JasperBridge` type and JSP-file discovery exist *(v0.1.0 scaffold)*
- ⬜ JSP requests routed through `org.apache.jasper.servlet.JspServlet`
- ⬜ Precompile path: build JSP → servlet ahead of time, serve as a normal servlet
- ⬜ Scratch-directory lifecycle management per context
- ⬜ Jakarta EL kept on the JVM side, reachable from bridged requests

## Milestone 7 — HTTP/2 + WebSocket  ·  v0.7.0  ·  🟡 Scaffolded

- 🟡 HTTP/2 connector scaffold (`Error::protocol` at dispatch today)
- 🟡 TLS scaffold (`rustls` integration planned)
- ✅ WebSocket RFC 6455 handshake + frame codec *(v0.1.0)*
- ⬜ HTTP/2 framing, HPACK, multiplexed streams, flow control
- ⬜ TLS 1.2 / 1.3 termination via `rustls`, ALPN for h2
- ⬜ WebSocket transport in Rust, with events handed to the JVM Jakarta API

## Milestone 8 — Security hardening + fuzzing  ·  v0.8.0  ·  🟡 Foundations in place

- ✅ URI normalization, path-traversal / `WEB-INF` / `META-INF` rejection *(v0.1.0)*
- ✅ Request-limit enforcement helpers, BASIC auth, CSRF tokens *(v0.1.0)*
- ⬜ Continuous fuzzing of the HTTP/1.1, HTTP/2, and AJP parsers (`cargo-fuzz`)
- ⬜ AJP connector (clear-text, trusted-network only, secret required)
- ⬜ DIGEST and FORM authenticators completed
- ⬜ Realm backends: JDBC (via JVM bridge), file, LDAP
- ⬜ Process / container isolation guidance to replace the removed Security Manager

## Milestone 9 — Manager/API + observability  ·  v0.9.0  ·  🟡 Partially built

- ✅ Access logs (Common / Combined), Prometheus metrics, tracing init *(v0.1.0)*
- ⬜ Rust admin API (localhost / private-network only by default)
- ⬜ Manager / Host Manager compatible web UI
- ⬜ Hot redeploy and reloadable contexts
- ⬜ Clustering: `all-to-all` and `primary-backup` session replication backends
- ⬜ JMX bridge — expose Rust metrics to the JVM as MBeans
- ⬜ Health endpoint, OpenTelemetry export

## Milestone 10 — Production compatibility test suite  ·  v1.0.0  ·  ⬜

- ⬜ Differential test harness: run the same WAR on stock Tomcat and Tomcat-RS,
      diff status / headers / body / cookies / session behavior / logs
- ⬜ Sample WAR corpus: plain Servlet/JSP apps, a Spring Boot WAR, a legacy
      `web.xml` app
- ⬜ Protocol conformance suites for HTTP/1.1, HTTP/2, AJP, TLS
- ⬜ Documented, versioned compatibility guarantees (`docs/compatibility.md`)
- ⬜ Performance baseline vs. stock Tomcat
- ⬜ 1.0.0: stable public APIs across the `tomcatrs-*` crates

---

## Cross-cutting, always-on work

These are not milestones; they run continuously across every release.

- **Testing** — every crate keeps unit tests; integration and differential
  tests grow with each milestone. CI must stay green and warning-free.
- **Documentation** — `docs/` (architecture, compatibility, spec-target,
  `server.xml` support matrix, migration) tracks reality, not intent.
- **`server.xml` fidelity** — unknown elements/attributes are warned-and-skipped
  today; the support matrix in `docs/serverxml-support.md` is expanded as
  attributes become real.
- **FFI discipline** — keep the Rust ↔ JVM boundary thin: request handles, lazy
  materialization, buffered streaming, no per-header / per-byte JNI calls.

## How to contribute to the roadmap

Pick an unchecked (⬜) item, open an issue to claim it, and send a PR. The
highest-leverage help right now is **Milestone 3 (the JVM servlet bridge)** and
**Milestone 7 (HTTP/2 + TLS)** — see [`CONTRIBUTING.md`](CONTRIBUTING.md).
