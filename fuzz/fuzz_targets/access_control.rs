//! Fuzz target: security-layer URI normalization and validation.
//!
//! Feeds arbitrary bytes into
//! [`tomcatrs_security::access_control::normalize_and_validate_uri`], the
//! "reject, don't sanitize" hardening pass that percent-decodes a request URI
//! once, collapses `.`/`..` segments, and rejects anything ambiguous: encoded
//! separators, NUL bytes, backslashes, escaping traversal, and the
//! container-protected `/WEB-INF` and `/META-INF` directories.
//!
//! This is a *different* implementation from `tomcatrs_coyote::normalize` — it
//! is the defense-in-depth check applied deeper in the pipeline — so it earns
//! its own target.
//!
//! Invariant under test: **no input may make `normalize_and_validate_uri`
//! panic.** A suspicious URI must return `Err(Error::Rejected(_))` and a clean
//! one `Ok(String)` — never a slice-index panic in the percent decoder, an
//! overflow, or an `unwrap` on a malformed escape.
//!
//! The function takes a `&str`; non-UTF-8 inputs are skipped.

#![no_main]

use libfuzzer_sys::fuzz_target;

use tomcatrs_security::access_control::normalize_and_validate_uri;

fuzz_target!(|data: &[u8]| {
    // The URI reaches this validator as a `&str`; non-UTF-8 bytes are out of
    // scope for this surface.
    if let Ok(uri) = std::str::from_utf8(data) {
        // Must return `Ok` or `Err(Error::Rejected(_))` — never panic.
        let _ = normalize_and_validate_uri(uri);
    }
});
