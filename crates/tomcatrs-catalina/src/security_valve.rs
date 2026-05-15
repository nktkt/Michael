//! Security-oriented valves: response-header hardening and an HTTP-method
//! allow-list.
//!
//! These two valves complement [`crate::valve::RemoteAddrValve`] by tightening
//! the *content* of the response and the *shape* of the request. Both
//! implement the shared [`crate::valve::Valve`] trait so they slot directly
//! into any container pipeline.
//!
//! * [`SecurityHeadersValve`] runs after the basic valve has produced a
//!   response and adds the standard hardening headers — HSTS, the MIME-sniff
//!   guard, frame-options, a referrer policy, and a Content-Security-Policy —
//!   only setting each header when the downstream component has not already
//!   supplied its own value, so application overrides win.
//! * [`HttpMethodFilterValve`] short-circuits the pipeline with `405 Method
//!   Not Allowed` when the request method is not in the configured allow-list.
//!   `Allow:` is set to the configured list so the response is RFC-compliant.

use async_trait::async_trait;

use crate::valve::{NextValve, Valve, ValveContext};

// ---------------------------------------------------------------------------
// SecurityHeadersValve
// ---------------------------------------------------------------------------

/// Configuration for [`SecurityHeadersValve`].
///
/// Every field is the *value* of the corresponding response header. Setting a
/// field to `None` disables that header entirely. The defaults reflect the
/// OWASP "secure headers" baseline as of 2024.
#[derive(Debug, Clone)]
pub struct SecurityHeadersConfig {
    /// `Strict-Transport-Security` value. `None` disables HSTS emission.
    pub hsts: Option<String>,
    /// `X-Content-Type-Options` value (typically `nosniff`).
    pub x_content_type_options: Option<String>,
    /// `X-Frame-Options` value (typically `DENY` or `SAMEORIGIN`).
    pub x_frame_options: Option<String>,
    /// `Referrer-Policy` value.
    pub referrer_policy: Option<String>,
    /// `Content-Security-Policy` value.
    pub content_security_policy: Option<String>,
}

impl Default for SecurityHeadersConfig {
    fn default() -> Self {
        Self {
            // One year, include subdomains. No `preload` by default — that is
            // an opt-in commitment to the HSTS preload list.
            hsts: Some("max-age=31536000; includeSubDomains".into()),
            x_content_type_options: Some("nosniff".into()),
            x_frame_options: Some("DENY".into()),
            referrer_policy: Some("same-origin".into()),
            // A restrictive baseline. Applications that need scripts or styles
            // from third parties override via `with_content_security_policy`.
            content_security_policy: Some("default-src 'self'".into()),
        }
    }
}

impl SecurityHeadersConfig {
    /// Construct a fresh config with the secure defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the `Strict-Transport-Security` value; pass `None` to drop it.
    pub fn with_hsts(mut self, value: Option<String>) -> Self {
        self.hsts = value;
        self
    }

    /// Override the `X-Content-Type-Options` value.
    pub fn with_x_content_type_options(mut self, value: Option<String>) -> Self {
        self.x_content_type_options = value;
        self
    }

    /// Override the `X-Frame-Options` value (`DENY` / `SAMEORIGIN` / `None`).
    pub fn with_x_frame_options(mut self, value: Option<String>) -> Self {
        self.x_frame_options = value;
        self
    }

    /// Override the `Referrer-Policy` value.
    pub fn with_referrer_policy(mut self, value: Option<String>) -> Self {
        self.referrer_policy = value;
        self
    }

    /// Override the `Content-Security-Policy` value.
    pub fn with_content_security_policy(mut self, value: Option<String>) -> Self {
        self.content_security_policy = value;
        self
    }
}

/// Adds the standard security-hardening response headers after the rest of
/// the pipeline has run.
///
/// Each header is only added when the downstream component has not already
/// supplied its own value — applications that set, for example, a stricter
/// `Content-Security-Policy` keep their value.
#[derive(Debug, Default)]
pub struct SecurityHeadersValve {
    name: String,
    config: SecurityHeadersConfig,
}

impl SecurityHeadersValve {
    /// Create a valve named `"SecurityHeadersValve"` with default configuration.
    pub fn new() -> Self {
        Self {
            name: "SecurityHeadersValve".to_string(),
            config: SecurityHeadersConfig::default(),
        }
    }

    /// Create a valve with a caller-supplied configuration.
    pub fn with_config(config: SecurityHeadersConfig) -> Self {
        Self {
            name: "SecurityHeadersValve".to_string(),
            config,
        }
    }

    /// Borrow the configured headers.
    pub fn config(&self) -> &SecurityHeadersConfig {
        &self.config
    }

    /// Apply the configured headers to `response`, leaving any header an
    /// upstream component already set in place.
    fn apply_headers(&self, response: &mut tomcatrs_coyote::Response) {
        let set_if_absent = |resp: &mut tomcatrs_coyote::Response, name: &str, value: &str| {
            if resp.header(name).is_none() {
                resp.set_header(name, value);
            }
        };

        if let Some(v) = &self.config.hsts {
            set_if_absent(response, "Strict-Transport-Security", v);
        }
        if let Some(v) = &self.config.x_content_type_options {
            set_if_absent(response, "X-Content-Type-Options", v);
        }
        if let Some(v) = &self.config.x_frame_options {
            set_if_absent(response, "X-Frame-Options", v);
        }
        if let Some(v) = &self.config.referrer_policy {
            set_if_absent(response, "Referrer-Policy", v);
        }
        if let Some(v) = &self.config.content_security_policy {
            set_if_absent(response, "Content-Security-Policy", v);
        }
    }
}

#[async_trait]
impl Valve for SecurityHeadersValve {
    fn name(&self) -> &str {
        &self.name
    }

    async fn invoke(
        &self,
        ctx: &mut ValveContext<'_>,
        next: NextValve<'_>,
    ) -> tomcatrs_core::Result<()> {
        let result = next.invoke(ctx).await;
        self.apply_headers(ctx.response);
        result
    }
}

// ---------------------------------------------------------------------------
// HttpMethodFilterValve
// ---------------------------------------------------------------------------

/// Reject any request whose method is not in a configured allow-list.
///
/// A non-matching request short-circuits the pipeline with `405 Method Not
/// Allowed` and an `Allow:` header listing the permitted methods, per
/// [RFC 9110 §15.5.6](https://www.rfc-editor.org/rfc/rfc9110#section-15.5.6).
#[derive(Debug)]
pub struct HttpMethodFilterValve {
    name: String,
    allowed: Vec<String>,
}

impl HttpMethodFilterValve {
    /// Create a filter that allows only the given methods (case-insensitive).
    ///
    /// Methods are normalised to upper-case on insertion so the `Allow:`
    /// header always shows a canonical form.
    pub fn new(methods: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let allowed: Vec<String> = methods
            .into_iter()
            .map(|m| m.into().to_ascii_uppercase())
            .collect();
        Self {
            name: "HttpMethodFilterValve".to_string(),
            allowed,
        }
    }

    /// Returns `true` if `method` is in the allow-list (case-insensitive).
    pub fn permits(&self, method: &str) -> bool {
        let m = method.to_ascii_uppercase();
        self.allowed.iter().any(|a| a == &m)
    }

    /// The permitted methods, joined by `, ` for use in an `Allow:` header.
    pub fn allow_header_value(&self) -> String {
        self.allowed.join(", ")
    }
}

impl Default for HttpMethodFilterValve {
    fn default() -> Self {
        // Conservative baseline: only the methods a static-content servlet
        // needs. Applications mounting a REST API override via `new(...)`.
        Self::new(["GET", "HEAD", "OPTIONS"])
    }
}

#[async_trait]
impl Valve for HttpMethodFilterValve {
    fn name(&self) -> &str {
        &self.name
    }

    async fn invoke(
        &self,
        ctx: &mut ValveContext<'_>,
        next: NextValve<'_>,
    ) -> tomcatrs_core::Result<()> {
        if self.permits(&ctx.request.method) {
            return next.invoke(ctx).await;
        }
        tracing::warn!(
            target: "tomcatrs::valve",
            method = %ctx.request.method,
            "HttpMethodFilterValve rejected disallowed method"
        );
        ctx.response.status = 405;
        ctx.response.set_header("Allow", &self.allow_header_value());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use async_trait::async_trait;
    use tomcatrs_coyote::{Request, Response};

    use super::*;
    use crate::valve::NextValve;

    fn test_request(method: &str) -> Request {
        let addr: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        Request {
            method: method.into(),
            uri: "/test".into(),
            path: "/test".into(),
            query: None,
            version: "HTTP/1.1".into(),
            headers: vec![("Host".into(), "localhost".into())],
            body: Response::new(0).body,
            peer_addr: addr,
        }
    }

    /// A terminal valve that just sets a 200 — the stand-in for the
    /// downstream pipeline in these tests.
    struct OkBasic;

    #[async_trait]
    impl Valve for OkBasic {
        fn name(&self) -> &str {
            "ok-basic"
        }
        async fn invoke(
            &self,
            ctx: &mut ValveContext<'_>,
            _next: NextValve<'_>,
        ) -> tomcatrs_core::Result<()> {
            ctx.response.status = 200;
            Ok(())
        }
    }

    /// A terminal valve that supplies its own CSP — used to verify the
    /// security-headers valve respects existing values.
    struct OkWithCsp;

    #[async_trait]
    impl Valve for OkWithCsp {
        fn name(&self) -> &str {
            "ok-with-csp"
        }
        async fn invoke(
            &self,
            ctx: &mut ValveContext<'_>,
            _next: NextValve<'_>,
        ) -> tomcatrs_core::Result<()> {
            ctx.response.status = 200;
            ctx.response
                .set_header("Content-Security-Policy", "default-src 'none'");
            Ok(())
        }
    }

    #[tokio::test]
    async fn security_headers_valve_adds_each_default_header() {
        let valve = SecurityHeadersValve::new();
        let basic: Arc<dyn Valve> = Arc::new(OkBasic);
        let req = test_request("GET");
        let mut res = Response::new(0);
        let mut ctx = ValveContext::new(&req, &mut res);
        let next = NextValve::new(&[], &basic);

        valve.invoke(&mut ctx, next).await.unwrap();

        assert_eq!(res.status, 200);
        assert!(res
            .header("Strict-Transport-Security")
            .unwrap()
            .contains("max-age="));
        assert_eq!(res.header("X-Content-Type-Options"), Some("nosniff"));
        assert_eq!(res.header("X-Frame-Options"), Some("DENY"));
        assert_eq!(res.header("Referrer-Policy"), Some("same-origin"));
        assert_eq!(
            res.header("Content-Security-Policy"),
            Some("default-src 'self'")
        );
    }

    #[tokio::test]
    async fn security_headers_valve_honours_downstream_csp_override() {
        let valve = SecurityHeadersValve::new();
        let basic: Arc<dyn Valve> = Arc::new(OkWithCsp);
        let req = test_request("GET");
        let mut res = Response::new(0);
        let mut ctx = ValveContext::new(&req, &mut res);
        let next = NextValve::new(&[], &basic);

        valve.invoke(&mut ctx, next).await.unwrap();

        // Downstream value wins; the default never overwrites it.
        assert_eq!(
            res.header("Content-Security-Policy"),
            Some("default-src 'none'")
        );
        // Other headers are still added.
        assert_eq!(res.header("X-Frame-Options"), Some("DENY"));
    }

    #[tokio::test]
    async fn security_headers_config_overrides_take_effect() {
        let config = SecurityHeadersConfig::new()
            .with_x_frame_options(Some("SAMEORIGIN".into()))
            .with_content_security_policy(Some("script-src 'self' https://cdn.example".into()))
            .with_hsts(None);
        let valve = SecurityHeadersValve::with_config(config);
        let basic: Arc<dyn Valve> = Arc::new(OkBasic);
        let req = test_request("GET");
        let mut res = Response::new(0);
        let mut ctx = ValveContext::new(&req, &mut res);
        let next = NextValve::new(&[], &basic);

        valve.invoke(&mut ctx, next).await.unwrap();

        assert_eq!(res.header("X-Frame-Options"), Some("SAMEORIGIN"));
        assert_eq!(
            res.header("Content-Security-Policy"),
            Some("script-src 'self' https://cdn.example")
        );
        // HSTS was disabled.
        assert!(res.header("Strict-Transport-Security").is_none());
    }

    #[tokio::test]
    async fn method_filter_allows_listed_methods() {
        let valve = HttpMethodFilterValve::new(["GET", "POST"]);
        let basic: Arc<dyn Valve> = Arc::new(OkBasic);
        let req = test_request("GET");
        let mut res = Response::new(0);
        let mut ctx = ValveContext::new(&req, &mut res);
        let next = NextValve::new(&[], &basic);

        valve.invoke(&mut ctx, next).await.unwrap();
        assert_eq!(res.status, 200);
    }

    #[tokio::test]
    async fn method_filter_rejects_delete_when_only_get_post_allowed() {
        let valve = HttpMethodFilterValve::new(["GET", "POST"]);
        let basic: Arc<dyn Valve> = Arc::new(OkBasic);
        let req = test_request("DELETE");
        let mut res = Response::new(0);
        let mut ctx = ValveContext::new(&req, &mut res);
        let next = NextValve::new(&[], &basic);

        valve.invoke(&mut ctx, next).await.unwrap();
        assert_eq!(res.status, 405);
        let allow = res.header("Allow").expect("Allow header set on 405");
        assert!(allow.contains("GET"));
        assert!(allow.contains("POST"));
    }

    #[tokio::test]
    async fn method_filter_is_case_insensitive() {
        let valve = HttpMethodFilterValve::new(["get"]);
        assert!(valve.permits("GET"));
        assert!(valve.permits("get"));
        assert!(!valve.permits("POST"));
    }

    #[tokio::test]
    async fn method_filter_default_allows_safe_methods_only() {
        let valve = HttpMethodFilterValve::default();
        assert!(valve.permits("GET"));
        assert!(valve.permits("HEAD"));
        assert!(valve.permits("OPTIONS"));
        assert!(!valve.permits("POST"));
        assert!(!valve.permits("PUT"));
        assert!(!valve.permits("DELETE"));
    }
}
