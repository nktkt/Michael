//! Embedded-JVM lifecycle: configuration, the [`JvmRuntime`] handle, the
//! per-worker JNI-attached thread pool, and the [`WebappRuntime`] registry.
//!
//! Exactly **one** JVM is created per OS process — the JNI Invocation API
//! forbids more, and a second `JNI_CreateJavaVM` would fail anyway. The
//! [`JvmRuntime`] owns that single `JavaVM` (behind the `jvm` feature), a
//! registry of deployed web applications ([`WebappRuntime`]), and a pool of
//! worker threads each of which is *attached* to the JVM once and stays
//! attached.
//!
//! # The single JNI funnel
//!
//! JNI requires every thread that touches the JVM to be *attached* to it, and
//! attaching/detaching per call is costly. The bridge therefore owns a fixed
//! pool of worker threads that each attach once at start-up and stay attached
//! for their lifetime. All JNI work is dispatched onto that pool through
//! [`JvmRuntime::with_env`] — the single funnel for every JNI call in the
//! crate. Callers hand in a closure `FnOnce(&mut JNIEnv) -> Result<R>`; the
//! closure runs on a worker thread and the result is shuttled back over a
//! oneshot channel.
//!
//! # Two builds, one API
//!
//! * **`--features jvm`** — [`JvmRuntime`] wraps a real `jni::JavaVM`. Requires
//!   a JDK to build (`jni` needs headers) and a `libjvm` at run time.
//! * **default features** — [`JvmRuntime`] is a stub: the type and every method
//!   signature still exist, but [`JvmRuntime::start`] and [`JvmRuntime::with_env`]
//!   return [`tomcatrs_core::Error::Bridge`]. This keeps the rest of the
//!   workspace compilable and testable with no JDK present.

use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use tomcatrs_core::{ContextId, Error, Result};

/// Configuration for the embedded JVM.
#[derive(Debug, Clone)]
pub struct JvmConfig {
    /// The *common* classpath: at minimum the `tomcatrs-bridge.jar` (see
    /// [`crate::BRIDGE_JAR_NAME`]) plus the Jakarta Servlet API jars. Per-webapp
    /// classpaths are layered on top via [`JvmRuntime::register_webapp`].
    pub classpath: Vec<PathBuf>,
    /// Extra raw JVM arguments, e.g. `-Xmx512m`, `-XX:+UseZGC`,
    /// `-Djava.awt.headless=true`. Passed verbatim to `JNI_CreateJavaVM`.
    pub jvm_args: Vec<String>,
    /// Number of worker threads to spin up and attach to the JVM. Each handles
    /// servlet invocations; sizing this is the JVM-side analogue of the
    /// connector's executor pool.
    pub worker_threads: usize,
}

impl Default for JvmConfig {
    /// A minimal config: empty classpath, no extra args, and a worker pool
    /// sized to the host's parallelism.
    fn default() -> Self {
        Self {
            classpath: Vec::new(),
            jvm_args: Vec::new(),
            worker_threads: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
        }
    }
}

impl JvmConfig {
    /// Render [`JvmConfig::classpath`] as a single `-Djava.class.path=` option
    /// using the platform path separator. Shared by both feature paths so the
    /// behaviour is identical and unit-testable without a JDK.
    pub fn classpath_option(&self) -> String {
        let sep = if cfg!(windows) { ';' } else { ':' };
        let joined = self
            .classpath
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(&sep.to_string());
        format!("-Djava.class.path={joined}")
    }
}

// ---------------------------------------------------------------------------
// Webapp-side servlet instance handle.
// ---------------------------------------------------------------------------

/// An opaque handle to a JVM-side servlet (or filter) instance.
///
/// Behind the `jvm` feature this wraps a `jni::objects::GlobalRef` — a
/// JVM-rooted global reference to the materialised `jakarta.servlet.Servlet`
/// object — so the instance survives across JNI calls and worker threads.
/// Without the feature it is a process-unique opaque id, which is enough for
/// the registry plumbing and tests to exercise the same code paths.
#[cfg(feature = "jvm")]
#[derive(Clone)]
pub struct ServletInstanceHandle {
    /// The JVM-rooted global reference to the servlet/filter instance.
    global_ref: jni::objects::GlobalRef,
}

#[cfg(feature = "jvm")]
impl ServletInstanceHandle {
    /// Wrap a freshly-created JVM global reference.
    pub fn new(global_ref: jni::objects::GlobalRef) -> Self {
        Self { global_ref }
    }

    /// Borrow the underlying global reference for use inside a [`JvmRuntime::with_env`]
    /// closure.
    pub fn global_ref(&self) -> &jni::objects::GlobalRef {
        &self.global_ref
    }
}

#[cfg(feature = "jvm")]
impl std::fmt::Debug for ServletInstanceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServletInstanceHandle")
            .field("global_ref", &"<jni::GlobalRef>")
            .finish()
    }
}

/// An opaque handle to a JVM-side servlet (or filter) instance.
///
/// Without the `jvm` feature there is no JVM, so this is a process-unique
/// opaque id. It exists so the registry types and their tests compile and
/// behave identically on the default path.
#[cfg(not(feature = "jvm"))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServletInstanceHandle {
    /// A process-unique opaque id standing in for the JVM-side instance.
    id: u64,
}

#[cfg(not(feature = "jvm"))]
impl ServletInstanceHandle {
    /// Create a placeholder handle with a fresh process-unique id.
    pub fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// The opaque id backing this placeholder handle.
    pub fn id(&self) -> u64 {
        self.id
    }
}

#[cfg(not(feature = "jvm"))]
impl Default for ServletInstanceHandle {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Webapp classloader handle.
// ---------------------------------------------------------------------------

/// An opaque handle to a web application's isolating class loader.
///
/// Behind the `jvm` feature this wraps a `jni::objects::GlobalRef` to the
/// webapp's `java.lang.ClassLoader` (typically a `URLClassLoader` built over
/// the webapp's `WEB-INF/classes` and `WEB-INF/lib/*.jar`). Without the feature
/// it is a unit placeholder so [`WebappRuntime`] has the same shape on both
/// paths.
#[cfg(feature = "jvm")]
#[derive(Clone)]
pub struct ClassLoaderHandle {
    /// The JVM-rooted global reference to the webapp's `ClassLoader`.
    global_ref: jni::objects::GlobalRef,
}

#[cfg(feature = "jvm")]
impl ClassLoaderHandle {
    /// Wrap a JVM global reference to a `java.lang.ClassLoader`.
    pub fn new(global_ref: jni::objects::GlobalRef) -> Self {
        Self { global_ref }
    }

    /// Borrow the underlying global reference for use inside a [`JvmRuntime::with_env`]
    /// closure.
    pub fn global_ref(&self) -> &jni::objects::GlobalRef {
        &self.global_ref
    }
}

#[cfg(feature = "jvm")]
impl std::fmt::Debug for ClassLoaderHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClassLoaderHandle")
            .field("global_ref", &"<jni::GlobalRef>")
            .finish()
    }
}

/// An opaque handle to a web application's isolating class loader.
///
/// Without the `jvm` feature this is a unit placeholder: the classloader
/// module builds the real `URLClassLoader` only when a JVM exists.
#[cfg(not(feature = "jvm"))]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClassLoaderHandle;

#[cfg(not(feature = "jvm"))]
impl ClassLoaderHandle {
    /// Create a placeholder classloader handle.
    pub fn new() -> Self {
        Self
    }
}

// ---------------------------------------------------------------------------
// WebappRuntime — per-context JVM-side state.
// ---------------------------------------------------------------------------

/// The JVM-side runtime state of one deployed web application.
///
/// A `WebappRuntime` owns the webapp's isolating class loader and two
/// registries mapping servlet/filter *names* (as declared in `web.xml` or via
/// annotations) to their materialised JVM-side instance handles. It is created
/// by [`JvmRuntime::register_webapp`]; the classloader module later populates
/// [`WebappRuntime::set_class_loader`] and the registries.
///
/// `WebappRuntime` values live behind `Arc` in the [`JvmRuntime`] registry so
/// the connector, the worker pool, and the JNI callbacks can all share one.
#[derive(Debug)]
pub struct WebappRuntime {
    /// The context id (context path) this webapp is deployed under.
    context_id: ContextId,
    /// The webapp's per-context class path, as registered. The classloader
    /// module builds the JVM-side `URLClassLoader` from this.
    classpath: Vec<PathBuf>,
    /// The webapp's isolating class loader, once built. `None` until the
    /// classloader module calls [`WebappRuntime::set_class_loader`].
    class_loader: std::sync::Mutex<Option<ClassLoaderHandle>>,
    /// servlet-name → JVM-side servlet instance handle.
    servlet_registry: DashMap<String, ServletInstanceHandle>,
    /// filter-name → JVM-side filter instance handle.
    filter_registry: DashMap<String, ServletInstanceHandle>,
}

impl WebappRuntime {
    /// Create the runtime entry for a web application. The class loader is not
    /// built here — [`WebappRuntime::set_class_loader`] is the hook the
    /// classloader module calls once it has constructed the webapp's
    /// `URLClassLoader`.
    pub fn new(context_id: ContextId, classpath: Vec<PathBuf>) -> Self {
        Self {
            context_id,
            classpath,
            class_loader: std::sync::Mutex::new(None),
            servlet_registry: DashMap::new(),
            filter_registry: DashMap::new(),
        }
    }

    /// The context id (context path) this webapp is deployed under.
    pub fn context_id(&self) -> &ContextId {
        &self.context_id
    }

    /// The webapp's registered per-context class path.
    pub fn classpath(&self) -> &[PathBuf] {
        &self.classpath
    }

    /// Install the webapp's isolating class loader. Called by the classloader
    /// module once it has built the JVM-side `URLClassLoader`. Replaces any
    /// previously-installed loader.
    pub fn set_class_loader(&self, class_loader: ClassLoaderHandle) {
        *self
            .class_loader
            .lock()
            .expect("class_loader mutex poisoned") = Some(class_loader);
    }

    /// The webapp's isolating class loader, if it has been built yet.
    pub fn class_loader(&self) -> Option<ClassLoaderHandle> {
        self.class_loader
            .lock()
            .expect("class_loader mutex poisoned")
            .clone()
    }

    /// Whether the webapp's class loader has been built and installed.
    pub fn has_class_loader(&self) -> bool {
        self.class_loader
            .lock()
            .expect("class_loader mutex poisoned")
            .is_some()
    }

    /// Register a JVM-side servlet instance under `servlet_name`. Returns the
    /// previously-registered handle for that name, if any.
    pub fn register_servlet(
        &self,
        servlet_name: impl Into<String>,
        handle: ServletInstanceHandle,
    ) -> Option<ServletInstanceHandle> {
        self.servlet_registry.insert(servlet_name.into(), handle)
    }

    /// Look up the JVM-side instance handle for a servlet by name.
    pub fn servlet(&self, servlet_name: &str) -> Option<ServletInstanceHandle> {
        self.servlet_registry
            .get(servlet_name)
            .map(|h| h.value().clone())
    }

    /// Register a JVM-side filter instance under `filter_name`. Returns the
    /// previously-registered handle for that name, if any.
    pub fn register_filter(
        &self,
        filter_name: impl Into<String>,
        handle: ServletInstanceHandle,
    ) -> Option<ServletInstanceHandle> {
        self.filter_registry.insert(filter_name.into(), handle)
    }

    /// Look up the JVM-side instance handle for a filter by name.
    pub fn filter(&self, filter_name: &str) -> Option<ServletInstanceHandle> {
        self.filter_registry
            .get(filter_name)
            .map(|h| h.value().clone())
    }

    /// Number of servlets registered for this webapp.
    pub fn servlet_count(&self) -> usize {
        self.servlet_registry.len()
    }

    /// Number of filters registered for this webapp.
    pub fn filter_count(&self) -> usize {
        self.filter_registry.len()
    }
}

// ---------------------------------------------------------------------------
// Real implementation — only compiled with `--features jvm`.
// ---------------------------------------------------------------------------
#[cfg(feature = "jvm")]
mod imp {
    use super::*;
    use std::sync::mpsc::{Receiver, Sender};
    use std::thread::JoinHandle;

    use jni::{InitArgsBuilder, JNIEnv, JNIVersion, JavaVM};

    /// A unit of JNI work dispatched onto the worker pool. The closure runs on
    /// a permanently-attached worker thread with that thread's `JNIEnv`.
    type Job = Box<dyn FnOnce(&mut JNIEnv) + Send>;

    /// The real, JVM-backed runtime.
    ///
    /// Owns the single embedded `JavaVM`, the worker pool that funnels every
    /// JNI call, and the [`WebappRuntime`] registry.
    pub struct JvmRuntime {
        /// The single embedded `JavaVM`. `Arc` so each worker thread can hold a
        /// reference and attach itself.
        vm: Arc<JavaVM>,
        /// Sender half of the work queue feeding the worker pool. Wrapped in an
        /// `Option` so [`JvmRuntime::shutdown`] can drop it, signalling every
        /// worker to drain and exit.
        job_tx: std::sync::Mutex<Option<Sender<Job>>>,
        /// Join handles for the attached worker threads, taken by
        /// [`JvmRuntime::shutdown`].
        workers: std::sync::Mutex<Vec<JoinHandle<()>>>,
        /// Registry of deployed web applications, keyed by context id.
        webapps: DashMap<ContextId, Arc<WebappRuntime>>,
        /// Number of attached worker threads.
        worker_threads: usize,
    }

    /// The path to the bridge JAR exported by `build.rs` at compile time, or
    /// `None` if the build script did not produce one (e.g. no JDK was present
    /// when this crate was compiled, and `cargo:warning=` was emitted instead).
    ///
    /// Using `option_env!` instead of `env!` so the crate still builds when the
    /// jar was not produced — the run-time check then warns and the JVM bridge
    /// degrades to "JAR not on classpath" rather than failing the build.
    pub(super) const BRIDGE_JAR_PATH: Option<&'static str> = option_env!("TOMCATRS_BRIDGE_JAR");

    impl JvmRuntime {
        /// Create the embedded JVM and attach the worker pool.
        ///
        /// Builds the classpath option from [`JvmConfig::classpath`] **plus**
        /// the build-time `TOMCATRS_BRIDGE_JAR` (the jar `build.rs` produces),
        /// appends the caller's [`JvmConfig::jvm_args`], and applies a small
        /// set of sane defaults (a generous thread stack size and headless
        /// AWT). The resulting `JavaVM` is kept alive in the returned runtime.
        ///
        /// After the VM is up the native methods on the four facade classes
        /// (`NativeRequest`, `NativeResponse`, `NativeSession`,
        /// `NativeAsyncContext`) are registered via `RegisterNatives` on a
        /// worker thread. If that fails — typically because the bridge JAR is
        /// missing from the classpath — the runtime is shut down and an
        /// `Error::Bridge` is returned so the caller fails fast.
        pub fn start(cfg: JvmConfig) -> Result<JvmRuntime> {
            // Compose the effective classpath: caller's entries + the bridge
            // JAR `build.rs` produced (if any). The bridge JAR is *appended*
            // so caller entries that shadow it (rare, but possible) win.
            let mut effective_cfg = cfg.clone();
            match BRIDGE_JAR_PATH {
                Some(path) if !path.is_empty() => {
                    let jar = PathBuf::from(path);
                    if !effective_cfg.classpath.iter().any(|p| p == &jar) {
                        effective_cfg.classpath.push(jar);
                    }
                }
                _ => {
                    tracing::warn!(
                        "TOMCATRS_BRIDGE_JAR is not set (build.rs did not produce a bridge jar; \
                         was `javac` available at build time?). The embedded JVM will start, but \
                         the Tomcat-RS servlet bridge will not function because the bridge JAR is \
                         missing from -Djava.class.path. Supply it out-of-band via JvmConfig::classpath."
                    );
                }
            }

            let mut builder = InitArgsBuilder::new()
                .version(JNIVersion::V8)
                .option(effective_cfg.classpath_option())
                // Sane defaults — overridable by anything the caller passes in
                // `jvm_args` below, since later options win.
                .option("-Xss1m")
                .option("-Djava.awt.headless=true");
            for arg in &cfg.jvm_args {
                builder = builder.option(arg);
            }
            let init_args = builder
                .build()
                .map_err(|e| Error::bridge(format!("invalid JVM init args: {e}")))?;

            let vm = JavaVM::new(init_args)
                .map_err(|e| Error::bridge(format!("JNI_CreateJavaVM failed: {e}")))?;
            let vm = Arc::new(vm);

            let worker_threads = cfg.worker_threads.max(1);
            let (job_tx, job_rx) = std::sync::mpsc::channel::<Job>();
            let workers = Self::spawn_workers(&vm, worker_threads, job_rx)?;

            let runtime = JvmRuntime {
                vm,
                job_tx: std::sync::Mutex::new(Some(job_tx)),
                workers: std::sync::Mutex::new(workers),
                webapps: DashMap::new(),
                worker_threads,
            };

            // Wire up `RegisterNatives` on a worker thread. Failure means the
            // bridge JAR isn't on the classpath (or some other class-loading
            // problem); shut the runtime down so we don't leak workers and
            // surface a clear error.
            if let Err(e) = runtime.with_env(crate::jni::register_native_methods) {
                tracing::error!(error = %e, "registering bridge natives failed; shutting down JVM");
                runtime.shutdown();
                return Err(e);
            }

            Ok(runtime)
        }

        /// Spawn `count` OS threads, each attached to the JVM for its whole
        /// lifetime (the "per-worker JNI attach" design constraint), draining
        /// the shared work queue.
        ///
        /// The `Receiver` is shared across workers behind a `Mutex`: each
        /// worker locks it just long enough to pull one job, so jobs are
        /// load-balanced across the pool. When the last `Sender` is dropped
        /// (by [`JvmRuntime::shutdown`] or on `Drop`), `recv` returns `Err`
        /// and the worker detaches and exits.
        fn spawn_workers(
            vm: &Arc<JavaVM>,
            count: usize,
            job_rx: Receiver<Job>,
        ) -> Result<Vec<JoinHandle<()>>> {
            let shared_rx = Arc::new(std::sync::Mutex::new(job_rx));
            let mut workers = Vec::with_capacity(count);
            for i in 0..count {
                let vm = Arc::clone(vm);
                let rx = Arc::clone(&shared_rx);
                let handle = std::thread::Builder::new()
                    .name(format!("tomcatrs-jvm-worker-{i}"))
                    .spawn(move || {
                        // Attach once; `attach_current_thread` returns a guard
                        // that keeps this thread attached until it is dropped
                        // (i.e. until this closure returns).
                        let mut guard = match vm.attach_current_thread() {
                            Ok(g) => {
                                tracing::debug!(worker = i, "JVM worker attached");
                                g
                            }
                            Err(e) => {
                                tracing::error!(
                                    worker = i,
                                    error = %e,
                                    "JVM worker attach failed; thread exiting"
                                );
                                return;
                            }
                        };
                        loop {
                            // Lock the shared receiver only for the duration of
                            // the `recv` so siblings can pull concurrently.
                            let job = {
                                let rx = rx.lock().expect("job receiver mutex poisoned");
                                rx.recv()
                            };
                            match job {
                                Ok(job) => job(&mut guard),
                                // All senders dropped: drain complete, exit.
                                Err(_) => break,
                            }
                        }
                        tracing::debug!(worker = i, "JVM worker draining; detaching");
                        // `guard` drops here, detaching the thread.
                    })
                    .map_err(|e| Error::bridge(format!("spawning JVM worker failed: {e}")))?;
                workers.push(handle);
            }
            Ok(workers)
        }

        /// Dispatch a closure onto a worker thread and wait for its result.
        ///
        /// This is the **single funnel** for every JNI call in the crate: the
        /// closure runs on a permanently-attached worker with that worker's
        /// `JNIEnv`, and the result is returned over a oneshot channel. The
        /// caller blocks until the worker finishes.
        pub fn with_env<R>(&self, f: impl FnOnce(&mut JNIEnv) -> Result<R> + Send) -> Result<R>
        where
            R: Send,
        {
            // SAFETY: `with_env` blocks until the job has run to completion, so
            // the closure and its result never outlive this stack frame even
            // though they are sent to another thread. This lets `f` borrow
            // non-`'static` data, which is essential for ergonomic JNI calls.
            let (result_tx, result_rx) = std::sync::mpsc::sync_channel::<Result<R>>(1);
            let job: Box<dyn FnOnce(&mut JNIEnv) + Send> = Box::new(move |env| {
                let outcome = f(env);
                // If the receiver is gone the caller has given up; drop quietly.
                let _ = result_tx.send(outcome);
            });
            // Erase the lifetime: safe because of the blocking join below.
            let job: Job =
                unsafe { std::mem::transmute::<Box<dyn FnOnce(&mut JNIEnv) + Send>, Job>(job) };

            {
                let tx = self.job_tx.lock().expect("job_tx mutex poisoned");
                match tx.as_ref() {
                    Some(tx) => tx.send(job).map_err(|_| {
                        Error::bridge("JVM worker pool has shut down; cannot dispatch JNI work")
                    })?,
                    None => {
                        return Err(Error::bridge(
                            "JVM worker pool has shut down; cannot dispatch JNI work",
                        ))
                    }
                }
            }

            result_rx.recv().map_err(|_| {
                Error::bridge("JVM worker dropped the job without producing a result")
            })?
        }

        /// Register a web application, creating its [`WebappRuntime`] entry.
        ///
        /// The webapp's isolating `URLClassLoader` is *not* built here — that
        /// is the classloader module's job, via [`WebappRuntime::set_class_loader`].
        /// This method just creates and registers the entry and returns the
        /// shared handle. Re-registering the same context id replaces the
        /// previous entry.
        pub fn register_webapp(
            &self,
            context_id: ContextId,
            classpath: Vec<PathBuf>,
        ) -> Result<Arc<WebappRuntime>> {
            let webapp = Arc::new(WebappRuntime::new(context_id.clone(), classpath));
            self.webapps.insert(context_id, Arc::clone(&webapp));
            Ok(webapp)
        }

        /// The [`WebappRuntime`] registered under `context_id`, if any.
        pub fn webapp(&self, context_id: &ContextId) -> Option<Arc<WebappRuntime>> {
            self.webapps.get(context_id).map(|w| Arc::clone(w.value()))
        }

        /// Whether a web application is registered under `context_id`.
        pub fn has_webapp(&self, context_id: &str) -> bool {
            self.webapps.contains_key(context_id)
        }

        /// Number of attached worker threads.
        pub fn worker_count(&self) -> usize {
            self.worker_threads
        }

        /// Borrow the embedded `JavaVM`. Threads that are not part of the
        /// worker pool must attach themselves before using it; prefer
        /// [`JvmRuntime::with_env`] for all routine JNI work.
        pub fn java_vm(&self) -> &Arc<JavaVM> {
            &self.vm
        }

        /// Drain the worker pool and detach its threads.
        ///
        /// Drops the job-queue sender, which causes every worker's `recv` to
        /// return `Err` once the queue is empty; each worker then detaches its
        /// JNI thread and exits. This method joins them all. Idempotent: a
        /// second call is a no-op.
        pub fn shutdown(&self) {
            // Drop the sender so workers see end-of-queue.
            {
                let mut tx = self.job_tx.lock().expect("job_tx mutex poisoned");
                *tx = None;
            }
            let workers = {
                let mut w = self.workers.lock().expect("workers mutex poisoned");
                std::mem::take(&mut *w)
            };
            for handle in workers {
                if let Err(e) = handle.join() {
                    tracing::error!(?e, "JVM worker thread panicked during shutdown");
                }
            }
            tracing::debug!("JVM worker pool drained and detached");
        }
    }

    impl Drop for JvmRuntime {
        /// Ensure the worker pool is drained even if [`JvmRuntime::shutdown`]
        /// was never called explicitly.
        fn drop(&mut self) {
            self.shutdown();
        }
    }

    impl std::fmt::Debug for JvmRuntime {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("JvmRuntime")
                .field("worker_threads", &self.worker_threads)
                .field("webapps", &self.webapps.len())
                .finish()
        }
    }
}

// ---------------------------------------------------------------------------
// Stub implementation — compiled with default features (no JDK required).
// ---------------------------------------------------------------------------
#[cfg(not(feature = "jvm"))]
mod imp {
    use super::*;

    /// Stub [`JvmRuntime`] used when the crate is built without the `jvm`
    /// feature.
    ///
    /// The type and every method signature exist so that dependent crates
    /// compile unchanged, but no JVM is ever created: [`JvmRuntime::start`]
    /// always fails. The [`WebappRuntime`] registry, however, is fully
    /// functional with placeholder handles — only the methods that genuinely
    /// need a live JVM (`start`, `with_env`) refuse.
    #[derive(Debug, Default)]
    pub struct JvmRuntime {
        /// Registry of deployed web applications, keyed by context id. Present
        /// even on the stub so the registry plumbing and its tests exercise
        /// the same code as the real build.
        webapps: DashMap<ContextId, Arc<WebappRuntime>>,
    }

    impl JvmRuntime {
        /// Always fails: the embedded JVM was not compiled in.
        pub fn start(_cfg: JvmConfig) -> Result<JvmRuntime> {
            Err(Error::bridge(
                "JVM support not compiled in; rebuild with --features jvm",
            ))
        }

        /// Always fails: there is no JVM and no worker pool to dispatch onto.
        ///
        /// Mirrors the real [`JvmRuntime::with_env`] signature exactly so
        /// callers compile unchanged regardless of feature selection. The
        /// closure is accepted but never run.
        pub fn with_env<R>(&self, _f: impl FnOnce(&mut ()) -> Result<R> + Send) -> Result<R>
        where
            R: Send,
        {
            Err(Error::bridge(
                "JVM support not compiled in; rebuild with --features jvm",
            ))
        }

        /// Register a web application, creating its [`WebappRuntime`] entry.
        ///
        /// Fully functional on the stub: the entry is created with placeholder
        /// handles. The isolating class loader is left unset — building it
        /// needs a live JVM.
        pub fn register_webapp(
            &self,
            context_id: ContextId,
            classpath: Vec<PathBuf>,
        ) -> Result<Arc<WebappRuntime>> {
            let webapp = Arc::new(WebappRuntime::new(context_id.clone(), classpath));
            self.webapps.insert(context_id, Arc::clone(&webapp));
            Ok(webapp)
        }

        /// The [`WebappRuntime`] registered under `context_id`, if any.
        pub fn webapp(&self, context_id: &ContextId) -> Option<Arc<WebappRuntime>> {
            self.webapps.get(context_id).map(|w| Arc::clone(w.value()))
        }

        /// Whether a web application is registered under `context_id`.
        pub fn has_webapp(&self, context_id: &str) -> bool {
            self.webapps.contains_key(context_id)
        }

        /// Number of attached worker threads — always `0` without a JVM.
        pub fn worker_count(&self) -> usize {
            0
        }

        /// Drain the worker pool — a no-op without a JVM. Present so callers
        /// compile unchanged regardless of feature selection.
        pub fn shutdown(&self) {}
    }
}

pub use imp::JvmRuntime;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classpath_option_joins_with_separator() {
        let cfg = JvmConfig {
            classpath: vec![PathBuf::from("/a/x.jar"), PathBuf::from("/b/y.jar")],
            ..JvmConfig::default()
        };
        let opt = cfg.classpath_option();
        assert!(opt.starts_with("-Djava.class.path="));
        assert!(opt.contains("x.jar"));
        assert!(opt.contains("y.jar"));
    }

    #[test]
    fn default_config_sizes_worker_pool() {
        let cfg = JvmConfig::default();
        assert!(cfg.worker_threads >= 1);
        assert!(cfg.classpath.is_empty());
    }

    #[cfg(not(feature = "jvm"))]
    #[test]
    fn start_without_jvm_feature_returns_bridge_error() {
        let err =
            JvmRuntime::start(JvmConfig::default()).expect_err("stub runtime must refuse to start");
        match err {
            tomcatrs_core::Error::Bridge(msg) => {
                assert!(msg.contains("--features jvm"), "unexpected message: {msg}");
            }
            other => panic!("expected Error::Bridge, got {other:?}"),
        }
    }

    #[cfg(not(feature = "jvm"))]
    #[test]
    fn with_env_without_jvm_feature_returns_bridge_error() {
        let runtime = JvmRuntime::default();
        let err = runtime
            .with_env(|_env| Ok::<(), tomcatrs_core::Error>(()))
            .expect_err("stub runtime cannot dispatch JNI work");
        match err {
            tomcatrs_core::Error::Bridge(msg) => {
                assert!(msg.contains("--features jvm"), "unexpected message: {msg}");
            }
            other => panic!("expected Error::Bridge, got {other:?}"),
        }
    }

    #[cfg(not(feature = "jvm"))]
    #[test]
    fn webapp_registry_registers_and_looks_up() {
        let runtime = JvmRuntime::default();
        let ctx: ContextId = "/app".to_string();
        assert!(!runtime.has_webapp(&ctx));
        assert!(runtime.webapp(&ctx).is_none());

        let webapp = runtime
            .register_webapp(ctx.clone(), vec![PathBuf::from("/srv/app/WEB-INF/classes")])
            .expect("registering a webapp never fails on the stub");
        assert_eq!(webapp.context_id(), &ctx);
        assert_eq!(webapp.classpath().len(), 1);

        assert!(runtime.has_webapp(&ctx));
        let looked_up = runtime.webapp(&ctx).expect("just registered");
        assert_eq!(looked_up.context_id(), &ctx);
        assert!(Arc::ptr_eq(&webapp, &looked_up));
    }

    #[cfg(not(feature = "jvm"))]
    #[test]
    fn webapp_runtime_servlet_and_filter_registries() {
        let webapp = WebappRuntime::new("/shop".to_string(), Vec::new());
        assert_eq!(webapp.servlet_count(), 0);
        assert_eq!(webapp.filter_count(), 0);
        assert!(webapp.servlet("checkout").is_none());

        let handle = ServletInstanceHandle::new();
        let id = handle.id();
        assert!(webapp
            .register_servlet("checkout", handle.clone())
            .is_none());
        assert_eq!(webapp.servlet_count(), 1);
        let got = webapp.servlet("checkout").expect("just registered");
        assert_eq!(got.id(), id);

        // Re-registering returns the previous handle.
        let replacement = ServletInstanceHandle::new();
        let prev = webapp
            .register_servlet("checkout", replacement.clone())
            .expect("a handle was already registered");
        assert_eq!(prev.id(), id);
        assert_eq!(webapp.servlet("checkout").unwrap().id(), replacement.id());

        let filter = ServletInstanceHandle::new();
        assert!(webapp.register_filter("auth", filter.clone()).is_none());
        assert_eq!(webapp.filter_count(), 1);
        assert_eq!(webapp.filter("auth").unwrap().id(), filter.id());
        assert!(webapp.filter("missing").is_none());
    }

    #[cfg(not(feature = "jvm"))]
    #[test]
    fn webapp_runtime_class_loader_hook() {
        let webapp = WebappRuntime::new("/api".to_string(), Vec::new());
        assert!(!webapp.has_class_loader());
        assert!(webapp.class_loader().is_none());

        webapp.set_class_loader(ClassLoaderHandle::new());
        assert!(webapp.has_class_loader());
        assert!(webapp.class_loader().is_some());
    }

    #[cfg(not(feature = "jvm"))]
    #[test]
    fn worker_count_is_zero_without_jvm() {
        let runtime = JvmRuntime::default();
        assert_eq!(runtime.worker_count(), 0);
        // shutdown is a harmless no-op on the stub.
        runtime.shutdown();
    }
}
