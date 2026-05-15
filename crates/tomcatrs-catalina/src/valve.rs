//! The **Valve** abstraction — Tomcat's request-interception primitive.
//!
//! In Apache Tomcat every container ([`Engine`](crate::Engine),
//! [`Host`](crate::Host), [`Context`](crate::Context),
//! [`Wrapper`](crate::Wrapper)) owns a *pipeline* of [`Valve`]s. A request
//! entering a container is passed through that container's valves in order;
//! the last valve in a pipeline — the "basic" valve — is the one that hands
//! the request down to the next container (or, for a [`Wrapper`](crate::Wrapper), finally
//! invokes the servlet).
//!
//! This module ports `org.apache.catalina.Valve` and the standard valve
//! implementations. A valve receives a [`ValveContext`] (the mutable
//! per-request state) and a [`NextValve`] cursor; it does its work, then calls
//! [`NextValve::invoke`] to continue the chain. A valve that does *not* call
//! `next` short-circuits the pipeline — exactly how Tomcat's
//! `RemoteAddrValve` rejects a banned client without ever reaching the
//! servlet.
//!
//! The pipeline machinery that threads valves together lives in
//! [`crate::pipeline`].

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use tomcatrs_coyote::{Request, Response};

use crate::mapper::MappingResult;

/// The mutable per-request state threaded through a pipeline.
///
/// A `ValveContext` borrows the inbound [`Request`] immutably and the outbound
/// [`Response`] mutably for the duration of a single pipeline traversal. It
/// also optionally carries the [`MappingResult`] produced by the
/// [`Mapper`](crate::Mapper), so valves deeper in the tree (the host, context,
/// and wrapper valves) can see which container claimed the request.
pub struct ValveContext<'a> {
    /// The parsed, normalized inbound request.
    pub request: &'a Request,
    /// The response being assembled. Valves mutate this in place.
    pub response: &'a mut Response,
    /// The routing decision for this request, once a mapper has resolved it.
    ///
    /// `None` before mapping has run (e.g. in the engine valve); populated by
    /// the time the request reaches the host/context/wrapper valves.
    pub mapping: Option<MappingResult>,
}

impl<'a> ValveContext<'a> {
    /// Create a context for a request/response pair with no mapping yet.
    pub fn new(request: &'a Request, response: &'a mut Response) -> Self {
        Self {
            request,
            response,
            mapping: None,
        }
    }

    /// Create a context that already carries a [`MappingResult`].
    pub fn with_mapping(
        request: &'a Request,
        response: &'a mut Response,
        mapping: MappingResult,
    ) -> Self {
        Self {
            request,
            response,
            mapping: Some(mapping),
        }
    }
}

/// A cursor over the *remaining* valves in a pipeline.
///
/// A `NextValve` is handed to each [`Valve::invoke`] call. Invoking it runs
/// the next valve in the chain, passing along a freshly narrowed cursor; when
/// the ordinary valves are exhausted it runs the pipeline's terminal "basic"
/// valve exactly once. Calling [`NextValve::invoke`] zero times short-circuits
/// the rest of the pipeline — the canonical way a valve rejects or fully
/// handles a request on its own.
///
/// The basic valve is itself an ordinary [`Valve`], so it too receives a
/// `NextValve`; that cursor is *exhausted* (it runs nothing), which makes the
/// basic valve a true terminal even if it forwards to `next` out of habit.
pub struct NextValve<'a> {
    /// The valves not yet visited, in pipeline order.
    remaining: &'a [Arc<dyn Valve>],
    /// The terminal "basic" valve, run after `remaining` is empty — unless
    /// `basic_done` is already set, in which case the cursor is exhausted.
    basic: &'a Arc<dyn Valve>,
    /// Whether the basic valve has already been dispatched. Guards against the
    /// basic valve recursing into itself when it calls `next.invoke`.
    basic_done: bool,
}

impl<'a> NextValve<'a> {
    /// Construct a cursor over `remaining` valves terminating in `basic`.
    ///
    /// This is `pub(crate)` because only [`crate::pipeline::StandardPipeline`]
    /// is meant to originate a pipeline traversal; valves merely forward the
    /// cursor they were given.
    pub(crate) fn new(remaining: &'a [Arc<dyn Valve>], basic: &'a Arc<dyn Valve>) -> Self {
        Self {
            remaining,
            basic,
            basic_done: false,
        }
    }

    /// Run the next valve in the chain, or the basic valve if none remain.
    ///
    /// Once every ordinary valve *and* the basic valve have run, the cursor is
    /// exhausted and further calls are a no-op returning `Ok(())`.
    pub async fn invoke(self, ctx: &mut ValveContext<'_>) -> tomcatrs_core::Result<()> {
        match self.remaining.split_first() {
            Some((head, tail)) => {
                let next = NextValve {
                    remaining: tail,
                    basic: self.basic,
                    basic_done: self.basic_done,
                };
                head.invoke(ctx, next).await
            }
            None if !self.basic_done => {
                // Dispatch the terminal basic valve. It receives an exhausted
                // cursor (`basic_done = true`) so that if it forwards to
                // `next` the traversal simply ends instead of recursing.
                let next = NextValve {
                    remaining: &[],
                    basic: self.basic,
                    basic_done: true,
                };
                self.basic.invoke(ctx, next).await
            }
            None => {
                // Cursor fully exhausted: nothing left to run.
                Ok(())
            }
        }
    }
}

/// A request-processing stage in a container's pipeline.
///
/// Implementations are shared (`Arc<dyn Valve>`) and therefore must be
/// `Send + Sync`; all per-request state lives in the [`ValveContext`], never
/// in the valve itself.
#[async_trait]
pub trait Valve: Send + Sync {
    /// A short, stable name for this valve, used in logs and diagnostics.
    fn name(&self) -> &str;

    /// Process the request, then call `next.invoke(...)` to continue the chain.
    ///
    /// A valve that returns without calling `next` short-circuits the rest of
    /// the pipeline.
    async fn invoke(
        &self,
        ctx: &mut ValveContext<'_>,
        next: NextValve<'_>,
    ) -> tomcatrs_core::Result<()>;
}

// ---------------------------------------------------------------------------
// AccessLogValve
// ---------------------------------------------------------------------------

/// Records request timing and outcome, the port of
/// `org.apache.catalina.valves.AccessLogValve`.
///
/// It is normally the *first* valve in the engine pipeline so it can time the
/// entire downstream chain. After `next` returns it formats a log line —
/// roughly the Common Log Format — and emits it at `INFO` via [`tracing`].
#[derive(Debug, Default)]
pub struct AccessLogValve {
    name: String,
}

impl AccessLogValve {
    /// Create an access-log valve named `"AccessLogValve"`.
    pub fn new() -> Self {
        Self {
            name: "AccessLogValve".to_string(),
        }
    }

    /// Format a single access-log line for a finished request.
    ///
    /// Exposed (and `pub`) so tests and callers can assert on the exact text
    /// without having to capture `tracing` output.
    pub fn format_line(req: &Request, res: &Response, elapsed_ms: u128) -> String {
        format!(
            "{} \"{} {} {}\" {} {} {}ms",
            req.peer_addr.ip(),
            req.method,
            req.uri,
            req.version,
            res.status,
            res.body.len(),
            elapsed_ms,
        )
    }
}

#[async_trait]
impl Valve for AccessLogValve {
    fn name(&self) -> &str {
        &self.name
    }

    async fn invoke(
        &self,
        ctx: &mut ValveContext<'_>,
        next: NextValve<'_>,
    ) -> tomcatrs_core::Result<()> {
        let started = Instant::now();
        // Snapshot what we need from the request before the borrow is reused.
        let result = next.invoke(ctx).await;
        let elapsed = started.elapsed().as_millis();
        let line = AccessLogValve::format_line(ctx.request, ctx.response, elapsed);
        tracing::info!(target: "tomcatrs::access_log", "{line}");
        result
    }
}

// ---------------------------------------------------------------------------
// ErrorReportValve
// ---------------------------------------------------------------------------

/// Generates a simple HTML error page for failed requests with empty bodies.
///
/// The port of `org.apache.catalina.valves.ErrorReportValve`. It runs the rest
/// of the pipeline first, then — if the response is a 4xx/5xx **and** nothing
/// downstream produced a body — fills in a minimal HTML page and sets
/// `Content-Type: text/html`.
#[derive(Debug, Default)]
pub struct ErrorReportValve {
    name: String,
}

impl ErrorReportValve {
    /// Create an error-report valve named `"ErrorReportValve"`.
    pub fn new() -> Self {
        Self {
            name: "ErrorReportValve".to_string(),
        }
    }

    /// The human-readable reason phrase for a status code.
    fn reason_phrase(status: u16) -> &'static str {
        match status {
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            408 => "Request Timeout",
            500 => "Internal Server Error",
            501 => "Not Implemented",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            504 => "Gateway Timeout",
            _ if (400..500).contains(&status) => "Client Error",
            _ if (500..600).contains(&status) => "Server Error",
            _ => "Error",
        }
    }

    /// Render the canonical HTML error page for `status`.
    pub fn render_page(status: u16) -> String {
        let reason = Self::reason_phrase(status);
        format!(
            "<!DOCTYPE html>\n<html>\n<head><title>HTTP Status {status} \u{2013} {reason}</title></head>\n\
             <body>\n<h1>HTTP Status {status} \u{2013} {reason}</h1>\n<hr/>\n\
             <p>Tomcat-RS Compatibility Runtime</p>\n</body>\n</html>\n"
        )
    }
}

#[async_trait]
impl Valve for ErrorReportValve {
    fn name(&self) -> &str {
        &self.name
    }

    async fn invoke(
        &self,
        ctx: &mut ValveContext<'_>,
        next: NextValve<'_>,
    ) -> tomcatrs_core::Result<()> {
        let result = next.invoke(ctx).await;

        let status = ctx.response.status;
        let is_error = (400..600).contains(&status);
        if is_error && ctx.response.body.is_empty() {
            let page = ErrorReportValve::render_page(status);
            ctx.response
                .set_header("Content-Type", "text/html;charset=UTF-8");
            ctx.response.body = page.into_bytes().into();
            tracing::debug!(
                target: "tomcatrs::valve",
                status,
                "ErrorReportValve generated an error page"
            );
        }
        result
    }
}

// ---------------------------------------------------------------------------
// Container valves: Engine / Host / Context / Wrapper
// ---------------------------------------------------------------------------

/// The standard *basic* valve for an [`Engine`](crate::Engine) pipeline.
///
/// In Tomcat (`StandardEngineValve`) this valve selects the [`Host`](crate::Host) for the
/// request and delegates into the host's pipeline. This port keeps the
/// logging-and-delegate shape; host selection is performed by the
/// [`Mapper`](crate::Mapper) and surfaced via [`ValveContext::mapping`].
#[derive(Debug, Default)]
pub struct StandardEngineValve {
    name: String,
}

impl StandardEngineValve {
    /// Create the engine basic valve.
    pub fn new() -> Self {
        Self {
            name: "StandardEngineValve".to_string(),
        }
    }
}

#[async_trait]
impl Valve for StandardEngineValve {
    fn name(&self) -> &str {
        &self.name
    }

    async fn invoke(
        &self,
        ctx: &mut ValveContext<'_>,
        next: NextValve<'_>,
    ) -> tomcatrs_core::Result<()> {
        let host = ctx
            .mapping
            .as_ref()
            .map(|m| m.host.name().to_string())
            .unwrap_or_else(|| "<unmapped>".to_string());
        tracing::debug!(target: "tomcatrs::valve", host = %host, "StandardEngineValve");
        next.invoke(ctx).await
    }
}

/// The standard *basic* valve for a [`Host`](crate::Host) pipeline.
///
/// The port of `StandardHostValve`: logs the selected context and delegates
/// down the chain.
#[derive(Debug, Default)]
pub struct StandardHostValve {
    name: String,
}

impl StandardHostValve {
    /// Create the host basic valve.
    pub fn new() -> Self {
        Self {
            name: "StandardHostValve".to_string(),
        }
    }
}

#[async_trait]
impl Valve for StandardHostValve {
    fn name(&self) -> &str {
        &self.name
    }

    async fn invoke(
        &self,
        ctx: &mut ValveContext<'_>,
        next: NextValve<'_>,
    ) -> tomcatrs_core::Result<()> {
        let context = ctx
            .mapping
            .as_ref()
            .map(|m| m.context.path().to_string())
            .unwrap_or_else(|| "<unmapped>".to_string());
        tracing::debug!(target: "tomcatrs::valve", context = %context, "StandardHostValve");
        next.invoke(ctx).await
    }
}

/// The standard *basic* valve for a [`Context`](crate::Context) pipeline.
///
/// The port of `StandardContextValve`: logs the selected wrapper and delegates
/// down the chain.
#[derive(Debug, Default)]
pub struct StandardContextValve {
    name: String,
}

impl StandardContextValve {
    /// Create the context basic valve.
    pub fn new() -> Self {
        Self {
            name: "StandardContextValve".to_string(),
        }
    }
}

#[async_trait]
impl Valve for StandardContextValve {
    fn name(&self) -> &str {
        &self.name
    }

    async fn invoke(
        &self,
        ctx: &mut ValveContext<'_>,
        next: NextValve<'_>,
    ) -> tomcatrs_core::Result<()> {
        let servlet = ctx
            .mapping
            .as_ref()
            .map(|m| m.wrapper.servlet_name().to_string())
            .unwrap_or_else(|| "<unmapped>".to_string());
        tracing::debug!(target: "tomcatrs::valve", servlet = %servlet, "StandardContextValve");
        next.invoke(ctx).await
    }
}

/// The standard *basic* valve for a [`Wrapper`](crate::Wrapper) pipeline.
///
/// The port of `StandardWrapperValve`. This is the natural **terminal** point
/// of the whole nested-pipeline traversal: in a complete runtime it would
/// allocate a servlet instance, run the filter chain, and invoke
/// `Servlet::service`. Until the servlet bridge is wired in, it acts as a
/// placeholder — if nothing downstream set a non-default status it marks the
/// response `200 OK`.
#[derive(Debug, Default)]
pub struct StandardWrapperValve {
    name: String,
}

impl StandardWrapperValve {
    /// Create the wrapper basic valve.
    pub fn new() -> Self {
        Self {
            name: "StandardWrapperValve".to_string(),
        }
    }
}

#[async_trait]
impl Valve for StandardWrapperValve {
    fn name(&self) -> &str {
        &self.name
    }

    async fn invoke(
        &self,
        ctx: &mut ValveContext<'_>,
        next: NextValve<'_>,
    ) -> tomcatrs_core::Result<()> {
        let servlet = ctx
            .mapping
            .as_ref()
            .map(|m| m.wrapper.servlet_name().to_string())
            .unwrap_or_else(|| "<unmapped>".to_string());
        tracing::debug!(
            target: "tomcatrs::valve",
            servlet = %servlet,
            "StandardWrapperValve (terminal)"
        );

        // Placeholder servlet invocation: if no downstream component (there is
        // none yet) handled the request, default to 200 OK. We treat a 0
        // status as "untouched" since `Response::new(0)` is never produced by
        // the connector layer.
        if ctx.response.status == 0 {
            ctx.response.status = 200;
        }

        // The wrapper valve is terminal; calling `next` here would just run an
        // empty cursor, but we forward it for uniformity in case a pipeline
        // installs a different basic valve after it.
        next.invoke(ctx).await
    }
}

// ---------------------------------------------------------------------------
// RemoteAddrValve
// ---------------------------------------------------------------------------

/// Allow/deny access by client IP address — the port of
/// `org.apache.catalina.valves.RemoteAddrValve`.
///
/// The matching rules mirror Tomcat's precedence:
///
/// 1. If a **deny** set is configured and the client IP is in it, the request
///    is rejected with `403 Forbidden` and the pipeline is short-circuited.
/// 2. Otherwise, if an **allow** set is configured, the request proceeds only
///    when the client IP is in it; anything else is rejected with `403`.
/// 3. If neither set is configured, every request is allowed.
///
/// Unlike Tomcat, which matches against regular expressions, this port matches
/// exact [`IpAddr`] values — simple, allocation-free, and sufficient for the
/// compatibility runtime's needs.
#[derive(Debug, Default)]
pub struct RemoteAddrValve {
    name: String,
    allow: Option<HashSet<IpAddr>>,
    deny: Option<HashSet<IpAddr>>,
}

impl RemoteAddrValve {
    /// Create a valve with neither an allow nor a deny list (allows all).
    pub fn new() -> Self {
        Self {
            name: "RemoteAddrValve".to_string(),
            allow: None,
            deny: None,
        }
    }

    /// Configure the set of IPs explicitly permitted.
    ///
    /// Once an allow set is present, any client IP *not* in it is rejected.
    pub fn allow(mut self, ips: impl IntoIterator<Item = IpAddr>) -> Self {
        self.allow = Some(ips.into_iter().collect());
        self
    }

    /// Configure the set of IPs explicitly rejected.
    pub fn deny(mut self, ips: impl IntoIterator<Item = IpAddr>) -> Self {
        self.deny = Some(ips.into_iter().collect());
        self
    }

    /// Decide whether `ip` is permitted under the configured rules.
    pub fn permits(&self, ip: &IpAddr) -> bool {
        if let Some(deny) = &self.deny {
            if deny.contains(ip) {
                return false;
            }
        }
        if let Some(allow) = &self.allow {
            return allow.contains(ip);
        }
        true
    }
}

#[async_trait]
impl Valve for RemoteAddrValve {
    fn name(&self) -> &str {
        &self.name
    }

    async fn invoke(
        &self,
        ctx: &mut ValveContext<'_>,
        next: NextValve<'_>,
    ) -> tomcatrs_core::Result<()> {
        let ip = ctx.request.peer_addr.ip();
        if self.permits(&ip) {
            next.invoke(ctx).await
        } else {
            // Reject: short-circuit the pipeline by not calling `next`.
            tracing::warn!(target: "tomcatrs::valve", %ip, "RemoteAddrValve denied request");
            ctx.response.status = 403;
            ctx.response.body = tomcatrs_coyote::Response::new(403).body;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::net::SocketAddr;

    // `Request::body` / `Response::body` are `bytes::Bytes`, a crate we do not
    // depend on directly. The tests never need to *name* that type: an empty
    // body comes from `Response::new(_).body`, and a non-empty one from a
    // `Response::with_body(..)`'s `body` field.

    fn test_request(ip: &str) -> Request {
        let addr: SocketAddr = format!("{ip}:40000").parse().unwrap();
        Request {
            method: "GET".into(),
            uri: "/test".into(),
            path: "/test".into(),
            query: None,
            version: "HTTP/1.1".into(),
            headers: vec![("Host".into(), "localhost".into())],
            body: Response::new(0).body,
            peer_addr: addr,
        }
    }

    /// A valve that appends its label to a shared log, then continues.
    struct RecordingValve {
        label: &'static str,
        log: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl Valve for RecordingValve {
        fn name(&self) -> &str {
            self.label
        }

        async fn invoke(
            &self,
            ctx: &mut ValveContext<'_>,
            next: NextValve<'_>,
        ) -> tomcatrs_core::Result<()> {
            self.log.lock().push(self.label);
            next.invoke(ctx).await
        }
    }

    /// A terminal valve that records that it ran and sets a 200.
    struct TerminalValve {
        log: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl Valve for TerminalValve {
        fn name(&self) -> &str {
            "terminal"
        }

        async fn invoke(
            &self,
            ctx: &mut ValveContext<'_>,
            _next: NextValve<'_>,
        ) -> tomcatrs_core::Result<()> {
            self.log.lock().push("terminal");
            ctx.response.status = 200;
            Ok(())
        }
    }

    #[tokio::test]
    async fn pipeline_of_three_valves_runs_in_order_and_reaches_terminal() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let valves: Vec<Arc<dyn Valve>> = vec![
            Arc::new(RecordingValve {
                label: "a",
                log: Arc::clone(&log),
            }),
            Arc::new(RecordingValve {
                label: "b",
                log: Arc::clone(&log),
            }),
            Arc::new(RecordingValve {
                label: "c",
                log: Arc::clone(&log),
            }),
        ];
        let basic: Arc<dyn Valve> = Arc::new(TerminalValve {
            log: Arc::clone(&log),
        });

        let req = test_request("127.0.0.1");
        let mut res = Response::new(0);
        let mut ctx = ValveContext::new(&req, &mut res);

        let next = NextValve::new(&valves, &basic);
        next.invoke(&mut ctx).await.unwrap();

        assert_eq!(&*log.lock(), &["a", "b", "c", "terminal"]);
        assert_eq!(res.status, 200);
    }

    #[tokio::test]
    async fn remote_addr_valve_allows_listed_ip() {
        let valve = RemoteAddrValve::new().allow(["10.0.0.1".parse().unwrap()]);
        assert!(valve.permits(&"10.0.0.1".parse().unwrap()));
        assert!(!valve.permits(&"10.0.0.2".parse().unwrap()));

        let basic: Arc<dyn Valve> = Arc::new(StandardWrapperValve::new());
        let req = test_request("10.0.0.1");
        let mut res = Response::new(0);
        let mut ctx = ValveContext::new(&req, &mut res);
        let next = NextValve::new(&[], &basic);
        valve.invoke(&mut ctx, next).await.unwrap();
        // Allowed: the terminal wrapper valve ran and set 200.
        assert_eq!(res.status, 200);
    }

    #[tokio::test]
    async fn remote_addr_valve_denies_listed_ip() {
        let valve = RemoteAddrValve::new().deny(["192.168.1.5".parse().unwrap()]);
        assert!(!valve.permits(&"192.168.1.5".parse().unwrap()));
        assert!(valve.permits(&"192.168.1.6".parse().unwrap()));

        let basic: Arc<dyn Valve> = Arc::new(StandardWrapperValve::new());
        let req = test_request("192.168.1.5");
        let mut res = Response::new(0);
        let mut ctx = ValveContext::new(&req, &mut res);
        let next = NextValve::new(&[], &basic);
        valve.invoke(&mut ctx, next).await.unwrap();
        // Denied: short-circuited with 403, terminal valve never ran.
        assert_eq!(res.status, 403);
    }

    #[tokio::test]
    async fn remote_addr_valve_allows_all_when_unconfigured() {
        let valve = RemoteAddrValve::new();
        assert!(valve.permits(&"8.8.8.8".parse().unwrap()));
    }

    #[tokio::test]
    async fn error_report_valve_fills_body_on_500() {
        let valve = ErrorReportValve::new();

        // A basic valve that produces a bare 500 with no body.
        struct Fail;
        #[async_trait]
        impl Valve for Fail {
            fn name(&self) -> &str {
                "fail"
            }
            async fn invoke(
                &self,
                ctx: &mut ValveContext<'_>,
                _next: NextValve<'_>,
            ) -> tomcatrs_core::Result<()> {
                ctx.response.status = 500;
                Ok(())
            }
        }

        let basic: Arc<dyn Valve> = Arc::new(Fail);
        let req = test_request("127.0.0.1");
        let mut res = Response::new(0);
        let mut ctx = ValveContext::new(&req, &mut res);
        let next = NextValve::new(&[], &basic);
        valve.invoke(&mut ctx, next).await.unwrap();

        assert_eq!(res.status, 500);
        assert!(!res.body.is_empty());
        let body = String::from_utf8(res.body.to_vec()).unwrap();
        assert!(body.contains("HTTP Status 500"));
        assert!(body.contains("Internal Server Error"));
        assert_eq!(res.header("Content-Type"), Some("text/html;charset=UTF-8"));
    }

    #[tokio::test]
    async fn error_report_valve_leaves_existing_body_untouched() {
        let valve = ErrorReportValve::new();
        struct FailWithBody;
        #[async_trait]
        impl Valve for FailWithBody {
            fn name(&self) -> &str {
                "fail-body"
            }
            async fn invoke(
                &self,
                ctx: &mut ValveContext<'_>,
                _next: NextValve<'_>,
            ) -> tomcatrs_core::Result<()> {
                ctx.response.status = 404;
                ctx.response.body = Response::with_body(404, "custom not found").body;
                Ok(())
            }
        }
        let basic: Arc<dyn Valve> = Arc::new(FailWithBody);
        let req = test_request("127.0.0.1");
        let mut res = Response::new(0);
        let mut ctx = ValveContext::new(&req, &mut res);
        let next = NextValve::new(&[], &basic);
        valve.invoke(&mut ctx, next).await.unwrap();
        assert_eq!(&res.body[..], b"custom not found");
    }

    #[test]
    fn access_log_line_has_expected_shape() {
        let req = test_request("203.0.113.7");
        let res = Response::with_body(200, "hello");
        let line = AccessLogValve::format_line(&req, &res, 3);
        assert!(line.starts_with("203.0.113.7 \"GET /test HTTP/1.1\" 200 5 "));
        assert!(line.ends_with("3ms"));
    }
}
