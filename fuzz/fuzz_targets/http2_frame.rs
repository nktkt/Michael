//! Fuzz target: the HTTP/2 binary frame parser.
//!
//! Feeds arbitrary bytes into [`tomcatrs_coyote::http2::Frame::parse`], the
//! incremental frame-layer decoder. The parser reads the 9-octet frame header,
//! validates the declared length against `SETTINGS_MAX_FRAME_SIZE`, strips
//! padding, and decodes every RFC 9113 §6 frame type.
//!
//! Invariant under test: **no byte sequence may make `Frame::parse` panic.**
//! Truncated frames must return `Ok(None)` (need more data), malformed frames
//! must return `Err(Error::Protocol(_))`, and well-formed frames must return
//! `Ok(Some((frame, consumed)))` — never a slice-index panic, subtraction
//! overflow, or `unwrap` on a short buffer.
//!
//! To exercise the size-limit branch as well as the default path, the first
//! input byte selects between the RFC default `SETTINGS_MAX_FRAME_SIZE` and a
//! deliberately tiny limit; the remaining bytes are the frame stream.

#![no_main]

use libfuzzer_sys::fuzz_target;

use tomcatrs_coyote::http2::{Frame, DEFAULT_MAX_FRAME_SIZE};

fuzz_target!(|data: &[u8]| {
    // Use the first byte (if any) to vary `max_frame_size`: this lets the
    // fuzzer reach both the "frame fits" and "frame exceeds the limit" code
    // paths without needing to guess large length-prefixes.
    let (max_frame_size, frame_bytes) = match data.split_first() {
        Some((selector, rest)) => {
            let limit = if selector & 1 == 0 {
                DEFAULT_MAX_FRAME_SIZE
            } else {
                // A tiny limit so the FRAME_SIZE_ERROR branch is easy to hit.
                64
            };
            (limit, rest)
        }
        None => (DEFAULT_MAX_FRAME_SIZE, data),
    };

    // The only requirement: parsing must terminate without panicking. Every
    // one of the three outcomes (`Ok(None)`, `Ok(Some(_))`, `Err(_)`) is fine.
    let _ = Frame::parse(frame_bytes, max_frame_size);
});
