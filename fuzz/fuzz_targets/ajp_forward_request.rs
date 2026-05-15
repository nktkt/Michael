//! Fuzz target: the AJP/1.3 `Forward Request` decoder.
//!
//! Feeds arbitrary bytes into [`tomcatrs_coyote::ajp::ForwardRequest::decode`],
//! the container-side decoder for a type-`0x02` AJP packet. This is the most
//! security-sensitive parser in the AJP module: a `Forward Request` carries the
//! method code, request URI, headers, and the typed attributes whose
//! mishandling caused CVE-2020-1938 ("Ghostcat"). The decoder walks AJP strings
//! (`u16` length + bytes + NUL), common-header codes, and an attribute list.
//!
//! Invariant under test: **no packet payload may make `decode` panic.** A
//! truncated or malformed payload must return `Err(Error::Protocol(_))` — never
//! a slice-index panic or an `unwrap` on a short buffer. As a second step the
//! decoded request (when decoding succeeds) is run through `into_request`,
//! which performs URI validation and normalization, to fuzz that path too.
//!
//! The fuzz input *is* the raw AJP payload (magic prefix and length already
//! stripped, exactly as [`AjpMessage::from_payload`] expects), so the fuzzer
//! has full control over every field the decoder reads.

#![no_main]

use std::net::SocketAddr;

use libfuzzer_sys::fuzz_target;

use tomcatrs_coyote::ajp::{AjpMessage, ForwardRequest};

fuzz_target!(|data: &[u8]| {
    // `AjpMessage::from_payload` wraps an already-deframed payload; the fuzzer
    // controls every byte the `Forward Request` decoder will read.
    let mut msg = AjpMessage::from_payload(data.to_vec());

    // Decoding garbage must yield `Err`, not a panic.
    if let Ok(forward) = ForwardRequest::decode(&mut msg) {
        // On the rare valid decode, also fuzz URI validation + normalization.
        let peer: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let _ = forward.into_request(peer);
    }
});
