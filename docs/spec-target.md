# Specification Targets

This document lists the specifications Tomcat-RS targets and their
implementation status as of v0.1.0.

## Jakarta / application specifications

These are the container-level specifications. Their *application semantics*
run on the embedded JVM via the servlet bridge; Tomcat-RS provides the
surrounding runtime.

| Specification | Target level | v0.1.0 status |
| --- | --- | --- |
| Jakarta Servlet | The level shipped with Tomcat 11.0.x (`jakarta.*` namespace) | Bridge **scaffolded** — invocation path not yet wired end to end |
| Jakarta Pages (JSP) | The level shipped with Tomcat 11.0.x | **Scaffolded** — Jasper bridge glue only |
| Jakarta Expression Language (EL) | The level shipped with Tomcat 11.0.x | **Scaffolded** — evaluated on the JVM once the bridge is live |
| Jakarta WebSocket | The level shipped with Tomcat 11.0.x | **Partial** — handshake and frame codec implemented in Rust; endpoint dispatch scaffolded |

## Wire protocols

These are implemented (or planned) entirely on the Rust side in
`tomcatrs-coyote`.

| Protocol | v0.1.0 status |
| --- | --- |
| HTTP/1.1 | **Implemented** — request line, headers, chunked transfer, keep-alive |
| HTTP/2 | **Planned** — connector scaffolded, not functional |
| AJP | **Planned** — connector scaffolded, not functional |

## Transport security

| Feature | v0.1.0 status |
| --- | --- |
| TLS termination | **Planned** — scaffolded, not functional |

## Summary

In v0.1.0, the only fully working wire protocol is **HTTP/1.1**, and the only
fully working application-spec piece is the **WebSocket handshake and frame
codec**. Everything else listed here is scaffolded or planned. The roadmap in
the README sequences the remaining work; milestone 3 brings up the JVM and the
first end-to-end servlet, and milestone 7 brings HTTP/2 and full WebSocket
dispatch.
