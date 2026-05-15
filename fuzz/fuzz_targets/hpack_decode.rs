//! Fuzz target: the HPACK header-block decoder.
//!
//! Feeds arbitrary bytes into [`tomcatrs_coyote::hpack::HpackDecoder::decode`].
//! HPACK (RFC 7541) is a stateful compression format: the decoder walks
//! variable-length integers, Huffman-coded string literals, static- and
//! dynamic-table indices, and dynamic-table size updates. It is a notoriously
//! sharp parsing surface — exactly the kind of code a fuzzer should hammer.
//!
//! Invariant under test: **no encoded block may make `decode` panic.** A
//! corrupt block must return `Err(HpackError::_)` (truncated input, invalid
//! table index, bad Huffman padding, integer overflow, …) and must not
//! slice-index out of bounds, overflow, or allocate without bound.
//!
//! The decoder is stateful across calls, so the input is split into two halves
//! and decoded with the *same* decoder instance: this exercises dynamic-table
//! mutation carried from one block into the next, which a single-shot call
//! would never reach.

#![no_main]

use libfuzzer_sys::fuzz_target;

use tomcatrs_coyote::hpack::HpackDecoder;

fuzz_target!(|data: &[u8]| {
    // 4096 octets is the RFC 7541 default `SETTINGS_HEADER_TABLE_SIZE`.
    let mut decoder = HpackDecoder::new(4096);

    // Split the input in two and decode both halves through the same decoder,
    // so dynamic-table state established by the first block is carried into
    // the second. Either call returning `Err` is an acceptable outcome.
    let mid = data.len() / 2;
    let (first, second) = data.split_at(mid);

    let _ = decoder.decode(first);
    let _ = decoder.decode(second);
});
