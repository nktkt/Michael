//! [`ClassScanner`] — the entry point for classpath annotation scanning.
//!
//! A web application's effective classpath is `WEB-INF/classes` plus every
//! `WEB-INF/lib/*.jar`. The scanner walks that classpath, parses each Java
//! `.class` file with the hand-rolled parser in
//! [`annotations`](crate::annotations), and records the Servlet-spec
//! annotations it carries into an [`AnnotationIndex`].
//!
//! No JVM is involved: a `.class` file is a well-specified binary format, and
//! the subset needed for class-level annotation discovery — the constant pool
//! and the `RuntimeVisibleAnnotations` attribute — is small enough to parse
//! directly. See [`crate::annotations::ClassFile`].
//!
//! ## `metadata-complete`
//!
//! If a webapp's `WEB-INF/web.xml` declares `metadata-complete="true"`, the
//! Servlet spec says annotations must be ignored entirely. [`ClassScanner::scan_webapp`]
//! honours this: it returns an empty index without touching the classpath.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use tomcatrs_core::{Error, Result};

use crate::annotations::{AnnotationIndex, ClassFile, ClassMeta};
use crate::war::Webapp;

/// The largest `.class` file the scanner will load into memory. Real servlet
/// classes are a few kilobytes; this cap (8 MiB) is a generous guard against a
/// hostile or corrupt archive entry claiming an absurd size.
const MAX_CLASS_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Scans a web application's classpath for Servlet-spec annotations.
///
/// The scanner is stateless and cheap to create.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClassScanner;

impl ClassScanner {
    /// Create a new class scanner.
    pub fn new() -> ClassScanner {
        ClassScanner
    }

    /// Scan a `WEB-INF/classes`-style directory tree for annotated classes.
    ///
    /// Every `*.class` file found recursively under `dir` is parsed; classes
    /// carrying `@WebServlet` / `@WebFilter` / `@WebListener` are recorded in
    /// the returned [`AnnotationIndex`]. Files that are not valid class files
    /// are logged at `warn` and skipped rather than aborting the whole scan —
    /// one bad class should not sink an otherwise deployable application.
    ///
    /// A non-existent `dir` is treated as an empty classpath root and yields an
    /// empty index.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the directory tree cannot be traversed (e.g. a
    /// permissions failure on `read_dir`).
    pub fn scan_classes_dir(&self, dir: &Path) -> Result<AnnotationIndex> {
        let mut index = AnnotationIndex::new();
        if !dir.is_dir() {
            return Ok(index);
        }

        let mut class_files = Vec::new();
        collect_class_files(dir, &mut class_files)?;
        class_files.sort(); // deterministic ordering across platforms

        for path in class_files {
            match fs::read(&path) {
                Ok(bytes) => self.scan_bytes(&bytes, &path.display().to_string(), &mut index),
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "skipping unreadable class file"
                    );
                }
            }
        }

        Ok(index)
    }

    /// Scan a `.jar` file (a ZIP archive) for annotated classes.
    ///
    /// Every `*.class` entry in the archive is parsed; annotated classes are
    /// recorded in the returned [`AnnotationIndex`]. Entries that fail to read
    /// or parse are logged at `warn` and skipped.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Deployment`] if `jar` cannot be opened as a ZIP archive
    /// or its central directory is corrupt.
    pub fn scan_jar(&self, jar: &Path) -> Result<AnnotationIndex> {
        let mut index = AnnotationIndex::new();

        let file = fs::File::open(jar)
            .map_err(|e| Error::Deployment(format!("cannot open jar {}: {e}", jar.display())))?;
        let mut archive = zip::ZipArchive::new(file).map_err(|e| {
            Error::Deployment(format!(
                "{} is not a readable jar/zip archive: {e}",
                jar.display()
            ))
        })?;

        for i in 0..archive.len() {
            let mut entry = match archive.by_index(i) {
                Ok(entry) => entry,
                Err(e) => {
                    tracing::warn!(
                        jar = %jar.display(),
                        index = i,
                        error = %e,
                        "skipping unreadable jar entry"
                    );
                    continue;
                }
            };

            if !entry.is_file() {
                continue;
            }
            let name = entry.name().to_string();
            if !name.ends_with(".class") {
                continue;
            }
            // Module-info / package-info classes never carry servlet
            // annotations; skipping them is a cheap, harmless optimisation.
            if name.ends_with("module-info.class") || name.ends_with("package-info.class") {
                continue;
            }

            let declared = entry.size();
            if declared > MAX_CLASS_FILE_BYTES {
                tracing::warn!(
                    jar = %jar.display(),
                    entry = %name,
                    size = declared,
                    "skipping oversized jar class entry"
                );
                continue;
            }

            let mut bytes = Vec::with_capacity(declared as usize);
            if let Err(e) = entry.read_to_end(&mut bytes) {
                tracing::warn!(
                    jar = %jar.display(),
                    entry = %name,
                    error = %e,
                    "skipping unreadable jar class entry"
                );
                continue;
            }

            let origin = format!("{}!/{name}", jar.display());
            self.scan_bytes(&bytes, &origin, &mut index);
        }

        Ok(index)
    }

    /// Scan an entire deployed [`Webapp`]: its `WEB-INF/classes` directory plus
    /// every `WEB-INF/lib/*.jar`.
    ///
    /// If the webapp's `web.xml` declares `metadata-complete="true"`, the
    /// Servlet spec requires annotations to be ignored; in that case this
    /// returns an empty index immediately without reading any bytecode.
    ///
    /// Per-jar and per-directory failures are not swallowed: if a classpath
    /// entry cannot be traversed or opened at all, the error propagates so the
    /// deployer can report a genuinely broken application.
    ///
    /// # Errors
    ///
    /// Propagates [`Error::Io`] / [`Error::Deployment`] from
    /// [`Self::scan_classes_dir`] and [`Self::scan_jar`].
    pub fn scan_webapp(&self, webapp: &Webapp) -> Result<AnnotationIndex> {
        if webapp
            .descriptor()
            .map(|d| d.metadata_complete)
            .unwrap_or(false)
        {
            tracing::info!(
                context_path = %webapp.context_path(),
                "web.xml declares metadata-complete=true; skipping annotation scan"
            );
            return Ok(AnnotationIndex::new());
        }

        let mut index = AnnotationIndex::new();

        if let Some(classes) = webapp.classes_dir() {
            index.merge(self.scan_classes_dir(classes)?);
        }

        for jar in webapp.lib_jars() {
            index.merge(self.scan_jar(jar)?);
        }

        tracing::info!(
            context_path = %webapp.context_path(),
            servlets = index.web_servlets.len(),
            filters = index.web_filters.len(),
            listeners = index.web_listeners.len(),
            "completed annotation scan"
        );

        Ok(index)
    }

    /// Parse one in-memory class file and fold any Servlet-spec annotations it
    /// carries into `index`. A parse failure is logged and skipped — `origin`
    /// is a human-readable label (a path, or `jar!/entry`) for diagnostics.
    fn scan_bytes(&self, bytes: &[u8], origin: &str, index: &mut AnnotationIndex) {
        match ClassFile::parse(bytes) {
            Ok(class) => {
                if class.has_servlet_annotations() {
                    class.collect_into(index);
                }
            }
            Err(e) => {
                tracing::warn!(origin, error = %e, "skipping unparseable class file");
            }
        }
    }
}

/// Recursively collect every `*.class` file under `dir` into `out`.
///
/// `module-info.class` and `package-info.class` are excluded — they never
/// carry Servlet component annotations.
fn collect_class_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_class_files(&path, out)?;
        } else if file_type.is_file() {
            let is_class = path
                .extension()
                .map(|e| e.eq_ignore_ascii_case("class"))
                .unwrap_or(false);
            if !is_class {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if name == "module-info.class" || name == "package-info.class" {
                continue;
            }
            out.push(path);
        }
    }
    Ok(())
}

// ===========================================================================
// ClassgraphIndex — a `@HandlesTypes`-style class graph over a webapp's
// classpath (WEB-INF/classes/**/*.class + WEB-INF/lib/*.jar entries).
// ===========================================================================

/// A compact subtype / interface / annotation index over a webapp's
/// classpath, built for Servlet 6 `@HandlesTypes` scanning.
///
/// # What it does
///
/// The Servlet specification says that for each registered
/// `ServletContainerInitializer`, the container must scan the webapp's
/// classpath for every class that **extends**, **implements**, or **is
/// annotated by** any of the types named in the SCI's `@HandlesTypes`
/// annotation, and pass that set as the first argument to `onStartup`.
///
/// This index is the data structure that backs that scan. It walks
/// every `.class` file under `WEB-INF/classes` and every entry in every
/// `WEB-INF/lib/*.jar`, extracts a [`ClassMeta`] for each, and
/// precomputes a `direct_subtypes` map so the transitive closure from a
/// `@HandlesTypes` target can be answered by a cheap BFS at query time.
///
/// # Class-name format
///
/// All names — both in queries and in results — use the **dotted
/// fully-qualified form** (`com.example.Foo`, `java.lang.Object`).
/// This is the same form that `java.lang.Class.getName()` returns and
/// that [`ClassFile::class_name`] / [`ClassMeta`] already use, so the
/// names round-trip cleanly between the Rust class graph and the Java
/// side of the SCI plumbing.
#[derive(Debug, Clone, Default)]
pub struct ClassgraphIndex {
    /// Every parsed `ClassMeta`, keyed by dotted FQCN. The first
    /// occurrence wins on classpath shadowing — `WEB-INF/classes` is
    /// scanned before `WEB-INF/lib/*.jar`, mirroring child-first
    /// delegation.
    classes: HashMap<String, ClassMeta>,
    /// `target FQCN -> direct extenders/implementors` adjacency map.
    ///
    /// For each `ClassMeta` `C`, an entry `C.super_name -> C.name` and
    /// one entry per interface `I -> C.name` is recorded. Annotation
    /// hits are *not* in this map: an annotation cannot transitively
    /// annotate anything else through subtype edges, so annotation
    /// targets are answered by a direct scan at query time.
    direct_subtypes: HashMap<String, Vec<String>>,
}

impl ClassgraphIndex {
    /// Scan a deployed webapp's classpath (`WEB-INF/classes` + every
    /// `WEB-INF/lib/*.jar`) and build the index.
    ///
    /// Files that do not exist, or are not valid `.class` / `.jar`
    /// inputs, are logged at `warn` and skipped — one bad jar must not
    /// sink the whole index. The webapp's `web.xml` is *not* consulted
    /// here: `@HandlesTypes` scanning runs regardless of
    /// `metadata-complete`, per Servlet 6 §8.2.4.
    ///
    /// `webapp_root` is the document base — the directory holding the
    /// `WEB-INF/` subtree. If `webapp_root` has no `WEB-INF/classes`
    /// and no `WEB-INF/lib/*.jar`, the resulting index is empty
    /// (not an error).
    pub fn scan(webapp_root: &Path) -> Result<ClassgraphIndex> {
        let mut idx = ClassgraphIndex::default();
        let web_inf = webapp_root.join("WEB-INF");
        let classes_dir = web_inf.join("classes");
        let lib_dir = web_inf.join("lib");

        if classes_dir.is_dir() {
            idx.absorb_classes_dir(&classes_dir)?;
        }
        if lib_dir.is_dir() {
            // Sorted for deterministic shadowing on overlap.
            let mut jars: Vec<PathBuf> = Vec::new();
            for entry in fs::read_dir(&lib_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_file()
                    && path
                        .extension()
                        .map(|e| e.eq_ignore_ascii_case("jar"))
                        .unwrap_or(false)
                {
                    jars.push(path);
                }
            }
            jars.sort();
            for jar in jars {
                if let Err(e) = idx.absorb_jar(&jar) {
                    tracing::warn!(
                        jar = %jar.display(),
                        error = %e,
                        "ClassgraphIndex: skipping unreadable jar"
                    );
                }
            }
        }

        Ok(idx)
    }

    /// Walk a `WEB-INF/classes` directory tree and fold every parseable
    /// `.class` file into the index.
    fn absorb_classes_dir(&mut self, dir: &Path) -> Result<()> {
        let mut class_files = Vec::new();
        collect_class_files(dir, &mut class_files)?;
        class_files.sort();
        for path in class_files {
            match fs::read(&path) {
                Ok(bytes) => self.absorb_bytes(&bytes, &path.display().to_string()),
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "ClassgraphIndex: skipping unreadable class file"
                    );
                }
            }
        }
        Ok(())
    }

    /// Walk a single `.jar` (ZIP archive) and fold every parseable
    /// `.class` entry into the index.
    fn absorb_jar(&mut self, jar: &Path) -> Result<()> {
        let file = fs::File::open(jar)
            .map_err(|e| Error::Deployment(format!("cannot open jar {}: {e}", jar.display())))?;
        let mut archive = zip::ZipArchive::new(file).map_err(|e| {
            Error::Deployment(format!(
                "{} is not a readable jar/zip archive: {e}",
                jar.display()
            ))
        })?;
        for i in 0..archive.len() {
            let mut entry = match archive.by_index(i) {
                Ok(entry) => entry,
                Err(e) => {
                    tracing::warn!(
                        jar = %jar.display(),
                        index = i,
                        error = %e,
                        "ClassgraphIndex: skipping unreadable jar entry"
                    );
                    continue;
                }
            };
            if !entry.is_file() {
                continue;
            }
            let name = entry.name().to_string();
            if !name.ends_with(".class") {
                continue;
            }
            if name.ends_with("module-info.class") || name.ends_with("package-info.class") {
                continue;
            }
            let declared = entry.size();
            if declared > MAX_CLASS_FILE_BYTES {
                tracing::warn!(
                    jar = %jar.display(),
                    entry = %name,
                    size = declared,
                    "ClassgraphIndex: skipping oversized jar class entry"
                );
                continue;
            }
            let mut bytes = Vec::with_capacity(declared as usize);
            if let Err(e) = entry.read_to_end(&mut bytes) {
                tracing::warn!(
                    jar = %jar.display(),
                    entry = %name,
                    error = %e,
                    "ClassgraphIndex: skipping unreadable jar class entry"
                );
                continue;
            }
            let origin = format!("{}!/{name}", jar.display());
            self.absorb_bytes(&bytes, &origin);
        }
        Ok(())
    }

    /// Parse one in-memory class file and fold it into the index. Parse
    /// failures are logged and skipped.
    fn absorb_bytes(&mut self, bytes: &[u8], origin: &str) {
        match ClassFile::parse(bytes) {
            Ok(class) => self.insert(class.meta()),
            Err(e) => {
                tracing::warn!(origin, error = %e, "ClassgraphIndex: skipping unparseable class file");
            }
        }
    }

    /// Record one `ClassMeta`, populating the `direct_subtypes` adjacency
    /// map. First-write-wins on duplicates: when multiple jars (or
    /// `WEB-INF/classes` and a jar) ship the same FQCN, the first one
    /// scanned takes the slot — mirroring child-first delegation, given
    /// that `WEB-INF/classes` is absorbed before `WEB-INF/lib/*.jar`.
    fn insert(&mut self, meta: ClassMeta) {
        if self.classes.contains_key(&meta.name) {
            return;
        }
        if let Some(super_name) = &meta.super_name {
            self.direct_subtypes
                .entry(super_name.clone())
                .or_default()
                .push(meta.name.clone());
        }
        for iface in &meta.interfaces {
            self.direct_subtypes
                .entry(iface.clone())
                .or_default()
                .push(meta.name.clone());
        }
        self.classes.insert(meta.name.clone(), meta);
    }

    /// Number of distinct classes recorded in the index.
    pub fn len(&self) -> usize {
        self.classes.len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.classes.is_empty()
    }

    /// Return the dotted FQCNs of every class that is *handled by*
    /// `target` per the Servlet 6 `@HandlesTypes` rule:
    ///
    /// * every class that transitively extends or implements `target`
    ///   (BFS over `direct_subtypes`), and
    /// * every class whose [`ClassMeta::annotations`] mentions `target`
    ///   directly.
    ///
    /// `target` itself is **not** included in the result. The transitive
    /// closure is computed lazily on each call. Order in the returned
    /// vector is unspecified.
    pub fn classes_handled_by(&self, target: &str) -> Vec<&str> {
        let mut out: HashSet<&str> = HashSet::new();

        // BFS over the subtype adjacency map. We work in dotted-FQCN
        // strings throughout.
        let mut queue: VecDeque<&str> = VecDeque::new();
        let mut seen: HashSet<&str> = HashSet::new();
        if let Some(direct) = self.direct_subtypes.get(target) {
            for name in direct {
                if seen.insert(name.as_str()) {
                    queue.push_back(name.as_str());
                }
            }
        }
        while let Some(name) = queue.pop_front() {
            out.insert(name);
            if let Some(children) = self.direct_subtypes.get(name) {
                for c in children {
                    if seen.insert(c.as_str()) {
                        queue.push_back(c.as_str());
                    }
                }
            }
        }

        // Annotation hits. An annotation cannot transitively annotate
        // anything else through subtype edges (a subclass does not
        // inherit class-level annotations in general, and even
        // `@Inherited` is out of scope here per the spec), so a direct
        // pass over `classes` is sufficient.
        for (name, meta) in &self.classes {
            if meta.annotations.iter().any(|a| a == target) {
                out.insert(name.as_str());
            }
        }

        out.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotations::test_builder::{build_class, Val};
    use std::io::Write;
    use std::process::Command;

    /// A unique temp directory for one test, removed by the caller.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tomcatrs-scanner-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scan_classes_dir_finds_annotated_classes() {
        let root = temp_dir("classes");
        let pkg = root.join("com").join("example");
        fs::create_dir_all(&pkg).unwrap();

        let servlet = build_class(
            "com/example/HelloServlet",
            "Ljakarta/servlet/annotation/WebServlet;",
            &[("value", Val::StrArray(vec!["/hello"]))],
        );
        fs::write(pkg.join("HelloServlet.class"), &servlet).unwrap();

        let listener = build_class(
            "com/example/AppListener",
            "Ljakarta/servlet/annotation/WebListener;",
            &[],
        );
        fs::write(pkg.join("AppListener.class"), &listener).unwrap();

        // A plain class with an unrelated annotation must be ignored.
        let plain = build_class("com/example/Plain", "Lcom/example/Marker;", &[]);
        fs::write(pkg.join("Plain.class"), &plain).unwrap();

        // A bogus "class" file must be skipped, not fatal.
        fs::write(pkg.join("Broken.class"), b"not a class file").unwrap();

        let idx = ClassScanner::new().scan_classes_dir(&root).expect("scan");
        assert_eq!(idx.web_servlets.len(), 1);
        assert_eq!(idx.web_servlets[0].class_name, "com.example.HelloServlet");
        assert_eq!(idx.web_servlets[0].url_patterns, vec!["/hello"]);
        assert_eq!(idx.web_listeners.len(), 1);
        assert_eq!(idx.web_listeners[0].class_name, "com.example.AppListener");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn scan_classes_dir_missing_dir_is_empty() {
        let idx = ClassScanner::new()
            .scan_classes_dir(Path::new("/no/such/tomcatrs/dir"))
            .expect("scan");
        assert!(idx.is_empty());
    }

    #[test]
    fn scan_jar_finds_annotated_classes() {
        let dir = temp_dir("jar");
        let jar_path = dir.join("app.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);

        let servlet = build_class(
            "com/example/JarServlet",
            "Ljakarta/servlet/annotation/WebServlet;",
            &[
                ("name", Val::Str("jarman")),
                ("urlPatterns", Val::StrArray(vec!["/jar"])),
            ],
        );
        zip.start_file("com/example/JarServlet.class", opts)
            .unwrap();
        zip.write_all(&servlet).unwrap();

        let filter = build_class(
            "com/example/JarFilter",
            "Ljakarta/servlet/annotation/WebFilter;",
            &[("urlPatterns", Val::StrArray(vec!["/*"]))],
        );
        zip.start_file("com/example/JarFilter.class", opts).unwrap();
        zip.write_all(&filter).unwrap();

        // A non-class entry must be ignored.
        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Manifest-Version: 1.0\n").unwrap();

        zip.finish().unwrap();

        let idx = ClassScanner::new().scan_jar(&jar_path).expect("scan jar");
        assert_eq!(idx.web_servlets.len(), 1);
        assert_eq!(idx.web_servlets[0].servlet_name, "jarman");
        assert_eq!(idx.web_filters.len(), 1);
        assert_eq!(idx.web_filters[0].url_patterns, vec!["/*"]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_jar_rejects_non_zip() {
        let dir = temp_dir("badjar");
        let jar_path = dir.join("bad.jar");
        fs::write(&jar_path, b"definitely not a zip").unwrap();
        let err = ClassScanner::new().scan_jar(&jar_path).unwrap_err();
        assert!(matches!(err, Error::Deployment(_)));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_webapp_honours_metadata_complete() {
        // Build an exploded webapp whose web.xml is metadata-complete and whose
        // classes dir nonetheless contains an annotated class.
        let root = temp_dir("metacomplete");
        let classes = root
            .join("WEB-INF")
            .join("classes")
            .join("com")
            .join("example");
        fs::create_dir_all(&classes).unwrap();
        fs::create_dir_all(root.join("WEB-INF").join("lib")).unwrap();
        let servlet = build_class(
            "com/example/Ignored",
            "Ljakarta/servlet/annotation/WebServlet;",
            &[("value", Val::StrArray(vec!["/ignored"]))],
        );
        fs::write(classes.join("Ignored.class"), &servlet).unwrap();
        fs::write(
            root.join("WEB-INF").join("web.xml"),
            r#"<web-app metadata-complete="true"></web-app>"#,
        )
        .unwrap();

        let app = Webapp::open("/meta", &root).expect("open webapp");
        let idx = ClassScanner::new().scan_webapp(&app).expect("scan");
        assert!(
            idx.is_empty(),
            "metadata-complete webapp must yield an empty index"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn scan_webapp_scans_classes_and_lib() {
        let root = temp_dir("fullscan");
        let classes_pkg = root
            .join("WEB-INF")
            .join("classes")
            .join("com")
            .join("example");
        let lib = root.join("WEB-INF").join("lib");
        fs::create_dir_all(&classes_pkg).unwrap();
        fs::create_dir_all(&lib).unwrap();

        // An annotated class directly under WEB-INF/classes.
        let dir_servlet = build_class(
            "com/example/DirServlet",
            "Ljakarta/servlet/annotation/WebServlet;",
            &[("value", Val::StrArray(vec!["/dir"]))],
        );
        fs::write(classes_pkg.join("DirServlet.class"), &dir_servlet).unwrap();

        // An annotated class inside a WEB-INF/lib jar.
        let jar_file = fs::File::create(lib.join("dep.jar")).unwrap();
        let mut zip = zip::ZipWriter::new(jar_file);
        let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default();
        let jar_servlet = build_class(
            "com/example/LibServlet",
            "Ljakarta/servlet/annotation/WebServlet;",
            &[("value", Val::StrArray(vec!["/lib"]))],
        );
        zip.start_file("com/example/LibServlet.class", opts)
            .unwrap();
        zip.write_all(&jar_servlet).unwrap();
        zip.finish().unwrap();

        // No web.xml at all -> not metadata-complete -> scan runs.
        let app = Webapp::open("/full", &root).expect("open webapp");
        let idx = ClassScanner::new().scan_webapp(&app).expect("scan");

        assert_eq!(idx.web_servlets.len(), 2);
        let names: Vec<&str> = idx
            .web_servlets
            .iter()
            .map(|s| s.class_name.as_str())
            .collect();
        assert!(names.contains(&"com.example.DirServlet"));
        assert!(names.contains(&"com.example.LibServlet"));

        fs::remove_dir_all(&root).ok();
    }

    /// End-to-end test against bytecode produced by a *real* `javac`, so the
    /// hand-rolled parser is validated against the genuine class-file format.
    /// Skipped gracefully (the test still "passes") when no JDK is installed.
    #[test]
    fn scan_real_javac_compiled_servlet() {
        if Command::new("javac").arg("-version").output().is_err() {
            eprintln!("skipping scan_real_javac_compiled_servlet: javac not found on PATH");
            return;
        }

        let root = temp_dir("javac");
        let src_dir = root.join("src");
        let classes_dir = root.join("classes");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&classes_dir).unwrap();

        // A self-contained annotated class. We declare a local annotation type
        // with exactly the relevant name so no servlet-api jar is needed on the
        // compile classpath — the parser only cares about the descriptor
        // string `Ljakarta/servlet/annotation/WebServlet;`, which this
        // produces.
        let source = r#"
package jakarta.servlet.annotation;
import java.lang.annotation.*;
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.TYPE)
public @interface WebServlet {
    String name() default "";
    String[] value() default {};
    String[] urlPatterns() default {};
    int loadOnStartup() default -1;
    boolean asyncSupported() default false;
}
"#;
        let ann_path = src_dir.join("WebServlet.java");
        fs::write(&ann_path, source).unwrap();

        let servlet_src = r#"
package com.example;
import jakarta.servlet.annotation.WebServlet;
@WebServlet(name = "hello", urlPatterns = {"/hello"}, loadOnStartup = 1, asyncSupported = true)
public class HelloServlet {}
"#;
        let servlet_path = src_dir.join("HelloServlet.java");
        fs::write(&servlet_path, servlet_src).unwrap();

        let status = Command::new("javac")
            .arg("-d")
            .arg(&classes_dir)
            .arg(&ann_path)
            .arg(&servlet_path)
            .status()
            .expect("run javac");
        assert!(status.success(), "javac failed to compile the test sources");

        let idx = ClassScanner::new()
            .scan_classes_dir(&classes_dir)
            .expect("scan javac output");

        assert_eq!(
            idx.web_servlets.len(),
            1,
            "expected exactly one @WebServlet from javac-compiled bytecode"
        );
        let s = &idx.web_servlets[0];
        assert_eq!(s.class_name, "com.example.HelloServlet");
        assert_eq!(s.servlet_name, "hello");
        assert_eq!(s.url_patterns, vec!["/hello"]);
        assert_eq!(s.load_on_startup, Some(1));
        assert!(s.async_supported);

        fs::remove_dir_all(&root).ok();
    }

    // -----------------------------------------------------------------
    // ClassgraphIndex tests — exercise the @HandlesTypes-shaped subtype/
    // interface/annotation closure used by SCI dispatch.
    // -----------------------------------------------------------------

    use crate::annotations::test_builder::build_class_with_hierarchy;

    /// A class that implements `iface` (and extends `java.lang.Object`).
    fn class_implements(internal_name: &str, iface_internal: &str) -> Vec<u8> {
        build_class_with_hierarchy(internal_name, "java/lang/Object", &[iface_internal], &[])
    }

    /// A class that extends `super_internal`.
    fn class_extends(internal_name: &str, super_internal: &str) -> Vec<u8> {
        build_class_with_hierarchy(internal_name, super_internal, &[], &[])
    }

    /// A class with no superclass-of-interest, no interfaces, and a
    /// single class-level annotation of `ann_descriptor`.
    fn class_annotated(internal_name: &str, ann_descriptor: &str) -> Vec<u8> {
        build_class_with_hierarchy(
            internal_name,
            "java/lang/Object",
            &[],
            &[(ann_descriptor, &[])],
        )
    }

    /// An "interface" — for our purposes, just a plain class so the
    /// parser records its name. The hierarchy edges we care about
    /// (implementors) come from the implementing classes' interface
    /// lists, not from the interface declaration itself.
    fn iface_decl(internal_name: &str) -> Vec<u8> {
        build_class_with_hierarchy(internal_name, "java/lang/Object", &[], &[])
    }

    #[test]
    fn classgraph_index_finds_direct_and_transitive_implementors() {
        let root = temp_dir("classgraph-impl");
        let classes = root.join("WEB-INF").join("classes").join("com").join("ex");
        fs::create_dir_all(&classes).unwrap();

        // Marker interface.
        fs::write(classes.join("Marker.class"), iface_decl("com/ex/Marker")).unwrap();
        // Direct implementor.
        fs::write(
            classes.join("A.class"),
            class_implements("com/ex/A", "com/ex/Marker"),
        )
        .unwrap();
        // Indirect implementor: extends A.
        fs::write(
            classes.join("B.class"),
            class_extends("com/ex/B", "com/ex/A"),
        )
        .unwrap();
        // Unrelated.
        fs::write(
            classes.join("Unrelated.class"),
            class_extends("com/ex/Unrelated", "java/lang/Object"),
        )
        .unwrap();

        let idx = ClassgraphIndex::scan(&root).expect("scan");
        assert!(idx.len() >= 4);
        let mut handled: Vec<&str> = idx.classes_handled_by("com.ex.Marker");
        handled.sort();
        assert_eq!(handled, vec!["com.ex.A", "com.ex.B"]);
        // The target itself is not included.
        assert!(!handled.iter().any(|n| *n == "com.ex.Marker"));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn classgraph_index_finds_annotation_hits() {
        let root = temp_dir("classgraph-ann");
        let classes = root.join("WEB-INF").join("classes").join("com").join("ex");
        fs::create_dir_all(&classes).unwrap();
        fs::write(
            classes.join("Tagged.class"),
            class_annotated("com/ex/Tagged", "Lcom/ex/MyAnn;"),
        )
        .unwrap();
        fs::write(
            classes.join("Other.class"),
            class_annotated("com/ex/Other", "Lcom/ex/Different;"),
        )
        .unwrap();

        let idx = ClassgraphIndex::scan(&root).expect("scan");
        let hits = idx.classes_handled_by("com.ex.MyAnn");
        assert_eq!(hits, vec!["com.ex.Tagged"]);
        assert!(idx.classes_handled_by("com.ex.Nope").is_empty());

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn classgraph_index_scans_lib_jars_too() {
        let root = temp_dir("classgraph-jar");
        let classes = root.join("WEB-INF").join("classes").join("com").join("ex");
        let lib = root.join("WEB-INF").join("lib");
        fs::create_dir_all(&classes).unwrap();
        fs::create_dir_all(&lib).unwrap();

        // Marker interface lives in classes/.
        fs::write(classes.join("Marker.class"), iface_decl("com/ex/Marker")).unwrap();
        // Implementor lives in a jar under lib/.
        let jar_file = fs::File::create(lib.join("dep.jar")).unwrap();
        let mut zip = zip::ZipWriter::new(jar_file);
        let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default();
        zip.start_file("com/ex/JarImpl.class", opts).unwrap();
        zip.write_all(&class_implements("com/ex/JarImpl", "com/ex/Marker"))
            .unwrap();
        zip.finish().unwrap();

        let idx = ClassgraphIndex::scan(&root).expect("scan");
        let handled = idx.classes_handled_by("com.ex.Marker");
        assert_eq!(handled, vec!["com.ex.JarImpl"]);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn classgraph_index_empty_for_missing_webapp() {
        let idx = ClassgraphIndex::scan(Path::new("/no/such/tomcatrs/webapp")).expect("scan");
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
        assert!(idx.classes_handled_by("anything").is_empty());
    }
}
