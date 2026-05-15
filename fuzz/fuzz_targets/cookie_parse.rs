//! Fuzz target: the `Cookie:` request-header parser.
//!
//! Feeds arbitrary bytes into
//! [`tomcatrs_coyote::cookies::parse_cookie_header`], the lenient RFC 6265 §5.4
//! parser that splits a `Cookie:` header value into `(name, value)` pairs,
//! trimming whitespace and stripping a single layer of surrounding quotes.
//!
//! Invariant under test: **no header value may make the parser panic.** The
//! parser is total by design — malformed pairs are skipped rather than
//! aborting — so it must always return a `Vec<Cookie>` for any `&str` input,
//! with no slice-index panic on empty pairs, lone `=`, or unbalanced quotes.
//!
//! `parse_cookie_header` takes a `&str`; non-UTF-8 inputs are skipped, since a
//! header value only ever reaches this function after it has been validated as
//! text upstream.

#![no_main]

use libfuzzer_sys::fuzz_target;

use tomcatrs_coyote::cookies::parse_cookie_header;

fuzz_target!(|data: &[u8]| {
    // Cookie header values reach the parser as `&str`; non-UTF-8 bytes are out
    // of scope for this surface.
    if let Ok(header_value) = std::str::from_utf8(data) {
        // Total function: must always return a list, never panic.
        let _ = parse_cookie_header(header_value);
    }
});
