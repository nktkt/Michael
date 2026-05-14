# Tomcat-RS Compatibility Runtime

*An incremental Rust rewrite of Apache Tomcat that keeps your existing Java WARs running.*

> The GitHub repository for this project is named **Michael**
> (`https://github.com/nktkt/Michael`). "Tomcat-RS Compatibility Runtime"
> is the project name; "Michael" is just the repo name.

**Status: v0.1.0 — early MVP / scaffold. Not production ready.**

---

## Why

Apache Tomcat is often described as "a web server," but that undersells it.
Tomcat is a **Jakarta container**: it implements the Jakarta Servlet,
Jakarta Pages (JSP), Jakarta Expression Language (EL), and Jakarta WebSocket
specifications. Real applications depend on Servlet API semantics, on Jasper
compiling JSPs, on `web.xml` wiring, on Java classloader isolation, and on
listener/filter lifecycles. A naive "pure Rust rewrite" would throw all of
that away and break every existing WAR on day one.

So this project takes a deliberately **hybrid** approach:

- Reimplement the **dangerous, I/O-heavy, parser-heavy, control-plane** layers
  in Rust — the parts where Rust's memory safety, performance, and modern
  async networking pay off the most.
- **Keep Servlet / JSP / EL execution on an embedded JVM**, reached through a
  JNI bridge (the `tomcatrs-servlet-bridge` crate). The JVM remains the source
  of truth for Servlet API compatibility, Jasper, Jakarta EL, and Java
  classloader semantics.

The result: the attack surface and the hot networking path move to Rust, while
binary compatibility with the Java ecosystem is preserved.

### Rust side vs JVM side

| Concern | Rust side | JVM side |
| --- | --- | --- |
| TLS / TCP accept loop | ✅ | |
| HTTP/1.1, HTTP/2, AJP connectors (Coyote) | ✅ | |
| Request line / header / cookie parsing & normalization | ✅ | |
| URI hardening (path traversal, encoded slash, etc.) | ✅ | |
| Host / Context / Wrapper mapper (routing) | ✅ | |
| `server.xml` parsing & validation | ✅ | |
| Lifecycle orchestration (Server→Service→…→Wrapper) | ✅ | |
| Deployment watching / auto-deploy | ✅ | |
| Static file serving | ✅ | |
| Access logs, metrics, observability | ✅ | |
| Session storage backends (memory / file / cluster) | ✅ | |
| Clustering / replication transport | ✅ | |
| Servlet API execution | | ✅ |
| Filters, Listeners, `web.xml` wiring semantics | invoked from Rust | ✅ |
| Jasper / JSP compilation & runtime | | ✅ |
| Jakarta Expression Language | | ✅ |
| Java classloader isolation per webapp | | ✅ |
| WebSocket endpoint invocation | handshake + framing in Rust | endpoint logic ✅ |

---

## Architecture

```mermaid
flowchart LR
    Client([Client])

    subgraph Rust["Rust runtime (tomcatrs)"]
        direction LR
        Accept[TLS / TCP accept]
        Coyote["Coyote connectors<br/>HTTP/1.1 · HTTP/2 · AJP"]
        Norm[Request normalizer]
        Mapper["Host / Context / Wrapper<br/>mapper"]
        Pipeline[Valve / Filter pipeline]
        Static[Static resource handler]
        Bridge[JVM Servlet Bridge]
        Writer[Response writer]
    end

    subgraph JVM["Embedded JVM (via JNI)"]
        direction LR
        Container["WAR · Servlet · JSP · WebSocket"]
    end

    subgraph Control["Control plane"]
        direction LR
        Cfg["server.xml parser"]
        Life[Lifecycle orchestrator]
        Deploy[Deployment watcher]
    end

    Client --> Accept --> Coyote --> Norm --> Mapper --> Pipeline
    Pipeline --> Static --> Writer
    Pipeline --> Bridge --> Container --> Bridge --> Writer
    Writer --> Client

    Cfg --> Life
    Life --> Coyote
    Life --> Mapper
    Deploy --> Mapper
    Life -.-> JVM
```

The data-plane path (top) carries request/response traffic. The control-plane
path (bottom) parses configuration, drives the lifecycle state machine, and
watches `appBase` for deployments.

---

## Workspace layout

This is a Cargo workspace. Crates live under `crates/`:

```
Michael/
├── Cargo.toml                       # workspace manifest
├── crates/
│   ├── tomcatrs-core                # shared types, errors, lifecycle traits, component model
│   ├── tomcatrs-config              # server.xml / web.xml / catalina.properties parsing
│   ├── tomcatrs-coyote              # connectors: HTTP/1.1, HTTP/2, AJP, request/response codec
│   ├── tomcatrs-catalina            # container engine: Engine/Host/Context/Wrapper, valves, mapper
│   ├── tomcatrs-webapp              # webapp model, deployment, static resources, web.xml binding
│   ├── tomcatrs-servlet-bridge      # JNI bridge to the embedded JVM (Servlet/JSP/EL execution)
│   ├── tomcatrs-jsp                 # JSP/Jasper bridge glue and compilation orchestration
│   ├── tomcatrs-session             # session manager and storage backends (memory/file/cluster)
│   ├── tomcatrs-security            # request limits, URI hardening, path-traversal defenses
│   ├── tomcatrs-websocket           # WebSocket handshake and frame codec
│   ├── tomcatrs-observability       # access logs, metrics, tracing integration
│   └── tomcatrs-cli                 # `tomcatrs` binary: run / check-config / version
├── conf/                            # sample server.xml, web.xml, catalina.properties
├── webapps/                         # default webapp root (ROOT/)
├── docs/                            # architecture and compatibility documentation
└── tests/                           # compatibility / protocol / security / migration tests
```

| Crate | Responsibility |
| --- | --- |
| `tomcatrs-core` | Shared types, error model, the lifecycle trait, and the component model contracts. |
| `tomcatrs-config` | Parses and validates `server.xml`, `web.xml`, and `catalina.properties`. |
| `tomcatrs-coyote` | The Coyote layer: connector implementations and the HTTP/1.1, HTTP/2, and AJP codecs. |
| `tomcatrs-catalina` | The Catalina container: Engine/Host/Context/Wrapper, valve pipeline, and the request mapper. |
| `tomcatrs-webapp` | Webapp representation, deployment/auto-deploy, static resource serving, `web.xml` binding. |
| `tomcatrs-servlet-bridge` | The JNI bridge that boots and talks to the embedded JVM for Servlet/JSP/EL execution. |
| `tomcatrs-jsp` | JSP/Jasper orchestration glue on the Rust side. |
| `tomcatrs-session` | Session manager plus pluggable storage backends (in-memory, file, cluster). |
| `tomcatrs-security` | Security limits and request hardening (path traversal, encoded slash, body/header limits). |
| `tomcatrs-websocket` | WebSocket upgrade handshake and the frame codec. |
| `tomcatrs-observability` | Access logging, metrics export, and tracing hooks. |
| `tomcatrs-cli` | The `tomcatrs` command-line binary. |

---

## Build & run

Build everything:

```sh
cargo build --release
```

Run the test suite:

```sh
cargo test
```

Run the server:

```sh
# Run with defaults (reads conf/server.xml)
cargo run -p tomcatrs-cli -- run

# Override the connector port and the webapp base directory
cargo run -p tomcatrs-cli -- run --port 8080 --app-base ./webapps

# Validate a server.xml without starting the server
cargo run -p tomcatrs-cli -- check-config conf/server.xml

# Print version information
cargo run -p tomcatrs-cli -- version
```

### The `jvm` feature

`tomcatrs-servlet-bridge` exposes an optional `jvm` Cargo feature that enables
the JNI bridge to an embedded JVM. It is **off by default** so the project
builds and the Rust-side tests run without a JDK installed. Enabling it
requires a JDK (Java 17+) on the build and run hosts:

```sh
cargo build --release -p tomcatrs-servlet-bridge --features jvm
```

Without the `jvm` feature, servlet/JSP invocation paths return a
"bridge not available" result; the Rust connectors, mapper, static handler,
sessions, security limits, and WebSocket codec all still function.

---

## What works in v0.1.0

These subsystems are implemented and exercised by tests:

- **HTTP/1.1 connector** — request line, headers, chunked bodies, keep-alive.
- **`server.xml` parsing** — the component tree is parsed and validated.
- **Lifecycle model** — the `New → … → Destroyed` state machine and ordered
  startup/shutdown of the component tree.
- **Mapper / routing** — Host / Context / Wrapper selection from a request URI.
- **URI hardening** — normalization and rejection of path-traversal and
  encoded-separator attacks.
- **Sessions** — session manager with in-memory and file-backed stores.
- **Access logs** — configurable access log output.
- **Metrics** — basic counters and timings via the observability crate.
- **WebSocket** — upgrade handshake and frame codec (encode/decode).
- **Security limits** — header count/size, request body size, and related caps.

## What's stubbed / scaffolded

These exist as types and entry points but are **not** functional yet:

- **HTTP/2** connector — scaffolded.
- **AJP** connector — scaffolded.
- **TLS** termination — scaffolded.
- **JVM servlet invocation** — bridge scaffold only; needs the `jvm` feature
  and remaining JNI plumbing.
- **JSP runtime compilation** (Jasper bridge) — scaffolded.
- **Clustering / session replication** — scaffolded.
- **Manager UI / management API** — scaffolded.

---

## Roadmap

A ten-milestone path from scaffold to a compatibility-tested runtime:

1. **Skeleton + config + lifecycle** — workspace, component model, `server.xml`
   parsing, lifecycle state machine.
2. **HTTP/1.1 + static** — working HTTP/1.1 connector and static resource
   serving.
3. **JVM boot + classloader + single servlet** — embed the JVM, set up
   per-webapp classloaders, invoke one servlet end to end.
4. **WAR deploy + web.xml + mapping** — deploy real WARs, parse `web.xml`,
   wire it into the mapper.
5. **Filters + sessions + cookies** — filter chains, session integration,
   cookie handling parity.
6. **Jasper bridge** — JSP compilation and runtime via the embedded Jasper.
7. **HTTP/2 + WebSocket** — HTTP/2 connector and full WebSocket endpoint
   dispatch.
8. **Security hardening + fuzzing** — fuzz the parsers and connectors, harden
   limits.
9. **Manager / API + observability** — management API/UI and a complete
   observability story.
10. **Production compatibility test suite** — the comparison-testing harness
    against stock Tomcat across a broad WAR corpus.

---

## Relationship to Apache Tomcat

This is an **independent, experimental project**. It is **not affiliated with,
sponsored by, or endorsed by the Apache Software Foundation**. "Apache Tomcat"
and "Apache" are trademarks of the Apache Software Foundation.

Tomcat-RS targets compatibility with the **Apache Tomcat 11.0.x** (`main`)
component model and Jakarta Servlet semantics, running on **Java 17+**. It does
not redistribute Apache Tomcat source code; it is a clean-room-style
reimplementation of the runtime layers described above, designed to host the
same Java web applications.

---

## License

Licensed under the **Apache License, Version 2.0**. See [`LICENSE`](./LICENSE)
and [`NOTICE`](./NOTICE).
