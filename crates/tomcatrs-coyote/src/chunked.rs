//! HTTP/1.1 chunked transfer-encoding decoding (and a small encoding helper).
//!
//! This module implements the `chunked` coding defined in [RFC 9112 §7.1]:
//!
//! ```text
//! chunked-body   = *chunk
//!                  last-chunk
//!                  trailer-section
//!                  CRLF
//!
//! chunk          = chunk-size [ chunk-ext ] CRLF
//!                  chunk-data CRLF
//! chunk-size     = 1*HEXDIG
//! last-chunk     = 1*("0") [ chunk-ext ] CRLF
//!
//! chunk-ext      = *( BWS ";" BWS chunk-ext-name [ BWS "=" BWS chunk-ext-val ] )
//! trailer-section = *( field-line CRLF )
//! ```
//!
//! The [`ChunkedDecoder`] is fed raw bytes incrementally (whatever arrives off
//! the socket) and accumulates the *decoded* entity body. Chunk extensions are
//! parsed for framing purposes but their values are discarded — Tomcat does the
//! same. Trailer header fields are collected and exposed via
//! [`ChunkedDecoder::trailers`].
//!
//! Every decoder enforces a hard cap on the cumulative decoded body size
//! (`RequestLimits::max_post_size`) and on the length of a single
//! chunk-size/extension line, so a hostile peer cannot drive unbounded memory
//! use. Limit violations and grammar violations both surface as
//! [`tomcatrs_core::Error::Protocol`]; the caller maps those onto a `413` or
//! `400` response respectively (see [`ChunkedError`]).
//!
//! [RFC 9112 §7.1]: https://www.rfc-editor.org/rfc/rfc9112#section-7.1

use tomcatrs_core::Error;

/// Maximum length, in bytes, of a single `chunk-size [ chunk-ext ]` line
/// (excluding the terminating CRLF). A well-behaved client keeps this tiny; the
/// generous cap here only exists to bound memory against a malicious peer that
/// streams an endless extension.
const MAX_CHUNK_LINE_LEN: usize = 4096;

/// Maximum length, in bytes, of a single trailer header line (excluding CRLF).
const MAX_TRAILER_LINE_LEN: usize = 8192;

/// Maximum number of trailer header fields accepted.
const MAX_TRAILER_COUNT: usize = 32;

/// Why a chunked body failed to decode.
///
/// The connector translates this into an HTTP status: [`ChunkedError::TooLarge`]
/// maps to `413 Payload Too Large`, [`ChunkedError::Malformed`] maps to
/// `400 Bad Request`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkedError {
    /// The decoded body exceeded `RequestLimits::max_post_size`, or a framing
    /// line exceeded its hard cap.
    TooLarge(String),
    /// The byte stream did not conform to the chunked grammar.
    Malformed(String),
}

impl std::fmt::Display for ChunkedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChunkedError::TooLarge(m) => write!(f, "chunked body too large: {m}"),
            ChunkedError::Malformed(m) => write!(f, "malformed chunked body: {m}"),
        }
    }
}

impl std::error::Error for ChunkedError {}

impl From<ChunkedError> for Error {
    fn from(e: ChunkedError) -> Self {
        Error::protocol(e.to_string())
    }
}

/// The parser's position within the chunked grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Reading the `chunk-size [ chunk-ext ] CRLF` line.
    Size,
    /// Reading `chunk-data`; `remaining` data bytes are still expected.
    Data { remaining: usize },
    /// Reading the bare CRLF that terminates a `chunk-data`; `seen_cr` tracks
    /// whether the leading CR has already been consumed.
    DataCrlf { seen_cr: bool },
    /// Reading trailer header lines after the terminating `0` chunk.
    Trailer,
    /// The terminating CRLF (or the chunked body) has been fully consumed.
    Done,
}

/// An incremental decoder for an HTTP/1.1 chunked request body.
///
/// Feed bytes with [`push`](ChunkedDecoder::push); when
/// [`is_complete`](ChunkedDecoder::is_complete) returns `true` the full entity
/// body is available from [`into_body`](ChunkedDecoder::into_body) and any
/// trailer fields from [`trailers`](ChunkedDecoder::trailers).
#[derive(Debug)]
pub struct ChunkedDecoder {
    /// Hard cap on the cumulative decoded body length.
    max_body: usize,
    /// Current grammar state.
    state: State,
    /// Bytes seen for the in-progress framing line (size line or trailer line),
    /// not yet terminated by CRLF.
    line: Vec<u8>,
    /// Set by [`feed_line`](ChunkedDecoder::feed_line) when `line` holds a
    /// complete CRLF-terminated line ready to be parsed.
    line_terminated: bool,
    /// The decoded entity body accumulated so far.
    body: Vec<u8>,
    /// Trailer header fields collected after the last chunk.
    trailers: Vec<(String, String)>,
}

impl ChunkedDecoder {
    /// Create a decoder that will reject any body whose decoded length exceeds
    /// `max_body` bytes (typically `RequestLimits::max_post_size`).
    pub fn new(max_body: usize) -> Self {
        ChunkedDecoder {
            max_body,
            state: State::Size,
            line: Vec::new(),
            line_terminated: false,
            body: Vec::with_capacity(256),
            trailers: Vec::new(),
        }
    }

    /// Has the full chunked body — including the terminating CRLF — been
    /// consumed?
    pub fn is_complete(&self) -> bool {
        self.state == State::Done
    }

    /// The trailer header fields that followed the last chunk, in arrival order.
    ///
    /// Empty until [`is_complete`](Self::is_complete) is `true` (and usually
    /// empty even then — trailers are rare).
    pub fn trailers(&self) -> &[(String, String)] {
        &self.trailers
    }

    /// The number of decoded body bytes accumulated so far.
    pub fn body_len(&self) -> usize {
        self.body.len()
    }

    /// Consume the decoder and return the fully decoded entity body.
    ///
    /// Should be called only once [`is_complete`](Self::is_complete) is `true`;
    /// calling it earlier yields whatever has been decoded so far.
    pub fn into_body(self) -> Vec<u8> {
        self.body
    }

    /// Feed `input` into the decoder, advancing the parse as far as the data
    /// allows.
    ///
    /// Returns the number of bytes from `input` that were consumed. Because the
    /// chunked body may be followed by an unrelated pipelined request, the
    /// decoder stops exactly at the terminating CRLF: once
    /// [`is_complete`](Self::is_complete) is `true`, any unconsumed tail of
    /// `input` belongs to the next message and the caller must retain it.
    ///
    /// # Errors
    ///
    /// Returns [`ChunkedError::Malformed`] for a grammar violation (bad hex
    /// size, missing CRLF, oversized framing line) and [`ChunkedError::TooLarge`]
    /// when the decoded body would exceed the configured cap.
    pub fn push(&mut self, input: &[u8]) -> std::result::Result<usize, ChunkedError> {
        let mut idx = 0;
        while idx < input.len() {
            match self.state {
                State::Done => break,
                State::Size => {
                    idx += self.feed_line(&input[idx..], MAX_CHUNK_LINE_LEN, "chunk-size line")?;
                    if self.line_terminated {
                        self.parse_size_line()?;
                    }
                }
                State::Data { remaining } => {
                    let take = remaining.min(input.len() - idx);
                    self.body.extend_from_slice(&input[idx..idx + take]);
                    idx += take;
                    let left = remaining - take;
                    self.state = if left == 0 {
                        State::DataCrlf { seen_cr: false }
                    } else {
                        State::Data { remaining: left }
                    };
                }
                State::DataCrlf { seen_cr } => {
                    let b = input[idx];
                    idx += 1;
                    if !seen_cr {
                        if b != b'\r' {
                            return Err(ChunkedError::Malformed(
                                "chunk-data not followed by CRLF".into(),
                            ));
                        }
                        self.state = State::DataCrlf { seen_cr: true };
                    } else {
                        if b != b'\n' {
                            return Err(ChunkedError::Malformed(
                                "chunk-data CR not followed by LF".into(),
                            ));
                        }
                        self.state = State::Size;
                    }
                }
                State::Trailer => {
                    idx += self.feed_line(&input[idx..], MAX_TRAILER_LINE_LEN, "trailer line")?;
                    if self.line_terminated {
                        if self.line.is_empty() {
                            // The blank line ends the trailer section and the
                            // whole chunked body.
                            self.state = State::Done;
                        } else {
                            self.parse_trailer_line()?;
                        }
                    }
                }
            }
        }
        Ok(idx)
    }

    /// Accumulate bytes from `input` into `self.line` until a CRLF is seen or
    /// `max_len` is exceeded. Returns the count of `input` bytes consumed and
    /// sets [`line_terminated`](Self::line_terminated) when a full line (sans
    /// CRLF) sits in `self.line`.
    ///
    /// A bare LF without a preceding CR is rejected: RFC 9112 permits lenient
    /// LF-only line endings, but Tomcat-RS is strict here to keep the smuggling
    /// surface small.
    fn feed_line(
        &mut self,
        input: &[u8],
        max_len: usize,
        what: &str,
    ) -> std::result::Result<usize, ChunkedError> {
        self.line_terminated = false;
        let mut idx = 0;
        while idx < input.len() {
            let b = input[idx];
            idx += 1;
            if b == b'\n' {
                // Must be preceded by CR, which we stored as the last line byte.
                match self.line.last() {
                    Some(b'\r') => {
                        self.line.pop();
                        self.line_terminated = true;
                        return Ok(idx);
                    }
                    _ => {
                        return Err(ChunkedError::Malformed(format!(
                            "{what}: bare LF without CR"
                        )));
                    }
                }
            }
            self.line.push(b);
            if self.line.len() > max_len {
                return Err(ChunkedError::TooLarge(format!(
                    "{what} exceeds {max_len} bytes"
                )));
            }
        }
        Ok(idx)
    }

    /// Parse the accumulated `chunk-size [ chunk-ext ]` line and transition out
    /// of [`State::Size`]. Clears `self.line`.
    fn parse_size_line(&mut self) -> std::result::Result<(), ChunkedError> {
        let line = std::mem::take(&mut self.line);
        // Split off any chunk extensions at the first ';' — they are parsed for
        // framing (i.e. located and skipped) but their content is ignored.
        let size_part = match line.iter().position(|&b| b == b';') {
            Some(p) => &line[..p],
            None => &line[..],
        };
        let size_str = std::str::from_utf8(size_part)
            .map_err(|_| ChunkedError::Malformed("chunk-size is not ASCII".into()))?
            .trim();
        if size_str.is_empty() {
            return Err(ChunkedError::Malformed("empty chunk-size".into()));
        }
        if !size_str.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ChunkedError::Malformed(format!(
                "chunk-size {size_str:?} is not hexadecimal"
            )));
        }
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|_| ChunkedError::Malformed(format!("chunk-size {size_str:?} overflows")))?;

        if size == 0 {
            // last-chunk: move on to the trailer section.
            self.state = State::Trailer;
        } else {
            // Reject before buffering: the decoded body must stay within cap.
            if self.body.len().saturating_add(size) > self.max_body {
                return Err(ChunkedError::TooLarge(format!(
                    "decoded body would exceed {} bytes",
                    self.max_body
                )));
            }
            self.state = State::Data { remaining: size };
        }
        Ok(())
    }

    /// Parse one accumulated trailer header line into a `(name, value)` pair.
    /// Clears `self.line`.
    fn parse_trailer_line(&mut self) -> std::result::Result<(), ChunkedError> {
        let line = std::mem::take(&mut self.line);
        if self.trailers.len() >= MAX_TRAILER_COUNT {
            return Err(ChunkedError::TooLarge(format!(
                "more than {MAX_TRAILER_COUNT} trailer fields"
            )));
        }
        let text = std::str::from_utf8(&line)
            .map_err(|_| ChunkedError::Malformed("trailer field is not ASCII".into()))?;
        let colon = text
            .find(':')
            .ok_or_else(|| ChunkedError::Malformed("trailer field missing ':'".into()))?;
        let name = text[..colon].trim();
        let value = text[colon + 1..].trim();
        if name.is_empty() {
            return Err(ChunkedError::Malformed(
                "trailer field has empty name".into(),
            ));
        }
        self.trailers.push((name.to_string(), value.to_string()));
        Ok(())
    }
}

/// Encode `body` as a single-chunk chunked body followed by the terminating
/// `0` chunk and final CRLF.
///
/// This is the minimal, v1.0.0-scoped chunked **response** encoder: it is used
/// when a [`crate::Response`] body length is known at write time but the caller
/// has explicitly opted into chunked framing (for example to omit
/// `Content-Length`). Streaming, multi-chunk encoding is intentionally out of
/// scope for this release.
///
/// The returned bytes are the complete `chunked-body` production and can be
/// written straight after the response header block.
pub fn encode_chunked(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 16);
    if !body.is_empty() {
        out.extend_from_slice(format!("{:x}\r\n", body.len()).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\r\n");
    }
    // last-chunk + (empty) trailer-section + final CRLF.
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode a body delivered in one buffer, across several chunks.
    #[test]
    fn decodes_multi_chunk_body() {
        let mut d = ChunkedDecoder::new(1024);
        let raw = b"4\r\nWiki\r\n5\r\npedia\r\ne\r\n in\r\n\r\nchunks.\r\n0\r\n\r\n";
        let consumed = d.push(raw).unwrap();
        assert_eq!(consumed, raw.len());
        assert!(d.is_complete());
        assert_eq!(d.into_body(), b"Wikipedia in\r\n\r\nchunks.");
    }

    /// The decoder must reassemble correctly even when bytes are fed one at a
    /// time (the realistic socket case).
    #[test]
    fn decodes_body_fed_byte_by_byte() {
        let raw = b"3\r\nabc\r\n3\r\ndef\r\n0\r\n\r\n";
        let mut d = ChunkedDecoder::new(1024);
        for b in raw {
            d.push(std::slice::from_ref(b)).unwrap();
        }
        assert!(d.is_complete());
        assert_eq!(d.into_body(), b"abcdef");
    }

    /// Chunk extensions are located and skipped; their values never leak into
    /// the decoded body.
    #[test]
    fn chunk_extensions_are_ignored() {
        let mut d = ChunkedDecoder::new(1024);
        let raw = b"5;name=value;flag\r\nhello\r\n4 ; x=y \r\nbye!\r\n0\r\n\r\n";
        d.push(raw).unwrap();
        assert!(d.is_complete());
        assert_eq!(d.into_body(), b"hellobye!");
    }

    /// Trailer header fields after the last chunk are collected.
    #[test]
    fn collects_trailer_headers() {
        let mut d = ChunkedDecoder::new(1024);
        let raw = b"5\r\nhello\r\n0\r\nX-Checksum: abc123\r\nX-Note:  spaced \r\n\r\n";
        d.push(raw).unwrap();
        assert!(d.is_complete());
        assert_eq!(
            d.trailers(),
            &[
                ("X-Checksum".to_string(), "abc123".to_string()),
                ("X-Note".to_string(), "spaced".to_string()),
            ]
        );
        assert_eq!(d.into_body(), b"hello");
    }

    /// A body that exceeds the configured cap is rejected with `TooLarge`,
    /// and the check fires before the oversized chunk data is buffered.
    #[test]
    fn rejects_oversize_body() {
        let mut d = ChunkedDecoder::new(4);
        // First chunk (3 bytes) fits; second would push past the 4-byte cap.
        let raw = b"3\r\nabc\r\n3\r\ndef\r\n0\r\n\r\n";
        let err = d.push(raw).unwrap_err();
        assert!(matches!(err, ChunkedError::TooLarge(_)), "got {err:?}");
    }

    /// A non-hexadecimal chunk size is rejected with `Malformed`.
    #[test]
    fn rejects_malformed_chunk_size() {
        let mut d = ChunkedDecoder::new(1024);
        let err = d.push(b"xyz\r\nabc\r\n0\r\n\r\n").unwrap_err();
        assert!(matches!(err, ChunkedError::Malformed(_)), "got {err:?}");
    }

    /// An empty chunk-size line is malformed.
    #[test]
    fn rejects_empty_chunk_size() {
        let mut d = ChunkedDecoder::new(1024);
        let err = d.push(b"\r\nabc\r\n").unwrap_err();
        assert!(matches!(err, ChunkedError::Malformed(_)), "got {err:?}");
    }

    /// An over-long chunk-size/extension line is rejected before it can drive
    /// unbounded memory use.
    #[test]
    fn rejects_oversized_chunk_size_line() {
        let mut d = ChunkedDecoder::new(usize::MAX);
        let mut raw = b"1;".to_vec();
        raw.extend(std::iter::repeat_n(b'a', MAX_CHUNK_LINE_LEN + 10));
        raw.extend_from_slice(b"\r\nX\r\n0\r\n\r\n");
        let err = d.push(&raw).unwrap_err();
        assert!(matches!(err, ChunkedError::TooLarge(_)), "got {err:?}");
    }

    /// chunk-data not followed by CRLF is a framing violation.
    #[test]
    fn rejects_missing_chunk_data_crlf() {
        let mut d = ChunkedDecoder::new(1024);
        // "abc" then "XX" instead of CRLF.
        let err = d.push(b"3\r\nabcXX0\r\n\r\n").unwrap_err();
        assert!(matches!(err, ChunkedError::Malformed(_)), "got {err:?}");
    }

    /// The decoder stops exactly at the terminating CRLF, leaving any pipelined
    /// bytes for the caller.
    #[test]
    fn stops_at_end_leaving_pipelined_bytes() {
        let mut d = ChunkedDecoder::new(1024);
        let raw = b"3\r\nabc\r\n0\r\n\r\nGET / HTTP/1.1\r\n";
        let consumed = d.push(raw).unwrap();
        assert!(d.is_complete());
        assert_eq!(&raw[consumed..], b"GET / HTTP/1.1\r\n");
        assert_eq!(d.into_body(), b"abc");
    }

    /// An empty chunked body (just the last-chunk) decodes to an empty entity.
    #[test]
    fn decodes_empty_body() {
        let mut d = ChunkedDecoder::new(1024);
        d.push(b"0\r\n\r\n").unwrap();
        assert!(d.is_complete());
        assert!(d.into_body().is_empty());
    }

    /// The response encoder produces a well-formed chunked-body.
    #[test]
    fn encode_chunked_round_trips_through_decoder() {
        let encoded = encode_chunked(b"hello world");
        assert_eq!(&encoded, b"b\r\nhello world\r\n0\r\n\r\n");

        let mut d = ChunkedDecoder::new(1024);
        d.push(&encoded).unwrap();
        assert!(d.is_complete());
        assert_eq!(d.into_body(), b"hello world");
    }

    /// Encoding an empty body emits only the last-chunk.
    #[test]
    fn encode_chunked_empty_body() {
        assert_eq!(&encode_chunked(b""), b"0\r\n\r\n");
    }
}
