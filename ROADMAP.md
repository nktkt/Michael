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
| **1.1.0** | Compatibility & polish — DefaultServlet writes, LDAP realm, Manager HTML UI, published perf baselines | ⬜ Q3 2026 |
| **1.2.0** | WebSocket completeness — full Jakarta WebSocket Jakarta-API JVM dispatch + extensions | ⬜ Q4 2026 |
| **1.3.0** | Operations & containers — Kubernetes operator, Helm chart, OCI images, sidecar integration | ⬜ Q1 2027 |
| **1.4.0** | Tribes-style cluster transport — wire-compatible session replication backend | ⬜ Q2 2027 |
| **1.5.0** | Observability deepening — OTel traces and logs, distributed tracing across the bridge, profiling | ⬜ Q3 2027 |
| **1.6.0** | Native image + JDK-less mode — pure-Rust runtime subset for Java-free deployments | ⬜ Q4 2027 |
| **1.7.0** | Rust-native JSP compiler (alpha) — JSP→Rust translation, coexists with Jasper as fallback | ⬜ Q1 2028 |
| **1.8.0** | HTTP/3 + QUIC connector | ⬜ Q2 2028 |
| **1.9.0** | Compliance & certification — Jakarta EE Web Profile TCK runs, CVE response process | ⬜ Q3 2028 |
| **2.0.0** | Pure-Rust servlet container — optional JVM-less mode for Servlet 6.x; first major breaking release | ⬜ 2029 |

The pre-1.0 minor versions were collapsed into a single `1.0.0` cut once the
work for each had landed and stabilised; see [`CHANGELOG.md`](CHANGELOG.md)
for the per-milestone list of what shipped. The 1.x line follows ordinary
semver: backward-compatible additions in minor releases, breaking changes
reserved for 2.0.

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

## Milestone 11 — Compatibility & polish  ·  v1.1.0  ·  ⬜ Planned (Q3 2026)

The "make 1.0 boringly complete" release. Every item is something a user
genuinely missed in 1.0.

- ⬜ **`DefaultServlet` writes** — `PUT` and `DELETE` with `If-Match` /
      `If-Unmodified-Since` preconditions, atomic temp-file + rename writes,
      `WEB-INF`/`META-INF` path safety, configurable per-context read-only
      overrides, multipart upload handling
- ⬜ **LDAP realm backend** — `LdapRealm` against the existing `Realm`
      trait, with bind/search modes, connection pooling, role-search
      mapping, TLS to the directory, and a `MockLdapServer` for tests
- ⬜ **Manager / Host Manager HTML console** — full web UI on top of the
      existing text/JSON API: tree view of services/hosts/contexts, per-
      context start/stop/reload/undeploy actions, WAR upload form, session
      browser, server-info panel, log tail (read-only)
- ⬜ **Published performance baselines** — a documented `cargo bench`
      methodology, a reproducible harness using `wrk` and `k6` against
      both stock Tomcat and Tomcat-RS, and committed numbers in
      `docs/perf-1.1.md`
- ⬜ **Reference Spring Boot deployment** — a real Spring Boot fat WAR in
      the corpus + an end-to-end CI job booting it on Tomcat-RS and
      diffing against stock Tomcat
- ⬜ **`server.xml` parity** — fill out the support matrix to "Supported"
      for every element/attribute on the v1.x roadmap (Resources, Realm
      sub-elements, Cluster sub-elements, common Valves)
- ⬜ **CLI quality of life** — `tomcatrs status` (talks to a running
      Manager API), `tomcatrs deploy <war>` shortcut, JSON-formatted logs
      switch, `--print-config` for the resolved model
- ⬜ **Errors & diagnostics** — every error path carries a stable
      `TR####` code, linked to a troubleshooting page in `docs/errors/`

## Milestone 12 — WebSocket completeness  ·  v1.2.0  ·  ⬜ Planned (Q4 2026)

The Rust transport is done in 1.0; this milestone closes the JVM-side gap.

- ⬜ **Full Jakarta WebSocket Jakarta-API dispatch** — `@ServerEndpoint`,
      `@OnOpen` / `@OnMessage` / `@OnClose` / `@OnError`, the
      `Session` / `RemoteEndpoint` (Basic + Async) facades, encoders /
      decoders, message handlers (whole + partial)
- ⬜ **`ServerContainer`** programmatic endpoint registration through the
      bridge
- ⬜ **`permessage-deflate` full implementation** with `flate2` (the
      negotiation parser already ships)
- ⬜ **WebSocket extensions framework** — pluggable extension chain
      mirroring the Servlet filter chain shape
- ⬜ **WebSocket security extensions** — origin checks, subprotocol allow-
      list, per-endpoint authz tied to the Realm
- ⬜ **WebSocket-over-HTTP/2** (RFC 8441) — `:protocol` extended CONNECT,
      ALPN-aware negotiation
- ⬜ **Backpressure controls** — bounded outbound queues with explicit
      `try_send` / drop-oldest / disconnect policies
- ⬜ **WebSocket conformance** — Autobahn fuzzing suite green

## Milestone 13 — Operations & containers  ·  v1.3.0  ·  ⬜ Planned (Q1 2027)

Make Tomcat-RS a first-class container citizen.

- ⬜ **Kubernetes operator** — a `TomcatRSService` CRD that reconciles
      Deployments, headless Services for cluster membership, ConfigMaps
      for `server.xml`, Secrets for keystores
- ⬜ **Helm chart** — `helm install` story with sane defaults, the
      Manager API gated to in-cluster only, HPA hooks, Pod Disruption
      Budget defaults
- ⬜ **Official OCI images** — multi-arch (amd64/arm64), one with
      `--features jvm` and a JDK baked in, one bare for the JDK-less mode
      (lands in 1.6), reproducible-build pipeline
- ⬜ **StatefulSet-aware clustering** — automatic peer discovery via the
      Kubernetes API, pod-ordinal-aware primary/backup placement
- ⬜ **Service-mesh sidecar integration** — graceful drain on `SIGTERM`
      with `lameduck` window, readiness flipping to fail-on-drain, mesh
      mTLS termination patterns documented
- ⬜ **Secrets reload** — keystore + realm credential hot-reload without a
      restart (file-watcher + `inotify`-style on Linux, polling on other
      platforms)
- ⬜ **`/livez` + `/readyz`** Kubernetes-conventional aliases on the
      health adapter

## Milestone 14 — Tribes-style cluster transport  ·  v1.4.0  ·  ⬜ Planned (Q2 2027)

The cluster *logic* (DeltaManager / BackupManager) ships in 1.0; this lands
the wire-compatible transport.

- ⬜ **TCP + UDP multicast Tribes-style transport** — Group communication,
      sequence numbering, ack tracking
- ⬜ **Static membership** + **dynamic discovery** modes
- ⬜ **Group communication primitives** — reliable broadcast, ordered
      unicast, failure detector
- ⬜ **Wire compatibility with Apache Tomcat Tribes** at the protocol level
      so a Tomcat-RS node and a Tomcat 11 node can replicate sessions
      across the same cluster
- ⬜ **Failover testing matrix** — chaos-tested split brain, packet loss,
      slow networks, asymmetric partitions
- ⬜ **`docs/clustering.md`** — operator's guide

## Milestone 15 — Observability deepening  ·  v1.5.0  ·  ⬜ Planned (Q3 2027)

Metrics ship in 1.0; this brings traces and logs.

- ⬜ **OpenTelemetry traces** — every request gets a trace, with spans for
      accept / TLS handshake / protocol decode / mapper / valve pipeline /
      servlet invocation / response commit
- ⬜ **Distributed tracing across the JVM bridge** — span context
      propagated through JNI, Java-side `@WithSpan` honoured
- ⬜ **OTLP/HTTP logs export** — `tracing` events shipped to a collector
      alongside the existing metrics exporter
- ⬜ **Exemplars on metrics** — high-cardinality request samples linked
      from counters/histograms to traces
- ⬜ **Continuous profiling integration** — pprof endpoint, async-profiler
      attach point, off-CPU profiling for the JNI side
- ⬜ **Manager-side traces tab** — query and render traces stored locally
      for quick triage (full-trace retention stays in the collector)
- ⬜ **RED + USE dashboards** — published Grafana dashboards shipped with
      the Helm chart

## Milestone 16 — Native image + JDK-less mode  ·  v1.6.0  ·  ⬜ Planned (Q4 2027)

For apps that don't need the servlet bridge: a single, statically-linked
Rust binary that serves static content, the Manager API, and websockets
without a JDK at all.

- ⬜ **`tomcatrs-native` build profile** — Cargo features that compile out
      the JVM bridge, Jasper bridge, and JNI deps entirely
- ⬜ **musl static binary target** for Linux (single self-contained file)
- ⬜ **Documented "no Java" subset** — what works (HTTP/1.1/2/TLS,
      static content, WebSocket transport, Manager API, health, OTel) vs.
      what isn't available (Servlet execution, JSP, EL-from-JSP)
- ⬜ **Rust-only servlet equivalent** — a minimal `RustServlet` trait so
      you can register Rust handlers in the same routing/lifecycle that
      the JVM bridge uses (the JNI-less counterpart to `ServletInvoker`)
- ⬜ **Distroless / `scratch` OCI image** under 10MB
- ⬜ **`docs/no-jvm-mode.md`** — feature matrix and migration recipes

## Milestone 17 — Rust-native JSP compiler (alpha)  ·  v1.7.0  ·  ⬜ Planned (Q1 2028)

The piece intentionally deferred since 1.0. Treat as alpha until 1.8.

- ⬜ **JSP grammar parser** — directives, scriptlets, declarations,
      expressions, actions, JSTL/EL embedding
- ⬜ **Translation unit model** — `.jsp` → AST → generated Rust async fn
      (or a generated servlet on the JVM side as a fallback)
- ⬜ **EL integration** — reuse the existing `tomcatrs_jsp::el` evaluator
- ⬜ **Tag library (JSTL) support** — at minimum `c:`, `fmt:`, `fn:`
- ⬜ **Coexistence with Jasper** — per-context choice between the Rust
      compiler and the Jasper bridge while the Rust path matures
- ⬜ **Conformance** — pick a JSP test suite (HikariCP-style examples
      first), document gaps, file issues for each
- ⬜ **Precompile path** — `tomcatrs jspc` for ahead-of-time generation
      to Rust source dropped into the build

## Milestone 18 — HTTP/3 + QUIC  ·  v1.8.0  ·  ⬜ Planned (Q2 2028)

- ⬜ **QUIC transport via `quinn`** — UDP listener, connection
      multiplexing, congestion control
- ⬜ **HTTP/3 connector** — RFC 9114 framing, QPACK
- ⬜ **ALPN h3** + **Alt-Svc** advertisement from HTTP/1.1 / HTTP/2
- ⬜ **0-RTT support** with documented replay-safety guidance
- ⬜ **Connection migration** (path validation, NAT rebinding)
- ⬜ **HTTP/3 conformance harness** — h3spec runs as part of CI
- ⬜ **`<Connector protocol="HTTP/3">`** in `server.xml`

## Milestone 19 — Compliance & certification  ·  v1.9.0  ·  ⬜ Planned (Q3 2028)

Get Tomcat-RS officially measurable.

- ⬜ **Jakarta EE Web Profile TCK runs** — public results published,
      every failure tracked as an issue, a documented conformance matrix
      in `docs/conformance.md`
- ⬜ **Servlet TCK pass-rate dashboard** — automated, gated on CI
- ⬜ **CVE response process** — `SECURITY.md` with the disclosure
      contact, an embargoed-disclosure workflow, a `tomcatrs-security`
      advisory feed, integration with `cargo audit`
- ⬜ **OpenSSF Scorecard ≥8** — signed releases, SBOMs (CycloneDX +
      SPDX), pinned action versions, branch protection
- ⬜ **Supply-chain hardening** — `cargo-deny` config in CI, dependency
      review, reproducible builds for OCI images
- ⬜ **Documented security model** — threat model, trust boundaries, the
      Rust ↔ JVM FFI attack surface, formal recommendations for AJP /
      Manager / cluster wire deployment

## Milestone 20 — Pure-Rust servlet container  ·  v2.0.0  ·  ⬜ Planned (2029)

The first **major** release. Breaks the assumption that the JVM is required
for Servlet API execution.

- ⬜ **Native Rust Servlet 6.x runtime** — a Rust implementation of the
      Jakarta Servlet API (request/response, filter, listener, context),
      callable without a JVM
- ⬜ **Rust annotation processing** — compile-time `@WebServlet` /
      `@WebFilter` / `@WebListener` discovery from Rust crates that
      implement the trait equivalents
- ⬜ **Servlet 6.x conformance** — full TCK pass for the Rust runtime
- ⬜ **Existing JVM bridge stays** — it remains the default for Java
      WARs; the pure-Rust path is opt-in for apps that don't ship `.class`
      files
- ⬜ **Breaking changes** — `tomcatrs-*` crate API tidy-up: rename the
      bridge feature to `jvm-bridge`, separate compatibility-shim crates
      from the new core, drop deprecations accumulated through 1.x
- ⬜ **Two-runtime story documented** — when to pick Rust vs. JVM,
      perf/observability/footprint trade-offs, mixed deployments
- ⬜ **Migration guides** — 1.9 → 2.0 with `cargo fix` aiding where
      possible

---

## Long horizon — research, not commitments

These are ideas worth noting but not yet on any release. They may move into
a numbered milestone, be split, merged, or dropped:

- **Servlet 7 / future Jakarta EE web profiles** as the spec evolves.
- **WASM-based servlet runtime** — load `.wasm` modules implementing a
  Servlet-shaped interface (`wasi-http`-style), running in the same
  request-routing pipeline as Java WARs and Rust servlets.
- **Multi-tenant single-process hosting** — run many isolated webapps in
  one Tomcat-RS process with hard memory + CPU caps per tenant,
  classloader / runtime isolation via per-tenant async runtimes.
- **eBPF-assisted observability** — kernel-side instrumentation of
  accepted connections, TLS handshakes, and per-connection latency for
  zero-overhead production tracing.
- **Edge / serverless mode** — a stripped-down Tomcat-RS that boots into
  request-serving in <100ms on cold start, suitable for Lambda-style or
  CDN-worker deployments.
- **HTTP/3 priorities (RFC 9218)** and **Extensible Prioritization** for
  HTTP/2 and HTTP/3 streams.
- **Memory-safe replacement for Jakarta EL on the JVM side** — push the
  Rust EL evaluator across the bridge as the Jasper-side EL implementation
  for risky inputs.
- **WAF-style adaptive defences** — anomaly-detection valve, rate-limit
  policies from `tracing` analytics, automated 429 backoff.

---

## Release cadence and stability

- **Cadence target.** Minor releases roughly every **3 months** during
  the 1.x line; patch releases as needed for security or correctness fixes.
  The 2.0 cut waits until 1.9 has shipped and the conformance dashboard
  is green.
- **Semver discipline.** The 1.x line is backward-compatible at the
  source level for the `tomcatrs-*` library crates. Breaking changes —
  type renames, signature changes, removed re-exports — are reserved for
  the next major (2.0). Deprecations land with `#[deprecated]` and a
  documented replacement, kept for at least two minor releases before
  removal.
- **Crate stability tiers.**
  - **Tier 1 (semver-stable):** `tomcatrs-core`, `tomcatrs-coyote`,
    `tomcatrs-catalina`, `tomcatrs-config`, `tomcatrs-session`,
    `tomcatrs-security`, `tomcatrs-observability`, `tomcatrs-cli`.
  - **Tier 2 (mostly stable, may shift in minor releases):**
    `tomcatrs-webapp`, `tomcatrs-jsp`, `tomcatrs-websocket`.
  - **Tier 3 (unstable until 2.0):** `tomcatrs-servlet-bridge` —
    public API may change as the JVM-less Servlet runtime lands in 2.0.
- **MSRV policy.** Tomcat-RS supports the **latest stable Rust** and the
  two prior minor versions. MSRV bumps land in minor releases and are
  called out in the CHANGELOG.
- **JDK matrix.** With the `jvm` feature: **Java 17 LTS** and **Java
  21 LTS** are the support targets through 1.x. Java 25 LTS becomes the
  default for 2.0.

## Compatibility commitments

- **Wire formats.** HTTP/1.1, HTTP/2, AJP/1.3, RFC 6455 WebSocket: stable
  through 1.x. HTTP/3 lands in 1.8 and is stable thereafter.
- **`server.xml`.** New attributes may be added; existing ones do not
  change meaning. Unknown attributes stay warned-and-skipped, never
  rejected.
- **`web.xml`.** Jakarta web-app `6.0` is the baseline; new schema versions
  are added as they become Jakarta standards.
- **CLI.** `tomcatrs run`, `tomcatrs check-config`, `tomcatrs version`
  stay backward-compatible through 1.x; new subcommands are additive.
- **Manager API.** `/manager/text/*` and `/manager/list` JSON shape are
  stable through 1.x; new endpoints are additive.
- **Bridge ABI.** The JNI surface to Java is stable within a 1.x release;
  breaking changes can land at a minor boundary if the bridge JAR is
  rebuilt and shipped together.

---

## Cross-cutting, always-on work

These are not milestones; they run continuously across every release.

- **Testing.** Every crate keeps unit tests; integration and differential
  tests grow with each release. CI must stay green and warning-free.
  New milestones do not ship until their conformance suite (where one
  exists) is green.
- **Documentation.** `docs/` (architecture, compatibility, spec-target,
  `server.xml` support matrix, migration, release notes) tracks reality,
  not intent. Every milestone produces a corresponding `docs/release-
  notes-<version>.md` entry.
- **`server.xml` fidelity.** Unknown elements/attributes are warned-and-
  skipped; the support matrix in `docs/serverxml-support.md` is expanded
  as attributes become real.
- **FFI discipline.** Keep the Rust ↔ JVM boundary thin: request handles,
  lazy materialization, buffered streaming, no per-header / per-byte JNI
  calls. New milestones do not add per-byte JNI without explicit perf
  justification.
- **Fuzzing.** The `cargo-fuzz` harness runs continuously; every new
  parser / decoder / state machine gets a fuzz target before it merges.
- **Security review.** Each minor release ships with a "what changed in
  the trust boundary" note and a re-run of the security conformance
  suite.
- **Dependency hygiene.** `cargo update` runs at least monthly;
  `cargo audit` blocks CI on advisories.
- **Performance regression guards.** Criterion baselines from 1.0 (and
  refreshed at each major milestone) are checked against in CI;
  regressions over a documented threshold block the release.

## How to contribute to the roadmap

Pick a 🟡 item or anything in a future-milestone (1.1+) section, open an
issue to claim it, and send a PR. The highest-leverage areas right now
are:

1. **`DefaultServlet` writes** (Milestone 11) — small, well-scoped,
   user-visible.
2. **Manager HTML UI** (Milestone 11) — pure additive work on top of the
   shipped Manager API.
3. **Full Jakarta WebSocket integration** (Milestone 12) — finishes the
   one major 1.0 carve-out.
4. **Rust-native JSP compiler** (Milestone 17) — the longest-running
   ambitious item; an alpha grammar parser is a great first PR.
5. **Kubernetes operator and Helm chart** (Milestone 13) — first-class
   container deployment story.

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the practical workflow.
Smaller help — fuzz corpora, docs polish, new test fixtures, bug reports
with reproducer WARs — is just as valuable as feature work.
