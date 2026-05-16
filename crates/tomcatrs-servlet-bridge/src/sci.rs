//! Servlet 6 `ServletContainerInitializer` discovery and invocation.
//!
//! # Where this sits
//!
//! The Servlet specification mandates a `ServiceLoader`-style bootstrap path
//! that *every* modern Java framework relies on. A jar or `WEB-INF/classes`
//! tree may ship a UTF-8 text resource:
//!
//! ```text
//!   META-INF/services/jakarta.servlet.ServletContainerInitializer
//! ```
//!
//! containing one fully-qualified class name per line. Each named class
//! implements [`jakarta.servlet.ServletContainerInitializer`] (SCI). At
//! deployment time, *after* the webapp's class loader is built and the
//! `ServletContext` exists, but *before* any servlet's `init()` fires and
//! before `ServletContextListener.contextInitialized` callbacks, the
//! container:
//!
//! 1. discovers every SCI class visible to the webapp's class loader;
//! 2. for each SCI, builds the "set of handled types" (from the SCI's
//!    `@HandlesTypes` annotation, by scanning application classes);
//! 3. instantiates the SCI via its public no-arg constructor;
//! 4. invokes `sci.onStartup(handledTypes, ctx)`.
//!
//! Spring Boot's `SpringServletContainerInitializer` is the canonical
//! example: declared via `META-INF/services/...`, annotated with
//! `@HandlesTypes(WebApplicationInitializer.class)`, it bootstraps the entire
//! Spring application context from SCI. Tomcat-RS cannot start Spring Boot
//! WAR deployments without this discovery + invocation path.
//!
//! # What this module is responsible for
//!
//! This module is the **Rust driver** for the SCI dance. The heavy lifting
//! lives in `ServletContainerInitializerInvoker` on the Java side (see
//! [`crate::registration`] and the `java/` tree); `sci.rs` orchestrates the
//! call sequence from Rust:
//!
//! ```text
//!   run_sci(jvm, ctx_id, webapp_root)
//!       │
//!       ▼  (under --features jvm)
//!   ClassgraphIndex::scan(webapp_root)        ───▶ subtype + annotation graph
//!   JvmRuntime::with_env(env => {
//!       discoverServiceClasses(loader)        ───▶ Vec<String>  (SCI class names)
//!       for each name:
//!         readHandlesTypeNames(sciClass)      ───▶ Vec<String>  (target FQCNs)
//!         index.classes_handled_by(target)    ───▶ Vec<String>  (matched FQCNs)
//!         load each match through `loader`    ───▶ Set<Class<?>>
//!         invoke(loader, name, handled, ctx)
//!   })
//! ```
//!
//! Under default features (no JVM) [`run_sci`] returns an empty
//! [`SciReport`] and logs that SCI requires the `jvm` feature.
//!
//! # `@HandlesTypes` scanning
//!
//! The Servlet specification (§8.2.4) says the container is responsible for
//! scanning the webapp's classes/jars for types that *extend, implement,
//! or are annotated with* any of the classes named in an SCI's
//! `@HandlesTypes` value array, and passing the resulting
//! `Set<Class<?>>` as the first argument to `onStartup`.
//!
//! This is implemented. [`run_sci`] now:
//!
//! 1. Builds a [`tomcatrs_webapp::ClassgraphIndex`] once for the webapp
//!    by parsing every `.class` file under `WEB-INF/classes/` and every
//!    `.class` entry inside every `WEB-INF/lib/*.jar`. No JVM is
//!    involved for the scan itself — the existing hand-rolled
//!    class-file parser in `tomcatrs-webapp` handles it.
//! 2. For each discovered SCI, calls the Java helper
//!    `ServletContainerInitializerInvoker.readHandlesTypeNames(sciClass)`
//!    to extract the FQCNs listed in `@HandlesTypes(value = …)`.
//! 3. Queries [`ClassgraphIndex::classes_handled_by`] for each target
//!    FQCN — this returns every class that transitively extends or
//!    implements the target, plus every class directly annotated by
//!    it.
//! 4. Loads each matched class through the webapp's class loader and
//!    builds a `java.util.HashSet<Class<?>>` to pass to `onStartup`.
//!    If a matched class fails to load it is logged and skipped — one
//!    bad class does not abort the SCI.
//!
//! ## Class-name format (FQCN)
//!
//! All class names — both the targets read off `@HandlesTypes` and the
//! match results from the index — use the **dotted, fully-qualified
//! form** (`com.example.Foo`, `java.lang.Object`). This is the same
//! form `java.lang.Class.getName()` returns and that the existing
//! [`tomcatrs_webapp::annotations::ClassFile::class_name`] convention
//! uses, so names round-trip cleanly between the Rust class graph and
//! the Java side.
//!
//! ## Spring Boot impact
//!
//! `SpringServletContainerInitializer` declares
//! `@HandlesTypes(WebApplicationInitializer.class)`. With this scanner
//! in place, the set passed to Spring's SCI now contains every
//! `WebApplicationInitializer` implementor reachable through the
//! webapp's classpath — exactly what Spring needs to bootstrap the
//! application context. SCIs that declare no `@HandlesTypes` are
//! unaffected: they continue to receive an empty set, which is
//! correct.
//!
//! # Two builds, one API
//!
//! As with the rest of the crate, [`run_sci`] and [`SciReport`] exist on
//! both feature paths. The default-feature path is a no-op that logs and
//! returns an empty report; the `--features jvm` path does the real work.

use std::path::Path;
use std::sync::Arc;

use tomcatrs_core::{ContextId, Result};

use crate::jvm::JvmRuntime;

/// What [`run_sci`] reports back to the caller (typically
/// [`crate::registration::WebappRegistrar::register`]).
///
/// `initializers` is the ordered list of SCI class names that were
/// discovered AND invoked successfully. `errors` collects per-SCI failure
/// messages — the discovery walk does *not* abort on the first failure;
/// each SCI is independent.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SciReport {
    /// Fully-qualified class names of SCIs that ran to completion.
    pub initializers: Vec<String>,
    /// Per-SCI failure messages: one entry per SCI that either could not be
    /// loaded, could not be instantiated, or whose `onStartup` threw.
    pub errors: Vec<String>,
}

impl SciReport {
    /// Construct an empty report.
    pub fn new() -> Self {
        Self::default()
    }

    /// Total number of SCIs that ran successfully.
    pub fn invocation_count(&self) -> usize {
        self.initializers.len()
    }

    /// Whether any SCI failed.
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Whether the report is completely empty (no SCIs and no errors). This
    /// is the expected outcome for a webapp that ships no
    /// `META-INF/services/jakarta.servlet.ServletContainerInitializer`
    /// resources.
    pub fn is_empty(&self) -> bool {
        self.initializers.is_empty() && self.errors.is_empty()
    }
}

/// Run SCI discovery + invocation for the web application registered under
/// `context_id` on `jvm`.
///
/// **Ordering.** This must be called *after*
/// [`crate::classloader::ClassLoaderFactory::webapp_loader`] has installed
/// the webapp's class loader (so `discoverServiceClasses` can see the
/// webapp's resources) and *before* any `Servlet.init()` fires
/// (Servlet-spec ordering: SCIs run first).
///
/// `webapp_root` is the deployed application's document base — the
/// directory containing `WEB-INF/`. It is consumed by
/// [`tomcatrs_webapp::ClassgraphIndex::scan`] to build the
/// `@HandlesTypes` subtype/annotation graph. Passing a path that does
/// not contain a `WEB-INF/` tree is not an error: the index is simply
/// empty and `@HandlesTypes`-annotated SCIs receive an empty handled
/// set.
///
/// **Errors.** Per-SCI failures are recorded in [`SciReport::errors`] and
/// do not return `Err`. A returned `Err` indicates an *infrastructural*
/// failure (no webapp registered for the context, no class loader yet, the
/// JVM worker pool is unreachable, …).
///
/// # Two builds
///
/// * `--features jvm` — does the real work via the
///   `ServletContainerInitializerInvoker` Java helper.
/// * default features — logs that SCI requires the `jvm` feature and
///   returns an empty [`SciReport`].
pub async fn run_sci(
    jvm: &Arc<JvmRuntime>,
    context_id: &ContextId,
    webapp_root: &Path,
) -> Result<SciReport> {
    run_sci_impl(jvm, context_id, webapp_root)
}

// ---------------------------------------------------------------------------
// Real implementation — only compiled with `--features jvm`.
// ---------------------------------------------------------------------------
#[cfg(feature = "jvm")]
fn run_sci_impl(
    jvm: &Arc<JvmRuntime>,
    context_id: &ContextId,
    webapp_root: &Path,
) -> Result<SciReport> {
    use jni::objects::{JObject, JValue};
    use tomcatrs_core::Error;
    use tomcatrs_webapp::ClassgraphIndex;

    // 1. Look up the webapp — must exist (and have a class loader) by the
    //    time `run_sci` runs.
    let webapp = jvm.webapp(context_id).ok_or_else(|| {
        Error::bridge(format!(
            "run_sci: no webapp registered for context '{context_id}'"
        ))
    })?;
    let loader = webapp.class_loader().ok_or_else(|| {
        Error::bridge(format!(
            "run_sci: webapp '{context_id}' has no class loader yet; \
             SCI discovery must run after the loader is built"
        ))
    })?;
    let context_path = webapp.context_id().clone();

    tracing::debug!(
        context_id = %context_id,
        webapp_root = %webapp_root.display(),
        "running ServletContainerInitializer discovery"
    );

    // 2. Build the @HandlesTypes class graph once for this webapp. This
    //    runs entirely in Rust (no JNI), so it is cheap and does not
    //    contend with the JVM worker pool. A scan failure is treated as
    //    "empty index" rather than fatal: it means the webapp had no
    //    classpath to scan (no WEB-INF/), which is unusual but not a
    //    reason to abort SCI dispatch.
    let classgraph = match ClassgraphIndex::scan(webapp_root) {
        Ok(idx) => {
            tracing::debug!(
                context_id = %context_path,
                classes = idx.len(),
                "ClassgraphIndex built for @HandlesTypes scanning"
            );
            idx
        }
        Err(e) => {
            tracing::warn!(
                context_id = %context_path,
                error = %e,
                "ClassgraphIndex::scan failed; @HandlesTypes scans will return empty sets"
            );
            ClassgraphIndex::default()
        }
    };

    // 3. Discover SCI class names, then invoke each one, all on the JNI
    //    funnel. Errors per SCI are accumulated into the report; only an
    //    infrastructural failure (e.g. the JNI dispatch itself failing)
    //    propagates upward.
    jvm.with_env(|env| -> Result<SciReport> {
        // 3a. Discover.
        let names_jobj = env
            .call_static_method(
                "org/apache/tomcatrs/bridge/ServletContainerInitializerInvoker",
                "discoverServiceClasses",
                "(Ljava/lang/ClassLoader;)Ljava/util/List;",
                &[JValue::Object(loader.global_ref().as_obj())],
            )
            .and_then(|v| v.l())
            .map_err(|e| {
                let _ = env.exception_clear();
                Error::bridge(format!("SCI discoverServiceClasses failed: {e}"))
            })?;
        let names = read_string_list(env, &names_jobj)?;

        if names.is_empty() {
            tracing::debug!(
                context_id = %context_path,
                "no ServletContainerInitializer service entries found"
            );
            return Ok(SciReport::new());
        }

        tracing::info!(
            context_id = %context_path,
            sci_count = names.len(),
            "discovered ServletContainerInitializer(s); invoking onStartup"
        );

        // 3b. Build the ServletContext facade once. Every SCI is passed the
        //     same `ctx`.
        let ctx_path_jstr = env
            .new_string(&context_path)
            .map_err(|e| Error::bridge(format!("new_string(context_path) failed: {e}")))?;
        let ctx = env
            .new_object(
                "org/apache/tomcatrs/bridge/TomcatRsServletContext",
                "(Ljava/lang/String;)V",
                &[JValue::Object(&JObject::from(ctx_path_jstr))],
            )
            .map_err(|e| {
                let _ = env.exception_clear();
                Error::bridge(format!(
                    "constructing TomcatRsServletContext for SCI failed: {e}"
                ))
            })?;

        // 3c. For each SCI, read its @HandlesTypes targets, query the
        //     class graph, load each match, build the Set<Class<?>> and
        //     invoke.
        let mut report = SciReport::new();
        for name in names {
            // 3c-i. Load the SCI class through the webapp loader so we
            // can ask Java to read its @HandlesTypes annotation. Class
            // loading is what the invoker does anyway; doing it here
            // additionally is cheap because the JVM caches class
            // resolutions in the loader.
            let sci_class = match load_class_via_loader(env, &loader, &name) {
                Ok(c) => c,
                Err(e) => {
                    let _ = env.exception_clear();
                    let msg = format!("SCI '{name}' failed: cannot load class: {e}");
                    tracing::warn!(
                        context_id = %context_path,
                        sci = %name,
                        error = %e,
                        "loading SCI class failed; reporting failure and skipping"
                    );
                    report.errors.push(msg);
                    continue;
                }
            };

            // 3c-ii. Pull the FQCNs out of @HandlesTypes (or get an
            // empty list if the SCI declares no @HandlesTypes).
            let targets = match read_handles_type_names(env, &sci_class) {
                Ok(t) => t,
                Err(e) => {
                    let _ = env.exception_clear();
                    tracing::warn!(
                        context_id = %context_path,
                        sci = %name,
                        error = %e,
                        "readHandlesTypeNames failed; treating @HandlesTypes as empty"
                    );
                    Vec::new()
                }
            };

            // 3c-iii. Walk the class graph for each target FQCN and load
            // each matched class through the webapp loader.
            let handled = if targets.is_empty() {
                empty_class_set(env)?
            } else {
                build_handled_type_set(env, &loader, &classgraph, &targets, &name)?
            };

            let name_jstr = env
                .new_string(&name)
                .map_err(|e| Error::bridge(format!("new_string({name}) failed: {e}")))?;

            let invoke_result = env.call_static_method(
                "org/apache/tomcatrs/bridge/ServletContainerInitializerInvoker",
                "invoke",
                "(Ljava/lang/ClassLoader;Ljava/lang/String;Ljava/util/Set;\
                 Ljakarta/servlet/ServletContext;)V",
                &[
                    JValue::Object(loader.global_ref().as_obj()),
                    JValue::Object(&JObject::from(name_jstr)),
                    JValue::Object(&handled),
                    JValue::Object(&ctx),
                ],
            );

            match invoke_result {
                Ok(_) => {
                    tracing::info!(
                        context_id = %context_path,
                        sci = %name,
                        "ServletContainerInitializer.onStartup completed"
                    );
                    report.initializers.push(name);
                }
                Err(e) => {
                    // Drain the pending Java exception (if any) so the worker
                    // is usable for the next SCI.
                    let _ = env.exception_clear();
                    let msg = format!("SCI '{name}' failed: {e}");
                    tracing::warn!(
                        context_id = %context_path,
                        sci = %name,
                        error = %e,
                        "ServletContainerInitializer.invoke failed"
                    );
                    report.errors.push(msg);
                }
            }
        }

        // 3d. After all SCIs have run, drive `Servlet.init(ServletConfig)`
        //     on every dynamically-registered servlet whose load-on-startup
        //     is non-negative (Servlet 6 §10.3). Spring's `DispatcherServlet`
        //     declares load-on-startup=1, so without this step a subsequent
        //     dispatch would hit a not-yet-init'd servlet and fall over
        //     inside Spring's `FrameworkServlet.processRequest`.
        match env.call_method(&ctx, "initLoadOnStartupServlets", "()I", &[]) {
            Ok(v) => match v.i() {
                Ok(n) => tracing::info!(
                    context_id = %context_path,
                    initialised = n,
                    "load-on-startup servlets initialised"
                ),
                Err(_) => tracing::warn!(
                    "initLoadOnStartupServlets returned non-int — bridge JAR out of sync?"
                ),
            },
            Err(e) => {
                let _ = env.exception_clear();
                tracing::warn!(
                    context_id = %context_path,
                    error = %e,
                    "initLoadOnStartupServlets failed; dynamic servlets may not be ready"
                );
            }
        }

        Ok(report)
    })
}

/// Load `name` via `loader` (`Class.forName(name, true, loader)`),
/// returning the resulting `Class<?>` as a `JObject`. The result is a
/// local reference: the caller must not hold it across worker
/// boundaries.
#[cfg(feature = "jvm")]
fn load_class_via_loader<'l>(
    env: &mut jni::JNIEnv<'l>,
    loader: &crate::jvm::ClassLoaderHandle,
    name: &str,
) -> Result<jni::objects::JObject<'l>> {
    use jni::objects::{JObject, JValue};
    use tomcatrs_core::Error;

    let name_jstr = env
        .new_string(name)
        .map_err(|e| Error::bridge(format!("new_string({name}) failed: {e}")))?;
    let class = env
        .call_static_method(
            "java/lang/Class",
            "forName",
            "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;",
            &[
                JValue::Object(&JObject::from(name_jstr)),
                JValue::Bool(jni::sys::JNI_TRUE),
                JValue::Object(loader.global_ref().as_obj()),
            ],
        )
        .and_then(|v| v.l())
        .map_err(|e| Error::bridge(format!("Class.forName({name}) failed: {e}")))?;
    Ok(class)
}

/// Call
/// `ServletContainerInitializerInvoker.readHandlesTypeNames(sciClass)`
/// and turn the resulting `String[]` into a `Vec<String>` of dotted
/// FQCNs. An SCI with no `@HandlesTypes` yields an empty vector.
#[cfg(feature = "jvm")]
fn read_handles_type_names(
    env: &mut jni::JNIEnv,
    sci_class: &jni::objects::JObject,
) -> Result<Vec<String>> {
    use jni::objects::{JObjectArray, JString, JValue};
    use tomcatrs_core::Error;

    let arr_obj = env
        .call_static_method(
            "org/apache/tomcatrs/bridge/ServletContainerInitializerInvoker",
            "readHandlesTypeNames",
            "(Ljava/lang/Class;)[Ljava/lang/String;",
            &[JValue::Object(sci_class)],
        )
        .and_then(|v| v.l())
        .map_err(|e| {
            let _ = env.exception_clear();
            Error::bridge(format!("readHandlesTypeNames failed: {e}"))
        })?;
    let arr = JObjectArray::from(arr_obj);
    let len = env
        .get_array_length(&arr)
        .map_err(|e| Error::bridge(format!("get_array_length(handlesTypes) failed: {e}")))?;
    let mut out = Vec::with_capacity(len.max(0) as usize);
    for i in 0..len {
        let elem = env
            .get_object_array_element(&arr, i)
            .map_err(|e| Error::bridge(format!("array[{i}] failed: {e}")))?;
        let jstr = JString::from(elem);
        let s: String = env
            .get_string(&jstr)
            .map_err(|e| Error::bridge(format!("get_string(handlesTypes[{i}]) failed: {e}")))?
            .into();
        out.push(s);
    }
    Ok(out)
}

/// Build a `java.util.HashSet<Class<?>>` populated with every class
/// from the webapp's classpath that is handled by any of `targets`,
/// loaded through `loader`. Classes that fail to load are logged and
/// skipped — the SCI still sees a usable (possibly smaller) set.
#[cfg(feature = "jvm")]
fn build_handled_type_set<'l>(
    env: &mut jni::JNIEnv<'l>,
    loader: &crate::jvm::ClassLoaderHandle,
    classgraph: &tomcatrs_webapp::ClassgraphIndex,
    targets: &[String],
    sci_name: &str,
) -> Result<jni::objects::JObject<'l>> {
    use jni::objects::JValue;
    use std::collections::HashSet;
    use tomcatrs_core::Error;

    // De-duplicate matched class names across targets.
    let mut matched: HashSet<String> = HashSet::new();
    for target in targets {
        for hit in classgraph.classes_handled_by(target) {
            matched.insert(hit.to_string());
        }
    }

    let set = env
        .new_object("java/util/HashSet", "()V", &[])
        .map_err(|e| {
            let _ = env.exception_clear();
            Error::bridge(format!("new HashSet() failed: {e}"))
        })?;

    if matched.is_empty() {
        tracing::debug!(
            sci = %sci_name,
            target_count = targets.len(),
            "no classpath classes match any @HandlesTypes target"
        );
        return Ok(set);
    }

    let mut loaded = 0usize;
    let mut failed = 0usize;
    for fqcn in &matched {
        match load_class_via_loader(env, loader, fqcn) {
            Ok(cls) => {
                match env.call_method(
                    &set,
                    "add",
                    "(Ljava/lang/Object;)Z",
                    &[JValue::Object(&cls)],
                ) {
                    Ok(_) => {
                        loaded += 1;
                    }
                    Err(e) => {
                        let _ = env.exception_clear();
                        tracing::warn!(
                            sci = %sci_name,
                            class = %fqcn,
                            error = %e,
                            "HashSet.add(matched class) failed; skipping"
                        );
                        failed += 1;
                    }
                }
            }
            Err(e) => {
                let _ = env.exception_clear();
                tracing::debug!(
                    sci = %sci_name,
                    class = %fqcn,
                    error = %e,
                    "loading @HandlesTypes-matched class failed; skipping"
                );
                failed += 1;
            }
        }
    }
    tracing::info!(
        sci = %sci_name,
        target_count = targets.len(),
        matched = matched.len(),
        loaded,
        failed,
        "built @HandlesTypes set"
    );

    Ok(set)
}

/// Turn a `java.util.List<String>` into a `Vec<String>` by calling
/// `size()` + `get(i)` over JNI. Used to drain
/// `discoverServiceClasses`'s return value.
#[cfg(feature = "jvm")]
fn read_string_list(env: &mut jni::JNIEnv, list: &jni::objects::JObject) -> Result<Vec<String>> {
    use jni::objects::{JString, JValue};
    use tomcatrs_core::Error;

    let size = env
        .call_method(list, "size", "()I", &[])
        .and_then(|v| v.i())
        .map_err(|e| {
            let _ = env.exception_clear();
            Error::bridge(format!("List.size() failed: {e}"))
        })?;
    if size < 0 {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(size as usize);
    for i in 0..size {
        let elem = env
            .call_method(list, "get", "(I)Ljava/lang/Object;", &[JValue::Int(i)])
            .and_then(|v| v.l())
            .map_err(|e| {
                let _ = env.exception_clear();
                Error::bridge(format!("List.get({i}) failed: {e}"))
            })?;
        let jstr = JString::from(elem);
        let s: String = env
            .get_string(&jstr)
            .map_err(|e| Error::bridge(format!("get_string(List[{i}]) failed: {e}")))?
            .into();
        out.push(s);
    }
    Ok(out)
}

/// Build an empty `java.util.HashSet<Class<?>>` to pass as the
/// handled-types argument until the `@HandlesTypes` scanner lands.
#[cfg(feature = "jvm")]
fn empty_class_set<'l>(env: &mut jni::JNIEnv<'l>) -> Result<jni::objects::JObject<'l>> {
    use tomcatrs_core::Error;
    env.new_object("java/util/HashSet", "()V", &[])
        .map_err(|e| {
            let _ = env.exception_clear();
            Error::bridge(format!("new HashSet() failed: {e}"))
        })
}

// ---------------------------------------------------------------------------
// Stub implementation — compiled with default features (no JDK required).
// ---------------------------------------------------------------------------
#[cfg(not(feature = "jvm"))]
fn run_sci_impl(
    _jvm: &Arc<JvmRuntime>,
    context_id: &ContextId,
    _webapp_root: &Path,
) -> Result<SciReport> {
    tracing::info!(
        context_id = %context_id,
        "ServletContainerInitializer discovery requires --features jvm; \
         returning an empty SciReport"
    );
    Ok(SciReport::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sci_report_defaults_are_empty() {
        let r = SciReport::new();
        assert!(r.is_empty());
        assert!(!r.has_errors());
        assert_eq!(r.invocation_count(), 0);
    }

    #[test]
    fn sci_report_tracks_invocations_and_errors() {
        let r = SciReport {
            initializers: vec!["com.example.OkSci".into()],
            errors: vec!["com.example.BadSci: boom".into()],
        };
        assert!(!r.is_empty());
        assert!(r.has_errors());
        assert_eq!(r.invocation_count(), 1);
    }

    #[test]
    fn sci_report_eq() {
        let a = SciReport {
            initializers: vec!["a".into(), "b".into()],
            errors: Vec::new(),
        };
        let b = SciReport {
            initializers: vec!["a".into(), "b".into()],
            errors: Vec::new(),
        };
        assert_eq!(a, b);
    }

    /// Without the `jvm` feature `run_sci` must succeed with an empty report
    /// and no errors — the no-JVM path is informational-only.
    #[cfg(not(feature = "jvm"))]
    #[tokio::test]
    async fn run_sci_without_jvm_returns_empty_report() {
        let jvm = Arc::new(JvmRuntime::default());
        let ctx: ContextId = "/app".to_string();
        let root = std::path::PathBuf::from("/no/such/webapp");
        let report = run_sci(&jvm, &ctx, &root)
            .await
            .expect("stub run_sci never errors");
        assert!(report.is_empty());
        assert_eq!(report.invocation_count(), 0);
        assert!(!report.has_errors());
    }
}
