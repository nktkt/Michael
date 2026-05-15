//! Fuzz target: request-target URI normalization.
//!
//! Feeds arbitrary bytes into [`tomcatrs_coyote::normalize::normalize_target`],
//! the routine that turns a raw HTTP request target into a percent-decoded,
//! dot-segment-collapsed path plus an undecoded query string. Getting this
//! wrong is a classic path-traversal / request-smuggling vector, so the
//! function is deliberately strict — and therefore worth fuzzing hard.
//!
//! Invariant under test: **no input may make `normalize_target` panic.** A
//! malformed target must return a `NormalizeError` (path traversal, backslash,
//! NUL byte, encoded slash, bad percent-encoding, invalid UTF-8, not absolute)
//! — never a slice-index panic, an arithmetic overflow in the percent decoder,
//! or an `unwrap` on a malformed escape.
//!
//! `normalize_target` takes a `&str`, so the raw bytes are first interpreted as
//! UTF-8; non-UTF-8 inputs are simply skipped (they could never appear as the
//! `&str` request target this function is defined over).

#![no_main]

use libfuzzer_sys::fuzz_target;

use tomcatrs_coyote::normalize::normalize_target;

fuzz_target!(|data: &[u8]| {
    // The request target reaches `normalize_target` as a `&str`; bytes that
    // are not valid UTF-8 are out of scope for this surface.
    if let Ok(target) = std::str::from_utf8(data) {
        // Must return `Ok(NormalizedUri)` or `Err(NormalizeError)` — never panic.
        let _ = normalize_target(target);
    }
});
