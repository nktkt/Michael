# Changelog

All notable changes to the Tomcat-RS Compatibility Runtime are documented in
this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/nktkt/Michael/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/nktkt/Michael/releases/tag/v0.1.0
