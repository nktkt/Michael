# Specification Targets

This document lists the specifications Tomcat-RS targets and their
implementation status as of **v1.0.0**.

## Jakarta / application specifications

These are the container-level specifications. Their *application semantics*
run on the embedded JVM via the servlet bridge (`--features jvm`); Tomcat-RS
provides the surrounding runtime.

| Specification | Target level | v1.0.0 status |
| --- | --- | --- |
| Jakarta Servlet | The level shipped with Tomcat 11.0.x (`jakarta.*` namespace) | **Implemented** — end-to-end servlet/filter/listener invocation across the bridge; `web.xml` and `@WebServlet`/`@WebFilter`/`@WebListener` annotation wiring; `AsyncContext` |
| Jakarta Pages (JSP) | The level shipped with Tomcat 11.0.x | **Implemented via Jasper** — JSP runtime routed through `org.apache.jasper.servlet.JspServlet`; precompile path uses `JspC` with Tomcat-compatible name mangling and `web.xml` fragment generation |
| Jakarta Expression Language (EL) | The level shipped with Tomcat 11.0.x | **Implemented** — self-contained Rust evaluator for `${...}`/`#{...}` and template interpolation; JVM-side EL remains available inside the JSP runtime |
| Jakarta WebSocket | The level shipped with Tomcat 11.0.x | **Partial** — full Rust transport (handshake, framing, reassembly, control frames, close, `permessage-deflate`); JVM-side `@ServerEndpoint` lifecycle dispatch through the bridge is shallow in 1.0 and Rust-native endpoint adapters are the supported integration |

## Wire protocols

These are implemented entirely on the Rust side in `tomcatrs-coyote`.

| Protocol | v1.0.0 status |
| --- | --- |
| HTTP/1.1 | **Implemented** — request line, headers, chunked transfer encode/decode, keep-alive, expect-continue, request limits, fuzzed |
| HTTP/2 | **Implemented** — RFC 9113 framing, HPACK (RFC 7541) with static + dynamic table and Huffman, multiplexed streams, flow control, `GOAWAY`/`RST_STREAM`, fuzzed |
| AJP/1.3 | **Implemented (server side)** — `Forward Request` decode, `Send Headers` / `Send Body Chunk` / `End Response` encode; trusted-network-only deployment posture documented (Ghostcat-aware) |

## Transport security

| Feature | v1.0.0 status |
| --- | --- |
| TLS termination | **Implemented** — `rustls` (using the `ring` provider) with ALPN-negotiated `h2` and `http/1.1`; certificate/key loaded from `server.xml`; behind the default-on `tls` feature |

## Security and authentication

| Feature | v1.0.0 status |
| --- | --- |
| `BASIC` authentication | **Implemented** (RFC 7617) |
| `DIGEST` authentication | **Implemented** (RFC 2617 / 7616) — keyed nonces, replay defence |
| `FORM` authentication | **Implemented** — `j_security_check`, original-request replay |
| `<security-constraint>` evaluation | **Implemented** — Servlet-spec aggregation, including `<auth-constraint>` and `<user-data-constraint>` |
| Realm backends — in-memory / `tomcat-users.xml` / combined / lock-out / JDBC | **Implemented** |
| Realm backend — LDAP | **Planned** (post-1.0) |
| Response-header hardening valve (HSTS, CSP, frame-options, …) | **Implemented** |
| HTTP-method allow-list valve | **Implemented** |
| CSRF token helpers | **Implemented** |

## Operations and observability

| Feature | v1.0.0 status |
| --- | --- |
| Auto-deploy + hot redeploy | **Implemented** — `HostDeployer`, `DeploymentWatcher`, `ContainerBackgroundProcessor` |
| Manager API (text / JSON) | **Implemented** — `/manager/text/list`, `serverinfo`, `sessions`, `reload`, `stop`, `start`, `deploy`, `undeploy` |
| Manager / Host Manager HTML UI | **Planned** (post-1.0) |
| Access logs (Common / Combined) | **Implemented** |
| Prometheus metrics | **Implemented** |
| Health endpoint (`/health`, `/health/live`, `/health/ready`) | **Implemented** — IETF "health-check"-shaped |
| JMX bridge (Rust metrics → JVM MBeans) | **Implemented** |
| OpenTelemetry export (OTLP/HTTP) | **Implemented** |
| `tracing` integration | **Implemented** |

## Sessions

| Backend | v1.0.0 status |
| --- | --- |
| In-memory | **Implemented** |
| File | **Implemented** |
| Redis | **Implemented** (`--features redis`) |
| JDBC | **Implemented via the `JdbcExecutor` trait** — bridge-supplied executor at assembly time |
| Clustered — `DeltaManager` (all-to-all) | **Implemented** |
| Clustered — `BackupManager` (primary-backup) | **Implemented** |
| Tribes-compatible TCP/UDP cluster transport | **Planned** (post-1.0); the in-memory channel transport is the testable default |

## Summary

In v1.0.0, **every wire protocol** Tomcat ships (HTTP/1.1, HTTP/2, AJP, plus
TLS termination) is implemented and exercised by tests and fuzzing.
Application-spec compatibility — Servlet, JSP, EL, WebSocket — is delivered
through the JVM bridge with the partial-area exceptions noted above and in
the README. The roadmap's milestones 2–10 have all landed; see
[`../ROADMAP.md`](../ROADMAP.md) for the per-milestone breakdown and the
"Future" section for what comes after 1.0.
