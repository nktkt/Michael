//! Minimal HTML console for the Manager service.
//!
//! In Apache Tomcat the `/manager/html` mount renders a single, pragmatic
//! operator page: a table of every deployed application together with
//! per-context start/stop/reload/undeploy forms. This module ports the
//! same idea in the smallest possible shape — pure server-side HTML, no
//! JavaScript and no templating engine, exposed through
//! [`ManagerHtmlPage::render`].
//!
//! It is wired into [`crate::manager::ManagerService`] by an additive
//! `handle_html` entry point, which the
//! `/manager/html` route dispatches to. The page is gated by the same
//! loopback/auth checks as the JSON endpoints — see
//! [`crate::manager`] for the security contract.

use std::sync::Arc;

use bytes::Bytes;
use tomcatrs_coyote::Response;

use crate::server::Server;

/// Renderer for the `/manager/html` operator page.
///
/// Held by [`crate::manager::ManagerService`] and re-rendered on every
/// request, so the table always reflects the live component tree.
#[derive(Debug, Clone)]
pub struct ManagerHtmlPage {
    server: Arc<Server>,
    mount_path: String,
}

impl ManagerHtmlPage {
    /// Build a renderer for `server`, generating links and form actions
    /// under `mount_path` (e.g. `/manager`).
    pub fn new(server: Arc<Server>, mount_path: impl Into<String>) -> Self {
        Self {
            server,
            mount_path: mount_path.into(),
        }
    }

    /// Render the page as a `text/html` [`Response`].
    pub fn render(&self) -> Response {
        let html = self.render_html();
        let mut resp = Response::with_body(200, Bytes::from(html));
        resp.set_header("Content-Type", "text/html; charset=utf-8")
            .set_header("Cache-Control", "no-store");
        resp
    }

    /// Render the raw HTML body. Exposed for testing.
    pub fn render_html(&self) -> String {
        let mut body = String::new();
        body.push_str(PAGE_HEAD);

        body.push_str("<h1>Tomcat-RS Manager</h1>");
        body.push_str(&format!(
            "<p>JSON view: <a href=\"{mount}/list\">{mount}/list</a> &middot; \
             <a href=\"{mount}/serverinfo\">{mount}/serverinfo</a> &middot; \
             <a href=\"{mount}/health\">{mount}/health</a></p>",
            mount = escape_html(&self.mount_path),
        ));

        for service in self.server.services().iter() {
            let engine = service.engine();
            body.push_str(&format!(
                "<h2>Service: {} <small>(engine: {}, state: {:?})</small></h2>",
                escape_html(service.name()),
                escape_html(engine.name()),
                service.state(),
            ));

            let hosts: Vec<_> = engine
                .hosts()
                .iter()
                .map(|h| Arc::clone(h.value()))
                .collect();

            if hosts.is_empty() {
                body.push_str("<p><em>No hosts configured.</em></p>");
                continue;
            }

            for host in &hosts {
                body.push_str(&format!(
                    "<h3>Host: {} <small>(app_base: {}, auto_deploy: {}, state: {:?})</small></h3>",
                    escape_html(host.name()),
                    escape_html(&host.app_base().display().to_string()),
                    host.auto_deploy(),
                    host.state(),
                ));

                let contexts: Vec<_> = host
                    .contexts()
                    .iter()
                    .map(|c| Arc::clone(c.value()))
                    .collect();

                if contexts.is_empty() {
                    body.push_str("<p><em>No contexts deployed under this host.</em></p>");
                    continue;
                }

                body.push_str(
                    "<table border=\"1\" cellpadding=\"4\" cellspacing=\"0\">\
                     <thead><tr>\
                     <th>Context Path</th><th>Doc Base</th><th>State</th>\
                     <th>Servlets</th><th>Actions</th>\
                     </tr></thead><tbody>",
                );

                for ctx in &contexts {
                    let display_path = if ctx.path().is_empty() {
                        "/"
                    } else {
                        ctx.path()
                    };
                    let encoded_path = url_encode(ctx.path());
                    let mount = escape_html(&self.mount_path);

                    body.push_str(&format!(
                        "<tr>\
                         <td>{path}</td>\
                         <td>{doc_base}</td>\
                         <td>{state:?}</td>\
                         <td>{servlets}</td>\
                         <td>\
                           <form method=\"post\" action=\"{mount}/start?context={enc}\" style=\"display:inline\">\
                             <button type=\"submit\">start</button>\
                           </form> \
                           <form method=\"post\" action=\"{mount}/stop?context={enc}\" style=\"display:inline\">\
                             <button type=\"submit\">stop</button>\
                           </form> \
                           <form method=\"post\" action=\"{mount}/reload?context={enc}\" style=\"display:inline\">\
                             <button type=\"submit\">reload</button>\
                           </form> \
                           <form method=\"post\" action=\"{mount}/undeploy?path={enc}\" style=\"display:inline\">\
                             <button type=\"submit\">undeploy</button>\
                           </form>\
                         </td>\
                         </tr>",
                        path = escape_html(display_path),
                        doc_base = escape_html(&ctx.doc_base().display().to_string()),
                        state = ctx.state(),
                        servlets = ctx.wrappers().len(),
                        enc = encoded_path,
                    ));
                }

                body.push_str("</tbody></table>");
            }
        }

        body.push_str(PAGE_FOOT);
        body
    }
}

/// Minimal, dependency-free HTML escaper for the five XML-reserved characters.
///
/// Sufficient for the manager page: host names, context paths and document
/// bases are operator-controlled, but they can still contain `&` or `<` and
/// must not break the markup.
fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Minimal `application/x-www-form-urlencoded`-style encoder for context
/// paths embedded in query strings. Encodes everything outside the unreserved
/// set so the value round-trips through
/// [`crate::manager::parse_query`](crate::manager).
fn url_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    out
}

const PAGE_HEAD: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<title>Tomcat-RS Manager</title>
<style>
  body { font-family: -apple-system, system-ui, sans-serif; margin: 1.5rem; }
  h1 { margin-top: 0; }
  table { border-collapse: collapse; margin-bottom: 1rem; }
  th, td { text-align: left; padding: 4px 8px; }
  small { color: #555; font-weight: normal; }
  form button { cursor: pointer; }
</style>
</head>
<body>
"#;

const PAGE_FOOT: &str = "</body></html>\n";

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    use tomcatrs_config::{
        ConnectorConfig, ContextConfig, EngineConfig, HostConfig, Protocol, RequestLimits,
        ServerConfig, ServiceConfig,
    };

    /// A server fixture with one host (`localhost`) and one context (`/app`),
    /// so rendering has something to display.
    fn server_with_app() -> Arc<Server> {
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

    #[test]
    fn html_contains_host_and_context_names() {
        let page = ManagerHtmlPage::new(server_with_app(), "/manager");
        let html = page.render_html();

        assert!(html.contains("Tomcat-RS Manager"), "header missing");
        assert!(html.contains("Service: Catalina"), "service header missing");
        assert!(html.contains("Host: localhost"), "host name missing");
        assert!(html.contains("/app"), "context path missing");
        assert!(
            html.contains("/manager/start?context=%2Fapp"),
            "start form action missing or wrong: {html}"
        );
        assert!(
            html.contains("/manager/stop?context=%2Fapp"),
            "stop form action missing or wrong"
        );
        assert!(
            html.contains("/manager/reload?context=%2Fapp"),
            "reload form action missing or wrong"
        );
        assert!(
            html.contains("/manager/undeploy?path=%2Fapp"),
            "undeploy form action missing or wrong"
        );
        assert!(html.contains("/manager/list"), "JSON list link missing");
    }

    #[test]
    fn render_returns_html_response_with_correct_headers() {
        let page = ManagerHtmlPage::new(server_with_app(), "/manager");
        let resp = page.render();

        assert_eq!(resp.status, 200);
        assert_eq!(
            resp.header("Content-Type"),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(resp.header("Cache-Control"), Some("no-store"));
        let body = std::str::from_utf8(&resp.body).unwrap();
        assert!(body.contains("<!doctype html>"));
        assert!(body.contains("</html>"));
    }

    #[test]
    fn html_escapes_special_characters() {
        assert_eq!(escape_html("a & b < c > d"), "a &amp; b &lt; c &gt; d");
        assert_eq!(escape_html("\"hi\""), "&quot;hi&quot;");
        assert_eq!(escape_html("'x'"), "&#39;x&#39;");
    }

    #[test]
    fn url_encode_preserves_unreserved_and_encodes_slash() {
        assert_eq!(url_encode("/app"), "%2Fapp");
        assert_eq!(url_encode("foo-bar_1.0~"), "foo-bar_1.0~");
        assert_eq!(url_encode("a b"), "a%20b");
    }
}
