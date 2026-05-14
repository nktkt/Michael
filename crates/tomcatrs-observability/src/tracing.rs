//! Process-wide initialization of the `tracing` ecosystem.
//!
//! Tomcat-RS uses the [`tracing`](https://docs.rs/tracing) crate for all
//! structured, leveled diagnostics. A `tracing` subscriber may only be
//! installed *once* per process, so [`init_tracing`] is guarded by a
//! [`once_cell::sync::OnceCell`]: the first call installs the subscriber and
//! every subsequent call is a cheap no-op.

use once_cell::sync::OnceCell;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

/// Tracks whether the global subscriber has already been installed.
static INIT: OnceCell<()> = OnceCell::new();

/// Initialize the global `tracing` subscriber.
///
/// `level` is used as the *default* directive for the env-filter, e.g.
/// `"info"`, `"warn"`, or a fuller spec like `"tomcatrs_coyote=debug,info"`.
/// The `RUST_LOG` environment variable, if set, takes precedence over `level`.
///
/// The installed subscriber combines an [`EnvFilter`] with a human-readable
/// `fmt` layer. The call is **idempotent**: only the first invocation installs
/// anything, so libraries and tests can call it freely without risking the
/// "a global default subscriber has already been set" panic.
///
/// # Examples
///
/// ```
/// use tomcatrs_observability::tracing::init_tracing;
///
/// init_tracing("info");
/// // Safe to call again — this is a no-op.
/// init_tracing("debug");
/// ```
pub fn init_tracing(level: &str) {
    INIT.get_or_init(|| {
        // Prefer RUST_LOG; fall back to the caller-supplied level; and if even
        // that fails to parse, fall back to "info" so we never panic here.
        let filter = EnvFilter::try_from_default_env()
            .or_else(|_| EnvFilter::try_new(level))
            .unwrap_or_else(|_| EnvFilter::new("info"));

        let fmt_layer = fmt::layer().with_target(true);

        // `try_init` returns Err if another subscriber raced us in; we treat
        // that as success since the goal — "a subscriber is installed" — holds.
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .try_init();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_tracing_is_idempotent() {
        // Multiple calls, including with different levels, must not panic.
        init_tracing("debug");
        init_tracing("info");
        init_tracing("warn");
        // Emitting an event after init must also be fine.
        tracing::info!("tracing initialized in test");
    }
}
