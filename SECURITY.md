# Security Policy

This document covers how the **Tomcat-RS Compatibility Runtime** project
handles security: which versions get fixes, how to report a vulnerability,
how disclosure is coordinated, what the threat model looks like, what we
recommend for production hardening, and which known limitations should
inform a deployment decision.

The project is independent and experimental, but it is also intentionally
positioned to host real Java web applications. Security is therefore not an
afterthought; it is one of the load-bearing reasons the project exists.

---

## Supported versions

Security fixes are produced for the line below. Other releases — including
older minor lines and any pre-1.0 tags — do **not** receive backported
fixes. Upgrade to the latest supported line to stay on a patched runtime.

| Version line  | Status                | Security fixes |
| ------------- | --------------------- | -------------- |
| `1.0.x`       | Current stable        | Yes            |
| `< 1.0`       | Pre-stable / archival | No             |

When a new minor line becomes current (for example `1.1.x` after
`1.0.x`), security fixes for the previous line continue for a transitional
window announced in that minor's release notes; outside that window only
the current line is covered.

---

## Reporting a vulnerability

**Please do not open a public GitHub issue for a suspected vulnerability.**
Use the private channels below instead, so a fix can ship before details
become public.

### Preferred channel — GitHub Security Advisories (private)

Open a private advisory through GitHub's "Security" tab on the
[Michael](https://github.com/nktkt/Michael) repository:

1. Navigate to **Security → Advisories → Report a vulnerability**.
2. Fill in the form. GitHub keeps the conversation private to the
   maintainers and any collaborators you invite.
3. We will acknowledge the report within **5 business days** and start
   triage.

This is the preferred channel because it gives us a private space to
discuss details, coordinate a fix, and request a CVE.

### Fallback — public maintainer contact

If you cannot use GitHub Security Advisories (for example, no GitHub
account), email the maintainer line published on the
[Michael repository](https://github.com/nktkt/Michael) profile page. If
you cannot reach the maintainers privately at all, open a deliberately
**vague** GitHub issue ("possible security issue, please reach out for
private details") and we will follow up off-issue. Do **not** include
exploit details, proof-of-concept code, or attack traces in any public
issue.

For **non-sensitive** hardening questions, request-for-comment-type
discussions, and "is this design intentional?" questions, the public
issue tracker is the right place — only suspected vulnerabilities go
through the private channel.

### What to include in a report

A useful report contains, at minimum:

- The affected version(s) (e.g. `1.0.0`, or `main` at commit `<sha>`).
- The build feature flags relevant to the issue (notably whether
  `--features jvm` was on).
- A description of the vulnerability and its impact: confidentiality,
  integrity, availability, scope (single webapp vs runtime-wide).
- A reproducer: configuration excerpt (`server.xml` snippet), a
  minimal request or input, the observed behaviour, and the expected
  behaviour.
- A suggested CVSS vector if you have one. We are happy to compute one
  if you do not.
- Your preferred attribution string and contact channel (or "anonymous"
  if you prefer no credit).

Encrypted payloads attached to the GitHub advisory are fine; we will
work out a key out-of-band if the report is sensitive enough to warrant
one.

---

## Disclosure policy

We follow **coordinated disclosure**. The shape we aim for:

1. **Acknowledgement** within 5 business days of the report.
2. **Triage and impact assessment** within 14 days. We aim to confirm
   reproducibility, narrow down the affected components, and assign a
   severity in this window.
3. **Fix development** under embargo. Severity drives urgency:
   - **Critical** (RCE, auth bypass, request smuggling, container
     escape): aim for a patch release within 30 days.
   - **High** (information disclosure across webapps, persistent DoS):
     aim for a patch release within 60 days.
   - **Medium / Low**: rolled into the next regular patch release.
4. **Coordinated release**. By default we hold publication until a
   fixed release is available, with an embargo of **up to 90 days**
   from the initial private report. If you need a different window
   (vendor coordination, conference deadline) please say so in the
   report — we will accommodate where we can.
5. **Public advisory**. When the fix ships we publish a GitHub Security
   Advisory and request a CVE assignment via GitHub's CNA. The
   `CHANGELOG.md` entry for the fixed release links to the advisory.

### Credit

Reporters who follow this process are credited in the published
advisory and in `CHANGELOG.md`, unless they request to remain
anonymous. We will not name a reporter without permission.

### CVE assignment

CVEs are requested through GitHub's CNA when an advisory is published.
For issues with cross-ecosystem impact we may coordinate with other
CNAs as appropriate.

### Bug bounty

There is no monetary bug-bounty program at this time. The project is
volunteer-run. We will still publicly credit accepted reports.

---

## Threat model

Tomcat-RS sits at the boundary between **untrusted public traffic** and
**trusted Java code running in an embedded JVM**. Several distinct
trust boundaries cross that path, and a useful threat model names each
of them explicitly. For each boundary we name the *attackers we worry
about*, the *protections in place today*, and the *residual risks* that
operators should mitigate at the deployment layer.

### 1. Public network ↔ Coyote connectors

This is the front door. The attacker is anyone on the network that can
open a TCP connection.

- **In scope**: malformed HTTP/1.1, HTTP/2, and AJP frames; oversize
  headers / URIs / bodies; request smuggling via header desync; HPACK
  table-overflow tricks; slow-loris-style resource exhaustion; URI
  ambiguity (encoded slashes, double-decoding, traversal, Windows-style
  separators); TLS misconfiguration.
- **Protections**:
  - Connector-level `RequestLimits` cap header count and size,
    parameter count, post size, URI length, and request / keep-alive
    timeouts (`tomcatrs-config::RequestLimits`).
  - URI normalization and validation in
    `tomcatrs-security::access_control` follows a *reject, don't
    sanitize* policy: encoded separators (`%2f`, `%5c`), NUL bytes,
    escaping traversal, and direct hits on `/WEB-INF` / `/META-INF`
    are turned into rejections rather than silently rewritten.
  - HTTP/1.1, HTTP/2, AJP, chunked-decode, cookie, URI, and HPACK
    parsers are exercised by `cargo-fuzz` corpora.
  - TLS is `rustls`-backed, ALPN-negotiated; weak protocols and
    ciphers are off the table by virtue of the rustls defaults.
- **Residual risks**: TCP-level DoS (SYN flood, connection storms)
  must still be handled by upstream infrastructure (firewall, load
  balancer, rate-limiter); see *Hardening recommendations* below.

### 2. Rust runtime ↔ embedded JVM (JNI bridge)

When `--features jvm` is enabled the runtime hosts a JVM in-process
and dispatches requests across a JNI bridge. The trust boundary here
is **inverted** compared to the network boundary: the JVM-side code
(servlets, JSP, filters) is the application code we are running on
behalf of operators, but the data it processes is attacker-controlled.

- **In scope**: data validation across the FFI; classloader isolation
  between webapps; preventing a malicious WAR from corrupting the
  Rust runtime; bridge-JAR substitution.
- **Protections**:
  - Per-webapp classloader hierarchy (Bootstrap → System → Common →
    Webapp) keeps one webapp's classes from observing another's.
  - Lazy header / attribute materialisation on the FFI keeps the
    bridge surface narrow and explicit.
  - `tomcatrs-bridge.jar` is shipped alongside each Rust release and
    is expected to match version-for-version; running an older or
    locally-modified bridge JAR against a newer Rust runtime (or vice
    versa) is unsupported.
- **Residual risks**: a webapp that escapes its classloader (e.g. via
  a privileged native library it loads itself) is outside of the
  servlet container's control; operators must vet third-party WARs.

### 3. Clustered nodes ↔ cluster transport

When session replication is enabled (`DeltaManager` or
`BackupManager`), nodes exchange session state over a
`ClusterTransport`. The attacker is anyone with access to the cluster
network.

- **In scope**: session replay, session injection, transport
  interception, malicious "node" joining the membership.
- **Protections**:
  - The transport is a trait (`tomcatrs-session::cluster`) so
    operators can plug in their own authenticated / encrypted
    transport.
  - The default bundled transports are loopback / development-shaped
    and are **not** intended to face untrusted networks.
- **Residual risks**: the published 1.0.0 line does **not** ship a
  Tribes-compatible authenticated TCP transport — that is on the
  roadmap (1.4.0). Until then cluster traffic must run on a dedicated
  trusted network segment (e.g. a private VLAN or a service mesh
  doing mTLS for the operator).

### 4. The Manager API

The Manager API exposes operational verbs (`list`, `serverinfo`,
`sessions`, `reload`, `start`, `stop`, `deploy`, `undeploy`) over
HTTP. Its attacker model assumes a network-adjacent adversary trying
to deploy or remove applications.

- **In scope**: unauthenticated access; non-loopback access without a
  realm; remote deploy of a malicious WAR.
- **Protections**:
  - Defaults are deny-by-default: `allow_remote = false` (only
    loopback peers admitted), `require_auth = true`, and the service
    refuses every request with `503` until a realm is configured.
  - HTTP Basic against a configured `Realm` is the documented
    authentication path; realm backends include in-memory,
    `tomcat-users.xml`-style file, combined, and lock-out wrappers.
- **Residual risks**: operators who flip `allow_remote = true` without
  pairing it with a realm + role grant effectively give the network
  the deploy verb. The `tomcatrs preflight` subcommand exists to
  catch that combination before it ships.

---

## Hardening recommendations

The defaults are conservative, but production deployments should still
apply the following.

### Run behind a reverse proxy or load balancer

A fronting `nginx`, `httpd`, Envoy, ALB, or equivalent gives you
TLS termination options, rate-limiting, IP allowlists, request
inspection, and absorbs TCP-level DoS. Tomcat-RS is happy running
behind one (HTTP/1.1 over loopback, or AJP on a private network).

### Set realistic `RequestLimits`

Override the defaults on each `<Connector>` to match your application:

- `maxHeaderCount`, `maxHttpHeaderSize` — keep headers small unless
  you genuinely run cookie-heavy SSO; the defaults are 100 / 8192.
- `maxHttpRequestHeaderSize` — caps URI length.
- `maxPostSize`, `maxPartCount` — cap upload body and multipart parts.
- `connectionTimeout`, `keepAliveTimeout` — keep slow clients from
  parking connections.

The `tomcatrs preflight` command emits a warning when every limit on
every connector is left at the default, since that is rarely the right
shape for a real workload.

### Disable AJP unless it's on a trusted network with a secret

AJP was historically a "behind-`httpd`" protocol, and the Ghostcat
vulnerability class still shapes its threat model. If you do enable
the AJP connector:

- Bind it to a **private** address (`127.0.0.1`, a dedicated VLAN
  address — never `0.0.0.0`).
- Configure a **secret** on the connector so unauthorised peers can't
  send arbitrary requests. `tomcatrs preflight --strict` fails when an
  AJP connector ships without a secret.
- Front it only with a reverse proxy that knows the secret.

### Manager API: loopback-only or auth + role

Keep `allow_remote = false` whenever possible. If you need to admit
non-loopback peers, **pair** it with `require_auth = true` and a
configured realm + a `manager-script` / `manager-gui` role. The
preflight check fails in `--strict` mode when `allow_remote = true`
without a realm.

### Confirm the bridge JAR provenance

`tomcatrs-bridge.jar` must match the Rust release version. Use the
checksum published alongside the release; do **not** mix a bridge JAR
from one release with the runtime binary from another. A mismatched
bridge JAR can produce subtle behavioural drift and undefined-behaviour
across the FFI.

### Pin the JDK version under `--features jvm`

The embedded JVM is sensitive to JDK build; pin to a specific JDK
LTS line (Java 17 or 21) in your build and deployment pipelines.
Mixing JDK versions between build-time JNI generation and run-time
load can break the bridge in ways that are easy to misdiagnose.

### Use `tls = "tls"` (default) with `rustls`; rotate certificates

The `tls` Cargo feature is on by default and links `rustls` for TLS
termination. Do not disable it on a production build. Operate your
TLS material like any other secret: rotate certificates regularly,
ship the chain (not just the leaf), and watch expiry.

### Run as an unprivileged user; consider container or VM isolation

Tomcat-RS does not need root. Run the process under a dedicated
service account, drop capabilities, and prefer running inside a
container or VM that isolates the JVM heap and the filesystem from
the rest of the host. `tomcatrs preflight` warns when invoked as
`uid 0` on Unix.

### Watch the change log

`CHANGELOG.md` and the GitHub Releases page are the source of truth
for security-relevant changes. Subscribe to releases on GitHub to be
notified when a security advisory is published.

---

## Known limitations

We would rather be honest about the rough edges than ship a security
policy that overstates the runtime's maturity.

- **JSP runtime compile uses embedded Jasper.** JSP compilation on
  demand is delegated to `org.apache.jasper.servlet.JspServlet` on
  the JVM side. Jasper is a sizeable JVM-side attack surface that
  Tomcat-RS does not rewrite in Rust; if a vulnerability lands in
  upstream Jasper it lands in Tomcat-RS too. The precompile-first
  workflow keeps Jasper off the request path and is the recommended
  posture for production.
- **JVM bridge has not yet been exercised against a real Spring Boot
  WAR in CI.** The bridge is exercised by the `tomcatrs-compat-tests`
  harness and by unit / integration tests inside the workspace; a
  full Spring-Boot-WAR-against-stock-Tomcat-vs-Tomcat-RS comparison
  is on the 1.1.x agenda. Until that lands, treat Spring Boot deploys
  as "should work" rather than "tested".
- **LDAP realm is not yet implemented.** The realm backends shipped
  in 1.0.0 are in-memory, `tomcat-users.xml`-style file, combined,
  and lock-out. Sites that need LDAP / Active Directory authentication
  must wait for 1.1.0 or implement the `Realm` trait themselves.
- **No published Jakarta TCK results yet.** We track Servlet,
  JSP, EL, and WebSocket compatibility through differential tests
  against stock Tomcat — not through the Jakarta TCK. A formal TCK
  run is desirable but not in scope for 1.0.x.
- **No public security audit.** The runtime has not yet been audited
  by an independent third party. Internal review, fuzzing, and the
  security conformance suite all give us reasonable confidence in
  the protections described above, but they are not a substitute for
  an audit. We welcome reports that find gaps we missed.

---

## Continuous security checks

The policy above is enforced by automation on every pull request and on
a daily schedule, so passive advisories surface without anyone needing
to remember to run them. The workflows live under
[`.github/workflows/`](.github/workflows/):

- [`ci.yml`](.github/workflows/ci.yml) — formatting, build, default test
  suite, the JVM-feature test suite (JDK 21), and a no-default-features
  build that proves the `tls` Cargo feature on `tomcatrs-coyote` is
  honestly optional. Clippy currently runs in non-blocking mode while a
  small backlog of warnings is burnt down; see the comment at the top of
  the file. Matrix: `ubuntu-latest` and `macos-latest`. Rust toolchain
  pinned through [`rust-toolchain.toml`](rust-toolchain.toml).
- [`security.yml`](.github/workflows/security.yml) — supply-chain
  automation. Runs `cargo audit` (RustSec advisories), `cargo deny`
  (license, ban, advisory, and source policies driven by
  [`deny.toml`](deny.toml)), and Google's `osv-scanner`. Triggered on
  push, on pull request, and on a daily cron at 06:00 UTC.
- [`release.yml`](.github/workflows/release.yml) — runs on `v*.*.*`
  tags; produces Linux + macOS release archives with `--features jvm`
  enabled, plus SHA-256 sidecars, and attaches them to the GitHub
  Release.

Justified advisory ignores must be recorded in
[`.cargo/audit.toml`](.cargo/audit.toml) with a comment and a tracking
issue; the ignore list is reviewed at every release. Dependency updates
are proposed weekly by Dependabot (configuration in
[`.github/dependabot.yml`](.github/dependabot.yml)), grouping minor and
patch bumps per ecosystem.

---

## Quick links

- GitHub Security Advisories (private): **Security → Advisories →
  Report a vulnerability** on the
  [Michael repository](https://github.com/nktkt/Michael).
- Public issue tracker (non-sensitive only):
  [Issues](https://github.com/nktkt/Michael/issues).
- Hardening preflight: `tomcatrs preflight --config <server.xml>`,
  optionally with `--strict`. See `crates/tomcatrs-cli` for source.
- Change log: [`CHANGELOG.md`](CHANGELOG.md).
- License: [`LICENSE`](LICENSE) (Apache-2.0), [`NOTICE`](NOTICE).
