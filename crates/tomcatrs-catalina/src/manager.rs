//! [`ManagerService`] — the Tomcat **Manager** application, ported as an
//! in-process HTTP service.
//!
//! In Apache Tomcat the `/manager` web application is a privileged HTML/JSON
//! console for inspecting services, hosts and contexts and for performing
//! lifecycle transitions (`/manager/text/list`, `.../reload`, `.../stop`,
//! `.../start`, `.../deploy`, `.../undeploy`). It is the operations interface
//! for a running container.
//!
//! This module ports the **text/JSON** subset of that surface and wires it
//! directly into the Catalina component tree. There is no servlet involved —
//! the [`ManagerService`] implements [`tomcatrs_coyote::Adapter`] and can be
//! mounted alongside the regular [`CatalinaAdapter`](crate::CatalinaAdapter)
//! by inspecting the request path. v1.0.0 keeps it intentionally minimal:
//!
//! | Path                                  | Method | Purpose                                  |
//! |---------------------------------------|--------|------------------------------------------|
//! | `<mount>/list`                        | GET    | List services / hosts / contexts (JSON). |
//! | `<mount>/serverinfo`                  | GET    | Version, uptime, OS, runtime info.       |
//! | `<mount>/sessions?context=/foo`       | GET    | Session count for the named context.    |
//! | `<mount>/reload?context=/foo`         | POST   | Best-effort re-deploy of a context.      |
//! | `<mount>/stop?context=/foo`           | POST   | Drive a context to `Stopped`.            |
//! | `<mount>/start?context=/foo`          | POST   | Drive a context back to `Started`.       |
//! | `<mount>/deploy?path=/foo&war=...`    | POST   | Record intent, re-run host scanner.      |
//! | `<mount>/undeploy?path=/foo`          | POST   | Remove a context from its host.          |
//! | `<mount>/health`                      | GET    | Trivial liveness probe.                  |
//! | `<mount>/html`                        | GET    | Minimal HTML operator console.           |
//!
//! Anything else under the mount path returns `404` with a small JSON error
//! body. A full WAR upload pipeline for `/deploy` is future work — for now the
//! endpoint logs the requested parameters and re-runs the host's
//! [`crate::deployer::HostDeployer`] so that an operator who has
//! already dropped an exploded webapp into the host's `app_base` can pick it
//! up without restarting the server.
//!
//! # Security
//!
//! The manager service is meant to be reachable only by trusted operators.
//! [`ManagerConfig`] defaults to:
//!
//! * **Localhost only.** Requests from a non-loopback peer (anything other than
//!   `127.0.0.0/8` or `::1`) are rejected with `403`. Set
//!   [`ManagerConfig::allow_remote`] to `true` to lift this — typically only
//!   when the connector is itself bound behind a trusted reverse proxy.
//! * **HTTP Basic over a [`Realm`].** If [`ManagerConfig::require_auth`] is
//!   set and a [`Realm`] is configured, every request must carry an
//!   `Authorization: Basic …` header that the realm authenticates *and* whose
//!   resulting [`Principal`] holds either the `manager-gui` or
//!   `manager-script` role — the same role names Apache Tomcat uses. A missing
//!   `Authorization` header yields a `401` with
//!   `WWW-Authenticate: Basic realm="Tomcat-RS Manager"`; a present-but-bad
//!   credential yields `401`; an authenticated user without a manager role
//!   yields `403`.
//!
//! [`Realm`]: tomcatrs_security::Realm
//! [`Principal`]: tomcatrs_security::Principal

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use tomcatrs_core::{Lifecycle, LifecycleContext};
use tomcatrs_coyote::{Adapter, Request, Response};
use tomcatrs_security::auth_basic::BasicAuthenticator;
use tomcatrs_security::realm::Realm;

use crate::deployer::HostDeployer;
use crate::manager_ui::ManagerHtmlPage;
use crate::server::Server;

/// Default URL mount point for the manager service.
pub const DEFAULT_MOUNT_PATH: &str = "/manager";

/// The challenge realm name advertised in `WWW-Authenticate`.
pub const MANAGER_REALM: &str = "Tomcat-RS Manager";

/// Role names accepted for manager operations. Mirrors Apache Tomcat's
/// `tomcat-users.xml` convention.
const MANAGER_ROLES: &[&str] = &["manager-gui", "manager-script"];

/// Knobs that control [`ManagerService`].
///
/// Created by [`ManagerConfig::default`] in a locked-down, deny-by-default
/// shape: `mount_path = "/manager"`, `allow_remote = false`, `require_auth =
/// true`, no realm. Builder-style `with_*` methods make it easy to opt into a
/// specific deployment posture.
#[derive(Clone)]
pub struct ManagerConfig {
    /// URL prefix the manager mounts under, e.g. `/manager`.
    pub mount_path: String,
    /// When `false`, only loopback peers are admitted; non-loopback requests
    /// get a `403`.
    pub allow_remote: bool,
    /// When `true`, every request must authenticate via HTTP Basic against
    /// `basic_realm`.
    pub require_auth: bool,
    /// Realm that authenticates incoming Basic credentials. Required when
    /// `require_auth` is `true`; otherwise the service refuses every request
    /// with `503` because it has nothing to authenticate against.
    pub basic_realm: Option<Arc<dyn Realm>>,
}

impl std::fmt::Debug for ManagerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagerConfig")
            .field("mount_path", &self.mount_path)
            .field("allow_remote", &self.allow_remote)
            .field("require_auth", &self.require_auth)
            .field("basic_realm", &self.basic_realm.as_ref().map(|_| "<realm>"))
            .finish()
    }
}

impl Default for ManagerConfig {
    fn default() -> Self {
        ManagerConfig {
            mount_path: DEFAULT_MOUNT_PATH.to_string(),
            allow_remote: false,
            require_auth: true,
            basic_realm: None,
        }
    }
}

impl ManagerConfig {
    /// Builder-style setter for [`Self::mount_path`].
    pub fn with_mount_path(mut self, mount: impl Into<String>) -> Self {
        self.mount_path = mount.into();
        self
    }

    /// Builder-style setter for [`Self::allow_remote`].
    pub fn with_allow_remote(mut self, allow: bool) -> Self {
        self.allow_remote = allow;
        self
    }

    /// Builder-style setter for [`Self::require_auth`].
    pub fn with_require_auth(mut self, require: bool) -> Self {
        self.require_auth = require;
        self
    }

    /// Builder-style setter for [`Self::basic_realm`].
    pub fn with_realm(mut self, realm: Arc<dyn Realm>) -> Self {
        self.basic_realm = Some(realm);
        self
    }
}

/// The Tomcat Manager application, mounted as a [`tomcatrs_coyote::Adapter`].
///
/// Construct one with [`ManagerService::new`] and either:
///
/// * mount its `service` directly on a connector dedicated to administrative
///   traffic, or
/// * delegate from a top-level adapter when the request path starts with the
///   configured mount prefix (see [`ManagerService::owns_path`]).
pub struct ManagerService {
    server: Arc<Server>,
    config: ManagerConfig,
    /// Process start time, used for the `serverinfo` uptime field.
    started_at: Instant,
}

impl std::fmt::Debug for ManagerService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagerService")
            .field("config", &self.config)
            .field("services", &self.server.services().len())
            .finish()
    }
}

impl ManagerService {
    /// Wire a manager service to `server` with `config`.
    pub fn new(server: Arc<Server>, config: ManagerConfig) -> Self {
        ManagerService {
            server,
            config,
            started_at: Instant::now(),
        }
    }

    /// Read-only access to the configured mount prefix (e.g. `/manager`).
    pub fn mount_path(&self) -> &str {
        &self.config.mount_path
    }

    /// Returns `true` if `request_path` is mounted under this service's
    /// [`mount_path`](Self::mount_path), accounting for an exact match or a
    /// `<mount>/...` sub-path. A request to `/managerX` is **not** owned —
    /// the next character must be `/` or the path must end exactly at the
    /// mount point.
    pub fn owns_path(&self, request_path: &str) -> bool {
        let mount = self.mount_path();
        if request_path == mount {
            return true;
        }
        request_path
            .strip_prefix(mount)
            .is_some_and(|rest| rest.starts_with('/'))
    }

    /// Dispatch a single Manager API request, returning the response that
    /// should be sent on the wire.
    ///
    /// This is the entry point used by the `Adapter` impl and is also exposed
    /// publicly so a higher-level adapter can delegate to the manager when it
    /// recognises the mount path.
    pub async fn service(&self, req: Request) -> Response {
        // 1. Peer admission: localhost-only unless explicitly opened up.
        if !self.config.allow_remote && !is_loopback(&req.peer_addr.ip()) {
            return forbidden("remote access disabled; manager is localhost-only");
        }

        // 2. Authentication: HTTP Basic against the configured realm.
        if self.config.require_auth {
            let Some(realm) = self.config.basic_realm.as_ref() else {
                // require_auth set without a realm is a configuration error;
                // refuse rather than silently letting requests through.
                return service_unavailable("manager realm not configured");
            };
            let auth_header = req.header("Authorization");
            let basic = BasicAuthenticator::new(MANAGER_REALM);
            match basic.authenticate(realm.as_ref(), auth_header).await {
                Ok(Some(principal)) => {
                    if !MANAGER_ROLES.iter().any(|r| principal.has_role(r)) {
                        return forbidden(&format!(
                            "user '{}' lacks a manager role",
                            principal.name
                        ));
                    }
                }
                Ok(None) => {
                    return unauthorized(&basic.challenge());
                }
                Err(err) => {
                    tracing::warn!(error = %err, "manager realm backend failure");
                    return service_unavailable("realm backend failure");
                }
            }
        }

        // 3. Dispatch by path + method.
        let Some(action) = req.path.strip_prefix(self.mount_path()) else {
            return not_found(&req.path);
        };
        // Allow an exact `<mount>` request to show health text rather than
        // 404; otherwise the suffix must begin with `/`.
        let action = if action.is_empty() {
            "/health"
        } else if let Some(rest) = action.strip_prefix('/') {
            if rest.is_empty() {
                "health"
            } else {
                rest
            }
        } else {
            return not_found(&req.path);
        };

        let query = parse_query(req.query.as_deref().unwrap_or(""));

        match (req.method.as_str(), action) {
            ("GET", "list") => self.handle_list(),
            ("GET", "serverinfo") => self.handle_serverinfo(),
            ("GET", "sessions") => self.handle_sessions(&query),
            ("POST", "reload") => self.handle_reload(&query).await,
            ("POST", "stop") => self.handle_stop(&query).await,
            ("POST", "start") => self.handle_start(&query).await,
            ("POST", "deploy") => self.handle_deploy(&query),
            ("POST", "undeploy") => self.handle_undeploy(&query),
            ("GET", "health") => plain_text(200, "200 OK\n"),
            ("GET", "html") => self.handle_html(),
            _ => not_found(&req.path),
        }
    }

    /// Render the minimal HTML operator page; see [`crate::manager_ui`].
    fn handle_html(&self) -> Response {
        ManagerHtmlPage::new(Arc::clone(&self.server), self.config.mount_path.clone()).render()
    }

    fn handle_list(&self) -> Response {
        let services: Vec<serde_json::Value> = self
            .server
            .services()
            .iter()
            .map(|service| {
                let engine = service.engine();
                let hosts: Vec<serde_json::Value> = engine
                    .hosts()
                    .iter()
                    .map(|entry| {
                        let host = entry.value();
                        let contexts: Vec<serde_json::Value> = host
                            .contexts()
                            .iter()
                            .map(|c| {
                                let ctx = c.value();
                                serde_json::json!({
                                    "path": ctx.path(),
                                    "doc_base": ctx.doc_base().display().to_string(),
                                    "state": format!("{:?}", ctx.state()),
                                    "wrappers": ctx.wrappers().len(),
                                })
                            })
                            .collect();
                        let connectors: Vec<serde_json::Value> = service
                            .connector_configs()
                            .iter()
                            .map(|c| {
                                serde_json::json!({
                                    "protocol": format!("{:?}", c.protocol),
                                    "port": c.port,
                                })
                            })
                            .collect();
                        serde_json::json!({
                            "name": host.name(),
                            "aliases": host.aliases(),
                            "app_base": host.app_base().display().to_string(),
                            "auto_deploy": host.auto_deploy(),
                            "state": format!("{:?}", host.state()),
                            "contexts": contexts,
                            "connectors": connectors,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "name": service.name(),
                    "state": format!("{:?}", service.state()),
                    "engine": {
                        "name": engine.name(),
                        "default_host": engine.default_host(),
                        "state": format!("{:?}", engine.state()),
                    },
                    "hosts": hosts,
                })
            })
            .collect();

        json_response(200, &serde_json::json!({ "services": services }))
    }

    fn handle_serverinfo(&self) -> Response {
        let uptime_secs = self.started_at.elapsed().as_secs();
        let body = serde_json::json!({
            "tomcatrs_version": tomcatrs_core::VERSION,
            "uptime_seconds": uptime_secs,
            "os": {
                "family": std::env::consts::FAMILY,
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
            },
            "runtime": {
                "rustc_target": std::env::consts::ARCH,
            },
            "shutdown_port": self.server.shutdown_port(),
            "services": self.server.services().len(),
        });
        json_response(200, &body)
    }

    fn handle_sessions(&self, query: &Query) -> Response {
        let Some(path) = query.get("context") else {
            return bad_request("missing 'context' query parameter");
        };
        let Some(_ctx) = self.find_context(path) else {
            return not_found_context(path);
        };
        // v1.0.0 does not track sessions per Catalina context inside this
        // crate (the session manager lives elsewhere and is not plumbed back
        // here yet). Report zero and surface the limitation explicitly so
        // operators can tell the answer apart from "no sessions exist".
        let body = serde_json::json!({
            "context": path,
            "session_count": 0,
            "note": "per-context session tracking is not wired into v1.0.0 manager",
        });
        json_response(200, &body)
    }

    async fn handle_reload(&self, query: &Query) -> Response {
        let Some(path) = query.get("context") else {
            return bad_request("missing 'context' query parameter");
        };
        let Some(ctx) = self.find_context(path) else {
            return not_found_context(path);
        };
        tracing::info!(context = %path, "manager: reload (best-effort)");
        // Best-effort reload: re-running `deploy()` is a no-op for contexts
        // that have already had their descriptor wired in (deploy_descriptor
        // is single-shot), so we treat that case as a successful no-op.
        let outcome = match ctx.deploy() {
            Ok(report) => serde_json::json!({
                "reloaded": true,
                "had_web_xml": report.had_web_xml,
                "servlets": report.servlet_count,
            }),
            Err(err) => serde_json::json!({
                "reloaded": false,
                "note": "deploy already applied or descriptor unavailable",
                "error": err.to_string(),
            }),
        };
        json_response(
            200,
            &serde_json::json!({ "context": path, "result": outcome }),
        )
    }

    async fn handle_stop(&self, query: &Query) -> Response {
        let Some(path) = query.get("context") else {
            return bad_request("missing 'context' query parameter");
        };
        let Some(ctx) = self.find_context(path) else {
            return not_found_context(path);
        };
        let lc = LifecycleContext::new("Manager");
        if let Err(err) = ctx.stop(&lc).await {
            return server_error(&format!("stop failed: {err}"));
        }
        json_response(
            200,
            &serde_json::json!({
                "context": path,
                "state": format!("{:?}", ctx.state()),
            }),
        )
    }

    async fn handle_start(&self, query: &Query) -> Response {
        let Some(path) = query.get("context") else {
            return bad_request("missing 'context' query parameter");
        };
        let Some(ctx) = self.find_context(path) else {
            return not_found_context(path);
        };
        let lc = LifecycleContext::new("Manager");
        if let Err(err) = ctx.start(&lc).await {
            return server_error(&format!("start failed: {err}"));
        }
        json_response(
            200,
            &serde_json::json!({
                "context": path,
                "state": format!("{:?}", ctx.state()),
            }),
        )
    }

    fn handle_deploy(&self, query: &Query) -> Response {
        let Some(path) = query.get("path") else {
            return bad_request("missing 'path' query parameter");
        };
        let war = query.get("war").map(String::as_str).unwrap_or("");
        tracing::info!(
            context_path = %path,
            war = %war,
            "manager: deploy requested; v1.0.0 only re-runs the host scanner",
        );
        // For every service+host, ask the deployer to scan again. This picks
        // up exploded webapps already on disk under `app_base`.
        let deployer = HostDeployer::new();
        let mut deployed_total = 0usize;
        let mut errors: Vec<String> = Vec::new();
        for service in self.server.services() {
            for entry in service.engine().hosts().iter() {
                let host = entry.value();
                match deployer.deploy_all(host) {
                    Ok(n) => deployed_total += n,
                    Err(err) => errors.push(format!("host '{}': {err}", host.name())),
                }
            }
        }
        json_response(
            200,
            &serde_json::json!({
                "requested_path": path,
                "requested_war": war,
                "scanned_and_deployed": deployed_total,
                "errors": errors,
                "note": "WAR upload is future work; deployment recorded as a host re-scan",
            }),
        )
    }

    fn handle_undeploy(&self, query: &Query) -> Response {
        let Some(path) = query.get("path") else {
            return bad_request("missing 'path' query parameter");
        };
        // Search every host for a context with this exact path; remove it
        // from the first host that owns it.
        for service in self.server.services() {
            for entry in service.engine().hosts().iter() {
                let host = entry.value();
                if host.remove_context(path).is_some() {
                    tracing::info!(context_path = %path, host = %host.name(), "manager: undeployed");
                    return json_response(
                        200,
                        &serde_json::json!({
                            "undeployed": path,
                            "host": host.name(),
                        }),
                    );
                }
            }
        }
        not_found_context(path)
    }

    /// Find a context by its exact context path across every service+host.
    fn find_context(&self, path: &str) -> Option<Arc<crate::context::Context>> {
        for service in self.server.services() {
            for entry in service.engine().hosts().iter() {
                if let Some(ctx) = entry.value().context(path) {
                    return Some(ctx);
                }
            }
        }
        None
    }
}

#[async_trait]
impl Adapter for ManagerService {
    async fn service(&self, req: Request) -> Response {
        ManagerService::service(self, req).await
    }
}

// ---------- helpers ----------

/// A parsed `?a=b&c=d` query string.
type Query = std::collections::HashMap<String, String>;

/// Parse a query string into a flat `name → value` map.
///
/// Values are percent-decoded on a best-effort basis: `+` becomes space and
/// `%XX` sequences are decoded when they form valid UTF-8; malformed input is
/// passed through unchanged rather than rejecting the whole request.
fn parse_query(raw: &str) -> Query {
    let mut out = Query::new();
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        out.insert(percent_decode(k), percent_decode(v));
    }
    out
}

/// Decode `+` → space and `%XX` percent-escapes; best-effort.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push(((h << 4) | l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|e| {
        // Fall back to the input as-is if the decoded bytes aren't UTF-8.
        let _ = e;
        input.to_string()
    })
}

/// Returns `true` if `ip` is a loopback address (`127.0.0.0/8` or `::1`).
fn is_loopback(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.octets()[0] == 127 || *v4 == Ipv4Addr::LOCALHOST,
        IpAddr::V6(v6) => *v6 == Ipv6Addr::LOCALHOST,
    }
}

fn json_response(status: u16, body: &serde_json::Value) -> Response {
    let text = serde_json::to_string(body).unwrap_or_else(|_| "{}".to_string());
    let mut resp = Response::with_body(status, Bytes::from(text));
    resp.set_header("Content-Type", "application/json")
        .set_header("Cache-Control", "no-store");
    resp
}

fn plain_text(status: u16, body: &'static str) -> Response {
    let mut resp = Response::with_body(status, Bytes::from_static(body.as_bytes()));
    resp.set_header("Content-Type", "text/plain; charset=utf-8");
    resp
}

fn json_error(status: u16, message: &str) -> Response {
    json_response(status, &serde_json::json!({ "error": message }))
}

fn not_found(path: &str) -> Response {
    json_error(404, &format!("unknown manager endpoint: {path}"))
}

fn not_found_context(path: &str) -> Response {
    json_error(404, &format!("no context deployed at '{path}'"))
}

fn bad_request(message: &str) -> Response {
    json_error(400, message)
}

fn forbidden(message: &str) -> Response {
    json_error(403, message)
}

fn server_error(message: &str) -> Response {
    json_error(500, message)
}

fn service_unavailable(message: &str) -> Response {
    json_error(503, message)
}

fn unauthorized(challenge: &str) -> Response {
    let mut resp = json_error(401, "authentication required");
    resp.set_header("WWW-Authenticate", challenge);
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::path::PathBuf;

    use tomcatrs_config::ServerConfig;
    use tomcatrs_core::Lifecycle;
    use tomcatrs_security::realm::InMemoryRealm;

    /// Build a `Request` against the manager with the given path, peer and
    /// optional Authorization header value.
    fn req(method: &str, full_path: &str, peer: &str, auth: Option<&str>) -> Request {
        let (path, query) = match full_path.split_once('?') {
            Some((p, q)) => (p.to_string(), Some(q.to_string())),
            None => (full_path.to_string(), None),
        };
        let peer_addr: SocketAddr = peer.parse().expect("peer addr");
        let mut headers = vec![("Host".to_string(), "localhost".to_string())];
        if let Some(value) = auth {
            headers.push(("Authorization".to_string(), value.to_string()));
        }
        Request {
            method: method.to_string(),
            uri: full_path.to_string(),
            path,
            query,
            version: "HTTP/1.1".to_string(),
            headers,
            body: Bytes::new(),
            peer_addr,
        }
    }

    /// A `Server` built from the dev-default config (`localhost` host with no
    /// contexts pre-deployed).
    fn dev_server() -> Arc<Server> {
        Arc::new(Server::from_config(&ServerConfig::default_dev()).unwrap())
    }

    /// A `Server` whose single host has one context mounted at `/app`. Used
    /// by lifecycle / undeploy tests where the dev server's empty host map
    /// would leave us nothing to manipulate.
    fn server_with_app_context() -> Arc<Server> {
        use tomcatrs_config::{
            ConnectorConfig, ContextConfig, EngineConfig, HostConfig, Protocol, RequestLimits,
            ServiceConfig,
        };
        let cfg = ServerConfig {
            port: 8005,
            shutdown: "SHUTDOWN".to_string(),
            services: vec![ServiceConfig {
                name: "Catalina".to_string(),
                connectors: vec![ConnectorConfig {
                    protocol: Protocol::Http11,
                    address: None,
                    port: 8080,
                    tls: None,
                    limits: RequestLimits::default(),
                }],
                engine: EngineConfig {
                    name: "Catalina".to_string(),
                    default_host: "localhost".to_string(),
                    hosts: vec![HostConfig {
                        name: "localhost".to_string(),
                        app_base: PathBuf::from("webapps"),
                        aliases: Vec::new(),
                        auto_deploy: false,
                        contexts: vec![ContextConfig {
                            path: "/app".to_string(),
                            doc_base: PathBuf::from("/tmp/app"),
                            reloadable: false,
                        }],
                    }],
                },
            }],
        };
        Arc::new(Server::from_config(&cfg).unwrap())
    }

    /// Tiny RFC 4648 base64 encoder for test fixtures. Encoding only — the
    /// crate proper uses [`tomcatrs_security`] to parse the decoded form.
    fn b64_encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        let mut i = 0;
        while i + 3 <= bytes.len() {
            let b0 = bytes[i] as u32;
            let b1 = bytes[i + 1] as u32;
            let b2 = bytes[i + 2] as u32;
            let triple = (b0 << 16) | (b1 << 8) | b2;
            out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
            out.push(ALPHABET[((triple >> 6) & 0x3f) as usize] as char);
            out.push(ALPHABET[(triple & 0x3f) as usize] as char);
            i += 3;
        }
        let rem = bytes.len() - i;
        if rem == 1 {
            let b0 = bytes[i] as u32;
            let triple = b0 << 16;
            out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        } else if rem == 2 {
            let b0 = bytes[i] as u32;
            let b1 = bytes[i + 1] as u32;
            let triple = (b0 << 16) | (b1 << 8);
            out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
            out.push(ALPHABET[((triple >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        out
    }

    fn basic_header(user: &str, pass: &str) -> String {
        format!("Basic {}", b64_encode(format!("{user}:{pass}").as_bytes()))
    }

    fn admin_realm() -> Arc<dyn Realm> {
        let realm =
            InMemoryRealm::new().with_user("admin", "secret", vec!["manager-script".into()]);
        Arc::new(realm)
    }

    fn open_config_no_auth() -> ManagerConfig {
        ManagerConfig::default().with_require_auth(false)
    }

    #[tokio::test]
    async fn list_includes_configured_services_hosts_and_contexts() {
        let mgr = ManagerService::new(dev_server(), open_config_no_auth());
        let resp = mgr
            .service(req("GET", "/manager/list", "127.0.0.1:0", None))
            .await;

        assert_eq!(resp.status, 200);
        assert_eq!(resp.header("Content-Type"), Some("application/json"));

        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        let services = body["services"].as_array().expect("services array");
        assert!(!services.is_empty(), "default dev config defines a service");

        let hosts = services[0]["hosts"].as_array().expect("hosts array");
        assert!(!hosts.is_empty(), "default dev config defines a host");
        assert_eq!(hosts[0]["name"].as_str(), Some("localhost"));
    }

    #[tokio::test]
    async fn serverinfo_reports_version() {
        let mgr = ManagerService::new(dev_server(), open_config_no_auth());
        let resp = mgr
            .service(req("GET", "/manager/serverinfo", "127.0.0.1:0", None))
            .await;
        assert_eq!(resp.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(
            body["tomcatrs_version"].as_str(),
            Some(tomcatrs_core::VERSION)
        );
        assert!(body["uptime_seconds"].as_u64().is_some());
        assert!(body["os"]["family"].as_str().is_some());
    }

    #[tokio::test]
    async fn health_is_a_simple_liveness_probe() {
        let mgr = ManagerService::new(dev_server(), open_config_no_auth());
        let resp = mgr
            .service(req("GET", "/manager/health", "127.0.0.1:0", None))
            .await;
        assert_eq!(resp.status, 200);
        assert_eq!(
            resp.header("Content-Type"),
            Some("text/plain; charset=utf-8")
        );
        assert_eq!(&resp.body[..], b"200 OK\n");
    }

    #[tokio::test]
    async fn unknown_path_under_mount_is_404() {
        let mgr = ManagerService::new(dev_server(), open_config_no_auth());
        let resp = mgr
            .service(req("GET", "/manager/no-such-thing", "127.0.0.1:0", None))
            .await;
        assert_eq!(resp.status, 404);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert!(body["error"].as_str().unwrap().contains("unknown"));
    }

    #[tokio::test]
    async fn non_loopback_peer_is_rejected_when_remote_disallowed() {
        let mgr = ManagerService::new(dev_server(), open_config_no_auth());
        let resp = mgr
            .service(req("GET", "/manager/list", "8.8.8.8:1234", None))
            .await;
        assert_eq!(resp.status, 403);
    }

    #[tokio::test]
    async fn non_loopback_peer_is_admitted_when_remote_allowed() {
        let mgr = ManagerService::new(
            dev_server(),
            ManagerConfig::default()
                .with_allow_remote(true)
                .with_require_auth(false),
        );
        let resp = mgr
            .service(req("GET", "/manager/list", "8.8.8.8:1234", None))
            .await;
        assert_eq!(resp.status, 200);
    }

    #[tokio::test]
    async fn missing_auth_yields_401_with_challenge() {
        let mgr = ManagerService::new(
            dev_server(),
            ManagerConfig::default().with_realm(admin_realm()),
        );
        let resp = mgr
            .service(req("GET", "/manager/list", "127.0.0.1:0", None))
            .await;
        assert_eq!(resp.status, 401);
        let challenge = resp.header("WWW-Authenticate").expect("challenge present");
        assert!(challenge.contains(MANAGER_REALM));
    }

    #[tokio::test]
    async fn bad_credentials_yield_401() {
        let mgr = ManagerService::new(
            dev_server(),
            ManagerConfig::default().with_realm(admin_realm()),
        );
        let resp = mgr
            .service(req(
                "GET",
                "/manager/list",
                "127.0.0.1:0",
                Some(&basic_header("admin", "wrong-password")),
            ))
            .await;
        assert_eq!(resp.status, 401);
    }

    #[tokio::test]
    async fn authenticated_user_without_manager_role_is_forbidden() {
        let realm: Arc<dyn Realm> =
            Arc::new(InMemoryRealm::new().with_user("guest", "guestpw", vec!["staff".into()]));
        let mgr = ManagerService::new(dev_server(), ManagerConfig::default().with_realm(realm));
        let resp = mgr
            .service(req(
                "GET",
                "/manager/list",
                "127.0.0.1:0",
                Some(&basic_header("guest", "guestpw")),
            ))
            .await;
        assert_eq!(resp.status, 403);
    }

    #[tokio::test]
    async fn good_basic_auth_with_manager_role_grants_access() {
        let mgr = ManagerService::new(
            dev_server(),
            ManagerConfig::default().with_realm(admin_realm()),
        );
        let resp = mgr
            .service(req(
                "GET",
                "/manager/list",
                "127.0.0.1:0",
                Some(&basic_header("admin", "secret")),
            ))
            .await;
        assert_eq!(resp.status, 200);
    }

    #[tokio::test]
    async fn undeploy_removes_context_from_host() {
        let server = server_with_app_context();
        let host = server
            .services()
            .first()
            .unwrap()
            .engine()
            .host("localhost")
            .unwrap();
        assert!(host.context("/app").is_some());

        let mgr = ManagerService::new(Arc::clone(&server), open_config_no_auth());
        let resp = mgr
            .service(req(
                "POST",
                "/manager/undeploy?path=%2Fapp",
                "127.0.0.1:0",
                None,
            ))
            .await;
        assert_eq!(resp.status, 200, "body = {:?}", resp.body);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["undeployed"].as_str(), Some("/app"));
        assert!(host.context("/app").is_none());
    }

    #[tokio::test]
    async fn undeploy_for_unknown_path_is_404() {
        let mgr = ManagerService::new(dev_server(), open_config_no_auth());
        let resp = mgr
            .service(req(
                "POST",
                "/manager/undeploy?path=%2Fnope",
                "127.0.0.1:0",
                None,
            ))
            .await;
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn start_then_stop_drives_context_state() {
        let server = server_with_app_context();
        let lc = LifecycleContext::new("Test");
        server.init(&lc).await.unwrap();
        server.start(&lc).await.unwrap();

        let mgr = ManagerService::new(Arc::clone(&server), open_config_no_auth());
        let resp = mgr
            .service(req(
                "POST",
                "/manager/stop?context=%2Fapp",
                "127.0.0.1:0",
                None,
            ))
            .await;
        assert_eq!(resp.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert!(body["state"].as_str().unwrap().contains("Stopped"));

        let resp = mgr
            .service(req(
                "POST",
                "/manager/start?context=%2Fapp",
                "127.0.0.1:0",
                None,
            ))
            .await;
        assert_eq!(resp.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert!(body["state"].as_str().unwrap().contains("Started"));
    }

    #[tokio::test]
    async fn owns_path_matches_mount_prefix_only_on_segment_boundary() {
        let mgr = ManagerService::new(dev_server(), ManagerConfig::default());
        assert!(mgr.owns_path("/manager"));
        assert!(mgr.owns_path("/manager/list"));
        assert!(!mgr.owns_path("/managerX"));
        assert!(!mgr.owns_path("/app/manager"));
    }

    #[test]
    fn loopback_detection_covers_v4_and_v6() {
        assert!(is_loopback(&"127.0.0.1".parse().unwrap()));
        assert!(is_loopback(&"127.99.0.1".parse().unwrap()));
        assert!(is_loopback(&"::1".parse().unwrap()));
        assert!(!is_loopback(&"10.0.0.1".parse().unwrap()));
        assert!(!is_loopback(&"8.8.8.8".parse().unwrap()));
        assert!(!is_loopback(&"2001:db8::1".parse().unwrap()));
    }

    #[test]
    fn parse_query_handles_percent_and_plus() {
        let q = parse_query("path=%2Fmy+app&war=&extra");
        assert_eq!(q.get("path").map(String::as_str), Some("/my app"));
        assert_eq!(q.get("war").map(String::as_str), Some(""));
        assert_eq!(q.get("extra").map(String::as_str), Some(""));
    }
}
