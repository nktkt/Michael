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
| `<Connector>` | Partial | HTTP/1.1 connectors are honored; HTTP/2, AJP, and TLS connectors are parsed but not yet functional. |
| `<Engine>` | Supported | Top-level container; `name` and `defaultHost` honored. |
| `<Host>` | Supported | Virtual host; `name`, `appBase`, `autoDeploy` honored. |
| `<Context>` | Partial | `path` and `docBase` honored; `reloadable` parsed but reload not yet wired. |
| `<Valve>` | Partial | Access-log valve honored; other valve classes ignored-with-warning. |
| `<Listener>` | Ignored (warn) | Parsed; lifecycle listeners not yet dispatched. |
| `<Realm>` | Planned | Recognized; authentication realms not yet implemented. |
| `<Resources>` | Planned | Recognized; custom resource roots not yet implemented. |
| `<GlobalNamingResources>` | Ignored (warn) | Parsed; JNDI is out of scope for v0.1.0. |
| `<Cluster>` | Planned | Recognized; clustering transport is scaffolded only. |

## Common attributes

| Attribute | On element | Status | Notes |
| --- | --- | --- | --- |
| `port` | `Server`, `Connector` | Supported | Shutdown port and connector listen port. |
| `shutdown` | `Server` | Supported | Shutdown command string. |
| `name` | `Service`, `Engine`, `Host` | Supported | Component identity. |
| `protocol` | `Connector` | Partial | `HTTP/1.1` honored; HTTP/2 and AJP protocols parsed but not functional. |
| `address` | `Connector` | Supported | Bind address for the connector. |
| `SSLEnabled` | `Connector` | Planned | Parsed; TLS termination is scaffolded only. |
| `defaultHost` | `Engine` | Supported | Host used when the `Host` header matches nothing. |
| `appBase` | `Host` | Supported | Directory scanned for deployments. |
| `autoDeploy` | `Host` | Supported | Enables the deployment watcher for the Host. |
| `docBase` | `Context` | Supported | Application directory or WAR for the Context. |
| `reloadable` | `Context` | Partial | Parsed; hot-reload on class change is not yet wired. |

## Behavior on unknown input

Unknown elements and attributes are **not** fatal. The parser keeps going and
emits a warning per unrecognized item, so an existing `server.xml` with
features Tomcat-RS does not yet implement will still load — you simply get a
clear log of what was skipped. Use `tomcatrs check-config conf/server.xml` to
see the full report without starting the server.
