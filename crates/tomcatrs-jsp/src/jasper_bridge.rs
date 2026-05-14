//! [`JasperBridge`] — the Rust-side handle that wires JVM-hosted Jasper into
//! the Tomcat-RS servlet bridge as a real JSP runtime path.
//!
//! # How Tomcat runs a JSP
//!
//! Apache Tomcat does not have a dedicated "JSP engine" sitting next to the
//! servlet container — *Jasper is a servlet*. `conf/web.xml` declares
//! `org.apache.jasper.servlet.JspServlet` and maps it to `*.jsp` and `*.jspx`.
//! When a request for a `*.jsp` URL arrives, the container routes it to that
//! `JspServlet` instance exactly like any other servlet; `JspServlet` then:
//!
//! 1. locates the `.jsp` source under the web application,
//! 2. compiles it to a servlet `.java` + `.class` in the context's *scratch*
//!    (work) directory if the page is stale (or always, in development mode),
//! 3. loads the generated servlet class through the webapp's class loader, and
//! 4. invokes it to produce the response.
//!
//! # Tomcat-RS strategy: delegate to JVM-side Jasper
//!
//! Reimplementing the JSP compiler in Rust is out of scope for the incremental
//! rewrite. Instead Tomcat-RS keeps Jasper on the JVM side of the servlet
//! bridge and treats it as just another servlet:
//!
//! * [`JasperBridge::register`] registers `org.apache.jasper.servlet.JspServlet`
//!   into a context's [`WebappRuntime`] under the `*.jsp` / `*.jspx` mappings,
//!   mirroring how [`crate`](crate)'s sibling servlet-bridge crate registers
//!   ordinary `web.xml` servlets — only here the servlet, its class, and its
//!   init-params are synthesised from [`JspConfig`] rather than parsed from a
//!   descriptor.
//! * Once registered, an actual `*.jsp` request needs **no JSP-specific code
//!   path at all**: it flows through the standard
//!   [`JvmServletInvoker`](tomcatrs_servlet_bridge::JvmServletInvoker), which
//!   resolves the `JspServlet` instance from the [`WebappRuntime`] and
//!   `service()`s it over JNI. See [`JasperBridge::service_jsp`].
//!
//! # Development vs. production
//!
//! Jasper supports two operating models, selected by [`JspConfig::development`]:
//!
//! * **development** (`development = true`) — `JspServlet` compiles each JSP on
//!   demand and, with [`JspConfig::modification_test_interval`] governing how
//!   often it re-checks, *recompiles* a page whenever its source changes. This
//!   is convenient while editing but pays first-request compilation latency and
//!   requires a Java compiler in the runtime.
//! * **production** (`development = false`) — JSPs are *precompiled* ahead of
//!   time (see [`PrecompileTask`](crate::precompile::PrecompileTask)) into
//!   servlet classes on the webapp class path. `JspServlet` then simply loads
//!   and invokes those classes; it never recompiles and no compiler is needed
//!   at runtime. This is the preferred Tomcat-RS deployment model.
//!
//! Either way the wiring is identical — only the init-params handed to
//! `JspServlet` differ — which is why [`JasperBridge::register`] is the single
//! entry point for both.
//!
//! # Two builds, one API
//!
//! Like the rest of the bridge, every public item here exists on both feature
//! paths. Under `--features jvm`, [`JasperBridge::register`] performs the real
//! JNI registration against the embedded JVM. On the default (no-JVM) build it
//! records the *intended* registration into the [`WebappRuntime`] registry with
//! a placeholder handle and logs it — so the data model and registry plumbing
//! are fully exercised by `cargo test` with no JDK installed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tomcatrs_core::{ContextId, Result};
use tomcatrs_servlet_bridge::JvmRuntime;

/// The fully-qualified JVM class name of Apache Jasper's JSP servlet.
pub const JSP_SERVLET_CLASS: &str = "org.apache.jasper.servlet.JspServlet";

/// The logical servlet name `JspServlet` is registered under, matching the
/// name Tomcat's stock `conf/web.xml` uses.
pub const JSP_SERVLET_NAME: &str = "jsp";

/// The URL patterns Jasper handles, matching Tomcat's stock `conf/web.xml`.
pub const JSP_URL_PATTERNS: [&str; 2] = ["*.jsp", "*.jspx"];

/// Configuration for the JVM-hosted Jasper engine.
///
/// The fields mirror the `<init-param>`s Tomcat exposes for
/// `org.apache.jasper.servlet.JspServlet` in `conf/web.xml`; [`JspConfig`] is
/// turned into exactly those init-params by [`JspConfig::init_params`] and
/// handed to Jasper at registration time.
#[derive(Debug, Clone)]
pub struct JspConfig {
    /// Development mode: recompile JSPs when their source changes and surface
    /// detailed compilation diagnostics (Jasper's `development` init-param).
    /// Production deployments set this to `false` and rely on precompilation.
    pub development: bool,
    /// The scratch directory Jasper uses for generated `.java` and `.class`
    /// files (Jasper's `scratchdir` init-param). This is the per-context work
    /// directory managed by [`ScratchDir`](crate::scratchdir::ScratchDir).
    pub scratch_dir: PathBuf,
    /// Whether generated servlet source should be kept on disk for debugging
    /// (Jasper's `keepgenerated` init-param).
    pub keep_generated: bool,
    /// Whether Jasper should generate a `SMAP` / line-number mapping back to the
    /// original JSP so stack traces and debuggers point at JSP lines
    /// (Jasper's `mappedfile` init-param).
    pub mapped_file: bool,
    /// The JVM bytecode target the generated servlets are compiled for
    /// (Jasper's `compilerTargetVM` init-param). Defaults to `"17"`.
    pub compiler_target_vm: String,
    /// Whether template text whitespace should be trimmed (Jasper's
    /// `trimSpaces` init-param). Reduces generated-page size.
    pub trim_spaces: bool,
    /// How often, in seconds, Jasper re-checks a JSP's source for modification
    /// before serving it (Jasper's `modificationTestInterval` init-param). `0`
    /// means "check on every request"; larger values trade staleness for
    /// throughput. Only meaningful when [`development`](Self::development) is
    /// `true`.
    pub modification_test_interval: u64,
    /// The fully-qualified class name of the JVM-side JSP servlet requests are
    /// delegated to. Overridable for alternative Jasper-compatible engines;
    /// defaults to [`JSP_SERVLET_CLASS`].
    pub jsp_servlet_class: String,
}

impl JspConfig {
    /// Build a default configuration rooted at `scratch_dir`.
    ///
    /// Defaults match a production-leaning Tomcat: `development = false`,
    /// `keep_generated = false`, `mapped_file = true` (line maps cost nothing
    /// at run time and make stack traces usable), `trim_spaces = false`,
    /// `compiler_target_vm = "17"`, `modification_test_interval = 4` (Tomcat's
    /// own default), delegating to the stock
    /// `org.apache.jasper.servlet.JspServlet`.
    pub fn new(scratch_dir: impl AsRef<Path>) -> JspConfig {
        JspConfig {
            development: false,
            scratch_dir: scratch_dir.as_ref().to_path_buf(),
            keep_generated: false,
            mapped_file: true,
            compiler_target_vm: "17".to_string(),
            trim_spaces: false,
            modification_test_interval: 4,
            jsp_servlet_class: JSP_SERVLET_CLASS.to_string(),
        }
    }

    /// Enable development mode (runtime recompilation, verbose diagnostics).
    pub fn with_development(mut self, development: bool) -> JspConfig {
        self.development = development;
        self
    }

    /// Keep generated servlet sources on disk for debugging.
    pub fn with_keep_generated(mut self, keep: bool) -> JspConfig {
        self.keep_generated = keep;
        self
    }

    /// Whether Jasper should emit JSP↔servlet line maps.
    pub fn with_mapped_file(mut self, mapped: bool) -> JspConfig {
        self.mapped_file = mapped;
        self
    }

    /// Set the JVM bytecode target the generated servlets are compiled for.
    pub fn with_compiler_target_vm(mut self, target: impl Into<String>) -> JspConfig {
        self.compiler_target_vm = target.into();
        self
    }

    /// Whether template-text whitespace should be trimmed.
    pub fn with_trim_spaces(mut self, trim: bool) -> JspConfig {
        self.trim_spaces = trim;
        self
    }

    /// Set how often (seconds) Jasper re-checks a JSP for modification.
    pub fn with_modification_test_interval(mut self, secs: u64) -> JspConfig {
        self.modification_test_interval = secs;
        self
    }

    /// Render this configuration as the ordered list of `(name, value)`
    /// `<init-param>` pairs Jasper's `JspServlet` understands.
    ///
    /// This is the single place [`JspConfig`] is translated into Jasper's
    /// vocabulary; [`JasperBridge::register`] feeds the result straight into
    /// the servlet registration on both feature paths.
    pub fn init_params(&self) -> Vec<(String, String)> {
        vec![
            ("development".to_string(), self.development.to_string()),
            (
                "scratchdir".to_string(),
                self.scratch_dir.display().to_string(),
            ),
            ("keepgenerated".to_string(), self.keep_generated.to_string()),
            ("mappedfile".to_string(), self.mapped_file.to_string()),
            (
                "compilerTargetVM".to_string(),
                self.compiler_target_vm.clone(),
            ),
            ("trimSpaces".to_string(), self.trim_spaces.to_string()),
            (
                "modificationTestInterval".to_string(),
                self.modification_test_interval.to_string(),
            ),
        ]
    }
}

/// The Rust-side handle that wires JVM-hosted Jasper into the servlet bridge.
///
/// A `JasperBridge` is cheap to construct and clone; it holds only the
/// [`JspConfig`] describing how Jasper should be set up. The real work is
/// [`JasperBridge::register`], which installs `JspServlet` into a context's
/// [`WebappRuntime`] so that subsequent `*.jsp` requests dispatch through the
/// ordinary servlet path.
#[derive(Debug, Clone)]
pub struct JasperBridge {
    config: JspConfig,
}

impl JasperBridge {
    /// Create a bridge handle for the given [`JspConfig`].
    pub fn new(config: JspConfig) -> JasperBridge {
        tracing::info!(
            jsp_servlet_class = %config.jsp_servlet_class,
            development = config.development,
            scratch_dir = %config.scratch_dir.display(),
            "initialised Jasper bridge handle; JSP execution is delegated to \
             the JVM-side JspServlet through the servlet bridge"
        );
        JasperBridge { config }
    }

    /// The configuration this bridge was built with.
    pub fn config(&self) -> &JspConfig {
        &self.config
    }

    /// The fully-qualified JVM class name JSP requests are delegated to.
    pub fn jsp_servlet_class(&self) -> &str {
        &self.config.jsp_servlet_class
    }

    /// Whether `path` names a JSP resource — i.e. ends with `.jsp` or `.jspx`,
    /// case-insensitively. This is the same predicate Jasper's `*.jsp` /
    /// `*.jspx` URL mappings encode, surfaced so the connector/mapper can route
    /// a request to the registered `JspServlet` without a JVM round-trip.
    pub fn is_jsp_path(path: &str) -> bool {
        let lower = path.to_ascii_lowercase();
        lower.ends_with(".jsp") || lower.ends_with(".jspx")
    }

    /// Register Apache Jasper's `JspServlet` into the web application
    /// identified by `context_id`, so that `*.jsp` / `*.jspx` requests in that
    /// context are served by JVM-hosted Jasper.
    ///
    /// This mirrors the sibling servlet-bridge crate's `web.xml` registration
    /// path: it ensures a [`WebappRuntime`] exists for the context (creating it
    /// with `webapp_classpath` as its class path if necessary), then registers
    /// the `JspServlet` instance under the logical name [`JSP_SERVLET_NAME`].
    /// The difference is purely in provenance — the servlet class
    /// ([`JspConfig::jsp_servlet_class`]) and its `<init-param>`s
    /// ([`JspConfig::init_params`]) are synthesised from [`JspConfig`] rather
    /// than parsed from a descriptor.
    ///
    /// After this returns, an actual `*.jsp` request requires no JSP-specific
    /// handling: it dispatches through the standard
    /// [`JvmServletInvoker`](tomcatrs_servlet_bridge::JvmServletInvoker) like
    /// any other servlet. See [`JasperBridge::service_jsp`].
    ///
    /// # Behaviour by feature
    ///
    /// * **`--features jvm`** — instantiates `JspServlet` through the webapp's
    ///   class loader inside the JVM, calls `init()` on it with the
    ///   Jasper init-params, and stores the resulting
    ///   [`ServletInstanceHandle`](tomcatrs_servlet_bridge::jvm::ServletInstanceHandle)
    ///   in the [`WebappRuntime`] servlet registry.
    /// * **default features** — records the *intended* registration into the
    ///   [`WebappRuntime`] servlet registry with a placeholder handle and logs
    ///   it; fully testable with no JDK installed.
    ///
    /// # Errors
    ///
    /// Returns [`tomcatrs_core::Error::Bridge`] if the [`WebappRuntime`] cannot
    /// be created or — on the `jvm` build — if any JNI step fails.
    pub fn register(
        &self,
        runtime: &Arc<JvmRuntime>,
        context_id: &ContextId,
        webapp_classpath: &[PathBuf],
    ) -> Result<()> {
        let init_params = self.config.init_params();
        tracing::info!(
            context_id = %context_id,
            jsp_servlet_class = %self.config.jsp_servlet_class,
            development = self.config.development,
            init_params = init_params.len(),
            "registering JVM-side Jasper JspServlet for *.jsp / *.jspx"
        );
        register_impl(self, runtime, context_id, webapp_classpath, &init_params)
    }

    /// Documentation hook for *servicing* a JSP request.
    ///
    /// There is intentionally **no JSP-specific request path** in Tomcat-RS.
    /// Once [`JasperBridge::register`] has installed `JspServlet` into a
    /// context's [`WebappRuntime`], a `*.jsp` request is an ordinary servlet
    /// request: the connector resolves it to the `jsp` servlet name (via the
    /// `*.jsp` / `*.jspx` mappings — see [`JasperBridge::is_jsp_path`]) and
    /// hands it to the standard
    /// [`JvmServletInvoker`](tomcatrs_servlet_bridge::JvmServletInvoker), which
    /// looks the `JspServlet` instance up in the [`WebappRuntime`] and
    /// `service()`s it over JNI. Jasper itself performs compilation (in
    /// development mode) or class loading (in production) inside that call.
    ///
    /// This method exists only to *document* that contract and to give callers
    /// a stable place to assert it: it returns the logical servlet name
    /// (`context` aside) that the invoker should be asked for. It does **not**
    /// duplicate the invoker.
    ///
    /// `request_path` is the request URI (or webapp-relative path); it is used
    /// only to confirm the path is in fact a JSP and is otherwise opaque here —
    /// Jasper resolves the concrete `.jsp` source on the JVM side.
    ///
    /// # Errors
    ///
    /// Returns [`tomcatrs_core::Error::NotFound`] if `request_path` is not a
    /// `*.jsp` / `*.jspx` path — such a request should never have been routed
    /// to Jasper.
    pub fn service_jsp(&self, request_path: &str) -> Result<&'static str> {
        if !Self::is_jsp_path(request_path) {
            return Err(tomcatrs_core::Error::NotFound(format!(
                "{request_path} is not a JSP resource; only *.jsp / *.jspx are \
                 served by Jasper"
            )));
        }
        tracing::debug!(
            request_path = %request_path,
            servlet_name = JSP_SERVLET_NAME,
            "JSP request dispatches through the standard JvmServletInvoker to \
             the registered JspServlet instance"
        );
        Ok(JSP_SERVLET_NAME)
    }
}

// ---------------------------------------------------------------------------
// Real implementation — only compiled with `--features jvm`.
// ---------------------------------------------------------------------------
#[cfg(feature = "jvm")]
fn register_impl(
    bridge: &JasperBridge,
    runtime: &Arc<JvmRuntime>,
    context_id: &ContextId,
    webapp_classpath: &[PathBuf],
    init_params: &[(String, String)],
) -> Result<()> {
    use jni::objects::{JObject, JValue};
    use tomcatrs_core::Error;
    use tomcatrs_servlet_bridge::jvm::ServletInstanceHandle;

    // Ensure the per-context WebappRuntime exists; reuse it if a `web.xml`
    // registration already created it.
    let webapp = match runtime.webapp(context_id) {
        Some(existing) => existing,
        None => runtime.register_webapp(context_id.clone(), webapp_classpath.to_vec())?,
    };

    let class_binary = bridge.config.jsp_servlet_class.replace('.', "/");
    let servlet_name = JSP_SERVLET_NAME.to_string();
    let init_params = init_params.to_vec();

    // All JNI work runs on a single worker thread through the `with_env`
    // funnel; the closure returns the freshly-built handle so it can be stored
    // in the thread-safe WebappRuntime registry afterwards.
    let handle = runtime.with_env(|env| -> Result<ServletInstanceHandle> {
        // Resolve and instantiate JspServlet. Jasper's JspServlet has a public
        // no-arg constructor, just like any servlet.
        let class = env.find_class(&class_binary).map_err(|e| {
            Error::bridge(format!("loading {class_binary} (JspServlet) failed: {e}"))
        })?;
        let instance = env
            .new_object(&class, "()V", &[])
            .map_err(|e| Error::bridge(format!("instantiating {class_binary} failed: {e}")))?;

        // Build the init-param Map<String,String> for JspServlet.init().
        let map = env
            .new_object("java/util/HashMap", "()V", &[])
            .map_err(|e| Error::bridge(format!("new HashMap() failed: {e}")))?;
        for (k, v) in &init_params {
            let jk = env
                .new_string(k)
                .map_err(|e| Error::bridge(format!("new_string(init-param name) failed: {e}")))?;
            let jv = env
                .new_string(v)
                .map_err(|e| Error::bridge(format!("new_string(init-param value) failed: {e}")))?;
            env.call_method(
                &map,
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[
                    JValue::Object(&JObject::from(jk)),
                    JValue::Object(&JObject::from(jv)),
                ],
            )
            .map_err(|e| Error::bridge(format!("HashMap.put(init-param) failed: {e}")))?;
        }

        // Build the bridge ServletConfig facade and init() the servlet.
        let jname = env
            .new_string(&servlet_name)
            .map_err(|e| Error::bridge(format!("new_string(servlet name) failed: {e}")))?;
        let config = env
            .new_object(
                "org/apache/tomcatrs/bridge/TomcatRsServletConfig",
                "(Ljava/lang/String;Ljava/util/Map;)V",
                &[JValue::Object(&JObject::from(jname)), JValue::Object(&map)],
            )
            .map_err(|e| {
                Error::bridge(format!(
                    "new TomcatRsServletConfig(String, Map) failed: {e}"
                ))
            })?;
        env.call_method(
            &instance,
            "init",
            "(Ljakarta/servlet/ServletConfig;)V",
            &[JValue::Object(&config)],
        )
        .map_err(|e| Error::bridge(format!("JspServlet.init(ServletConfig) failed: {e}")))?;

        let global = env
            .new_global_ref(&instance)
            .map_err(|e| Error::bridge(format!("new_global_ref(JspServlet) failed: {e}")))?;
        Ok(ServletInstanceHandle::new(global))
    })?;

    webapp.register_servlet(servlet_name, handle);
    tracing::info!(
        context_id = %context_id,
        "JspServlet registered against the embedded JVM for *.jsp / *.jspx"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Stub implementation — compiled with default features (no JDK required).
// ---------------------------------------------------------------------------
#[cfg(not(feature = "jvm"))]
fn register_impl(
    _bridge: &JasperBridge,
    runtime: &Arc<JvmRuntime>,
    context_id: &ContextId,
    webapp_classpath: &[PathBuf],
    init_params: &[(String, String)],
) -> Result<()> {
    use tomcatrs_servlet_bridge::jvm::ServletInstanceHandle;

    // Ensure the per-context WebappRuntime exists; reuse it if a `web.xml`
    // registration already created it. This is the same registry plumbing the
    // real path uses, so the data model is fully exercised without a JDK.
    let webapp = match runtime.webapp(context_id) {
        Some(existing) => existing,
        None => runtime.register_webapp(context_id.clone(), webapp_classpath.to_vec())?,
    };

    tracing::info!(
        context_id = %context_id,
        servlet_name = JSP_SERVLET_NAME,
        init_params = init_params.len(),
        "would instantiate + init() JspServlet; recording intended JSP \
         registration with a placeholder handle (no-JVM stub)"
    );
    for (k, v) in init_params {
        tracing::debug!(context_id = %context_id, init_param = %k, value = %v, "Jasper init-param");
    }

    // Record the intended registration with a placeholder handle, exactly as
    // the sibling servlet-bridge crate's no-JVM registrar does.
    webapp.register_servlet(JSP_SERVLET_NAME.to_string(), ServletInstanceHandle::new());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_are_production_leaning() {
        let cfg = JspConfig::new("/tmp/scratch");
        assert!(!cfg.development);
        assert!(!cfg.keep_generated);
        assert!(cfg.mapped_file);
        assert!(!cfg.trim_spaces);
        assert_eq!(cfg.compiler_target_vm, "17");
        assert_eq!(cfg.modification_test_interval, 4);
        assert_eq!(cfg.scratch_dir, PathBuf::from("/tmp/scratch"));
        assert_eq!(cfg.jsp_servlet_class, JSP_SERVLET_CLASS);
    }

    #[test]
    fn config_builders_set_fields() {
        let cfg = JspConfig::new("/tmp/scratch")
            .with_development(true)
            .with_keep_generated(true)
            .with_mapped_file(false)
            .with_compiler_target_vm("21")
            .with_trim_spaces(true)
            .with_modification_test_interval(0);
        assert!(cfg.development);
        assert!(cfg.keep_generated);
        assert!(!cfg.mapped_file);
        assert!(cfg.trim_spaces);
        assert_eq!(cfg.compiler_target_vm, "21");
        assert_eq!(cfg.modification_test_interval, 0);
    }

    #[test]
    fn init_params_cover_every_jasper_setting() {
        let cfg = JspConfig::new("/work/Catalina/localhost/ROOT")
            .with_development(true)
            .with_keep_generated(true)
            .with_trim_spaces(true)
            .with_compiler_target_vm("21")
            .with_modification_test_interval(10);
        let params = cfg.init_params();
        let get = |k: &str| {
            params
                .iter()
                .find(|(name, _)| name == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("development"), Some("true"));
        assert_eq!(get("keepgenerated"), Some("true"));
        assert_eq!(get("mappedfile"), Some("true"));
        assert_eq!(get("trimSpaces"), Some("true"));
        assert_eq!(get("compilerTargetVM"), Some("21"));
        assert_eq!(get("modificationTestInterval"), Some("10"));
        assert_eq!(get("scratchdir"), Some("/work/Catalina/localhost/ROOT"));
    }

    #[test]
    fn is_jsp_path_recognises_jsp_and_jspx() {
        assert!(JasperBridge::is_jsp_path("/index.jsp"));
        assert!(JasperBridge::is_jsp_path("/WEB-INF/jsp/admin.jsp"));
        assert!(JasperBridge::is_jsp_path("/pages/about.jspx"));
        // Case-insensitive, matching Tomcat's pattern matching on extensions.
        assert!(JasperBridge::is_jsp_path("/INDEX.JSP"));
        assert!(JasperBridge::is_jsp_path("/About.JspX"));
        // Non-JSP resources are rejected.
        assert!(!JasperBridge::is_jsp_path("/index.html"));
        assert!(!JasperBridge::is_jsp_path("/style.css"));
        assert!(!JasperBridge::is_jsp_path("/app/jsp"));
        assert!(!JasperBridge::is_jsp_path("/jspsomething"));
        assert!(!JasperBridge::is_jsp_path(""));
    }

    #[test]
    fn service_jsp_returns_servlet_name_for_jsp_paths() {
        let bridge = JasperBridge::new(JspConfig::new("/tmp/scratch"));
        assert_eq!(bridge.service_jsp("/index.jsp").unwrap(), JSP_SERVLET_NAME);
        assert_eq!(
            bridge.service_jsp("/pages/x.jspx").unwrap(),
            JSP_SERVLET_NAME
        );
    }

    #[test]
    fn service_jsp_rejects_non_jsp_paths() {
        let bridge = JasperBridge::new(JspConfig::new("/tmp/scratch"));
        let err = bridge.service_jsp("/index.html").unwrap_err();
        assert!(matches!(err, tomcatrs_core::Error::NotFound(_)));
    }

    #[cfg(not(feature = "jvm"))]
    #[test]
    fn register_records_jsp_servlet_on_no_jvm_stub() {
        let runtime = Arc::new(JvmRuntime::default());
        let ctx: ContextId = "/app".to_string();
        let bridge = JasperBridge::new(JspConfig::new("/work/Catalina/localhost/app"));

        // No webapp registered yet.
        assert!(runtime.webapp(&ctx).is_none());

        bridge
            .register(&runtime, &ctx, &[PathBuf::from("/srv/app/WEB-INF/classes")])
            .expect("no-JVM register never fails");

        // The WebappRuntime was created and the JspServlet recorded under the
        // conventional `jsp` servlet name.
        let webapp = runtime.webapp(&ctx).expect("webapp registered");
        assert_eq!(webapp.classpath().len(), 1);
        assert!(webapp.servlet(JSP_SERVLET_NAME).is_some());
        assert_eq!(webapp.servlet_count(), 1);
    }

    #[cfg(not(feature = "jvm"))]
    #[test]
    fn register_reuses_an_existing_webapp_runtime() {
        let runtime = Arc::new(JvmRuntime::default());
        let ctx: ContextId = "/shop".to_string();
        // Pre-register the webapp, as a `web.xml` registration would have.
        let pre = runtime
            .register_webapp(
                ctx.clone(),
                vec![PathBuf::from("/srv/shop/WEB-INF/classes")],
            )
            .expect("pre-register");

        let bridge = JasperBridge::new(JspConfig::new("/work/Catalina/localhost/shop"));
        bridge
            .register(&runtime, &ctx, &[])
            .expect("no-JVM register never fails");

        let webapp = runtime.webapp(&ctx).expect("still registered");
        // Same Arc — the existing entry was reused, not replaced.
        assert!(Arc::ptr_eq(&pre, &webapp));
        assert!(webapp.servlet(JSP_SERVLET_NAME).is_some());
    }
}
