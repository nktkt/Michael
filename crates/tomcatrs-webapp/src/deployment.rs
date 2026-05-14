//! [`DeploymentScanner`] — discover deployable web applications in a host's
//! `app_base` directory.
//!
//! Apache Tomcat's `HostConfig` watches each virtual host's `appBase`
//! (conventionally `webapps/`) and auto-deploys whatever it finds: exploded
//! directories and packed `.war` archives. The directory / archive *name*
//! determines the context path the application is mounted at, following
//! Tomcat's `#`-encoding convention for nested paths.
//!
//! | Name on disk | Context path |
//! |--------------|--------------|
//! | `ROOT`       | `/`          |
//! | `foo`        | `/foo`       |
//! | `foo#bar`    | `/foo/bar`   |
//! | `a#b#c`      | `/a/b/c`     |
//! | `app.war`    | `/app`       |
//! | `ROOT.war`   | `/`          |

use std::path::{Path, PathBuf};

use tomcatrs_core::{Error, Result};

/// The on-disk form of a discovered deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeploymentKind {
    /// An exploded WAR: a directory containing `WEB-INF/`.
    ExplodedDirectory,
    /// A packed `.war` ZIP archive (expansion is deferred to a later version).
    PackedWar,
}

/// One deployable unit discovered by the [`DeploymentScanner`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentUnit {
    /// The context path the application should be mounted at (`/` for ROOT).
    pub context_path: String,
    /// The document base on disk — a directory for [`DeploymentKind::ExplodedDirectory`],
    /// or the `.war` file for [`DeploymentKind::PackedWar`].
    pub doc_base: PathBuf,
    /// Whether the unit is exploded or packed.
    pub kind: DeploymentKind,
}

/// Scans a host `app_base` directory for deployable web applications.
///
/// The scanner is stateless; [`DeploymentScanner::scan`] is a pure function of
/// the directory contents. A future version will gain change-watching for
/// hot (re)deploy, which is why this is a type rather than a free function.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeploymentScanner;

impl DeploymentScanner {
    /// Create a new scanner.
    pub fn new() -> DeploymentScanner {
        DeploymentScanner
    }

    /// Scan `app_base` and return every deployable unit found, sorted by
    /// context path for deterministic deployment ordering.
    ///
    /// Rules, matching Tomcat's `HostConfig`:
    ///
    /// * A subdirectory is a deployment if it contains a `WEB-INF/` directory.
    /// * A file ending in `.war` (case-insensitive) is a packed deployment.
    /// * If both `foo/` and `foo.war` exist, the exploded directory wins (it
    ///   is treated as the canonical, possibly already-expanded, form).
    /// * Hidden entries (names starting with `.`) are ignored.
    ///
    /// # Errors
    ///
    /// * [`Error::Deployment`] if `app_base` does not exist or is not a
    ///   directory.
    /// * [`Error::Io`] if the directory cannot be read.
    pub fn scan(&self, app_base: &Path) -> Result<Vec<DeploymentUnit>> {
        if !app_base.exists() {
            return Err(Error::Deployment(format!(
                "app base {} does not exist",
                app_base.display()
            )));
        }
        if !app_base.is_dir() {
            return Err(Error::Deployment(format!(
                "app base {} is not a directory",
                app_base.display()
            )));
        }

        // Collect exploded dirs first so packed wars with the same base name
        // can be skipped.
        let mut units: Vec<DeploymentUnit> = Vec::new();
        let mut exploded_names: Vec<String> = Vec::new();

        for entry in std::fs::read_dir(app_base)? {
            let entry = entry?;
            let path = entry.path();
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();

            if name.starts_with('.') {
                continue;
            }

            if path.is_dir() {
                if !path.join("WEB-INF").is_dir() {
                    tracing::debug!(
                        dir = %path.display(),
                        "skipping directory without WEB-INF/"
                    );
                    continue;
                }
                let context_path = context_path_for_name(&name);
                exploded_names.push(name.into_owned());
                units.push(DeploymentUnit {
                    context_path,
                    doc_base: path,
                    kind: DeploymentKind::ExplodedDirectory,
                });
            }
        }

        // Second pass: packed .war files whose base name is not already
        // present as an exploded directory.
        for entry in std::fs::read_dir(app_base)? {
            let entry = entry?;
            let path = entry.path();
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();

            if name.starts_with('.') || !path.is_file() {
                continue;
            }
            let is_war = path
                .extension()
                .map(|e| e.eq_ignore_ascii_case("war"))
                .unwrap_or(false);
            if !is_war {
                continue;
            }

            let base = &name[..name.len() - 4]; // strip ".war"
            if exploded_names.iter().any(|n| n == base) {
                tracing::debug!(
                    war = %path.display(),
                    "skipping packed war shadowed by an exploded directory"
                );
                continue;
            }

            units.push(DeploymentUnit {
                context_path: context_path_for_name(base),
                doc_base: path,
                kind: DeploymentKind::PackedWar,
            });
        }

        units.sort_by(|a, b| a.context_path.cmp(&b.context_path));
        tracing::info!(
            app_base = %app_base.display(),
            count = units.len(),
            "deployment scan complete"
        );
        Ok(units)
    }
}

/// Map a deployment name (directory name, or `.war` base name) to a context
/// path following Tomcat's conventions.
///
/// * `ROOT` (case-sensitive, as Tomcat uses) maps to `/`.
/// * `#` is decoded to a path separator: `foo#bar` becomes `/foo/bar`.
/// * Any other name `foo` becomes `/foo`.
pub fn context_path_for_name(name: &str) -> String {
    if name == "ROOT" {
        return "/".to_string();
    }
    let joined = name.replace('#', "/");
    format!("/{joined}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn name_to_context_path_mapping() {
        assert_eq!(context_path_for_name("ROOT"), "/");
        assert_eq!(context_path_for_name("foo"), "/foo");
        assert_eq!(context_path_for_name("foo#bar"), "/foo/bar");
        assert_eq!(context_path_for_name("a#b#c"), "/a/b/c");
        assert_eq!(context_path_for_name("my-app"), "/my-app");
    }

    fn unique_app_base(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tomcatrs-webapp-deploy-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn make_exploded(app_base: &Path, name: &str) {
        fs::create_dir_all(app_base.join(name).join("WEB-INF")).unwrap();
    }

    #[test]
    fn scan_discovers_and_maps_units() {
        let app_base = unique_app_base("scan");
        fs::create_dir_all(&app_base).unwrap();

        make_exploded(&app_base, "ROOT");
        make_exploded(&app_base, "foo");
        make_exploded(&app_base, "foo#bar");
        // A directory without WEB-INF must be ignored.
        fs::create_dir_all(app_base.join("not-a-webapp")).unwrap();
        // A packed war with a distinct name.
        fs::write(app_base.join("packed.war"), b"PK").unwrap();
        // A packed war shadowed by an exploded dir of the same base name.
        fs::write(app_base.join("foo.war"), b"PK").unwrap();
        // A hidden entry must be ignored.
        fs::create_dir_all(app_base.join(".hidden").join("WEB-INF")).unwrap();

        let units = DeploymentScanner::new().scan(&app_base).unwrap();
        let paths: Vec<&str> = units.iter().map(|u| u.context_path.as_str()).collect();

        // Sorted by context path: "/", "/foo", "/foo/bar", "/packed".
        assert_eq!(paths, vec!["/", "/foo", "/foo/bar", "/packed"]);

        let root = units.iter().find(|u| u.context_path == "/").unwrap();
        assert_eq!(root.kind, DeploymentKind::ExplodedDirectory);

        let packed = units.iter().find(|u| u.context_path == "/packed").unwrap();
        assert_eq!(packed.kind, DeploymentKind::PackedWar);

        // foo.war was shadowed by the exploded foo/ directory.
        let foo = units.iter().find(|u| u.context_path == "/foo").unwrap();
        assert_eq!(foo.kind, DeploymentKind::ExplodedDirectory);

        fs::remove_dir_all(&app_base).ok();
    }

    #[test]
    fn scan_rejects_missing_app_base() {
        let missing = unique_app_base("missing");
        let err = DeploymentScanner::new().scan(&missing).unwrap_err();
        assert!(matches!(err, Error::Deployment(_)));
    }
}
