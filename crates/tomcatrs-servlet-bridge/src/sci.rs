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
//!   run_sci(jvm, ctx_id)
//!       │
//!       ▼  (under --features jvm)
//!   JvmRuntime::with_env(env => {
//!       discoverServiceClasses(loader)  ───▶ Vec<String>  (the class names)
//!       for each name:
//!         readHandlesTypes(class)       ───▶ Class<?>[]   (TODO: scan for types)
//!         invoke(loader, name, EMPTY, ctx)
//!   })
//! ```
//!
//! Under default features (no JVM) [`run_sci`] returns an empty
//! [`SciReport`] and logs that SCI requires the `jvm` feature.
//!
//! # The honest gap: `@HandlesTypes` scanning is partial
//!
//! The Servlet spec says the container is responsible for scanning the
//! webapp's classes/jars for types that *extend, implement, or are annotated
//! with* any of the classes named in an SCI's `@HandlesTypes` value array,
//! and passing the resulting `Set<Class<?>>` as the first argument to
//! `onStartup`.
//!
//! That scan is a substantial separate undertaking: it requires walking
//! every `.class` file in `WEB-INF/classes` and every `WEB-INF/lib/*.jar`,
//! parsing class metadata (Tomcat uses the BCEL/Commons-DBCP scanner;
//! Spring's `MetadataReader` does the same job differently), and tracking
//! the supertype/interface/annotation closure. None of that is in scope for
//! this task.
//!
//! For v1, [`run_sci`] passes every SCI an **empty** `HashSet<Class<?>>` as
//! its handled-types argument. This is "safe but degraded":
//!
//! * SCIs that do not declare `@HandlesTypes` (e.g. a custom bootstrapper
//!   that hard-codes its initialisation) run **exactly correctly**.
//! * SCIs that *do* declare `@HandlesTypes` (e.g. Spring Boot's
//!   `SpringServletContainerInitializer @HandlesTypes(WebApplicationInitializer.class)`)
//!   are invoked, but `c` is empty, so they either:
//!   - silently skip their work (Spring's case — no `WebApplicationInitializer`
//!     classes are reported, so nothing is bootstrapped); or
//!   - fall through to a manual classpath search of their own (some
//!     frameworks do this defensively).
//!
//! The follow-up issue is a Tomcat-style annotation scanner that produces
//! the `Class<?>[]` set for every SCI's `@HandlesTypes` from the webapp's
//! class path. Wiring that into [`run_sci`] is purely additive: it replaces
//! the `HashSet::new()` call below.
//!
//! # Two builds, one API
//!
//! As with the rest of the crate, [`run_sci`] and [`SciReport`] exist on
//! both feature paths. The default-feature path is a no-op that logs and
//! returns an empty report; the `--features jvm` path does the real work.

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
pub async fn run_sci(jvm: &Arc<JvmRuntime>, context_id: &ContextId) -> Result<SciReport> {
    run_sci_impl(jvm, context_id)
}

// ---------------------------------------------------------------------------
// Real implementation — only compiled with `--features jvm`.
// ---------------------------------------------------------------------------
#[cfg(feature = "jvm")]
fn run_sci_impl(jvm: &Arc<JvmRuntime>, context_id: &ContextId) -> Result<SciReport> {
    use jni::objects::{JObject, JValue};
    use tomcatrs_core::Error;

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
        "running ServletContainerInitializer discovery"
    );

    // 2. Discover SCI class names, then invoke each one, all on the JNI
    //    funnel. Errors per SCI are accumulated into the report; only an
    //    infrastructural failure (e.g. the JNI dispatch itself failing)
    //    propagates upward.
    jvm.with_env(|env| -> Result<SciReport> {
        // 2a. Discover.
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

        // 2b. Build the ServletContext facade once. Every SCI is passed the
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

        // 2c. For each SCI, build the (empty for v1) handled-types set and
        //     invoke. Per-SCI failures are accumulated.
        //
        // TODO(@HandlesTypes): scan the webapp's classpath for classes that
        // extend/implement/are-annotated-with any class returned by
        // `ServletContainerInitializerInvoker.readHandlesTypes(sciClass)`
        // and pass that set instead of the empty one. The plumbing is here;
        // only the classpath scan needs writing.
        let mut report = SciReport::new();
        for name in names {
            let handled = empty_class_set(env)?;
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

        Ok(report)
    })
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
fn run_sci_impl(_jvm: &Arc<JvmRuntime>, context_id: &ContextId) -> Result<SciReport> {
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
        let report = run_sci(&jvm, &ctx)
            .await
            .expect("stub run_sci never errors");
        assert!(report.is_empty());
        assert_eq!(report.invocation_count(), 0);
        assert!(!report.has_errors());
    }
}
