# Tests

This directory holds the Tomcat-RS test suite. It is organized around one
central idea: **differential compatibility testing against stock Apache
Tomcat**.

## Methodology

For compatibility and migration tests, the approach is:

1. Take a web application (a WAR or an expanded webapp directory).
2. Deploy it twice with equivalent configuration: once on a stock **Apache
   Tomcat 11.0.x** instance, once on **Tomcat-RS**.
3. Drive the **same sequence of requests** at both.
4. **Diff the observable outputs**:
   - response status codes,
   - response headers,
   - response bodies,
   - `Set-Cookie` headers and overall cookie behavior,
   - access log lines.
5. A divergence is either a bug to fix in Tomcat-RS or a deliberate,
   documented difference. Either way it is recorded.

Protocol and security tests are more direct: they send crafted byte sequences
at the connector and assert on the exact response, without needing a stock
Tomcat for comparison.

Note that until the JVM bridge is wired end to end (see the README roadmap),
servlet/JSP-dependent compatibility tests focus on deployment, routing, and
static behavior; the dynamic-execution assertions are staged behind the same
milestones.

## Directory layout

```
tests/
├── compatibility/        differential tests vs stock Tomcat
│   ├── servlet/          servlet invocation, mapping, lifecycle
│   ├── filter/           filter chains and ordering
│   ├── listener/         context/session/request listeners
│   ├── session/          HttpSession semantics across stores
│   ├── cookie/           cookie parsing and Set-Cookie behavior
│   ├── static/           static resource serving and welcome files
│   ├── jsp/              JSP compilation and rendering
│   └── websocket/        WebSocket endpoint behavior
├── protocol/             wire-protocol conformance
│   ├── http1/            HTTP/1.1 request/response, chunking, keep-alive
│   ├── http2/            HTTP/2 framing and streams (planned)
│   ├── ajp/              AJP protocol (planned)
│   └── tls/              TLS termination (planned)
├── security/             hardening and abuse-resistance
│   ├── path_traversal/   ../ and traversal rejection
│   ├── encoded_slash/    encoded path-separator handling
│   ├── max_post_size/    request body size limits
│   ├── multipart_limits/ multipart upload limits
│   └── header_limits/    header count and size limits
└── migration/            real-world migration scenarios
    ├── sample_wars/      assorted small sample WARs
    ├── spring_boot_war/  a Spring Boot application packaged as a WAR
    └── legacy_web_xml/   apps exercising older web.xml descriptors

fixtures/
└── wars/                 exploded WAR corpus shared across crates
    ├── hello/            minimal servlet declaration + URL mapping
    ├── static/           pure-static webapp (no servlets), html/css/js assets
    ├── filtered/         servlet behind a filter mapped to /*
    ├── listener/         single ServletContextListener declaration
    ├── welcome/          web.xml with <welcome-file-list> + index.html
    └── secure/           <security-constraint> on /admin/* (basic auth)
```

Each leaf directory under the test suite tree currently contains a `.gitkeep`
placeholder so the tree is committed before the tests themselves are written.

## WAR fixture corpus

`tests/fixtures/wars/` is a workspace-shared corpus of **exploded** webapp
directories. Each fixture ships a real, well-formed Jakarta EE 6.0 `web.xml`
under `WEB-INF/`, plus, where useful, `WEB-INF/src/*.java` documentation-only
source files describing what the declared servlets, filters, or listeners are
*meant* to do (the test harness does not compile these sources).

The fixtures cover one slice of the deployment-descriptor surface each:

| Fixture     | What it covers                                                  |
|-------------|-----------------------------------------------------------------|
| `hello/`    | one `<servlet>` (`HelloServlet`) mapped to `/hello`             |
| `static/`   | empty servlet list, static assets (`index.html`, CSS, JS)       |
| `filtered/` | a `<filter>` mapped to `/*` in front of a servlet               |
| `listener/` | a single `<listener>` declaring a `ServletContextListener`      |
| `welcome/`  | a three-entry `<welcome-file-list>` plus an `index.html`        |
| `secure/`   | a `<security-constraint>` on `/admin/*` plus `<login-config>`   |

The corpus is exercised by
`crates/tomcatrs-webapp/tests/corpus.rs`, which opens every fixture via
`Webapp::open`, asserts the parsed `WebDescriptor` matches expectations, and
runs `DeploymentScanner::scan` over the corpus root to confirm each fixture is
discovered as an exploded deployment.

