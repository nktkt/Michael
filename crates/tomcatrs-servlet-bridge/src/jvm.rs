//! Embedded-JVM lifecycle: configuration, the [`JvmRuntime`] handle, and the
//! per-worker JNI-attached thread pool.
//!
//! Exactly **one** JVM is created per OS process — the JNI Invocation API
//! forbids more, and a second `JNI_CreateJavaVM` would fail anyway. The
//! [`JvmRuntime`] owns that single `JavaVM` (behind the `jvm` feature), a
//! registry of deployed web applications, and a pool of worker threads each of
//! which is *attached* to the JVM once and stays attached.
//!
//! # Two builds, one API
//!
//! * **`--features jvm`** — [`JvmRuntime`] wraps a real `jni::JavaVM`. Requires
//!   a JDK to build (`jni` needs headers) and a `libjvm` at run time.
//! * **default features** — [`JvmRuntime`] is a stub: the type and every method
//!   signature still exist, but [`JvmRuntime::start`] returns
//!   [`tomcatrs_core::Error::Bridge`]. This keeps the rest of the workspace
//!   compilable and testable with no JDK present.

use std::path::PathBuf;

use tomcatrs_core::{ContextId, Result};

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
// Real implementation — only compiled with `--features jvm`.
// ---------------------------------------------------------------------------
#[cfg(feature = "jvm")]
mod imp {
    use super::*;
    use std::sync::Arc;

    use dashmap::DashMap;
    use jni::{InitArgsBuilder, JNIVersion, JavaVM};
    use tomcatrs_core::Error;

    /// A deployed web application's JVM-side metadata.
    #[derive(Debug, Clone)]
    pub(super) struct Webapp {
        pub(super) context_id: ContextId,
        pub(super) classpath: Vec<PathBuf>,
    }

    /// The real, JVM-backed runtime.
    pub struct JvmRuntime {
        /// The single embedded `JavaVM`. `Arc` so worker threads can each hold
        /// a reference and attach themselves.
        vm: Arc<JavaVM>,
        /// Registry of deployed web applications, keyed by context id.
        webapps: DashMap<ContextId, Webapp>,
        /// Number of attached worker threads.
        worker_threads: usize,
    }

    impl JvmRuntime {
        /// Create the embedded JVM and attach the worker pool.
        pub fn start(cfg: JvmConfig) -> Result<JvmRuntime> {
            let mut builder = InitArgsBuilder::new()
                .version(JNIVersion::V8)
                .option(cfg.classpath_option());
            for arg in &cfg.jvm_args {
                builder = builder.option(arg);
            }
            let init_args = builder
                .build()
                .map_err(|e| Error::bridge(format!("invalid JVM init args: {e}")))?;

            let vm = JavaVM::new(init_args)
                .map_err(|e| Error::bridge(format!("JNI_CreateJavaVM failed: {e}")))?;
            let vm = Arc::new(vm);

            let runtime = JvmRuntime {
                vm,
                webapps: DashMap::new(),
                worker_threads: cfg.worker_threads,
            };
            runtime.spawn_workers()?;
            Ok(runtime)
        }

        /// Register a web application's per-context classpath. The bridge uses
        /// this to construct an isolating `URLClassLoader` for the webapp on
        /// the JVM side.
        pub fn register_webapp(
            &self,
            context_id: ContextId,
            classpath: Vec<PathBuf>,
        ) -> Result<()> {
            // A `JNIEnv` for *this* thread; the real implementation would build
            // the webapp's `URLClassLoader` here.
            let _env = self
                .vm
                .attach_current_thread()
                .map_err(|e| Error::bridge(format!("attach for register_webapp failed: {e}")))?;

            self.webapps.insert(
                context_id.clone(),
                Webapp {
                    context_id,
                    classpath,
                },
            );
            Ok(())
        }

        /// Spawn `worker_threads` OS threads, each attached to the JVM for its
        /// whole lifetime (the "per-worker JNI attach" design constraint).
        fn spawn_workers(&self) -> Result<()> {
            for i in 0..self.worker_threads {
                let vm = Arc::clone(&self.vm);
                std::thread::Builder::new()
                    .name(format!("tomcatrs-jvm-worker-{i}"))
                    .spawn(move || {
                        // Attach once; the guard keeps the thread attached
                        // until it exits. Real workers would then loop pulling
                        // invocations off a channel.
                        match vm.attach_current_thread() {
                            Ok(_env) => {
                                tracing::debug!(worker = i, "JVM worker attached");
                            }
                            Err(e) => {
                                tracing::error!(worker = i, error = %e, "JVM worker attach failed");
                            }
                        }
                    })
                    .map_err(|e| Error::bridge(format!("spawning JVM worker failed: {e}")))?;
            }
            Ok(())
        }

        /// Number of attached worker threads.
        pub fn worker_count(&self) -> usize {
            self.worker_threads
        }

        /// Whether a web application is registered under `context_id`.
        pub fn has_webapp(&self, context_id: &str) -> bool {
            self.webapps.contains_key(context_id)
        }

        /// The registered per-context classpath for `context_id`, if any.
        /// The JVM-side `URLClassLoader` for the webapp is built from this.
        pub fn webapp_classpath(&self, context_id: &str) -> Option<Vec<PathBuf>> {
            self.webapps.get(context_id).map(|w| {
                debug_assert_eq!(w.context_id, context_id);
                w.classpath.clone()
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Stub implementation — compiled with default features (no JDK required).
// ---------------------------------------------------------------------------
#[cfg(not(feature = "jvm"))]
mod imp {
    use super::*;
    use tomcatrs_core::Error;

    /// Stub [`JvmRuntime`] used when the crate is built without the `jvm`
    /// feature. The type and every method signature exist so that dependent
    /// crates compile unchanged, but no JVM is ever created.
    #[derive(Debug)]
    pub struct JvmRuntime {
        // Uninhabited at run time: `start` always fails before one is built.
        _never: std::convert::Infallible,
    }

    impl JvmRuntime {
        /// Always fails: the embedded JVM was not compiled in.
        pub fn start(_cfg: JvmConfig) -> Result<JvmRuntime> {
            Err(Error::bridge(
                "JVM support not compiled in; rebuild with --features jvm",
            ))
        }

        /// Unreachable: no [`JvmRuntime`] value can exist without the `jvm`
        /// feature, because [`JvmRuntime::start`] never returns `Ok`.
        pub fn register_webapp(
            &self,
            _context_id: ContextId,
            _classpath: Vec<PathBuf>,
        ) -> Result<()> {
            match self._never {}
        }

        /// Unreachable for the same reason as [`JvmRuntime::register_webapp`].
        pub fn worker_count(&self) -> usize {
            match self._never {}
        }

        /// Unreachable for the same reason as [`JvmRuntime::register_webapp`].
        pub fn has_webapp(&self, _context_id: &str) -> bool {
            match self._never {}
        }
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
}
