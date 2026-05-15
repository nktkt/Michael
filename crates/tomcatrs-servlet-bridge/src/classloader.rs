//! Tomcat's classloader hierarchy, reproduced on the embedded JVM.
//!
//! # Tomcat's hierarchy
//!
//! Apache Tomcat does not use the plain JVM parent-first delegation model for
//! web applications. Its classloader tree is:
//!
//! ```text
//!   Bootstrap   (the JVM's own bootstrap loader — `null` parent)
//!       │
//!   System      (the application/system loader — Tomcat's own `bin/*.jar`)
//!       │
//!   Common      (`$CATALINA_HOME/lib` — classes shared by the container and
//!       │        every web application: the Servlet API, etc.)
//!       │
//!   ┌───┴───┬───────────┐
//! Webapp1 Webapp2  …  WebappN   (one *per deployed web application*)
//! ```
//!
//! The **Common** loader is an ordinary parent-first `java.net.URLClassLoader`.
//! Each **Webapp** loader, however, is *child-first* (a.k.a. parent-last): when
//! asked for a class it consults *itself* before delegating to Common, so that
//! a web application's bundled copy of a library wins over the container's.
//! There are a handful of always-delegated package prefixes (`java.*`,
//! `jakarta.servlet.*`, …) that Tomcat never lets a webapp override; those are
//! the exception that the real `WebappClassLoaderBase` encodes.
//!
//! A Webapp loader's own search path, in order, is:
//!
//! 1. `/WEB-INF/classes` — unpacked application classes;
//! 2. `/WEB-INF/lib/*.jar` — bundled dependency jars.
//!
//! # What this module builds
//!
//! [`ClassLoaderFactory`] constructs the JVM-side loaders over JNI. For
//! **v1.0.0** the webapp loader is materialised as a `java.net.URLClassLoader`
//! with the correct URL array and parent (Common). That gives correct
//! *visibility* — the webapp sees its own classes and jars plus everything
//! Common exposes — but uses standard parent-first delegation. A fully
//! child-first loader equivalent to Tomcat's `WebappClassLoaderBase` (with the
//! delegate-prefix allow-list and reversed `loadClass` order) is **future
//! work**; the [`WebappClassLoaderConfig::delegate`] flag is already threaded
//! through so callers can express the intent today.
//!
//! # Two builds, one API
//!
//! As with the rest of the crate, every public item exists on **both** the
//! `jvm` and the default feature path. The pure data types
//! ([`ClassLoaderSpec`], [`ClassLoaderKind`], [`WebappClassLoaderConfig`]) and
//! the [`path_to_file_url`] helper are feature-independent and fully unit
//! tested without a JDK. The JNI-touching methods of [`ClassLoaderFactory`]
//! return [`tomcatrs_core::Error::Bridge`] when the `jvm` feature is off.

use std::path::{Path, PathBuf};

use tomcatrs_core::ContextId;

/// Which tier of [Tomcat's classloader hierarchy](self) a loader belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClassLoaderKind {
    /// The **Common** loader: classes shared by the container and every web
    /// application (the Servlet API, container utilities, …). Parent-first.
    Common,
    /// A **Webapp** loader: one per deployed web application, child-first in
    /// Tomcat (see the module docs for the v1.0.0 caveat).
    Webapp,
}

/// A resolved, JVM-agnostic description of one classloader: which tier it is
/// and the ordered list of filesystem entries (directories and/or jars) on its
/// search path.
///
/// This is the bridge between the Rust-side view of a webapp's layout and the
/// `URL[]` that ultimately gets handed to a `java.net.URLClassLoader`. It is a
/// plain data type with no JVM dependency, so it can be built and inspected
/// without the `jvm` feature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassLoaderSpec {
    /// The tier this loader occupies.
    pub kind: ClassLoaderKind,
    /// The ordered search path: directories and jar files, in delegation
    /// order. For a [`ClassLoaderKind::Webapp`] this is `WEB-INF/classes`
    /// followed by each `WEB-INF/lib/*.jar`.
    pub urls: Vec<PathBuf>,
}

impl ClassLoaderSpec {
    /// Construct a spec for the given tier from an ordered list of search-path
    /// entries.
    pub fn new(kind: ClassLoaderKind, urls: Vec<PathBuf>) -> Self {
        Self { kind, urls }
    }

    /// Render every entry on the search path as a `file:` URL string, in order.
    ///
    /// This is exactly the array that becomes the `URL[]` argument to
    /// `java.net.URLClassLoader`'s constructor. See [`path_to_file_url`] for
    /// the per-path conversion rules.
    pub fn file_urls(&self) -> Vec<String> {
        self.urls.iter().map(|p| path_to_file_url(p)).collect()
    }
}

/// Everything needed to build one web application's classloader.
///
/// Construct one directly, or with [`webapp_config`] / [`webapp_config_parts`]
/// from a deployed application's layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebappClassLoaderConfig {
    /// The context id of the web application this loader serves (e.g. `/myapp`
    /// or `""` for the root context). Used purely for diagnostics/identity.
    pub context_id: ContextId,
    /// The application's `WEB-INF/classes` directory, if it has one.
    pub classes_dir: Option<PathBuf>,
    /// The application's `WEB-INF/lib/*.jar` files, in load order.
    pub jar_files: Vec<PathBuf>,
    /// Delegation model. `true` selects standard JVM **parent-first**
    /// delegation; `false` selects **child-first** (parent-last) delegation,
    /// which is Tomcat's default for web applications and therefore the
    /// default produced by the [`webapp_config`] helpers.
    ///
    /// Note the v1.0.0 caveat in the [module docs](self): regardless of this
    /// flag the JVM-side loader is currently a parent-first `URLClassLoader`;
    /// the flag records the *intended* model for when the child-first
    /// `WebappClassLoaderBase` equivalent lands.
    pub delegate: bool,
}

impl WebappClassLoaderConfig {
    /// Build a config explicitly. Prefer [`webapp_config`] /
    /// [`webapp_config_parts`] when starting from a deployed application's
    /// layout, as they pick Tomcat's child-first default for you.
    pub fn new(
        context_id: impl Into<ContextId>,
        classes_dir: Option<PathBuf>,
        jar_files: Vec<PathBuf>,
        delegate: bool,
    ) -> Self {
        Self {
            context_id: context_id.into(),
            classes_dir,
            jar_files,
            delegate,
        }
    }

    /// The ordered search path for this webapp loader: `WEB-INF/classes` first
    /// (when present), then every `WEB-INF/lib/*.jar` in order.
    pub fn search_path(&self) -> Vec<PathBuf> {
        let mut urls = Vec::with_capacity(self.jar_files.len() + 1);
        if let Some(classes) = &self.classes_dir {
            urls.push(classes.clone());
        }
        urls.extend(self.jar_files.iter().cloned());
        urls
    }

    /// Resolve this config into a [`ClassLoaderSpec`] of kind
    /// [`ClassLoaderKind::Webapp`].
    pub fn to_spec(&self) -> ClassLoaderSpec {
        ClassLoaderSpec::new(ClassLoaderKind::Webapp, self.search_path())
    }

    /// Render this webapp loader's search path as `file:` URL strings, in
    /// delegation order.
    pub fn file_urls(&self) -> Vec<String> {
        self.search_path()
            .iter()
            .map(|p| path_to_file_url(p))
            .collect()
    }

    /// Whether this loader is configured for child-first (parent-last)
    /// delegation — the Tomcat default for web applications.
    pub fn is_child_first(&self) -> bool {
        !self.delegate
    }
}

/// Build a [`WebappClassLoaderConfig`] from a deployed application's layout
/// parts, using Tomcat's **child-first** default delegation.
///
/// This takes the raw paths rather than a `&tomcatrs_webapp::Webapp` on
/// purpose: `tomcatrs-webapp` is *not* a dependency of this crate, and adding
/// one to convert a handful of paths is not worth the coupling. Callers that
/// hold a `Webapp` simply pass `webapp.classes_dir()` and `webapp.lib_jars()`.
pub fn webapp_config_parts(
    context_id: impl Into<ContextId>,
    classes_dir: Option<PathBuf>,
    jar_files: Vec<PathBuf>,
) -> WebappClassLoaderConfig {
    WebappClassLoaderConfig::new(
        context_id,
        classes_dir,
        jar_files,
        /* delegate = */ false,
    )
}

/// Build a [`WebappClassLoaderConfig`] from a context id and a webapp's
/// `WEB-INF/classes` directory plus `WEB-INF/lib/*.jar` slice, using Tomcat's
/// **child-first** default delegation.
///
/// This is the convenient form for callers that have borrowed slices/paths
/// (for example straight off a `tomcatrs_webapp::Webapp`):
///
/// ```no_run
/// # use std::path::Path;
/// # use tomcatrs_servlet_bridge::classloader::webapp_config;
/// // let app: tomcatrs_webapp::Webapp = ...;
/// // let cfg = webapp_config(app.context_path(), app.classes_dir(), app.lib_jars());
/// let cfg = webapp_config("/myapp", Some(Path::new("/srv/myapp/WEB-INF/classes")), &[]);
/// assert!(cfg.is_child_first());
/// ```
pub fn webapp_config(
    context_id: impl Into<ContextId>,
    classes_dir: Option<&Path>,
    jar_files: &[PathBuf],
) -> WebappClassLoaderConfig {
    webapp_config_parts(
        context_id,
        classes_dir.map(Path::to_path_buf),
        jar_files.to_vec(),
    )
}

/// Convert a filesystem path to a `file:` URL string suitable for
/// `java.net.URLClassLoader`.
///
/// The rules, chosen to match what the JDK's own `File.toURI().toURL()`
/// produces closely enough for classloading:
///
/// * the path is made into a `file://` URL with an empty authority;
/// * a directory path is given a trailing `/` (a `URLClassLoader` treats a
///   URL without a trailing slash as a jar and one with as a directory) — but
///   only when the path *already* ends in a separator or we can otherwise tell
///   it is a directory is not knowable from the string alone, so callers that
///   need the directory form should pass a path that already ends in `/`;
/// * characters that are unsafe in a URL — space, `#`, `?`, `%`, control
///   chars, and non-ASCII bytes — are percent-encoded;
/// * on Windows, backslashes are normalised to `/` and a drive-letter path
///   like `C:\x` becomes `file:///C:/x`.
///
/// **Directory semantics:** if `path` exists and is a directory, the
/// emitted URL ends with `/`. This matters because `java.net.URLClassLoader`
/// treats URLs *without* a trailing slash as JAR-file URLs and URLs *with*
/// one as directory classpath entries. Without the slash, classes in the
/// directory fail to load with `ClassNotFoundException`.
///
/// The directory check is filesystem-aware — for paths that don't exist
/// yet (tests, futures), call [`path_to_file_url_dir`] explicitly to force
/// the trailing slash.
///
/// Note: aside from the directory probe, this is a pure string
/// transformation so it works without the `jvm` feature and in tests.
pub fn path_to_file_url(path: &Path) -> String {
    let is_dir = path.is_dir();
    let url = encode_path_as_file_url(path);
    if is_dir && !url.ends_with('/') {
        format!("{url}/")
    } else {
        url
    }
}

/// Like [`path_to_file_url`] but unconditionally appends a trailing `/`,
/// producing a URL that `URLClassLoader` treats as a classpath directory
/// entry. Use this for paths that may not exist yet on disk (test
/// fixtures, configuration that hasn't been materialised, etc.).
pub fn path_to_file_url_dir(path: &Path) -> String {
    let url = encode_path_as_file_url(path);
    if url.ends_with('/') {
        url
    } else {
        format!("{url}/")
    }
}

fn encode_path_as_file_url(path: &Path) -> String {
    let raw = path.to_string_lossy();
    // Normalise separators so Windows paths produce forward-slash URLs.
    let normalised = raw.replace('\\', "/");

    let mut encoded = String::with_capacity(normalised.len() + 8);
    for byte in normalised.as_bytes() {
        let b = *byte;
        match b {
            // RFC 3986 unreserved set plus the path-safe sub-delims and the
            // separators that are legal in a URL path. Crucially `/`, `:`,
            // `-`, `.`, `_`, `~` pass through untouched so drive letters and
            // ordinary paths stay readable.
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'/'
            | b':'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'+'
            | b'@'
            | b','
            | b'='
            | b';'
            | b'$'
            | b'!'
            | b'('
            | b')'
            | b'*'
            | b'\'' => encoded.push(b as char),
            // Everything else — spaces, `#`, `?`, `%`, control bytes, and the
            // continuation/lead bytes of any non-ASCII UTF-8 sequence — is
            // percent-encoded.
            _ => {
                encoded.push('%');
                encoded.push(hex_digit(b >> 4));
                encoded.push(hex_digit(b & 0x0f));
            }
        }
    }

    // An absolute path already starts with `/`; a relative one does not, and
    // we still want a syntactically valid `file:` URL, so prefix a `/`.
    if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    }
}

/// Map a 4-bit nibble to its uppercase hexadecimal ASCII digit.
fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'A' + (nibble - 10)) as char,
        _ => unreachable!("nibble is masked to 4 bits"),
    }
}

// ===========================================================================
// Real implementation — only compiled with `--features jvm`.
// ===========================================================================
#[cfg(feature = "jvm")]
mod imp {
    use super::*;

    use jni::objects::{GlobalRef, JObject, JObjectArray, JValue};
    use jni::JNIEnv;
    use tomcatrs_core::{Error, Result};

    /// Builds the JVM-side classloaders of [Tomcat's hierarchy](super).
    ///
    /// Every method needs a `&mut jni::JNIEnv` for the *current* (JNI-attached)
    /// thread — see the per-worker attach design in [`crate::jvm`]. The
    /// returned [`GlobalRef`]s outlive the local JNI frame and can be stored in
    /// the webapp registry.
    #[derive(Debug, Default, Clone, Copy)]
    pub struct ClassLoaderFactory;

    impl ClassLoaderFactory {
        /// Create a factory. It is stateless; the JVM is reached purely through
        /// the `JNIEnv` passed to each method.
        pub fn new() -> Self {
            Self
        }

        /// Build the **Common** loader: a parent-first `java.net.URLClassLoader`
        /// whose parent is the JVM's system/application loader
        /// (`ClassLoader.getSystemClassLoader()`), and whose URLs are
        /// `urls.urls` (empty by default — the container's shared jars are
        /// usually already on the system classpath via
        /// [`crate::jvm::JvmConfig::classpath`], but extra entries can be
        /// layered here).
        ///
        /// # Errors
        ///
        /// [`Error::Bridge`] if any JNI call fails or a pending Java exception
        /// is detected.
        pub fn common_loader(&self, env: &mut JNIEnv) -> Result<GlobalRef> {
            self.common_loader_with(env, &[])
        }

        /// Like [`common_loader`](Self::common_loader) but with explicit extra
        /// URLs layered onto the Common loader's search path.
        pub fn common_loader_with(&self, env: &mut JNIEnv, urls: &[PathBuf]) -> Result<GlobalRef> {
            let system = system_class_loader(env)?;
            let url_strings: Vec<String> = urls.iter().map(|p| path_to_file_url(p)).collect();
            let loader = new_url_class_loader(env, &url_strings, &system)?;
            globalize(env, loader)
        }

        /// Build a **Webapp** loader for `config`, parented to `common`.
        ///
        /// The loader's `URL[]` is the webapp's search path —
        /// `WEB-INF/classes` then `WEB-INF/lib/*.jar` — and its parent is the
        /// Common loader, so the application sees the Servlet API and the rest
        /// of the container's shared classes.
        ///
        /// For v1.0.0 this is a parent-first `URLClassLoader` even when
        /// [`WebappClassLoaderConfig::delegate`] is `false`; see the
        /// [module docs](super) for the child-first follow-up. The intended
        /// model is logged so the gap is visible at run time.
        ///
        /// # Errors
        ///
        /// [`Error::Bridge`] if any JNI call fails or a pending Java exception
        /// is detected.
        pub fn webapp_loader(
            &self,
            env: &mut JNIEnv,
            config: &WebappClassLoaderConfig,
            common: &GlobalRef,
        ) -> Result<GlobalRef> {
            if config.is_child_first() {
                tracing::debug!(
                    context_id = %config.context_id,
                    "webapp classloader requested child-first; v1.0.0 builds a \
                     parent-first URLClassLoader (WebappClassLoaderBase equivalent \
                     is future work)"
                );
            }
            let url_strings = config.file_urls();
            let parent: &JObject = common.as_obj();
            let loader = new_url_class_loader(env, &url_strings, parent)?;
            globalize(env, loader)
        }

        /// Load (but do not instantiate) the class `fqcn` through `loader`.
        ///
        /// `fqcn` is a fully-qualified, dot-separated class name, e.g.
        /// `com.example.MyServlet`. The returned [`GlobalRef`] refers to the
        /// `java.lang.Class` object.
        ///
        /// # Errors
        ///
        /// [`Error::Bridge`] if the class cannot be found or any JNI call
        /// fails. A `ClassNotFoundException` on the Java side is surfaced as a
        /// bridge error.
        pub fn load_class(
            &self,
            env: &mut JNIEnv,
            loader: &GlobalRef,
            fqcn: &str,
        ) -> Result<GlobalRef> {
            let name = env
                .new_string(fqcn)
                .map_err(|e| Error::bridge(format!("new_string({fqcn}) failed: {e}")))?;
            // Class.forName(String name, boolean initialize, ClassLoader loader)
            let class = env
                .call_static_method(
                    "java/lang/Class",
                    "forName",
                    "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;",
                    &[
                        JValue::Object(&JObject::from(name)),
                        JValue::Bool(false as u8),
                        JValue::Object(loader.as_obj()),
                    ],
                )
                .and_then(|v| v.l())
                .map_err(|e| {
                    check_and_clear_exception(env);
                    Error::bridge(format!("Class.forName(\"{fqcn}\") failed: {e}"))
                })?;
            globalize(env, class)
        }

        /// Load `fqcn` through `loader` and instantiate it via its public
        /// no-argument constructor.
        ///
        /// Equivalent to Java's `loader.loadClass(fqcn).getDeclaredConstructor()
        /// .newInstance()`. The returned [`GlobalRef`] refers to the new
        /// object.
        ///
        /// # Errors
        ///
        /// [`Error::Bridge`] if the class cannot be found, has no accessible
        /// no-arg constructor, the constructor throws, or any JNI call fails.
        pub fn instantiate(
            &self,
            env: &mut JNIEnv,
            loader: &GlobalRef,
            fqcn: &str,
        ) -> Result<GlobalRef> {
            let class = self.load_class(env, loader, fqcn)?;
            // Class.newInstance() is deprecated but adequate for a public
            // no-arg constructor and avoids reflecting a Constructor object.
            let instance = env
                .call_method(class.as_obj(), "newInstance", "()Ljava/lang/Object;", &[])
                .and_then(|v| v.l())
                .map_err(|e| {
                    check_and_clear_exception(env);
                    Error::bridge(format!(
                        "instantiating \"{fqcn}\" via no-arg constructor failed: {e}"
                    ))
                })?;
            globalize(env, instance)
        }
    }

    /// Fetch `ClassLoader.getSystemClassLoader()` as a local reference.
    fn system_class_loader<'l>(env: &mut JNIEnv<'l>) -> Result<JObject<'l>> {
        env.call_static_method(
            "java/lang/ClassLoader",
            "getSystemClassLoader",
            "()Ljava/lang/ClassLoader;",
            &[],
        )
        .and_then(|v| v.l())
        .map_err(|e| {
            check_and_clear_exception(env);
            Error::bridge(format!("getSystemClassLoader() failed: {e}"))
        })
    }

    /// Construct a `java.net.URLClassLoader` from an array of `file:` URL
    /// strings and a parent loader.
    fn new_url_class_loader<'l>(
        env: &mut JNIEnv<'l>,
        url_strings: &[String],
        parent: &JObject,
    ) -> Result<JObject<'l>> {
        let urls = build_url_array(env, url_strings)?;
        env.new_object(
            "java/net/URLClassLoader",
            "([Ljava/net/URL;Ljava/lang/ClassLoader;)V",
            &[JValue::Object(&urls), JValue::Object(parent)],
        )
        .map_err(|e| {
            check_and_clear_exception(env);
            Error::bridge(format!(
                "new URLClassLoader(URL[], ClassLoader) failed: {e}"
            ))
        })
    }

    /// Turn a slice of `file:` URL strings into a Java `java.net.URL[]`.
    fn build_url_array<'l>(
        env: &mut JNIEnv<'l>,
        url_strings: &[String],
    ) -> Result<JObjectArray<'l>> {
        let url_class = env
            .find_class("java/net/URL")
            .map_err(|e| Error::bridge(format!("find_class(java/net/URL) failed: {e}")))?;
        let array = env
            .new_object_array(url_strings.len() as i32, &url_class, JObject::null())
            .map_err(|e| {
                Error::bridge(format!(
                    "new java.net.URL[{}] failed: {e}",
                    url_strings.len()
                ))
            })?;

        for (i, s) in url_strings.iter().enumerate() {
            let jstr = env
                .new_string(s)
                .map_err(|e| Error::bridge(format!("new_string({s}) failed: {e}")))?;
            let url = env
                .new_object(
                    "java/net/URL",
                    "(Ljava/lang/String;)V",
                    &[JValue::Object(&JObject::from(jstr))],
                )
                .map_err(|e| {
                    check_and_clear_exception(env);
                    Error::bridge(format!("new URL(\"{s}\") failed: {e}"))
                })?;
            env.set_object_array_element(&array, i as i32, &url)
                .map_err(|e| Error::bridge(format!("URL[{i}] = ... failed: {e}")))?;
        }
        Ok(array)
    }

    /// Promote a local reference to a [`GlobalRef`] so it survives past the
    /// current JNI frame.
    fn globalize(env: &mut JNIEnv, obj: JObject) -> Result<GlobalRef> {
        env.new_global_ref(obj)
            .map_err(|e| Error::bridge(format!("new_global_ref failed: {e}")))
    }

    /// If a Java exception is pending on this thread, clear it (so the thread
    /// is usable again) — the caller has already captured the failing JNI
    /// error and turned it into an [`Error::Bridge`].
    fn check_and_clear_exception(env: &mut JNIEnv) {
        if let Ok(true) = env.exception_check() {
            let _ = env.exception_clear();
        }
    }
}

// ===========================================================================
// Stub implementation — compiled with default features (no JDK required).
// ===========================================================================
#[cfg(not(feature = "jvm"))]
mod imp {
    use tomcatrs_core::{Error, Result};

    /// Stub [`ClassLoaderFactory`] used when the crate is built without the
    /// `jvm` feature.
    ///
    /// Every method that would touch JNI has the *same signature* as the real
    /// implementation but returns [`Error::Bridge`] instead, so dependent
    /// crates compile and link with no JDK present. The pure data types in the
    /// parent module ([`super::ClassLoaderSpec`],
    /// [`super::WebappClassLoaderConfig`], [`super::path_to_file_url`]) are
    /// fully functional on this path.
    #[derive(Debug, Default, Clone, Copy)]
    pub struct ClassLoaderFactory;

    /// The message every stub method fails with.
    const NO_JVM: &str =
        "JVM support not compiled in; rebuild tomcatrs-servlet-bridge with --features jvm";

    impl ClassLoaderFactory {
        /// Create a factory. Always succeeds — the failure is deferred to the
        /// JNI-touching methods.
        pub fn new() -> Self {
            Self
        }

        /// Stub: always returns [`Error::Bridge`]. See the type docs.
        pub fn common_loader(&self, _env: &mut ()) -> Result<()> {
            Err(Error::bridge(NO_JVM))
        }

        /// Stub: always returns [`Error::Bridge`]. See the type docs.
        pub fn common_loader_with(
            &self,
            _env: &mut (),
            _urls: &[std::path::PathBuf],
        ) -> Result<()> {
            Err(Error::bridge(NO_JVM))
        }

        /// Stub: always returns [`Error::Bridge`]. See the type docs.
        pub fn webapp_loader(
            &self,
            _env: &mut (),
            _config: &super::WebappClassLoaderConfig,
            _common: &(),
        ) -> Result<()> {
            Err(Error::bridge(NO_JVM))
        }

        /// Stub: always returns [`Error::Bridge`]. See the type docs.
        pub fn load_class(&self, _env: &mut (), _loader: &(), _fqcn: &str) -> Result<()> {
            Err(Error::bridge(NO_JVM))
        }

        /// Stub: always returns [`Error::Bridge`]. See the type docs.
        pub fn instantiate(&self, _env: &mut (), _loader: &(), _fqcn: &str) -> Result<()> {
            Err(Error::bridge(NO_JVM))
        }
    }
}

pub use imp::ClassLoaderFactory;

#[cfg(test)]
mod tests {
    use super::*;

    // --- WebappClassLoaderConfig construction --------------------------------

    #[test]
    fn webapp_config_parts_defaults_to_child_first() {
        let cfg = webapp_config_parts(
            "/myapp",
            Some(PathBuf::from("/srv/myapp/WEB-INF/classes")),
            vec![
                PathBuf::from("/srv/myapp/WEB-INF/lib/a.jar"),
                PathBuf::from("/srv/myapp/WEB-INF/lib/b.jar"),
            ],
        );
        assert_eq!(cfg.context_id, "/myapp");
        assert!(
            !cfg.delegate,
            "Tomcat default is parent-last (delegate=false)"
        );
        assert!(cfg.is_child_first());
        assert_eq!(cfg.jar_files.len(), 2);
    }

    #[test]
    fn webapp_config_from_borrowed_parts() {
        let jars = vec![PathBuf::from("/srv/app/WEB-INF/lib/dep.jar")];
        let cfg = webapp_config("/app", Some(Path::new("/srv/app/WEB-INF/classes")), &jars);
        assert_eq!(cfg.context_id, "/app");
        assert!(cfg.is_child_first());
        assert_eq!(
            cfg.classes_dir.as_deref(),
            Some(Path::new("/srv/app/WEB-INF/classes"))
        );
        assert_eq!(cfg.jar_files, jars);
    }

    #[test]
    fn search_path_is_classes_then_jars_in_order() {
        let cfg = webapp_config_parts(
            "/x",
            Some(PathBuf::from("/w/WEB-INF/classes")),
            vec![
                PathBuf::from("/w/WEB-INF/lib/a.jar"),
                PathBuf::from("/w/WEB-INF/lib/z.jar"),
            ],
        );
        let path = cfg.search_path();
        assert_eq!(
            path,
            vec![
                PathBuf::from("/w/WEB-INF/classes"),
                PathBuf::from("/w/WEB-INF/lib/a.jar"),
                PathBuf::from("/w/WEB-INF/lib/z.jar"),
            ]
        );
    }

    #[test]
    fn search_path_omits_absent_classes_dir() {
        let cfg = webapp_config_parts("/x", None, vec![PathBuf::from("/w/WEB-INF/lib/a.jar")]);
        assert_eq!(
            cfg.search_path(),
            vec![PathBuf::from("/w/WEB-INF/lib/a.jar")]
        );
    }

    #[test]
    fn to_spec_produces_webapp_kind_with_search_path() {
        let cfg = webapp_config_parts(
            "/x",
            Some(PathBuf::from("/w/WEB-INF/classes")),
            vec![PathBuf::from("/w/WEB-INF/lib/a.jar")],
        );
        let spec = cfg.to_spec();
        assert_eq!(spec.kind, ClassLoaderKind::Webapp);
        assert_eq!(spec.urls, cfg.search_path());
    }

    #[test]
    fn explicit_parent_first_config() {
        let cfg = WebappClassLoaderConfig::new("/legacy", None, Vec::new(), true);
        assert!(cfg.delegate);
        assert!(!cfg.is_child_first());
    }

    #[test]
    fn classloader_spec_common_kind() {
        let spec = ClassLoaderSpec::new(
            ClassLoaderKind::Common,
            vec![PathBuf::from("/opt/tomcatrs/lib/servlet-api.jar")],
        );
        assert_eq!(spec.kind, ClassLoaderKind::Common);
        assert_eq!(spec.file_urls().len(), 1);
    }

    // --- PathBuf -> file: URL conversion -------------------------------------

    #[test]
    fn file_url_for_plain_absolute_path() {
        assert_eq!(
            path_to_file_url(Path::new("/srv/app/WEB-INF/classes")),
            "file:///srv/app/WEB-INF/classes"
        );
    }

    #[test]
    fn file_url_percent_encodes_spaces() {
        assert_eq!(
            path_to_file_url(Path::new("/srv/my app/WEB-INF/lib/dep.jar")),
            "file:///srv/my%20app/WEB-INF/lib/dep.jar"
        );
    }

    #[test]
    fn file_url_percent_encodes_special_chars() {
        // '#', '?' and '%' are all URL-significant and must be escaped.
        assert_eq!(
            path_to_file_url(Path::new("/a/b#c?d%e")),
            "file:///a/b%23c%3Fd%25e"
        );
    }

    #[test]
    fn file_url_encodes_non_ascii() {
        // "café" — the 'é' is 0xC3 0xA9 in UTF-8.
        assert_eq!(
            path_to_file_url(Path::new("/srv/caf\u{e9}/x.jar")),
            "file:///srv/caf%C3%A9/x.jar"
        );
    }

    #[test]
    fn file_url_for_relative_path_still_valid() {
        let url = path_to_file_url(Path::new("WEB-INF/classes"));
        assert!(url.starts_with("file:///"), "got {url}");
        assert!(url.ends_with("WEB-INF/classes"));
    }

    #[test]
    fn file_url_normalises_backslashes() {
        // Backslashes are normalised to forward slashes regardless of host OS.
        assert_eq!(
            path_to_file_url(Path::new("C:\\apps\\my app\\x.jar")),
            "file:///C:/apps/my%20app/x.jar"
        );
    }

    #[test]
    fn config_file_urls_round_trip_through_search_path() {
        let cfg = webapp_config_parts(
            "/x",
            Some(PathBuf::from("/w/cls")),
            vec![PathBuf::from("/w/lib/a b.jar")],
        );
        assert_eq!(
            cfg.file_urls(),
            vec![
                "file:///w/cls".to_string(),
                "file:///w/lib/a%20b.jar".to_string(),
            ]
        );
    }

    // --- ClassLoaderFactory without the `jvm` feature ------------------------

    #[cfg(not(feature = "jvm"))]
    mod no_jvm {
        use super::*;
        use tomcatrs_core::Error;

        fn assert_bridge(result: tomcatrs_core::Result<()>) {
            match result {
                Err(Error::Bridge(msg)) => {
                    assert!(
                        msg.contains("--features jvm"),
                        "unexpected bridge message: {msg}"
                    );
                }
                Err(other) => panic!("expected Error::Bridge, got {other:?}"),
                Ok(()) => panic!("expected Error::Bridge, got Ok"),
            }
        }

        #[test]
        fn common_loader_returns_bridge_error() {
            let factory = ClassLoaderFactory::new();
            let mut env = ();
            assert_bridge(factory.common_loader(&mut env));
            assert_bridge(factory.common_loader_with(&mut env, &[]));
        }

        #[test]
        fn webapp_loader_returns_bridge_error() {
            let factory = ClassLoaderFactory::new();
            let mut env = ();
            let cfg = webapp_config_parts("/app", None, Vec::new());
            let common = ();
            assert_bridge(factory.webapp_loader(&mut env, &cfg, &common));
        }

        #[test]
        fn load_class_and_instantiate_return_bridge_error() {
            let factory = ClassLoaderFactory::new();
            let mut env = ();
            let loader = ();
            assert_bridge(factory.load_class(&mut env, &loader, "com.example.Foo"));
            assert_bridge(factory.instantiate(&mut env, &loader, "com.example.Foo"));
        }
    }
}
