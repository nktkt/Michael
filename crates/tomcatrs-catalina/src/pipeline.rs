//! [`StandardPipeline`] — an ordered chain of [`Valve`]s with a terminal
//! "basic" valve.
//!
//! This is the Rust port of `org.apache.catalina.core.StandardPipeline`. Every
//! container ([`Engine`](crate::Engine), [`Host`](crate::Host),
//! [`Context`](crate::Context), [`Wrapper`](crate::Wrapper)) owns one. A
//! request entering the container is run through the pipeline's valves in
//! insertion order; the **basic** valve always runs last and is the component
//! that hands the request onward (down to the next container, or — for a
//! wrapper — to the servlet itself).
//!
//! # Shape
//!
//! ```text
//! add_valve()ed valves, in order        basic valve (set_basic)
//! ┌───────────┐  ┌───────────┐  ┌───────────┐  ┌─────────────────┐
//! │ valve[0]  │->│ valve[1]  │->│ valve[..] │->│ basic (terminal)│
//! └───────────┘  └───────────┘  └───────────┘  └─────────────────┘
//! ```
//!
//! A valve may decline to call its [`NextValve`] cursor, which short-circuits
//! everything after it — that is how, for example, a
//! [`RemoteAddrValve`](crate::valve::RemoteAddrValve) rejects a banned client
//! before the request ever reaches a servlet.

use std::sync::Arc;

use parking_lot::RwLock;
use tomcatrs_coyote::{Request, Response};

use crate::valve::{NextValve, Valve, ValveContext};

/// An ordered chain of valves terminating in a single *basic* valve.
///
/// `StandardPipeline` is `Send + Sync`: the valve list lives behind a
/// [`parking_lot::RwLock`] so valves can be added during configuration and the
/// pipeline can then be shared (`Arc<StandardPipeline>`) and invoked
/// concurrently. A pipeline is usable only once a basic valve has been set
/// with [`set_basic`](Self::set_basic); invoking a pipeline without one is an
/// error.
#[derive(Default)]
pub struct StandardPipeline {
    /// The ordinary valves, in the order they will run.
    valves: RwLock<Vec<Arc<dyn Valve>>>,
    /// The terminal valve, run after every ordinary valve.
    basic: RwLock<Option<Arc<dyn Valve>>>,
}

impl std::fmt::Debug for StandardPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let valves = self.valves.read();
        let basic = self.basic.read();
        f.debug_struct("StandardPipeline")
            .field(
                "valves",
                &valves.iter().map(|v| v.name()).collect::<Vec<_>>(),
            )
            .field("basic", &basic.as_ref().map(|v| v.name()))
            .finish()
    }
}

impl StandardPipeline {
    /// Create an empty pipeline with no valves and no basic valve.
    pub fn new() -> Self {
        Self {
            valves: RwLock::new(Vec::new()),
            basic: RwLock::new(None),
        }
    }

    /// Append `valve` to the end of the ordinary-valve chain.
    ///
    /// Valves run in insertion order, *before* the basic valve.
    pub fn add_valve(&self, valve: Arc<dyn Valve>) {
        self.valves.write().push(valve);
    }

    /// Set (replacing any previous) the terminal *basic* valve.
    ///
    /// The basic valve always runs last and is mandatory before
    /// [`invoke`](Self::invoke) can be called.
    pub fn set_basic(&self, valve: Arc<dyn Valve>) {
        *self.basic.write() = Some(valve);
    }

    /// The names of the ordinary valves, in order — handy for diagnostics.
    pub fn valve_names(&self) -> Vec<String> {
        self.valves
            .read()
            .iter()
            .map(|v| v.name().to_string())
            .collect()
    }

    /// Whether a basic valve has been installed.
    pub fn has_basic(&self) -> bool {
        self.basic.read().is_some()
    }

    /// Run a request through the whole pipeline.
    ///
    /// The ordinary valves run in insertion order, then the basic valve. The
    /// [`Response`] is mutated in place by the valves.
    ///
    /// # Errors
    ///
    /// Returns [`tomcatrs_core::Error::Lifecycle`] if no basic valve has been
    /// set, or propagates any error returned by a valve in the chain.
    pub async fn invoke(&self, req: &Request, res: &mut Response) -> tomcatrs_core::Result<()> {
        let mut ctx = ValveContext::new(req, res);
        self.invoke_with(&mut ctx).await
    }

    /// Run a request through the pipeline using a caller-built [`ValveContext`].
    ///
    /// This is the form the nested-container traversal uses: an outer pipeline
    /// (say, the engine's) can attach a [`MappingResult`](crate::MappingResult)
    /// to the context before handing it to an inner pipeline.
    ///
    /// # Errors
    ///
    /// As [`invoke`](Self::invoke).
    pub async fn invoke_with(&self, ctx: &mut ValveContext<'_>) -> tomcatrs_core::Result<()> {
        // Snapshot the valve list and basic valve. Cloning `Arc`s is cheap and
        // releases the locks immediately, so the borrow does not span the
        // `.await` points of the chain.
        let valves: Vec<Arc<dyn Valve>> = self.valves.read().clone();
        let basic = self.basic.read().clone().ok_or_else(|| {
            tomcatrs_core::Error::lifecycle("StandardPipeline invoked without a basic valve")
        })?;

        let next = NextValve::new(&valves, &basic);
        next.invoke(ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::valve::{NextValve, RemoteAddrValve, Valve, ValveContext};
    use async_trait::async_trait;
    use parking_lot::Mutex;
    use std::net::SocketAddr;

    fn test_request(ip: &str) -> Request {
        let addr: SocketAddr = format!("{ip}:50000").parse().unwrap();
        Request {
            method: "GET".into(),
            uri: "/p".into(),
            path: "/p".into(),
            query: None,
            version: "HTTP/1.1".into(),
            headers: vec![("Host".into(), "localhost".into())],
            body: Response::new(0).body,
            peer_addr: addr,
        }
    }

    /// A valve that records its label, then continues down the chain.
    struct Recording {
        label: &'static str,
        log: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl Valve for Recording {
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

    /// A terminal valve that records it ran and produces a 200.
    struct Terminal {
        log: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl Valve for Terminal {
        fn name(&self) -> &str {
            "basic"
        }
        async fn invoke(
            &self,
            ctx: &mut ValveContext<'_>,
            _next: NextValve<'_>,
        ) -> tomcatrs_core::Result<()> {
            self.log.lock().push("basic");
            ctx.response.status = 200;
            Ok(())
        }
    }

    #[tokio::test]
    async fn pipeline_runs_three_valves_in_order_then_basic() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let pipeline = StandardPipeline::new();
        pipeline.add_valve(Arc::new(Recording {
            label: "first",
            log: Arc::clone(&log),
        }));
        pipeline.add_valve(Arc::new(Recording {
            label: "second",
            log: Arc::clone(&log),
        }));
        pipeline.add_valve(Arc::new(Recording {
            label: "third",
            log: Arc::clone(&log),
        }));
        pipeline.set_basic(Arc::new(Terminal {
            log: Arc::clone(&log),
        }));

        assert_eq!(
            pipeline.valve_names(),
            vec![
                "first".to_string(),
                "second".to_string(),
                "third".to_string()
            ]
        );

        let req = test_request("127.0.0.1");
        let mut res = Response::new(0);
        pipeline.invoke(&req, &mut res).await.unwrap();

        assert_eq!(&*log.lock(), &["first", "second", "third", "basic"]);
        assert_eq!(res.status, 200);
    }

    #[tokio::test]
    async fn pipeline_without_basic_valve_errors() {
        let pipeline = StandardPipeline::new();
        let req = test_request("127.0.0.1");
        let mut res = Response::new(0);
        let err = pipeline.invoke(&req, &mut res).await.unwrap_err();
        assert!(matches!(err, tomcatrs_core::Error::Lifecycle(_)));
    }

    #[tokio::test]
    async fn short_circuiting_valve_skips_the_rest_of_the_pipeline() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let pipeline = StandardPipeline::new();
        // A deny-listed client IP: the RemoteAddrValve must reject before the
        // recording valve or the basic valve run.
        pipeline.add_valve(Arc::new(
            RemoteAddrValve::new().deny(["10.10.10.10".parse().unwrap()]),
        ));
        pipeline.add_valve(Arc::new(Recording {
            label: "should-not-run",
            log: Arc::clone(&log),
        }));
        pipeline.set_basic(Arc::new(Terminal {
            log: Arc::clone(&log),
        }));

        let req = test_request("10.10.10.10");
        let mut res = Response::new(0);
        pipeline.invoke(&req, &mut res).await.unwrap();

        assert_eq!(res.status, 403);
        assert!(log.lock().is_empty(), "downstream valves must not have run");
    }

    #[tokio::test]
    async fn pipeline_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<StandardPipeline>();

        // And actually usable from another task behind an Arc.
        let pipeline = Arc::new(StandardPipeline::new());
        pipeline.set_basic(Arc::new(Terminal {
            log: Arc::new(Mutex::new(Vec::new())),
        }));
        let p = Arc::clone(&pipeline);
        let handle = tokio::spawn(async move {
            let req = test_request("127.0.0.1");
            let mut res = Response::new(0);
            p.invoke(&req, &mut res).await.unwrap();
            res.status
        });
        assert_eq!(handle.await.unwrap(), 200);
    }
}
