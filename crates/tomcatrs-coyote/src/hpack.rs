//! HPACK header compression for HTTP/2 — a complete implementation of
//! [RFC 7541](https://www.rfc-editor.org/rfc/rfc7541).
//!
//! HPACK is the header compression format HTTP/2 uses on every frame that
//! carries headers (`HEADERS`, `PUSH_PROMISE`, continuations). It is a stateful
//! codec: the encoder and decoder each maintain a *dynamic table* of recently
//! seen header fields, and the wire format references entries by index instead
//! of repeating their bytes. On top of that it offers a static table of 61
//! common header fields, a variable-length integer encoding, and an optional
//! Huffman code for string literals.
//!
//! # Module map
//!
//! | Item               | Responsibility                                              |
//! |--------------------|-------------------------------------------------------------|
//! | [`STATIC_TABLE`]   | The 61 RFC 7541 Appendix A entries.                        |
//! | [`DynamicTable`]   | FIFO table with a byte-size budget and eviction.           |
//! | [`HpackEncoder`]   | Compresses `(name, value)` pairs into an HPACK block.       |
//! | [`HpackDecoder`]   | Expands an HPACK block back into `(name, value)` pairs.     |
//! | [`HpackError`]     | Decode/encode failures; converts into [`tomcatrs_core::Error::Protocol`]. |
//!
//! Integer coding ([`encode_integer`] / [`decode_integer`]), string-literal
//! coding ([`encode_string`] / [`decode_string`]) and the Huffman codec
//! ([`huffman::encode`] / [`huffman::decode`]) are exposed for testing and
//! reuse.
//!
//! # Example
//!
//! ```
//! use tomcatrs_coyote::hpack::{HpackDecoder, HpackEncoder};
//!
//! let mut enc = HpackEncoder::new(4096);
//! let mut dec = HpackDecoder::new(4096);
//!
//! let headers = vec![
//!     (":method".to_string(), "GET".to_string()),
//!     (":path".to_string(), "/index.html".to_string()),
//!     ("custom-key".to_string(), "custom-value".to_string()),
//! ];
//! let block = enc.encode(&headers);
//! let round_tripped = dec.decode(&block).unwrap();
//! assert_eq!(headers, round_tripped);
//! ```

use std::fmt;

/// An HPACK encoding or decoding failure.
///
/// Every variant maps onto [`tomcatrs_core::Error::Protocol`] via the `From`
/// implementation, because an HPACK fault is, by RFC 7541 §4.1, a connection
/// error of type `COMPRESSION_ERROR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HpackError {
    /// A variable-length integer did not terminate within a sane number of
    /// continuation bytes, or overflowed.
    IntegerOverflow,
    /// The header block ended in the middle of an instruction.
    Truncated,
    /// An index referenced neither the static nor the dynamic table.
    InvalidIndex(usize),
    /// A Huffman-coded string contained an invalid code or invalid padding.
    InvalidHuffman(&'static str),
    /// A string literal claimed to contain non-UTF-8 bytes. HPACK strings are
    /// opaque octets, but the connector represents header values as `String`.
    InvalidUtf8,
    /// A dynamic table size update exceeded the limit negotiated by the
    /// `SETTINGS_HEADER_TABLE_SIZE` setting.
    TableSizeExceeded {
        /// The size the peer asked for.
        requested: usize,
        /// The maximum we allow.
        limit: usize,
    },
    /// The cumulative size of the decoded header list exceeded the configured
    /// `SETTINGS_MAX_HEADER_LIST_SIZE` budget.
    HeaderListTooLarge {
        /// The configured maximum.
        limit: usize,
    },
    /// A dynamic table size update appeared somewhere other than the very start
    /// of the header block (RFC 7541 §4.2).
    UnexpectedTableSizeUpdate,
}

impl fmt::Display for HpackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HpackError::IntegerOverflow => write!(f, "hpack integer overflow"),
            HpackError::Truncated => write!(f, "hpack block truncated"),
            HpackError::InvalidIndex(i) => write!(f, "hpack invalid table index {i}"),
            HpackError::InvalidHuffman(why) => write!(f, "hpack invalid huffman string: {why}"),
            HpackError::InvalidUtf8 => write!(f, "hpack string literal was not valid utf-8"),
            HpackError::TableSizeExceeded { requested, limit } => write!(
                f,
                "hpack dynamic table size update {requested} exceeds limit {limit}"
            ),
            HpackError::HeaderListTooLarge { limit } => {
                write!(f, "hpack header list exceeds limit {limit} bytes")
            }
            HpackError::UnexpectedTableSizeUpdate => {
                write!(f, "hpack dynamic table size update in unexpected position")
            }
        }
    }
}

impl std::error::Error for HpackError {}

impl From<HpackError> for tomcatrs_core::Error {
    fn from(e: HpackError) -> Self {
        tomcatrs_core::Error::protocol(e.to_string())
    }
}

/// A `Result` specialised for HPACK operations.
pub type Result<T> = std::result::Result<T, HpackError>;

// ===========================================================================
// Static table — RFC 7541 Appendix A
// ===========================================================================

/// The HPACK static table: the 61 fixed `(name, value)` entries from RFC 7541
/// Appendix A. Index 0 is unused on the wire; entry *n* in this slice is HPACK
/// index `n + 1`.
pub const STATIC_TABLE: [(&str, &str); 61] = [
    (":authority", ""),
    (":method", "GET"),
    (":method", "POST"),
    (":path", "/"),
    (":path", "/index.html"),
    (":scheme", "http"),
    (":scheme", "https"),
    (":status", "200"),
    (":status", "204"),
    (":status", "206"),
    (":status", "304"),
    (":status", "400"),
    (":status", "404"),
    (":status", "500"),
    ("accept-charset", ""),
    ("accept-encoding", "gzip, deflate"),
    ("accept-language", ""),
    ("accept-ranges", ""),
    ("accept", ""),
    ("access-control-allow-origin", ""),
    ("age", ""),
    ("allow", ""),
    ("authorization", ""),
    ("cache-control", ""),
    ("content-disposition", ""),
    ("content-encoding", ""),
    ("content-language", ""),
    ("content-length", ""),
    ("content-location", ""),
    ("content-range", ""),
    ("content-type", ""),
    ("cookie", ""),
    ("date", ""),
    ("etag", ""),
    ("expect", ""),
    ("expires", ""),
    ("from", ""),
    ("host", ""),
    ("if-match", ""),
    ("if-modified-since", ""),
    ("if-none-match", ""),
    ("if-range", ""),
    ("if-unmodified-since", ""),
    ("last-modified", ""),
    ("link", ""),
    ("location", ""),
    ("max-forwards", ""),
    ("proxy-authenticate", ""),
    ("proxy-authorization", ""),
    ("range", ""),
    ("referer", ""),
    ("refresh", ""),
    ("retry-after", ""),
    ("server", ""),
    ("set-cookie", ""),
    ("strict-transport-security", ""),
    ("transfer-encoding", ""),
    ("user-agent", ""),
    ("vary", ""),
    ("via", ""),
    ("www-authenticate", ""),
];

/// The per-entry overhead RFC 7541 §4.1 charges every dynamic table entry, on
/// top of the raw name and value octet counts.
const ENTRY_OVERHEAD: usize = 32;

// ===========================================================================
// Dynamic table — RFC 7541 §2.3.2 and §4
// ===========================================================================

/// The HPACK dynamic table: a FIFO of recently encoded/decoded header fields,
/// bounded by a byte-size budget.
///
/// "Size" is defined by RFC 7541 §4.1 as the sum, over every entry, of
/// `name.len() + value.len() + 32`. Inserting an entry evicts the oldest
/// entries until the new one fits; an entry larger than the whole budget
/// evicts everything and is itself *not* stored (this is legal and leaves the
/// table empty).
///
/// Newest entries sit at the front of `entries`; the HPACK index of the newest
/// dynamic entry is `STATIC_TABLE.len() + 1`.
#[derive(Debug, Clone)]
pub struct DynamicTable {
    /// `(name, value)` pairs, newest first.
    entries: std::collections::VecDeque<(String, String)>,
    /// Current total size in HPACK accounting units.
    size: usize,
    /// The active capacity. Never exceeds `max_capacity`.
    capacity: usize,
    /// The hard ceiling negotiated via `SETTINGS_HEADER_TABLE_SIZE`; a dynamic
    /// table size update may not raise `capacity` above this.
    max_capacity: usize,
}

impl DynamicTable {
    /// Create an empty dynamic table whose capacity *and* hard ceiling are
    /// `max_capacity` bytes.
    pub fn new(max_capacity: usize) -> Self {
        DynamicTable {
            entries: std::collections::VecDeque::new(),
            size: 0,
            capacity: max_capacity,
            max_capacity,
        }
    }

    /// The current number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table currently holds no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The current total size, in HPACK accounting units.
    pub fn size(&self) -> usize {
        self.size
    }

    /// The active capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The hard ceiling on capacity (the negotiated `SETTINGS_HEADER_TABLE_SIZE`).
    pub fn max_capacity(&self) -> usize {
        self.max_capacity
    }

    /// Raise or lower the hard ceiling (used when the peer sends a new
    /// `SETTINGS_HEADER_TABLE_SIZE`). The active capacity is clamped down to the
    /// new ceiling if necessary, evicting as needed.
    pub fn set_max_capacity(&mut self, max_capacity: usize) {
        self.max_capacity = max_capacity;
        if self.capacity > max_capacity {
            self.resize(max_capacity);
        }
    }

    /// Apply a dynamic table size update from the wire (RFC 7541 §6.3).
    ///
    /// # Errors
    ///
    /// Returns [`HpackError::TableSizeExceeded`] if `new_capacity` is larger
    /// than the negotiated ceiling.
    pub fn apply_size_update(&mut self, new_capacity: usize) -> Result<()> {
        if new_capacity > self.max_capacity {
            return Err(HpackError::TableSizeExceeded {
                requested: new_capacity,
                limit: self.max_capacity,
            });
        }
        self.resize(new_capacity);
        Ok(())
    }

    /// Set the active capacity to `new_capacity`, evicting oldest-first until
    /// the table fits. Does not touch `max_capacity`.
    pub fn resize(&mut self, new_capacity: usize) {
        self.capacity = new_capacity;
        self.evict_to_fit(0);
    }

    /// The HPACK accounting size of one `(name, value)` entry.
    fn entry_size(name: &str, value: &str) -> usize {
        name.len() + value.len() + ENTRY_OVERHEAD
    }

    /// Evict oldest entries until the table can accommodate `additional` more
    /// bytes within `capacity`.
    fn evict_to_fit(&mut self, additional: usize) {
        while self.size + additional > self.capacity {
            match self.entries.pop_back() {
                Some((n, v)) => {
                    self.size -= Self::entry_size(&n, &v);
                }
                None => break,
            }
        }
    }

    /// Insert a header field at the front of the table (RFC 7541 §2.3.2).
    ///
    /// Oldest entries are evicted first to make room. If the entry on its own
    /// is larger than the capacity, the table is emptied and the entry is not
    /// stored — this is explicitly permitted by RFC 7541 §4.4.
    pub fn insert(&mut self, name: &str, value: &str) {
        let entry_size = Self::entry_size(name, value);
        self.evict_to_fit(entry_size);
        if entry_size > self.capacity {
            // Cannot fit even in an empty table; spec says drop it silently.
            debug_assert!(self.entries.is_empty());
            return;
        }
        self.entries
            .push_front((name.to_string(), value.to_string()));
        self.size += entry_size;
    }

    /// Look up a dynamic table entry by its *zero-based* position from the
    /// newest entry (0 = most recently inserted).
    pub fn get(&self, idx: usize) -> Option<&(String, String)> {
        self.entries.get(idx)
    }
}

// ===========================================================================
// Combined index space — RFC 7541 §2.3.3
// ===========================================================================

/// Resolve a 1-based HPACK index into a `(name, value)` pair, consulting the
/// static table first and then the dynamic table.
fn resolve_index(idx: usize, dynamic: &DynamicTable) -> Result<(String, String)> {
    if idx == 0 {
        return Err(HpackError::InvalidIndex(0));
    }
    if idx <= STATIC_TABLE.len() {
        let (n, v) = STATIC_TABLE[idx - 1];
        return Ok((n.to_string(), v.to_string()));
    }
    let dyn_idx = idx - STATIC_TABLE.len() - 1;
    dynamic
        .get(dyn_idx)
        .cloned()
        .ok_or(HpackError::InvalidIndex(idx))
}

/// Find a full `(name, value)` match in the combined index space, returning the
/// 1-based HPACK index. The static table is searched first so its (lower,
/// stable) indices win.
fn find_full(name: &str, value: &str, dynamic: &DynamicTable) -> Option<usize> {
    for (i, (n, v)) in STATIC_TABLE.iter().enumerate() {
        if *n == name && *v == value {
            return Some(i + 1);
        }
    }
    for i in 0..dynamic.len() {
        let (n, v) = dynamic.get(i).unwrap();
        if n == name && v == value {
            return Some(STATIC_TABLE.len() + 1 + i);
        }
    }
    None
}

/// Find a name-only match in the combined index space, returning the 1-based
/// HPACK index. The static table is searched first.
fn find_name(name: &str, dynamic: &DynamicTable) -> Option<usize> {
    for (i, (n, _)) in STATIC_TABLE.iter().enumerate() {
        if *n == name {
            return Some(i + 1);
        }
    }
    for i in 0..dynamic.len() {
        let (n, _) = dynamic.get(i).unwrap();
        if n == name {
            return Some(STATIC_TABLE.len() + 1 + i);
        }
    }
    None
}

// ===========================================================================
// Integer representation — RFC 7541 §5.1
// ===========================================================================

/// Encode `value` as an HPACK variable-length integer with an `prefix_bits`-bit
/// prefix (RFC 7541 §5.1), appending to `out`.
///
/// The caller is responsible for OR-ing any flag bits into the first byte's
/// high bits; `prefix_bits` is the number of *low* bits available for the
/// integer in that first byte. The first byte is appended fresh (its high
/// `8 - prefix_bits` bits are zero).
pub fn encode_integer(value: usize, prefix_bits: u8, out: &mut Vec<u8>) {
    debug_assert!((1..=8).contains(&prefix_bits));
    let max_prefix = (1usize << prefix_bits) - 1;
    if value < max_prefix {
        out.push(value as u8);
        return;
    }
    out.push(max_prefix as u8);
    let mut remaining = value - max_prefix;
    while remaining >= 128 {
        out.push(((remaining & 0x7f) | 0x80) as u8);
        remaining >>= 7;
    }
    out.push(remaining as u8);
}

/// Decode an HPACK variable-length integer (RFC 7541 §5.1) with an
/// `prefix_bits`-bit prefix.
///
/// `bytes` must start at the byte containing the prefix; flag bits in that
/// byte's high bits are ignored. Returns the decoded value and the number of
/// bytes consumed.
///
/// # Errors
///
/// - [`HpackError::Truncated`] if the integer runs off the end of `bytes`.
/// - [`HpackError::IntegerOverflow`] if the value would not fit in a `usize`
///   or uses more continuation bytes than is ever sensible.
pub fn decode_integer(bytes: &[u8], prefix_bits: u8) -> Result<(usize, usize)> {
    debug_assert!((1..=8).contains(&prefix_bits));
    let max_prefix = (1usize << prefix_bits) - 1;
    let first = *bytes.first().ok_or(HpackError::Truncated)? as usize;
    let prefix = first & max_prefix;
    if prefix < max_prefix {
        return Ok((prefix, 1));
    }
    let mut value = max_prefix;
    let mut shift = 0u32;
    let mut consumed = 1usize;
    loop {
        let byte = *bytes.get(consumed).ok_or(HpackError::Truncated)?;
        consumed += 1;
        // A shift of 64 or beyond cannot contribute and signals a bogus stream.
        if shift >= usize::BITS {
            return Err(HpackError::IntegerOverflow);
        }
        let add = ((byte & 0x7f) as u128) << shift;
        let candidate = value as u128 + add;
        if candidate > usize::MAX as u128 {
            return Err(HpackError::IntegerOverflow);
        }
        value = candidate as usize;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    Ok((value, consumed))
}

// ===========================================================================
// Huffman coding — RFC 7541 Appendix B
// ===========================================================================

/// HPACK's canonical Huffman code, RFC 7541 Appendix B.
///
/// Encoding ([`encode`](huffman::encode)) and decoding
/// ([`decode`](huffman::decode)) of string literals. The 257-symbol alphabet is
/// the 256 octet values plus the end-of-string symbol (256), which is only ever
/// used as padding and must never actually be decoded.
pub mod huffman {
    use super::{HpackError, Result};

    /// `(code, bit_length)` for every symbol 0..=256. Symbol 256 is EOS.
    pub(super) const TABLE: [(u32, u8); 257] = [
        (0x1ff8, 13),
        (0x7fffd8, 23),
        (0xfffffe2, 28),
        (0xfffffe3, 28),
        (0xfffffe4, 28),
        (0xfffffe5, 28),
        (0xfffffe6, 28),
        (0xfffffe7, 28),
        (0xfffffe8, 28),
        (0xffffea, 24),
        (0x3ffffffc, 30),
        (0xfffffe9, 28),
        (0xfffffea, 28),
        (0x3ffffffd, 30),
        (0xfffffeb, 28),
        (0xfffffec, 28),
        (0xfffffed, 28),
        (0xfffffee, 28),
        (0xfffffef, 28),
        (0xffffff0, 28),
        (0xffffff1, 28),
        (0xffffff2, 28),
        (0x3ffffffe, 30),
        (0xffffff3, 28),
        (0xffffff4, 28),
        (0xffffff5, 28),
        (0xffffff6, 28),
        (0xffffff7, 28),
        (0xffffff8, 28),
        (0xffffff9, 28),
        (0xffffffa, 28),
        (0xffffffb, 28),
        (0x14, 6),
        (0x3f8, 10),
        (0x3f9, 10),
        (0xffa, 12),
        (0x1ff9, 13),
        (0x15, 6),
        (0xf8, 8),
        (0x7fa, 11),
        (0x3fa, 10),
        (0x3fb, 10),
        (0xf9, 8),
        (0x7fb, 11),
        (0xfa, 8),
        (0x16, 6),
        (0x17, 6),
        (0x18, 6),
        (0x0, 5),
        (0x1, 5),
        (0x2, 5),
        (0x19, 6),
        (0x1a, 6),
        (0x1b, 6),
        (0x1c, 6),
        (0x1d, 6),
        (0x1e, 6),
        (0x1f, 6),
        (0x5c, 7),
        (0xfb, 8),
        (0x7ffc, 15),
        (0x20, 6),
        (0xffb, 12),
        (0x3fc, 10),
        (0x1ffa, 13),
        (0x21, 6),
        (0x5d, 7),
        (0x5e, 7),
        (0x5f, 7),
        (0x60, 7),
        (0x61, 7),
        (0x62, 7),
        (0x63, 7),
        (0x64, 7),
        (0x65, 7),
        (0x66, 7),
        (0x67, 7),
        (0x68, 7),
        (0x69, 7),
        (0x6a, 7),
        (0x6b, 7),
        (0x6c, 7),
        (0x6d, 7),
        (0x6e, 7),
        (0x6f, 7),
        (0x70, 7),
        (0x71, 7),
        (0x72, 7),
        (0xfc, 8),
        (0x73, 7),
        (0xfd, 8),
        (0x1ffb, 13),
        (0x7fff0, 19),
        (0x1ffc, 13),
        (0x3ffc, 14),
        (0x22, 6),
        (0x7ffd, 15),
        (0x3, 5),
        (0x23, 6),
        (0x4, 5),
        (0x24, 6),
        (0x5, 5),
        (0x25, 6),
        (0x26, 6),
        (0x27, 6),
        (0x6, 5),
        (0x74, 7),
        (0x75, 7),
        (0x28, 6),
        (0x29, 6),
        (0x2a, 6),
        (0x7, 5),
        (0x2b, 6),
        (0x76, 7),
        (0x2c, 6),
        (0x8, 5),
        (0x9, 5),
        (0x2d, 6),
        (0x77, 7),
        (0x78, 7),
        (0x79, 7),
        (0x7a, 7),
        (0x7b, 7),
        (0x7ffe, 15),
        (0x7fc, 11),
        (0x3ffd, 14),
        (0x1ffd, 13),
        (0xffffffc, 28),
        (0xfffe6, 20),
        (0x3fffd2, 22),
        (0xfffe7, 20),
        (0xfffe8, 20),
        (0x3fffd3, 22),
        (0x3fffd4, 22),
        (0x3fffd5, 22),
        (0x7fffd9, 23),
        (0x3fffd6, 22),
        (0x7fffda, 23),
        (0x7fffdb, 23),
        (0x7fffdc, 23),
        (0x7fffdd, 23),
        (0x7fffde, 23),
        (0xffffeb, 24),
        (0x7fffdf, 23),
        (0xffffec, 24),
        (0xffffed, 24),
        (0x3fffd7, 22),
        (0x7fffe0, 23),
        (0xffffee, 24),
        (0x7fffe1, 23),
        (0x7fffe2, 23),
        (0x7fffe3, 23),
        (0x7fffe4, 23),
        (0x1fffdc, 21),
        (0x3fffd8, 22),
        (0x7fffe5, 23),
        (0x3fffd9, 22),
        (0x7fffe6, 23),
        (0x7fffe7, 23),
        (0xffffef, 24),
        (0x3fffda, 22),
        (0x1fffdd, 21),
        (0xfffe9, 20),
        (0x3fffdb, 22),
        (0x3fffdc, 22),
        (0x7fffe8, 23),
        (0x7fffe9, 23),
        (0x1fffde, 21),
        (0x7fffea, 23),
        (0x3fffdd, 22),
        (0x3fffde, 22),
        (0xfffff0, 24),
        (0x1fffdf, 21),
        (0x3fffdf, 22),
        (0x7fffeb, 23),
        (0x7fffec, 23),
        (0x1fffe0, 21),
        (0x1fffe1, 21),
        (0x3fffe0, 22),
        (0x1fffe2, 21),
        (0x7fffed, 23),
        (0x3fffe1, 22),
        (0x7fffee, 23),
        (0x7fffef, 23),
        (0xfffea, 20),
        (0x3fffe2, 22),
        (0x3fffe3, 22),
        (0x3fffe4, 22),
        (0x7ffff0, 23),
        (0x3fffe5, 22),
        (0x3fffe6, 22),
        (0x7ffff1, 23),
        (0x3ffffe0, 26),
        (0x3ffffe1, 26),
        (0xfffeb, 20),
        (0x7fff1, 19),
        (0x3fffe7, 22),
        (0x7ffff2, 23),
        (0x3fffe8, 22),
        (0x1ffffec, 25),
        (0x3ffffe2, 26),
        (0x3ffffe3, 26),
        (0x3ffffe4, 26),
        (0x7ffffde, 27),
        (0x7ffffdf, 27),
        (0x3ffffe5, 26),
        (0xfffff1, 24),
        (0x1ffffed, 25),
        (0x7fff2, 19),
        (0x1fffe3, 21),
        (0x3ffffe6, 26),
        (0x7ffffe0, 27),
        (0x7ffffe1, 27),
        (0x3ffffe7, 26),
        (0x7ffffe2, 27),
        (0xfffff2, 24),
        (0x1fffe4, 21),
        (0x1fffe5, 21),
        (0x3ffffe8, 26),
        (0x3ffffe9, 26),
        (0xffffffd, 28),
        (0x7ffffe3, 27),
        (0x7ffffe4, 27),
        (0x7ffffe5, 27),
        (0xfffec, 20),
        (0xfffff3, 24),
        (0xfffed, 20),
        (0x1fffe6, 21),
        (0x3fffe9, 22),
        (0x1fffe7, 21),
        (0x1fffe8, 21),
        (0x7ffff3, 23),
        (0x3fffea, 22),
        (0x3fffeb, 22),
        (0x1ffffee, 25),
        (0x1ffffef, 25),
        (0xfffff4, 24),
        (0xfffff5, 24),
        (0x3ffffea, 26),
        (0x7ffff4, 23),
        (0x3ffffeb, 26),
        (0x7ffffe6, 27),
        (0x3ffffec, 26),
        (0x3ffffed, 26),
        (0x7ffffe7, 27),
        (0x7ffffe8, 27),
        (0x7ffffe9, 27),
        (0x7ffffea, 27),
        (0x7ffffeb, 27),
        (0xffffffe, 28),
        (0x7ffffec, 27),
        (0x7ffffed, 27),
        (0x7ffffee, 27),
        (0x7ffffef, 27),
        (0x7fffff0, 27),
        (0x3ffffee, 26),
        (0x3fffffff, 30),
    ];

    /// Encode `data` with the HPACK Huffman code (RFC 7541 §5.2).
    ///
    /// The output is bit-packed MSB-first and padded to a byte boundary with
    /// the all-ones prefix of the EOS symbol, exactly as the RFC requires.
    pub fn encode(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        let mut acc: u64 = 0;
        let mut bits: u32 = 0;
        for &byte in data {
            let (code, len) = TABLE[byte as usize];
            acc = (acc << len) | code as u64;
            bits += len as u32;
            while bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
            }
        }
        if bits > 0 {
            // Pad the final byte with the high bits of the EOS code, which are
            // all ones.
            let pad = 8 - bits;
            acc = (acc << pad) | ((1u64 << pad) - 1);
            out.push(acc as u8);
        }
        out
    }

    /// The encoded length, in bytes, `data` would occupy under the Huffman
    /// code — computed without allocating the output.
    pub fn encoded_len(data: &[u8]) -> usize {
        let bits: u32 = data.iter().map(|&b| TABLE[b as usize].1 as u32).sum();
        bits.div_ceil(8) as usize
    }

    /// Decode an HPACK Huffman string (RFC 7541 §5.2).
    ///
    /// Decoding walks the code bit-by-bit. Per the RFC, the trailing padding
    /// must be the most-significant bits of the EOS code (i.e. all ones) and
    /// must be strictly shorter than 8 bits; anything else, or an embedded EOS
    /// symbol, is a decoding error.
    ///
    /// # Errors
    ///
    /// [`HpackError::InvalidHuffman`] for an embedded EOS symbol, over-long
    /// padding, or non-all-ones padding.
    pub fn decode(data: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(data.len() * 8 / 5);
        // Current partial code accumulated MSB-first. A 64-bit accumulator
        // comfortably holds one byte's worth of fresh input on top of a
        // not-yet-matched code of up to 30 bits.
        let mut acc: u64 = 0;
        let mut acc_bits: u32 = 0;
        for &byte in data {
            acc = (acc << 8) | byte as u64;
            acc_bits += 8;
            // Try to peel symbols off the front of `acc`.
            loop {
                let mut matched = false;
                // HPACK code lengths run from 5 to 30 bits.
                for len in 5..=30u32 {
                    if len > acc_bits {
                        break;
                    }
                    let candidate = ((acc >> (acc_bits - len)) & ((1u64 << len) - 1)) as u32;
                    if let Some(sym) = lookup(candidate, len) {
                        if sym == 256 {
                            return Err(HpackError::InvalidHuffman(
                                "embedded end-of-string symbol",
                            ));
                        }
                        out.push(sym as u8);
                        acc_bits -= len;
                        matched = true;
                        break;
                    }
                }
                if !matched {
                    break;
                }
            }
        }
        // Whatever bits are left must be valid EOS padding: at most 7 bits, all
        // ones.
        if acc_bits >= 8 {
            return Err(HpackError::InvalidHuffman("incomplete code in input"));
        }
        if acc_bits > 0 {
            let mask = (1u64 << acc_bits) - 1;
            if (acc & mask) != mask {
                return Err(HpackError::InvalidHuffman("padding is not all ones"));
            }
        }
        Ok(out)
    }

    /// Look up the symbol whose code is exactly `code` with exactly `len` bits,
    /// if any. This is a linear scan over the 257-entry table; the table is
    /// small and decode is not hot enough to warrant a packed decode tree.
    fn lookup(code: u32, len: u32) -> Option<u16> {
        for (sym, &(c, l)) in TABLE.iter().enumerate() {
            if l as u32 == len && c == code {
                return Some(sym as u16);
            }
        }
        None
    }
}

// ===========================================================================
// String literals — RFC 7541 §5.2
// ===========================================================================

/// Encode `s` as an HPACK string literal (RFC 7541 §5.2), appending to `out`.
///
/// The encoder uses Huffman coding when, and only when, it yields a strictly
/// shorter byte string — this matches the behaviour the RFC's worked examples
/// expect.
pub fn encode_string(s: &str, out: &mut Vec<u8>) {
    let raw = s.as_bytes();
    let huff_len = huffman::encoded_len(raw);
    if huff_len < raw.len() {
        // Huffman flag is the top bit of the length prefix.
        let mut len_prefix = Vec::new();
        encode_integer(huff_len, 7, &mut len_prefix);
        len_prefix[0] |= 0x80;
        out.extend_from_slice(&len_prefix);
        out.extend_from_slice(&huffman::encode(raw));
    } else {
        encode_integer(raw.len(), 7, out);
        out.extend_from_slice(raw);
    }
}

/// Decode an HPACK string literal (RFC 7541 §5.2) starting at `bytes[0]`.
///
/// Returns the decoded string and the number of bytes consumed.
///
/// # Errors
///
/// - [`HpackError::Truncated`] if the literal runs past the end of `bytes`.
/// - [`HpackError::InvalidHuffman`] for a malformed Huffman payload.
/// - [`HpackError::InvalidUtf8`] if the octets are not valid UTF-8.
pub fn decode_string(bytes: &[u8]) -> Result<(String, usize)> {
    let first = *bytes.first().ok_or(HpackError::Truncated)?;
    let huffman_flag = first & 0x80 != 0;
    let (len, len_bytes) = decode_integer(bytes, 7)?;
    let start = len_bytes;
    let end = start.checked_add(len).ok_or(HpackError::IntegerOverflow)?;
    if end > bytes.len() {
        return Err(HpackError::Truncated);
    }
    let payload = &bytes[start..end];
    let raw = if huffman_flag {
        huffman::decode(payload)?
    } else {
        payload.to_vec()
    };
    let s = String::from_utf8(raw).map_err(|_| HpackError::InvalidUtf8)?;
    Ok((s, end))
}

// ===========================================================================
// Decoder — RFC 7541 §6
// ===========================================================================

/// A stateful HPACK decoder.
///
/// One decoder instance is shared by every inbound HEADERS frame on a single
/// HTTP/2 connection; its dynamic table accumulates state across calls to
/// [`decode`](Self::decode). Construct with [`HpackDecoder::new`].
#[derive(Debug)]
pub struct HpackDecoder {
    dynamic: DynamicTable,
    /// `SETTINGS_MAX_HEADER_LIST_SIZE`-equivalent budget. The decoded header
    /// list (summed as `name.len() + value.len() + 32` per field, matching
    /// RFC 7541's accounting) may not exceed this.
    max_header_list_size: usize,
}

impl HpackDecoder {
    /// Create a decoder with `header_table_size` as the dynamic table ceiling
    /// (the local `SETTINGS_HEADER_TABLE_SIZE`).
    ///
    /// The header-list-size budget defaults to a generous 16 MiB; override it
    /// with [`set_max_header_list_size`](Self::set_max_header_list_size).
    pub fn new(header_table_size: usize) -> Self {
        HpackDecoder {
            dynamic: DynamicTable::new(header_table_size),
            max_header_list_size: 16 * 1024 * 1024,
        }
    }

    /// Set the maximum decoded header list size, in HPACK accounting units. A
    /// block that decodes to more than this is rejected with
    /// [`HpackError::HeaderListTooLarge`].
    pub fn set_max_header_list_size(&mut self, limit: usize) {
        self.max_header_list_size = limit;
    }

    /// Update the dynamic table ceiling after a peer `SETTINGS_HEADER_TABLE_SIZE`
    /// change.
    pub fn set_max_table_size(&mut self, max: usize) {
        self.dynamic.set_max_capacity(max);
    }

    /// Read-only access to the dynamic table, mostly for tests and diagnostics.
    pub fn dynamic_table(&self) -> &DynamicTable {
        &self.dynamic
    }

    /// Decode one complete HPACK header block into an ordered list of
    /// `(name, value)` pairs.
    ///
    /// The dynamic table is mutated as a side effect, so subsequent calls see
    /// the accumulated state — this is mandatory for HPACK correctness.
    ///
    /// # Errors
    ///
    /// Any [`HpackError`]; in particular [`HpackError::HeaderListTooLarge`] if
    /// the decoded list exceeds the configured budget, and
    /// [`HpackError::UnexpectedTableSizeUpdate`] if a size update appears after
    /// a header field.
    pub fn decode(&mut self, block: &[u8]) -> Result<Vec<(String, String)>> {
        let mut headers = Vec::new();
        let mut pos = 0usize;
        let mut total_size = 0usize;
        // Dynamic table size updates are only allowed at the very start of the
        // block, before any header field representation (RFC 7541 §4.2).
        let mut allow_size_update = true;

        while pos < block.len() {
            let byte = block[pos];
            if byte & 0x80 != 0 {
                // 1xxxxxxx — Indexed Header Field (§6.1).
                allow_size_update = false;
                let (idx, used) = decode_integer(&block[pos..], 7)?;
                pos += used;
                let (name, value) = resolve_index(idx, &self.dynamic)?;
                total_size += name.len() + value.len() + ENTRY_OVERHEAD;
                self.check_size(total_size)?;
                headers.push((name, value));
            } else if byte & 0x40 != 0 {
                // 01xxxxxx — Literal Header Field with Incremental Indexing (§6.2.1).
                allow_size_update = false;
                let (name, value, used) = self.decode_literal(&block[pos..], 6)?;
                pos += used;
                self.dynamic.insert(&name, &value);
                total_size += name.len() + value.len() + ENTRY_OVERHEAD;
                self.check_size(total_size)?;
                headers.push((name, value));
            } else if byte & 0x20 != 0 {
                // 001xxxxx — Dynamic Table Size Update (§6.3).
                if !allow_size_update {
                    return Err(HpackError::UnexpectedTableSizeUpdate);
                }
                let (new_size, used) = decode_integer(&block[pos..], 5)?;
                pos += used;
                self.dynamic.apply_size_update(new_size)?;
            } else {
                // 0000xxxx — Literal without Indexing (§6.2.2), or
                // 0001xxxx — Literal Never Indexed (§6.2.3).
                // Both use a 4-bit prefix; the never-indexed bit (0x10) only
                // affects re-encoding, not decoding semantics.
                allow_size_update = false;
                let (name, value, used) = self.decode_literal(&block[pos..], 4)?;
                pos += used;
                total_size += name.len() + value.len() + ENTRY_OVERHEAD;
                self.check_size(total_size)?;
                headers.push((name, value));
            }
        }
        Ok(headers)
    }

    /// Decode a literal header field representation whose index prefix is
    /// `prefix_bits` bits wide. Returns `(name, value, bytes_consumed)`.
    fn decode_literal(&self, bytes: &[u8], prefix_bits: u8) -> Result<(String, String, usize)> {
        let (idx, mut pos) = decode_integer(bytes, prefix_bits)?;
        let name = if idx == 0 {
            let (n, used) = decode_string(&bytes[pos..])?;
            pos += used;
            n
        } else {
            let (n, _) = resolve_index(idx, &self.dynamic)?;
            n
        };
        let (value, used) = decode_string(&bytes[pos..])?;
        pos += used;
        Ok((name, value, pos))
    }

    /// Reject the in-progress decode if the running header list size exceeds
    /// the configured budget.
    fn check_size(&self, total: usize) -> Result<()> {
        if total > self.max_header_list_size {
            return Err(HpackError::HeaderListTooLarge {
                limit: self.max_header_list_size,
            });
        }
        Ok(())
    }
}

// ===========================================================================
// Encoder — RFC 7541 §6
// ===========================================================================

/// How the encoder should represent one header field on the wire.
///
/// The default policy ([`HeaderHint::Index`]) lets the encoder index freely;
/// callers mark sensitive fields with [`HeaderHint::NeverIndex`] so values such
/// as `authorization` or `cookie` are emitted with the never-indexed
/// representation and stay out of both tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderHint {
    /// Index the field if possible, and add it to the dynamic table.
    Index,
    /// Emit a literal but do not add it to the dynamic table.
    WithoutIndex,
    /// Emit a literal with the "never indexed" bit set; intermediaries must
    /// not index it either. Use for sensitive header fields.
    NeverIndex,
}

/// A stateful HPACK encoder.
///
/// One encoder instance serves every outbound HEADERS frame on a connection;
/// its dynamic table is updated by each [`encode`](Self::encode) call. Construct
/// with [`HpackEncoder::new`].
///
/// By default the encoder treats `authorization`, `cookie`, `set-cookie` and
/// `proxy-authorization` as sensitive and emits them never-indexed. Override
/// the classification wholesale with
/// [`set_sensitivity_predicate`](Self::set_sensitivity_predicate), or per-call
/// with [`encode_with_hints`](Self::encode_with_hints).
pub struct HpackEncoder {
    dynamic: DynamicTable,
    /// Classifies a `(name, value)` pair as sensitive (→ never indexed).
    sensitive: Box<dyn Fn(&str, &str) -> bool + Send + Sync>,
    /// When `Some`, the next `encode` call first emits a dynamic table size
    /// update to this value.
    pending_size_update: Option<usize>,
}

impl fmt::Debug for HpackEncoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HpackEncoder")
            .field("dynamic", &self.dynamic)
            .field("pending_size_update", &self.pending_size_update)
            .finish_non_exhaustive()
    }
}

/// The default sensitivity policy: header fields that carry credentials or
/// session state are never indexed.
fn default_sensitive(name: &str, _value: &str) -> bool {
    matches!(
        name,
        "authorization" | "proxy-authorization" | "cookie" | "set-cookie"
    )
}

impl HpackEncoder {
    /// Create an encoder whose dynamic table capacity *and* ceiling are
    /// `header_table_size` bytes.
    pub fn new(header_table_size: usize) -> Self {
        HpackEncoder {
            dynamic: DynamicTable::new(header_table_size),
            sensitive: Box::new(default_sensitive),
            pending_size_update: None,
        }
    }

    /// Replace the sensitivity predicate. A field for which `pred` returns
    /// `true` is emitted with the never-indexed representation.
    pub fn set_sensitivity_predicate<F>(&mut self, pred: F)
    where
        F: Fn(&str, &str) -> bool + Send + Sync + 'static,
    {
        self.sensitive = Box::new(pred);
    }

    /// Read-only access to the dynamic table, mostly for tests and diagnostics.
    pub fn dynamic_table(&self) -> &DynamicTable {
        &self.dynamic
    }

    /// Shrink or grow the dynamic table and arrange for the next
    /// [`encode`](Self::encode) call to emit the corresponding dynamic table
    /// size update instruction.
    ///
    /// `new_size` is clamped to the table's hard ceiling.
    pub fn set_table_size(&mut self, new_size: usize) {
        let clamped = new_size.min(self.dynamic.max_capacity());
        self.dynamic.resize(clamped);
        self.pending_size_update = Some(clamped);
    }

    /// Encode `headers` with the default per-field policy (index everything
    /// except fields the sensitivity predicate flags).
    pub fn encode(&mut self, headers: &[(String, String)]) -> Vec<u8> {
        let hints: Vec<HeaderHint> = headers
            .iter()
            .map(|(n, v)| {
                if (self.sensitive)(n, v) {
                    HeaderHint::NeverIndex
                } else {
                    HeaderHint::Index
                }
            })
            .collect();
        self.encode_with_hints(headers, &hints)
    }

    /// Encode `headers`, taking the representation for field *i* from
    /// `hints[i]`. `hints` must be the same length as `headers`.
    ///
    /// # Panics
    ///
    /// Panics if `hints.len() != headers.len()`.
    pub fn encode_with_hints(
        &mut self,
        headers: &[(String, String)],
        hints: &[HeaderHint],
    ) -> Vec<u8> {
        assert_eq!(
            headers.len(),
            hints.len(),
            "every header needs exactly one hint"
        );
        let mut out = Vec::new();

        if let Some(size) = self.pending_size_update.take() {
            // 001xxxxx — Dynamic Table Size Update.
            let mut tmp = Vec::new();
            encode_integer(size, 5, &mut tmp);
            tmp[0] |= 0x20;
            out.extend_from_slice(&tmp);
        }

        for ((name, value), hint) in headers.iter().zip(hints) {
            self.encode_field(name, value, *hint, &mut out);
        }
        out
    }

    /// Encode a single header field according to `hint`.
    fn encode_field(&mut self, name: &str, value: &str, hint: HeaderHint, out: &mut Vec<u8>) {
        // An exact (name, value) match is always representable as a single
        // indexed field, regardless of hint — it adds nothing to any table.
        if let Some(idx) = find_full(name, value, &self.dynamic) {
            let mut tmp = Vec::new();
            encode_integer(idx, 7, &mut tmp);
            tmp[0] |= 0x80;
            out.extend_from_slice(&tmp);
            return;
        }

        let name_idx = find_name(name, &self.dynamic);
        match hint {
            HeaderHint::Index => {
                // 01xxxxxx — Literal with Incremental Indexing.
                self.encode_literal(name, value, name_idx, 6, 0x40, out);
                self.dynamic.insert(name, value);
            }
            HeaderHint::WithoutIndex => {
                // 0000xxxx — Literal without Indexing.
                self.encode_literal(name, value, name_idx, 4, 0x00, out);
            }
            HeaderHint::NeverIndex => {
                // 0001xxxx — Literal Never Indexed.
                self.encode_literal(name, value, name_idx, 4, 0x10, out);
            }
        }
    }

    /// Emit a literal header field representation. `prefix_bits` and
    /// `flag_bits` select which of the three literal forms is produced;
    /// `name_idx`, when `Some`, references an existing table entry for the
    /// name instead of spelling it out.
    fn encode_literal(
        &self,
        name: &str,
        value: &str,
        name_idx: Option<usize>,
        prefix_bits: u8,
        flag_bits: u8,
        out: &mut Vec<u8>,
    ) {
        let mut prefix = Vec::new();
        match name_idx {
            Some(idx) => {
                encode_integer(idx, prefix_bits, &mut prefix);
            }
            None => {
                encode_integer(0, prefix_bits, &mut prefix);
            }
        }
        prefix[0] |= flag_bits;
        out.extend_from_slice(&prefix);
        if name_idx.is_none() {
            encode_string(name, out);
        }
        encode_string(value, out);
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a hex string into bytes, ignoring all whitespace. Accepts both
    /// byte-per-token forms (`"82 86 84"`) and the RFC's grouped-nibble form
    /// (`"8286 8441 0f77"`), since hex digits are simply consumed in pairs.
    fn hex(s: &str) -> Vec<u8> {
        let digits: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(digits.len() % 2 == 0, "hex string has odd nibble count");
        (0..digits.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&digits[i..i + 2], 16).unwrap())
            .collect()
    }

    fn h(name: &str, value: &str) -> (String, String) {
        (name.to_string(), value.to_string())
    }

    // --- Static table sanity --------------------------------------------

    #[test]
    fn static_table_has_61_entries_and_known_anchors() {
        assert_eq!(STATIC_TABLE.len(), 61);
        assert_eq!(STATIC_TABLE[0], (":authority", ""));
        assert_eq!(STATIC_TABLE[1], (":method", "GET"));
        assert_eq!(STATIC_TABLE[60], ("www-authenticate", ""));
        // index 2 (1-based) -> :method GET
        assert_eq!(
            resolve_index(2, &DynamicTable::new(0)).unwrap(),
            h(":method", "GET")
        );
    }

    // --- Integer codec — RFC 7541 §5.1, including the §C.1 examples ------

    #[test]
    fn integer_codec_rfc_c1_examples() {
        // C.1.1 — 10 with a 5-bit prefix → 0x0a, one byte.
        let mut out = Vec::new();
        encode_integer(10, 5, &mut out);
        assert_eq!(out, vec![0x0a]);
        assert_eq!(decode_integer(&out, 5).unwrap(), (10, 1));

        // C.1.2 — 1337 with a 5-bit prefix → 31 154 10.
        let mut out = Vec::new();
        encode_integer(1337, 5, &mut out);
        assert_eq!(out, hex("1f 9a 0a"));
        assert_eq!(decode_integer(&out, 5).unwrap(), (1337, 3));

        // C.1.3 — 42 with an 8-bit prefix → 0x2a, one byte.
        let mut out = Vec::new();
        encode_integer(42, 8, &mut out);
        assert_eq!(out, vec![0x2a]);
        assert_eq!(decode_integer(&out, 8).unwrap(), (42, 1));
    }

    #[test]
    fn integer_codec_roundtrips_many_values_and_prefixes() {
        let values = [
            0usize,
            1,
            2,
            30,
            31,
            32,
            126,
            127,
            128,
            254,
            255,
            256,
            1337,
            16_383,
            16_384,
            1_000_000,
            usize::MAX / 2,
        ];
        for prefix in 1..=8u8 {
            for &v in &values {
                let mut out = Vec::new();
                encode_integer(v, prefix, &mut out);
                let (got, used) = decode_integer(&out, prefix).unwrap();
                assert_eq!(got, v, "prefix={prefix} value={v}");
                assert_eq!(used, out.len(), "prefix={prefix} value={v}");
            }
        }
    }

    #[test]
    fn integer_decode_rejects_truncation_and_overflow() {
        // All-continuation bytes never terminate → truncated.
        assert_eq!(
            decode_integer(&[0xff, 0x80, 0x80], 8),
            Err(HpackError::Truncated)
        );
        // A ridiculously long continuation run overflows usize.
        let bogus = [
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f,
        ];
        assert_eq!(decode_integer(&bogus, 8), Err(HpackError::IntegerOverflow));
        assert_eq!(decode_integer(&[], 8), Err(HpackError::Truncated));
    }

    // --- Huffman — RFC 7541 §5.2, Appendix B ----------------------------

    #[test]
    fn huffman_table_is_well_formed_and_prefix_free() {
        // Every code must fit within its declared bit length.
        for (sym, &(code, len)) in huffman::TABLE.iter().enumerate() {
            assert!(
                (5..=30).contains(&len),
                "sym {sym}: implausible length {len}"
            );
            assert_eq!(
                code >> len,
                0,
                "sym {sym}: code {code:#x} does not fit in {len} bits"
            );
        }
        // The code must be a prefix code: left-align every code to 32 bits and
        // confirm no code is a prefix of any other.
        let aligned: Vec<(u32, u8)> = huffman::TABLE
            .iter()
            .map(|&(code, len)| (code << (32 - len), len))
            .collect();
        for i in 0..aligned.len() {
            for j in (i + 1)..aligned.len() {
                let (ci, li) = aligned[i];
                let (cj, lj) = aligned[j];
                let shared = li.min(lj);
                let mask = if shared == 0 {
                    0
                } else {
                    !0u32 << (32 - shared)
                };
                assert_ne!(
                    ci & mask,
                    cj & mask,
                    "symbols {i} and {j} share a prefix — not a prefix-free code"
                );
            }
        }
    }

    #[test]
    fn huffman_known_strings() {
        // "www.example.com" → f1e3 c2e5 f23a 6ba0 ab90 f4ff (RFC §C.4.1).
        let enc = huffman::encode(b"www.example.com");
        assert_eq!(enc, hex("f1 e3 c2 e5 f2 3a 6b a0 ab 90 f4 ff"));
        assert_eq!(huffman::decode(&enc).unwrap(), b"www.example.com");

        // "no-cache" → a8eb 1064 9cbf (RFC §C.4.2).
        let enc = huffman::encode(b"no-cache");
        assert_eq!(enc, hex("a8 eb 10 64 9c bf"));
        assert_eq!(huffman::decode(&enc).unwrap(), b"no-cache");

        // "custom-key" → 25a8 49e9 5ba9 7d7f (RFC §C.4.3).
        let enc = huffman::encode(b"custom-key");
        assert_eq!(enc, hex("25 a8 49 e9 5b a9 7d 7f"));
        assert_eq!(huffman::decode(&enc).unwrap(), b"custom-key");

        // "custom-value" → 25a8 49e9 5bb8 e8b4 bf (RFC §C.4.3).
        let enc = huffman::encode(b"custom-value");
        assert_eq!(enc, hex("25 a8 49 e9 5b b8 e8 b4 bf"));
        assert_eq!(huffman::decode(&enc).unwrap(), b"custom-value");
    }

    #[test]
    fn huffman_roundtrips_all_octets_and_random_data() {
        // Every single octet.
        for b in 0u16..=255 {
            let data = [b as u8];
            let enc = huffman::encode(&data);
            assert_eq!(huffman::encoded_len(&data), enc.len());
            assert_eq!(huffman::decode(&enc).unwrap(), data);
        }
        // A deterministic pseudo-random byte string.
        let mut data = Vec::new();
        let mut state = 0x1234_5678u32;
        for _ in 0..2000 {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            data.push((state >> 16) as u8);
        }
        let enc = huffman::encode(&data);
        assert_eq!(huffman::encoded_len(&data), enc.len());
        assert_eq!(huffman::decode(&enc).unwrap(), data);
    }

    #[test]
    fn huffman_decode_rejects_bad_padding() {
        // "00" is a 5-bit code for '0'; padding with zeros (not ones) is bad.
        // 0x00 = 00000 000: first 5 bits decode '0', trailing 000 != all-ones.
        assert!(matches!(
            huffman::decode(&[0x00]),
            Err(HpackError::InvalidHuffman(_))
        ));
        // A full byte of padding (>= 8 bits left over) is illegal.
        // 0x3f = code for '0' is 00000 (5 bits) leaves 111 (ok). Use instead
        // a byte that leaves a complete unconsumed code-less run: 0xff alone
        // is 8 bits of ones — no symbol is all-ones in <=8 bits except via
        // padding, so it should be rejected as incomplete.
        assert!(matches!(
            huffman::decode(&[0xff]),
            Err(HpackError::InvalidHuffman(_))
        ));
    }

    // --- String literals — RFC 7541 §5.2 --------------------------------

    #[test]
    fn string_literal_raw_and_huffman_roundtrip() {
        // A short, high-entropy string is cheaper raw.
        let mut out = Vec::new();
        encode_string("!", &mut out);
        assert_eq!(out[0] & 0x80, 0, "tiny string should not pick huffman");
        let (s, used) = decode_string(&out).unwrap();
        assert_eq!((s.as_str(), used), ("!", out.len()));

        // A long, low-entropy string is cheaper Huffman-coded.
        let mut out = Vec::new();
        encode_string("aaaaaaaaaaaaaaaaaaaa", &mut out);
        assert_eq!(out[0] & 0x80, 0x80, "repetitive string should pick huffman");
        let (s, used) = decode_string(&out).unwrap();
        assert_eq!((s.as_str(), used), ("aaaaaaaaaaaaaaaaaaaa", out.len()));
    }

    #[test]
    fn string_literal_rejects_truncation() {
        // Claims 5 bytes, supplies 2.
        assert_eq!(
            decode_string(&[0x05, b'a', b'b']),
            Err(HpackError::Truncated)
        );
        assert_eq!(decode_string(&[]), Err(HpackError::Truncated));
    }

    // --- Decoder: literal header field examples — RFC 7541 §C.2 ---------

    #[test]
    fn rfc_c2_1_literal_with_incremental_indexing() {
        // custom-key: custom-header
        let block = hex("400a 6375 7374 6f6d 2d6b 6579 0d63 7573 \
             746f 6d2d 6865 6164 6572");
        let mut dec = HpackDecoder::new(4096);
        let headers = dec.decode(&block).unwrap();
        assert_eq!(headers, vec![h("custom-key", "custom-header")]);
        // The field was added to the dynamic table.
        assert_eq!(dec.dynamic_table().len(), 1);
        assert_eq!(dec.dynamic_table().size(), 55);
        assert_eq!(
            dec.dynamic_table().get(0).unwrap(),
            &h("custom-key", "custom-header")
        );
    }

    #[test]
    fn rfc_c2_2_literal_without_indexing() {
        // :path: /sample/path
        let block = hex("040c 2f73 616d 706c 652f 7061 7468");
        let mut dec = HpackDecoder::new(4096);
        let headers = dec.decode(&block).unwrap();
        assert_eq!(headers, vec![h(":path", "/sample/path")]);
        // Not indexed.
        assert_eq!(dec.dynamic_table().len(), 0);
    }

    #[test]
    fn rfc_c2_3_literal_never_indexed() {
        // password: secret
        let block = hex("1008 7061 7373 776f 7264 0673 6563 7265 74");
        let mut dec = HpackDecoder::new(4096);
        let headers = dec.decode(&block).unwrap();
        assert_eq!(headers, vec![h("password", "secret")]);
        assert_eq!(dec.dynamic_table().len(), 0);
    }

    #[test]
    fn rfc_c2_4_indexed_header_field() {
        // :method: GET — index 2.
        let block = hex("82");
        let mut dec = HpackDecoder::new(4096);
        let headers = dec.decode(&block).unwrap();
        assert_eq!(headers, vec![h(":method", "GET")]);
    }

    // --- Decoder: §C.3 request sequence without Huffman -----------------

    #[test]
    fn rfc_c3_request_sequence_without_huffman() {
        let mut dec = HpackDecoder::new(4096);

        // C.3.1
        let b1 = hex("8286 8441 0f77 7777 2e65 7861 6d70 6c65 2e63 6f6d");
        assert_eq!(
            dec.decode(&b1).unwrap(),
            vec![
                h(":method", "GET"),
                h(":scheme", "http"),
                h(":path", "/"),
                h(":authority", "www.example.com"),
            ]
        );
        assert_eq!(dec.dynamic_table().size(), 57);
        assert_eq!(
            dec.dynamic_table().get(0).unwrap(),
            &h(":authority", "www.example.com")
        );

        // C.3.2
        let b2 = hex("8286 84be 5808 6e6f 2d63 6163 6865");
        assert_eq!(
            dec.decode(&b2).unwrap(),
            vec![
                h(":method", "GET"),
                h(":scheme", "http"),
                h(":path", "/"),
                h(":authority", "www.example.com"),
                h("cache-control", "no-cache"),
            ]
        );
        assert_eq!(dec.dynamic_table().size(), 110);

        // C.3.3
        let b3 = hex("8287 85bf 400a 6375 7374 6f6d 2d6b 6579 \
             0c63 7573 746f 6d2d 7661 6c75 65");
        assert_eq!(
            dec.decode(&b3).unwrap(),
            vec![
                h(":method", "GET"),
                h(":scheme", "https"),
                h(":path", "/index.html"),
                h(":authority", "www.example.com"),
                h("custom-key", "custom-value"),
            ]
        );
        assert_eq!(dec.dynamic_table().size(), 164);
    }

    // --- Decoder: §C.4 request sequence with Huffman --------------------

    #[test]
    fn rfc_c4_request_sequence_with_huffman() {
        let mut dec = HpackDecoder::new(4096);

        // C.4.1
        let b1 = hex("8286 8441 8cf1 e3c2 e5f2 3a6b a0ab 90f4 ff");
        assert_eq!(
            dec.decode(&b1).unwrap(),
            vec![
                h(":method", "GET"),
                h(":scheme", "http"),
                h(":path", "/"),
                h(":authority", "www.example.com"),
            ]
        );
        assert_eq!(dec.dynamic_table().size(), 57);

        // C.4.2
        let b2 = hex("8286 84be 5886 a8eb 1064 9cbf");
        assert_eq!(
            dec.decode(&b2).unwrap(),
            vec![
                h(":method", "GET"),
                h(":scheme", "http"),
                h(":path", "/"),
                h(":authority", "www.example.com"),
                h("cache-control", "no-cache"),
            ]
        );
        assert_eq!(dec.dynamic_table().size(), 110);

        // C.4.3
        let b3 = hex("8287 85bf 4088 25a8 49e9 5ba9 7d7f 8925 \
             a849 e95b b8e8 b4bf");
        assert_eq!(
            dec.decode(&b3).unwrap(),
            vec![
                h(":method", "GET"),
                h(":scheme", "https"),
                h(":path", "/index.html"),
                h(":authority", "www.example.com"),
                h("custom-key", "custom-value"),
            ]
        );
        assert_eq!(dec.dynamic_table().size(), 164);
    }

    // --- Decoder: §C.5/§C.6 response sequence with eviction -------------

    #[test]
    fn rfc_c5_response_sequence_without_huffman_evicts() {
        // Table size is constrained to 256 so eviction is exercised.
        let mut dec = HpackDecoder::new(256);

        // C.5.1
        let b1 = hex("4803 3330 3258 0770 7269 7661 7465 611d \
             4d6f 6e2c 2032 3120 4f63 7420 3230 3133 \
             2032 303a 3133 3a32 3120 474d 546e 1768 \
             7474 7073 3a2f 2f77 7777 2e65 7861 6d70 \
             6c65 2e63 6f6d");
        assert_eq!(
            dec.decode(&b1).unwrap(),
            vec![
                h(":status", "302"),
                h("cache-control", "private"),
                h("date", "Mon, 21 Oct 2013 20:13:21 GMT"),
                h("location", "https://www.example.com"),
            ]
        );
        assert_eq!(dec.dynamic_table().size(), 222);

        // C.5.2 — status changes; an eviction happens.
        let b2 = hex("4803 3330 37c1 c0bf");
        assert_eq!(
            dec.decode(&b2).unwrap(),
            vec![
                h(":status", "307"),
                h("cache-control", "private"),
                h("date", "Mon, 21 Oct 2013 20:13:21 GMT"),
                h("location", "https://www.example.com"),
            ]
        );
        assert_eq!(dec.dynamic_table().size(), 222);

        // C.5.3
        let b3 = hex("88c1 611d 4d6f 6e2c 2032 3120 4f63 7420 \
             3230 3133 2032 303a 3133 3a32 3220 474d \
             54c0 5a04 677a 6970 7738 666f 6f3d 4153 \
             444a 4b48 514b 425a 584f 5157 454f 5049 \
             5541 5851 5745 4f49 553b 206d 6178 2d61 \
             6765 3d33 3630 303b 2076 6572 7369 6f6e \
             3d31");
        assert_eq!(
            dec.decode(&b3).unwrap(),
            vec![
                h(":status", "200"),
                h("cache-control", "private"),
                h("date", "Mon, 21 Oct 2013 20:13:22 GMT"),
                h("location", "https://www.example.com"),
                h("content-encoding", "gzip"),
                h(
                    "set-cookie",
                    "foo=ASDJKHQKBZXOQWEOPIUAXQWEOIU; max-age=3600; version=1"
                ),
            ]
        );
        assert_eq!(dec.dynamic_table().size(), 215);
    }

    #[test]
    fn rfc_c6_response_sequence_with_huffman_evicts() {
        let mut dec = HpackDecoder::new(256);

        // C.6.1
        let b1 = hex("4882 6402 5885 aec3 771a 4b61 96d0 7abe \
             9410 54d4 44a8 2005 9504 0b81 66e0 82a6 \
             2d1b ff6e 919d 29ad 1718 63c7 8f0b 97c8 \
             e9ae 82ae 43d3");
        assert_eq!(
            dec.decode(&b1).unwrap(),
            vec![
                h(":status", "302"),
                h("cache-control", "private"),
                h("date", "Mon, 21 Oct 2013 20:13:21 GMT"),
                h("location", "https://www.example.com"),
            ]
        );
        assert_eq!(dec.dynamic_table().size(), 222);

        // C.6.2
        let b2 = hex("4883 640e ffc1 c0bf");
        assert_eq!(
            dec.decode(&b2).unwrap(),
            vec![
                h(":status", "307"),
                h("cache-control", "private"),
                h("date", "Mon, 21 Oct 2013 20:13:21 GMT"),
                h("location", "https://www.example.com"),
            ]
        );
        assert_eq!(dec.dynamic_table().size(), 222);

        // C.6.3
        let b3 = hex("88c1 6196 d07a be94 1054 d444 a820 0595 \
             040b 8166 e084 a62d 1bff c05a 839b d9ab \
             77ad 94e7 821d d7f2 e6c7 b335 dfdf cd5b \
             3960 d5af 2708 7f36 72c1 ab27 0fb5 291f \
             9587 3160 65c0 03ed 4ee5 b106 3d50 07");
        assert_eq!(
            dec.decode(&b3).unwrap(),
            vec![
                h(":status", "200"),
                h("cache-control", "private"),
                h("date", "Mon, 21 Oct 2013 20:13:22 GMT"),
                h("location", "https://www.example.com"),
                h("content-encoding", "gzip"),
                h(
                    "set-cookie",
                    "foo=ASDJKHQKBZXOQWEOPIUAXQWEOIU; max-age=3600; version=1"
                ),
            ]
        );
        assert_eq!(dec.dynamic_table().size(), 215);
    }

    // --- Decoder: protocol-level rejections -----------------------------

    #[test]
    fn decoder_rejects_oversized_header_list() {
        let mut dec = HpackDecoder::new(4096);
        dec.set_max_header_list_size(64);
        // Two indexed fields: :method GET (3+3+32=38) then :scheme http
        // (7+4+32=43) → 81 > 64.
        let block = hex("82 86");
        assert_eq!(
            dec.decode(&block),
            Err(HpackError::HeaderListTooLarge { limit: 64 })
        );
    }

    #[test]
    fn decoder_rejects_invalid_index() {
        let mut dec = HpackDecoder::new(4096);
        // Index 62 with an empty dynamic table does not exist.
        let block = hex(" be");
        assert_eq!(dec.decode(&block), Err(HpackError::InvalidIndex(62)));
        // Index 0 is never valid as an indexed field.
        let mut dec = HpackDecoder::new(4096);
        assert_eq!(dec.decode(&hex("80")), Err(HpackError::InvalidIndex(0)));
    }

    #[test]
    fn decoder_rejects_table_size_update_over_limit() {
        let mut dec = HpackDecoder::new(4096);
        // 001xxxxx with value 8192 > 4096.
        let mut block = Vec::new();
        encode_integer(8192, 5, &mut block);
        block[0] |= 0x20;
        assert_eq!(
            dec.decode(&block),
            Err(HpackError::TableSizeExceeded {
                requested: 8192,
                limit: 4096
            })
        );
    }

    #[test]
    fn decoder_rejects_misplaced_table_size_update() {
        let mut dec = HpackDecoder::new(4096);
        // An indexed field, then a size update — illegal ordering.
        let mut block = hex("82");
        let mut upd = Vec::new();
        encode_integer(0, 5, &mut upd);
        upd[0] |= 0x20;
        block.extend_from_slice(&upd);
        assert_eq!(
            dec.decode(&block),
            Err(HpackError::UnexpectedTableSizeUpdate)
        );
    }

    #[test]
    fn decoder_accepts_leading_table_size_update() {
        let mut dec = HpackDecoder::new(4096);
        // Size update to 100, then :method GET.
        let mut block = Vec::new();
        encode_integer(100, 5, &mut block);
        block[0] |= 0x20;
        block.push(0x82);
        let headers = dec.decode(&block).unwrap();
        assert_eq!(headers, vec![h(":method", "GET")]);
        assert_eq!(dec.dynamic_table().capacity(), 100);
    }

    // --- DynamicTable unit behaviour ------------------------------------

    #[test]
    fn dynamic_table_fifo_eviction() {
        // Capacity for exactly two 33-byte entries ("a"/"b" -> 1+0+32=33).
        let mut t = DynamicTable::new(66);
        t.insert("a", "");
        t.insert("b", "");
        assert_eq!(t.len(), 2);
        assert_eq!(t.size(), 66);
        // Inserting a third evicts the oldest ("a").
        t.insert("c", "");
        assert_eq!(t.len(), 2);
        assert_eq!(t.get(0).unwrap().0, "c");
        assert_eq!(t.get(1).unwrap().0, "b");
    }

    #[test]
    fn dynamic_table_oversized_entry_empties_table() {
        let mut t = DynamicTable::new(40);
        t.insert("a", "");
        assert_eq!(t.len(), 1);
        // 10 + 10 + 32 = 52 > 40: table is emptied, entry not stored.
        t.insert("0123456789", "0123456789");
        assert_eq!(t.len(), 0);
        assert_eq!(t.size(), 0);
    }

    #[test]
    fn dynamic_table_resize_evicts() {
        let mut t = DynamicTable::new(200);
        t.insert("aa", "bb"); // 2+2+32 = 36
        t.insert("cc", "dd"); // 36
        assert_eq!(t.size(), 72);
        t.resize(36); // only the newest survives
        assert_eq!(t.len(), 1);
        assert_eq!(t.get(0).unwrap(), &h("cc", "dd"));
        t.resize(0);
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn dynamic_table_set_max_capacity_clamps_down() {
        let mut t = DynamicTable::new(200);
        t.insert("aa", "bb");
        t.insert("cc", "dd");
        t.set_max_capacity(36);
        assert_eq!(t.max_capacity(), 36);
        assert_eq!(t.capacity(), 36);
        assert_eq!(t.len(), 1);
        // A size update may not now go above 36.
        assert!(t.apply_size_update(37).is_err());
        assert!(t.apply_size_update(36).is_ok());
    }

    // --- Encoder behaviour ----------------------------------------------

    #[test]
    fn encoder_emits_indexed_for_static_match() {
        let mut enc = HpackEncoder::new(4096);
        // :method GET is static index 2 → single byte 0x82.
        let block = enc.encode(&[h(":method", "GET")]);
        assert_eq!(block, vec![0x82]);
    }

    #[test]
    fn encoder_indexes_repeatable_headers_into_dynamic_table() {
        let mut enc = HpackEncoder::new(4096);
        let headers = vec![h("custom-key", "custom-value")];
        let first = enc.encode(&headers);
        // First time: literal with incremental indexing (top bits 01).
        assert_eq!(first[0] & 0xc0, 0x40);
        assert_eq!(enc.dynamic_table().len(), 1);
        // Second time: now indexable as a single indexed field.
        let second = enc.encode(&headers);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0] & 0x80, 0x80);
    }

    #[test]
    fn encoder_marks_sensitive_headers_never_indexed() {
        let mut enc = HpackEncoder::new(4096);
        let headers = vec![h("authorization", "Bearer xyz")];
        let block = enc.encode(&headers);
        // Never-indexed literal: top four bits 0001.
        assert_eq!(block[0] & 0xf0, 0x10);
        // It must not have entered the dynamic table.
        assert_eq!(enc.dynamic_table().len(), 0);
        // Encoding again still produces a never-indexed literal.
        let again = enc.encode(&headers);
        assert_eq!(again[0] & 0xf0, 0x10);
    }

    #[test]
    fn encoder_custom_sensitivity_predicate() {
        let mut enc = HpackEncoder::new(4096);
        enc.set_sensitivity_predicate(|name, _| name == "x-secret");
        let block = enc.encode(&[h("x-secret", "v")]);
        assert_eq!(block[0] & 0xf0, 0x10);
        assert_eq!(enc.dynamic_table().len(), 0);
        // authorization is no longer special under the custom predicate.
        let block = enc.encode(&[h("authorization", "v")]);
        assert_eq!(block[0] & 0xc0, 0x40); // incremental indexing
    }

    #[test]
    fn encoder_emits_pending_table_size_update() {
        let mut enc = HpackEncoder::new(4096);
        enc.set_table_size(512);
        let block = enc.encode(&[h(":method", "GET")]);
        // First instruction is a dynamic table size update (001xxxxx).
        assert_eq!(block[0] & 0xe0, 0x20);
        let (size, used) = decode_integer(&block, 5).unwrap();
        assert_eq!(size, 512);
        // Followed by the indexed :method GET.
        assert_eq!(block[used], 0x82);

        // And a decoder applies it.
        let mut dec = HpackDecoder::new(4096);
        let headers = dec.decode(&block).unwrap();
        assert_eq!(headers, vec![h(":method", "GET")]);
        assert_eq!(dec.dynamic_table().capacity(), 512);
    }

    // --- Encoder → Decoder round-trips ----------------------------------

    #[test]
    fn encoder_decoder_roundtrip_arbitrary_lists() {
        let mut enc = HpackEncoder::new(4096);
        let mut dec = HpackDecoder::new(4096);

        let lists = vec![
            vec![
                h(":method", "GET"),
                h(":scheme", "https"),
                h(":path", "/index.html"),
                h(":authority", "example.com"),
                h("user-agent", "tomcatrs/0.1"),
            ],
            vec![
                h(":method", "POST"),
                h(":scheme", "https"),
                h(":path", "/submit"),
                h(":authority", "example.com"),
                h("content-type", "application/json"),
                h("content-length", "1024"),
                h("custom-key", "custom-value"),
            ],
            vec![
                h(":status", "200"),
                h("content-type", "text/html; charset=utf-8"),
                h("server", "tomcatrs"),
                h("date", "Wed, 14 May 2026 00:00:00 GMT"),
            ],
            // Repeat the first list — should now be highly indexed.
            vec![
                h(":method", "GET"),
                h(":scheme", "https"),
                h(":path", "/index.html"),
                h(":authority", "example.com"),
                h("user-agent", "tomcatrs/0.1"),
            ],
        ];

        for list in &lists {
            let block = enc.encode(list);
            let decoded = dec.decode(&block).unwrap();
            assert_eq!(&decoded, list);
            // Encoder and decoder dynamic tables must stay in lockstep.
            assert_eq!(
                enc.dynamic_table().size(),
                dec.dynamic_table().size(),
                "dynamic tables diverged"
            );
        }
    }

    #[test]
    fn encoder_decoder_roundtrip_with_eviction_under_tiny_table() {
        let mut enc = HpackEncoder::new(128);
        let mut dec = HpackDecoder::new(128);
        // Many distinct headers, each bigger than a fraction of the table, so
        // eviction churns constantly.
        for i in 0..50 {
            let list = vec![
                h("x-request-id", &format!("req-{i}")),
                h("x-trace", &format!("trace-{i}-{i}")),
                h(":method", "GET"),
            ];
            let block = enc.encode(&list);
            let decoded = dec.decode(&block).unwrap();
            assert_eq!(decoded, list);
            assert_eq!(enc.dynamic_table().size(), dec.dynamic_table().size());
        }
    }

    #[test]
    fn encoder_decoder_roundtrip_sensitive_and_hints() {
        let mut enc = HpackEncoder::new(4096);
        let mut dec = HpackDecoder::new(4096);
        let headers = vec![
            h(":method", "GET"),
            h("authorization", "Bearer secret-token"),
            h("cookie", "session=abc123"),
            h("x-public", "cacheable"),
        ];
        let hints = vec![
            HeaderHint::Index,
            HeaderHint::NeverIndex,
            HeaderHint::NeverIndex,
            HeaderHint::WithoutIndex,
        ];
        let block = enc.encode_with_hints(&headers, &hints);
        assert_eq!(dec.decode(&block).unwrap(), headers);
        // Only the first (indexed) field entered the encoder's dynamic table.
        assert_eq!(enc.dynamic_table().len(), 0); // :method GET is a static match
    }
}
