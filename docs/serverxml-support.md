# `server.xml` Support Matrix

This document describes which `server.xml` elements and attributes the
`tomcatrs-config` parser understands today. The goal is that an existing
Tomcat `server.xml` can be dropped into `conf/` and used directly.

## Status legend

| Status | Meaning |
| --- | --- |
| **Supported** | Parsed and honored as in Apache Tomcat. |
| **Partial** | Parsed; some attributes/behaviors are honored, others are not yet. |
| **Ignored (warn)** | Parsed but not acted on; the parser logs a warning so you know it had no effect. |
| **Planned** | Recognized in the schema but not yet implemented; treated as ignored-with-warning until then. |

## Elements

| Element | Status | Notes |
| --- | --- | --- |
| `<Server>` | Supported | Outermost container; `port` and `shutdown` honored. |
| `<Service>` | Supported | Groups Connectors with one Engine; `name` honored. |
| `<Connector>` | Supported | HTTP/1.1, HTTP/2, AJP/1.3, and TLS connectors are all honored. |
| `<Engine>` | Supported | Top-level container; `name` and `defaultHost` honored. |
| `<Host>` | Supported | Virtual host; `name`, `appBase`, `autoDeploy` honored. Hot redeploy via the `DeploymentWatcher`. |
| `<Context>` | Supported | `path`, `docBase`, and `reloadable` honored; reload wired via the Manager API (`/manager/text/reload`). |
| `<Valve>` | Partial | Access-log, `RemoteAddrValve`, `SecurityHeadersValve`, and `HttpMethodFilterValve` honored; other Tomcat valve classes still ignored-with-warning. |
| `<Listener>` | Partial | Parsed; lifecycle listeners declared in `web.xml` and via annotations are dispatched on the JVM side. `server.xml`-level listener classes are still ignored-with-warning. |
| `<Realm>` | Partial | In-memory, file (`tomcat-users.xml`), combined, lock-out, and JDBC realms are wired; LDAP is post-1.0. |
| `<Resources>` | Planned | Recognized; custom resource roots are still post-1.0. |
| `<GlobalNamingResources>` | Ignored (warn) | Parsed; JNDI remains out of scope. |
| `<Cluster>` | Supported | `DeltaManager` (all-to-all) and `BackupManager` (primary-backup) session replication are honored over a pluggable `ClusterTransport`; a Tribes-compatible TCP/UDP transport is post-1.0. |

## Common attributes

| Attribute | On element | Status | Notes |
| --- | --- | --- | --- |
| `port` | `Server`, `Connector` | Supported | Shutdown port and connector listen port. |
| `shutdown` | `Server` | Supported | Shutdown command string. |
| `name` | `Service`, `Engine`, `Host` | Supported | Component identity. |
| `protocol` | `Connector` | Supported | `HTTP/1.1`, `h2` (HTTP/2), and `AJP/1.3` all honored. |
| `address` | `Connector` | Supported | Bind address for the connector. |
| `SSLEnabled` | `Connector` | Supported | TLS termination via `rustls`; ALPN-negotiated `h2` / `http/1.1`. |
| `defaultHost` | `Engine` | Supported | Host used when the `Host` header matches nothing. |
| `appBase` | `Host` | Supported | Directory scanned for deployments. |
| `autoDeploy` | `Host` | Supported | Enables the deployment watcher for the Host. |
| `docBase` | `Context` | Supported | Application directory or WAR for the Context. |
| `reloadable` | `Context` | Supported | Reload driven by the `DeploymentWatcher` and by `/manager/text/reload`. |

## Behavior on unknown input

Unknown elements and attributes are **not** fatal. The parser keeps going and
emits a warning per unrecognized item, so an existing `server.xml` with
features Tomcat-RS does not yet implement will still load — you simply get a
clear log of what was skipped. Use `tomcatrs check-config conf/server.xml` to
see the full report without starting the server.
