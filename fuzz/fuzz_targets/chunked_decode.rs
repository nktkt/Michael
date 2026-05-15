//! Fuzz target: the HTTP/1.1 chunked transfer-encoding decoder.
//!
//! Feeds arbitrary bytes into [`tomcatrs_coyote::chunked::ChunkedDecoder`], the
//! incremental decoder for a `Transfer-Encoding: chunked` request body. The
//! decoder parses hex chunk sizes, chunk extensions, inter-chunk CRLFs, and a
//! trailing trailer-header section — all while enforcing hard caps on the
//! decoded body size and on framing-line length.
//!
//! Invariant under test: **no byte stream may make the decoder panic.** A
//! malformed stream must return `Err(ChunkedError::Malformed(_))`, an oversized
//! one `Err(ChunkedError::TooLarge(_))` — never a slice-index panic, a
//! subtraction overflow on the per-chunk `remaining` counter, or unbounded
//! allocation.
//!
//! Real sockets deliver a chunked body in arbitrarily-sized reads, so the fuzz
//! input is split into two slices and `push`-ed in sequence: this exercises the
//! decoder's resumable, mid-token state machine, which a single `push` of the
//! whole buffer would not.

#![no_main]

use libfuzzer_sys::fuzz_target;

use tomcatrs_coyote::chunked::ChunkedDecoder;

fuzz_target!(|data: &[u8]| {
    // Cap the decoded body at 64 KiB — a sane default standing in for
    // `RequestLimits::max_post_size`. A body past the cap must surface as
    // `ChunkedError::TooLarge`, not an allocation blow-up.
    let mut decoder = ChunkedDecoder::new(64 * 1024);

    // Split the input and feed it as two separate reads, mimicking a socket
    // that delivers the body in fragments and forcing the decoder to resume
    // from whatever mid-token state the first `push` left it in.
    let mid = data.len() / 2;
    let (first, second) = data.split_at(mid);

    // A malformed or oversized stream must return `Err`, never panic. Stop on
    // the first error, exactly as the real connector does.
    if decoder.push(first).is_ok() {
        let _ = decoder.push(second);
    }
});
