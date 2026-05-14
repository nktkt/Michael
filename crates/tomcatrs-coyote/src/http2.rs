//! HTTP/2 ([RFC 9113]) binary framing layer.
//!
//! This module implements the **frame layer** of HTTP/2: the connection
//! preface, the 9-octet frame header, and the wire encoding/decoding of every
//! frame type defined by RFC 9113 §6. It is deliberately *stateless* — it knows
//! nothing about streams, flow control, HPACK, or the connection lifecycle.
//! Those concerns live in the connection state machine (`http2_conn`), which
//! consumes the [`Frame`] and [`FrameHeader`] types defined here.
//!
//! # Incremental parsing
//!
//! [`Frame::parse`] follows the same incremental-parse contract as
//! [`crate::chunked`]: it is handed whatever bytes have arrived so far and
//! returns
//!
//! * `Ok(None)` — not enough bytes yet, call again with more,
//! * `Ok(Some((frame, consumed)))` — one frame parsed, `consumed` bytes used,
//! * `Err(_)` — a connection error ([`tomcatrs_core::Error::Protocol`]); the
//!   message names the RFC 9113 error code the caller should send in `GOAWAY`.
//!
//! # What this module validates
//!
//! Frame-layer errors that RFC 9113 classifies as *connection errors* are
//! caught here: a frame larger than the negotiated `SETTINGS_MAX_FRAME_SIZE`
//! (`FRAME_SIZE_ERROR`), a `SETTINGS` payload whose length is not a multiple of
//! 6 (`FRAME_SIZE_ERROR`), a `WINDOW_UPDATE` with a zero increment
//! (`PROTOCOL_ERROR`), a `PING` payload that is not exactly 8 octets
//! (`FRAME_SIZE_ERROR`), bad padding (`PROTOCOL_ERROR`), and so on. Stream-level
//! semantics (e.g. a `DATA` frame on stream 0) are intentionally left to the
//! connection state machine.
//!
//! [RFC 9113]: https://www.rfc-editor.org/rfc/rfc9113

use bytes::Bytes;
use tomcatrs_core::{Error, Result};

/// The HTTP/2 connection preface a client sends immediately after the
/// connection is established (RFC 9113 §3.4). The server must receive these
/// exact 24 octets before the first frame.
pub const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// The fixed length, in octets, of the frame header that prefixes every frame
/// (RFC 9113 §4.1): a 24-bit length, an 8-bit type, an 8-bit flags field, and a
/// 32-bit stream identifier (whose high bit is reserved).
pub const FRAME_HEADER_LEN: usize = 9;

/// The default value of `SETTINGS_MAX_FRAME_SIZE` (RFC 9113 §6.5.2): the
/// largest frame payload that may be sent before the peer advertises a larger
/// value. Also the minimum a peer is allowed to advertise.
pub const DEFAULT_MAX_FRAME_SIZE: u32 = 16_384;

/// The registered HTTP/2 frame types (RFC 9113 §6).
///
/// The discriminant is the on-wire type octet. Unknown frame types are *not*
/// represented here: per RFC 9113 §4.1 a receiver must ignore and discard
/// frames of unknown type, which the parser handles without constructing a
/// [`FrameType`].
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    /// `DATA` (§6.1) — carries arbitrary, variable-length octets of a stream.
    Data = 0,
    /// `HEADERS` (§6.2) — opens a stream and carries a header block fragment.
    Headers = 1,
    /// `PRIORITY` (§6.3) — advises the peer of a stream's priority.
    Priority = 2,
    /// `RST_STREAM` (§6.4) — abruptly terminates a stream.
    RstStream = 3,
    /// `SETTINGS` (§6.5) — conveys connection configuration parameters.
    Settings = 4,
    /// `PUSH_PROMISE` (§6.6) — signals an intent to push a stream.
    PushPromise = 5,
    /// `PING` (§6.7) — measures round-trip time and checks liveness.
    Ping = 6,
    /// `GOAWAY` (§6.8) — initiates connection shutdown.
    GoAway = 7,
    /// `WINDOW_UPDATE` (§6.9) — adjusts a flow-control window.
    WindowUpdate = 8,
    /// `CONTINUATION` (§6.10) — continues a header block fragment.
    Continuation = 9,
}

impl FrameType {
    /// Map an on-wire type octet to a [`FrameType`], or `None` if the type is
    /// not one this implementation models (an unknown/extension frame type).
    pub fn from_u8(v: u8) -> Option<FrameType> {
        Some(match v {
            0 => FrameType::Data,
            1 => FrameType::Headers,
            2 => FrameType::Priority,
            3 => FrameType::RstStream,
            4 => FrameType::Settings,
            5 => FrameType::PushPromise,
            6 => FrameType::Ping,
            7 => FrameType::GoAway,
            8 => FrameType::WindowUpdate,
            9 => FrameType::Continuation,
            _ => return None,
        })
    }
}

/// The 9-octet header that prefixes every HTTP/2 frame (RFC 9113 §4.1).
///
/// ```text
/// +-----------------------------------------------+
/// |                 Length (24)                   |
/// +---------------+---------------+---------------+
/// |   Type (8)    |   Flags (8)   |
/// +-+-------------+---------------+-------------------------------+
/// |R|                 Stream Identifier (31)                     |
/// +=+=============================================================+
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Length of the frame payload, in octets — does **not** include the 9
    /// header octets themselves. A 24-bit field, so at most `0xFF_FFFF`.
    pub length: u32,
    /// The frame type octet. Use [`FrameType::from_u8`] to interpret it; values
    /// with no [`FrameType`] are extension frames that must be ignored.
    pub frame_type: u8,
    /// The 8-bit flags field. Flag meanings are type-specific; see [`flags`].
    pub flags: u8,
    /// The 31-bit stream identifier, with the reserved high (`R`) bit already
    /// masked off. Stream `0` is the connection control stream.
    pub stream_id: u32,
}

impl FrameHeader {
    /// Parse a frame header from the first [`FRAME_HEADER_LEN`] octets of `buf`.
    ///
    /// Returns `None` if `buf` is shorter than [`FRAME_HEADER_LEN`]. The
    /// reserved high bit of the stream identifier is masked off and discarded,
    /// as RFC 9113 §4.1 requires of a receiver.
    pub fn parse(buf: &[u8]) -> Option<FrameHeader> {
        if buf.len() < FRAME_HEADER_LEN {
            return None;
        }
        let length = u32::from_be_bytes([0, buf[0], buf[1], buf[2]]);
        let frame_type = buf[3];
        let flags = buf[4];
        // Mask off the reserved R bit (RFC 9113 §4.1).
        let stream_id = u32::from_be_bytes([buf[5], buf[6], buf[7], buf[8]]) & STREAM_ID_MASK;
        Some(FrameHeader {
            length,
            frame_type,
            flags,
            stream_id,
        })
    }

    /// Encode this header into its 9-octet on-wire form.
    ///
    /// The reserved `R` bit is always emitted as `0`. Only the low 24 bits of
    /// `length` and the low 31 bits of `stream_id` are used; callers are
    /// expected to keep both within range.
    pub fn encode(&self) -> [u8; 9] {
        let len = self.length.to_be_bytes();
        let sid = (self.stream_id & STREAM_ID_MASK).to_be_bytes();
        [
            len[1],
            len[2],
            len[3],
            self.frame_type,
            self.flags,
            sid[0],
            sid[1],
            sid[2],
            sid[3],
        ]
    }
}

/// Mask selecting the 31-bit stream identifier, discarding the reserved `R`
/// bit (RFC 9113 §4.1, §5.1.1).
const STREAM_ID_MASK: u32 = 0x7FFF_FFFF;

/// Frame flag bits (RFC 9113 §6).
///
/// A flag bit's *meaning* depends on the frame type, but the bit *positions*
/// are shared across types — e.g. `0x1` is `END_STREAM` on `DATA`/`HEADERS` but
/// `ACK` on `SETTINGS`/`PING`. The parser interprets each in context.
pub mod flags {
    /// `END_STREAM` (`0x1`) on `DATA` and `HEADERS`: this frame is the last the
    /// endpoint will send on the stream.
    pub const END_STREAM: u8 = 0x1;
    /// `ACK` (`0x1`) on `SETTINGS` and `PING`: this frame acknowledges a
    /// previously received frame of the same type.
    pub const ACK: u8 = 0x1;
    /// `END_HEADERS` (`0x4`) on `HEADERS`, `PUSH_PROMISE`, and `CONTINUATION`:
    /// the header block ends with this frame (no `CONTINUATION` follows).
    pub const END_HEADERS: u8 = 0x4;
    /// `PADDED` (`0x8`) on `DATA`, `HEADERS`, and `PUSH_PROMISE`: the payload is
    /// prefixed with a `Pad Length` octet and suffixed with that many `0`
    /// padding octets.
    pub const PADDED: u8 = 0x8;
    /// `PRIORITY` (`0x20`) on `HEADERS`: the payload carries the 5-octet
    /// stream-dependency-and-weight priority block.
    pub const PRIORITY: u8 = 0x20;
}

/// HTTP/2 error codes (RFC 9113 §7), used in `RST_STREAM` and `GOAWAY` frames.
pub mod error_codes {
    /// `NO_ERROR` (`0x0`) — graceful shutdown.
    pub const NO_ERROR: u32 = 0x0;
    /// `PROTOCOL_ERROR` (`0x1`) — an unspecified protocol error.
    pub const PROTOCOL_ERROR: u32 = 0x1;
    /// `INTERNAL_ERROR` (`0x2`) — an unexpected internal error.
    pub const INTERNAL_ERROR: u32 = 0x2;
    /// `FLOW_CONTROL_ERROR` (`0x3`) — a flow-control protocol violation.
    pub const FLOW_CONTROL_ERROR: u32 = 0x3;
    /// `SETTINGS_TIMEOUT` (`0x4`) — a `SETTINGS` frame was not acknowledged.
    pub const SETTINGS_TIMEOUT: u32 = 0x4;
    /// `STREAM_CLOSED` (`0x5`) — a frame was received on a closed stream.
    pub const STREAM_CLOSED: u32 = 0x5;
    /// `FRAME_SIZE_ERROR` (`0x6`) — a frame had an invalid size.
    pub const FRAME_SIZE_ERROR: u32 = 0x6;
    /// `REFUSED_STREAM` (`0x7`) — the stream was refused before any processing.
    pub const REFUSED_STREAM: u32 = 0x7;
    /// `CANCEL` (`0x8`) — the stream is no longer needed.
    pub const CANCEL: u32 = 0x8;
    /// `COMPRESSION_ERROR` (`0x9`) — the HPACK decoder state was corrupted.
    pub const COMPRESSION_ERROR: u32 = 0x9;
    /// `CONNECT_ERROR` (`0xa`) — the connection established for a `CONNECT`
    /// request was reset or abnormally closed.
    pub const CONNECT_ERROR: u32 = 0xa;
    /// `ENHANCE_YOUR_CALM` (`0xb`) — the peer is generating excessive load.
    pub const ENHANCE_YOUR_CALM: u32 = 0xb;
    /// `INADEQUATE_SECURITY` (`0xc`) — the transport security is insufficient.
    pub const INADEQUATE_SECURITY: u32 = 0xc;
    /// `HTTP_1_1_REQUIRED` (`0xd`) — the peer should retry over HTTP/1.1.
    pub const HTTP_1_1_REQUIRED: u32 = 0xd;
}

/// `SETTINGS` parameter identifiers (RFC 9113 §6.5.2).
pub mod settings {
    /// `SETTINGS_HEADER_TABLE_SIZE` (`0x1`) — HPACK dynamic table size, octets.
    pub const HEADER_TABLE_SIZE: u16 = 0x1;
    /// `SETTINGS_ENABLE_PUSH` (`0x2`) — whether server push is permitted.
    pub const ENABLE_PUSH: u16 = 0x2;
    /// `SETTINGS_MAX_CONCURRENT_STREAMS` (`0x3`) — concurrent stream cap.
    pub const MAX_CONCURRENT_STREAMS: u16 = 0x3;
    /// `SETTINGS_INITIAL_WINDOW_SIZE` (`0x4`) — initial per-stream flow window.
    pub const INITIAL_WINDOW_SIZE: u16 = 0x4;
    /// `SETTINGS_MAX_FRAME_SIZE` (`0x5`) — largest acceptable frame payload.
    pub const MAX_FRAME_SIZE: u16 = 0x5;
    /// `SETTINGS_MAX_HEADER_LIST_SIZE` (`0x6`) — largest acceptable header list.
    pub const MAX_HEADER_LIST_SIZE: u16 = 0x6;
}

/// The stream-dependency-and-weight priority block (RFC 9113 §6.3).
///
/// This block appears as the body of a `PRIORITY` frame, and — when the
/// `PRIORITY` flag is set — as a prefix of a `HEADERS` frame payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Priority {
    /// The `E` bit: if set, the dependency is *exclusive* (RFC 9113 §5.3.1).
    pub exclusive: bool,
    /// The stream this stream depends on (`0` means it depends on the
    /// connection's root). The reserved high bit is masked off.
    pub stream_dependency: u32,
    /// The weight, on the wire as `weight - 1`; this field holds the value as
    /// transmitted, i.e. an effective weight of `weight + 1` in the range
    /// 1–256.
    pub weight: u8,
}

impl Priority {
    /// Parse a 5-octet priority block.
    fn parse(buf: &[u8]) -> Priority {
        let raw = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        Priority {
            exclusive: raw & 0x8000_0000 != 0,
            stream_dependency: raw & STREAM_ID_MASK,
            weight: buf[4],
        }
    }

    /// Append this priority block's 5 on-wire octets to `out`.
    fn encode_into(&self, out: &mut Vec<u8>) {
        let mut dep = self.stream_dependency & STREAM_ID_MASK;
        if self.exclusive {
            dep |= 0x8000_0000;
        }
        out.extend_from_slice(&dep.to_be_bytes());
        out.push(self.weight);
    }
}

/// A single decoded HTTP/2 frame (RFC 9113 §6).
///
/// Header block fragments (in [`Frame::Headers`] and [`Frame::Continuation`])
/// are carried *un-decompressed*: HPACK decoding is the connection state
/// machine's responsibility, not the frame layer's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// `DATA` (§6.1). Padding, if any, has already been stripped from `data`.
    Data {
        /// The stream this data belongs to (never `0`, though the frame layer
        /// does not enforce that).
        stream_id: u32,
        /// The application data octets, with any padding removed.
        data: Bytes,
        /// Whether the `END_STREAM` flag was set.
        end_stream: bool,
    },
    /// `HEADERS` (§6.2). Padding has been stripped; any priority block has been
    /// lifted out into `priority`. `block` is the raw HPACK-encoded fragment.
    Headers {
        /// The stream being opened or continued.
        stream_id: u32,
        /// The (still HPACK-compressed) header block fragment.
        block: Bytes,
        /// Whether the `END_STREAM` flag was set.
        end_stream: bool,
        /// Whether the `END_HEADERS` flag was set (no `CONTINUATION` follows).
        end_headers: bool,
        /// The priority block, present iff the `PRIORITY` flag was set.
        priority: Option<Priority>,
    },
    /// `PRIORITY` (§6.3).
    Priority {
        /// The stream whose priority is being set.
        stream_id: u32,
        /// The priority information.
        priority: Priority,
    },
    /// `RST_STREAM` (§6.4).
    RstStream {
        /// The stream being terminated.
        stream_id: u32,
        /// The reason, as an [`error_codes`] value.
        error_code: u32,
    },
    /// `SETTINGS` (§6.5).
    Settings {
        /// Whether this frame is an acknowledgement (`ACK` flag set). An ACK
        /// carries no parameters.
        ack: bool,
        /// The `(identifier, value)` parameter pairs, in wire order.
        params: Vec<(u16, u32)>,
    },
    /// `PING` (§6.7).
    Ping {
        /// Whether this frame is an acknowledgement (`ACK` flag set).
        ack: bool,
        /// The 8 opaque octets, echoed verbatim in the ACK.
        payload: [u8; 8],
    },
    /// `GOAWAY` (§6.8).
    GoAway {
        /// The highest-numbered stream the sender might have acted on.
        last_stream_id: u32,
        /// The reason, as an [`error_codes`] value.
        error_code: u32,
        /// Optional opaque debug data.
        debug: Bytes,
    },
    /// `WINDOW_UPDATE` (§6.9).
    WindowUpdate {
        /// The stream whose window is updated; `0` updates the connection
        /// window.
        stream_id: u32,
        /// The flow-control window increment (always non-zero — a zero
        /// increment is rejected at parse time).
        increment: u32,
    },
    /// `CONTINUATION` (§6.10).
    Continuation {
        /// The stream whose header block is being continued.
        stream_id: u32,
        /// The (still HPACK-compressed) header block fragment.
        block: Bytes,
        /// Whether the `END_HEADERS` flag was set.
        end_headers: bool,
    },
}

/// Build a `FRAME_SIZE_ERROR` connection error.
fn frame_size_error(msg: &str) -> Error {
    Error::protocol(format!("FRAME_SIZE_ERROR: {msg}"))
}

/// Build a `PROTOCOL_ERROR` connection error.
fn protocol_error(msg: &str) -> Error {
    Error::protocol(format!("PROTOCOL_ERROR: {msg}"))
}

/// Strip the `PADDED`-flag padding from a frame payload.
///
/// `payload` is the full frame payload (everything after the 9-octet header).
/// Returns the slice of *content* with the leading `Pad Length` octet and the
/// trailing padding octets removed.
///
/// # Errors
///
/// Returns a `PROTOCOL_ERROR` if the payload is empty (no room for the
/// `Pad Length` octet) or if `Pad Length` is larger than the remaining payload
/// (RFC 9113 §6.1).
fn strip_padding(payload: &[u8]) -> Result<&[u8]> {
    let (&pad_len, rest) = payload
        .split_first()
        .ok_or_else(|| protocol_error("padded frame has no Pad Length octet"))?;
    let pad_len = pad_len as usize;
    if pad_len > rest.len() {
        return Err(protocol_error(
            "Pad Length exceeds the remaining frame payload",
        ));
    }
    Ok(&rest[..rest.len() - pad_len])
}

impl Frame {
    /// Incrementally parse one frame from the front of `buf`.
    ///
    /// Returns `Ok(None)` if `buf` does not yet hold a complete frame (header
    /// plus the declared payload), or `Ok(Some((frame, consumed)))` with the
    /// number of octets consumed from `buf`. Unknown/extension frame types are
    /// skipped transparently: their octets are counted in `consumed` but no
    /// `Frame` is produced — instead the parser recurses onto the next frame.
    ///
    /// `max_frame_size` is the locally advertised `SETTINGS_MAX_FRAME_SIZE`; a
    /// frame whose declared length exceeds it is a connection error.
    ///
    /// # Errors
    ///
    /// Returns [`tomcatrs_core::Error::Protocol`] for any frame-layer
    /// connection error (oversized frame, malformed `SETTINGS`/`PING`/
    /// `WINDOW_UPDATE`/`RST_STREAM`/`PRIORITY` length, bad padding, zero
    /// `WINDOW_UPDATE` increment). The message is prefixed with the RFC 9113
    /// error-code name the caller should report.
    pub fn parse(buf: &[u8], max_frame_size: u32) -> Result<Option<(Frame, usize)>> {
        let mut offset = 0;
        loop {
            let header = match FrameHeader::parse(&buf[offset..]) {
                Some(h) => h,
                None => return Ok(None),
            };

            if header.length > max_frame_size {
                return Err(frame_size_error(&format!(
                    "frame length {} exceeds SETTINGS_MAX_FRAME_SIZE {}",
                    header.length, max_frame_size
                )));
            }

            let payload_len = header.length as usize;
            let frame_end = offset + FRAME_HEADER_LEN + payload_len;
            if buf.len() < frame_end {
                // The header is here but the payload has not fully arrived.
                return Ok(None);
            }
            let payload = &buf[offset + FRAME_HEADER_LEN..frame_end];

            match FrameType::from_u8(header.frame_type) {
                Some(ft) => {
                    let frame = Self::decode_payload(ft, &header, payload)?;
                    return Ok(Some((frame, frame_end)));
                }
                None => {
                    // RFC 9113 §4.1: ignore and discard frames of unknown type.
                    // Skip past it and try to parse the next frame.
                    offset = frame_end;
                }
            }
        }
    }

    /// Decode a known frame type's payload into a [`Frame`].
    fn decode_payload(ft: FrameType, header: &FrameHeader, payload: &[u8]) -> Result<Frame> {
        let flags = header.flags;
        match ft {
            FrameType::Data => {
                let content = if flags & flags::PADDED != 0 {
                    strip_padding(payload)?
                } else {
                    payload
                };
                Ok(Frame::Data {
                    stream_id: header.stream_id,
                    data: Bytes::copy_from_slice(content),
                    end_stream: flags & flags::END_STREAM != 0,
                })
            }
            FrameType::Headers => {
                // Strip padding first so the padding count and padding octets
                // bracket the *entire* remaining payload (RFC 9113 §6.2).
                let mut content = if flags & flags::PADDED != 0 {
                    strip_padding(payload)?
                } else {
                    payload
                };
                let priority = if flags & flags::PRIORITY != 0 {
                    if content.len() < 5 {
                        return Err(frame_size_error(
                            "HEADERS with PRIORITY flag is shorter than its 5-octet priority block",
                        ));
                    }
                    let p = Priority::parse(&content[..5]);
                    content = &content[5..];
                    Some(p)
                } else {
                    None
                };
                Ok(Frame::Headers {
                    stream_id: header.stream_id,
                    block: Bytes::copy_from_slice(content),
                    end_stream: flags & flags::END_STREAM != 0,
                    end_headers: flags & flags::END_HEADERS != 0,
                    priority,
                })
            }
            FrameType::Priority => {
                if payload.len() != 5 {
                    return Err(frame_size_error(
                        "PRIORITY frame payload must be exactly 5 octets",
                    ));
                }
                Ok(Frame::Priority {
                    stream_id: header.stream_id,
                    priority: Priority::parse(payload),
                })
            }
            FrameType::RstStream => {
                if payload.len() != 4 {
                    return Err(frame_size_error(
                        "RST_STREAM frame payload must be exactly 4 octets",
                    ));
                }
                Ok(Frame::RstStream {
                    stream_id: header.stream_id,
                    error_code: u32::from_be_bytes([
                        payload[0], payload[1], payload[2], payload[3],
                    ]),
                })
            }
            FrameType::Settings => {
                let ack = flags & flags::ACK != 0;
                if ack && !payload.is_empty() {
                    return Err(frame_size_error(
                        "SETTINGS ACK frame must carry an empty payload",
                    ));
                }
                if payload.len() % 6 != 0 {
                    return Err(frame_size_error(
                        "SETTINGS frame payload length must be a multiple of 6",
                    ));
                }
                let mut params = Vec::with_capacity(payload.len() / 6);
                for entry in payload.chunks_exact(6) {
                    let id = u16::from_be_bytes([entry[0], entry[1]]);
                    let value = u32::from_be_bytes([entry[2], entry[3], entry[4], entry[5]]);
                    params.push((id, value));
                }
                Ok(Frame::Settings { ack, params })
            }
            FrameType::PushPromise => {
                // A server frame layer never *receives* PUSH_PROMISE from a
                // client, and this runtime does not initiate push. Treat it as
                // a protocol error rather than modelling a frame we never act
                // on.
                Err(protocol_error(
                    "PUSH_PROMISE is not supported by this endpoint",
                ))
            }
            FrameType::Ping => {
                if payload.len() != 8 {
                    return Err(frame_size_error(
                        "PING frame payload must be exactly 8 octets",
                    ));
                }
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(payload);
                Ok(Frame::Ping {
                    ack: flags & flags::ACK != 0,
                    payload: bytes,
                })
            }
            FrameType::GoAway => {
                if payload.len() < 8 {
                    return Err(frame_size_error(
                        "GOAWAY frame payload must be at least 8 octets",
                    ));
                }
                let last_stream_id =
                    u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
                        & STREAM_ID_MASK;
                let error_code =
                    u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
                Ok(Frame::GoAway {
                    last_stream_id,
                    error_code,
                    debug: Bytes::copy_from_slice(&payload[8..]),
                })
            }
            FrameType::WindowUpdate => {
                if payload.len() != 4 {
                    return Err(frame_size_error(
                        "WINDOW_UPDATE frame payload must be exactly 4 octets",
                    ));
                }
                // The reserved high bit is masked off (RFC 9113 §6.9).
                let increment =
                    u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
                        & STREAM_ID_MASK;
                if increment == 0 {
                    return Err(protocol_error("WINDOW_UPDATE increment must be non-zero"));
                }
                Ok(Frame::WindowUpdate {
                    stream_id: header.stream_id,
                    increment,
                })
            }
            FrameType::Continuation => Ok(Frame::Continuation {
                stream_id: header.stream_id,
                block: Bytes::copy_from_slice(payload),
                end_headers: flags & flags::END_HEADERS != 0,
            }),
        }
    }

    /// Serialize this frame — header and payload — into its on-wire form.
    ///
    /// Frames are always emitted *without* padding: the frame layer never adds
    /// padding on the send side (it is purely a defensive/obfuscation feature a
    /// sender may opt into, and this runtime does not). Header block fragments
    /// are emitted verbatim, so the caller must hand in already-HPACK-encoded
    /// bytes.
    pub fn encode(&self) -> Vec<u8> {
        // `body` collects the frame payload; the 9-octet header is prepended
        // once the payload length is known.
        let mut body: Vec<u8> = Vec::new();
        let (frame_type, flags, stream_id) = match self {
            Frame::Data {
                stream_id,
                data,
                end_stream,
            } => {
                body.extend_from_slice(data);
                let flags = if *end_stream { flags::END_STREAM } else { 0 };
                (FrameType::Data as u8, flags, *stream_id)
            }
            Frame::Headers {
                stream_id,
                block,
                end_stream,
                end_headers,
                priority,
            } => {
                let mut flags = 0;
                if *end_stream {
                    flags |= flags::END_STREAM;
                }
                if *end_headers {
                    flags |= flags::END_HEADERS;
                }
                if let Some(p) = priority {
                    flags |= flags::PRIORITY;
                    p.encode_into(&mut body);
                }
                body.extend_from_slice(block);
                (FrameType::Headers as u8, flags, *stream_id)
            }
            Frame::Priority {
                stream_id,
                priority,
            } => {
                priority.encode_into(&mut body);
                (FrameType::Priority as u8, 0, *stream_id)
            }
            Frame::RstStream {
                stream_id,
                error_code,
            } => {
                body.extend_from_slice(&error_code.to_be_bytes());
                (FrameType::RstStream as u8, 0, *stream_id)
            }
            Frame::Settings { ack, params } => {
                for (id, value) in params {
                    body.extend_from_slice(&id.to_be_bytes());
                    body.extend_from_slice(&value.to_be_bytes());
                }
                let flags = if *ack { flags::ACK } else { 0 };
                (FrameType::Settings as u8, flags, 0)
            }
            Frame::Ping { ack, payload } => {
                body.extend_from_slice(payload);
                let flags = if *ack { flags::ACK } else { 0 };
                (FrameType::Ping as u8, flags, 0)
            }
            Frame::GoAway {
                last_stream_id,
                error_code,
                debug,
            } => {
                body.extend_from_slice(&(last_stream_id & STREAM_ID_MASK).to_be_bytes());
                body.extend_from_slice(&error_code.to_be_bytes());
                body.extend_from_slice(debug);
                (FrameType::GoAway as u8, 0, 0)
            }
            Frame::WindowUpdate {
                stream_id,
                increment,
            } => {
                body.extend_from_slice(&(increment & STREAM_ID_MASK).to_be_bytes());
                (FrameType::WindowUpdate as u8, 0, *stream_id)
            }
            Frame::Continuation {
                stream_id,
                block,
                end_headers,
            } => {
                body.extend_from_slice(block);
                let flags = if *end_headers { flags::END_HEADERS } else { 0 };
                (FrameType::Continuation as u8, flags, *stream_id)
            }
        };

        let header = FrameHeader {
            length: body.len() as u32,
            frame_type,
            flags,
            stream_id,
        };
        let mut out = Vec::with_capacity(FRAME_HEADER_LEN + body.len());
        out.extend_from_slice(&header.encode());
        out.extend_from_slice(&body);
        out
    }

    /// The stream identifier this frame applies to.
    ///
    /// Connection-level frames (`SETTINGS`, `PING`, `GOAWAY`) report `0`.
    pub fn stream_id(&self) -> u32 {
        match self {
            Frame::Data { stream_id, .. }
            | Frame::Headers { stream_id, .. }
            | Frame::Priority { stream_id, .. }
            | Frame::RstStream { stream_id, .. }
            | Frame::WindowUpdate { stream_id, .. }
            | Frame::Continuation { stream_id, .. } => *stream_id,
            Frame::Settings { .. } | Frame::Ping { .. } | Frame::GoAway { .. } => 0,
        }
    }
}

/// The canonical "HTTP/2 is not available yet" error.
///
/// The frame layer in this module is complete, but the HTTP/2 *connection*
/// driver (preface exchange, HPACK, stream multiplexing) is still being built
/// in a separate module; until it is wired into the protocol dispatcher,
/// [`crate::protocol`] reports HTTP/2 as unsupported through this helper.
pub fn unsupported() -> Error {
    Error::protocol("HTTP/2 not implemented in v0.1.0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preface_is_the_rfc_9113_octets() {
        assert_eq!(PREFACE, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
        assert_eq!(PREFACE.len(), 24);
    }

    #[test]
    fn unsupported_is_a_protocol_error() {
        assert!(matches!(unsupported(), Error::Protocol(_)));
    }

    #[test]
    fn frame_header_round_trips() {
        let h = FrameHeader {
            length: 0x01_2345,
            frame_type: 4,
            flags: 0x1,
            stream_id: 0x0102_0304,
        };
        let bytes = h.encode();
        assert_eq!(bytes.len(), FRAME_HEADER_LEN);
        let parsed = FrameHeader::parse(&bytes).unwrap();
        assert_eq!(parsed, h);
    }

    #[test]
    fn frame_header_encodes_big_endian() {
        let h = FrameHeader {
            length: 0x00_00FF,
            frame_type: 1,
            flags: 0,
            stream_id: 1,
        };
        // length 24-bit BE = 00 00 FF, type 01, flags 00, stream 00 00 00 01.
        assert_eq!(
            h.encode(),
            [0x00, 0x00, 0xFF, 0x01, 0x00, 0x00, 0x00, 0x00, 0x01]
        );
    }

    #[test]
    fn frame_header_masks_reserved_r_bit() {
        // The reserved high bit set on the stream id must be discarded.
        let raw = [0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF];
        let parsed = FrameHeader::parse(&raw).unwrap();
        assert_eq!(parsed.stream_id, 0x7FFF_FFFF);
    }

    #[test]
    fn frame_header_parse_needs_full_header() {
        assert!(FrameHeader::parse(&[0u8; 8]).is_none());
        assert!(FrameHeader::parse(&[0u8; 9]).is_some());
    }

    #[test]
    fn partial_buffer_returns_none() {
        // A SETTINGS frame declaring 6 payload octets, but only 3 supplied.
        let mut buf = vec![0x00, 0x00, 0x06, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00];
        buf.extend_from_slice(&[0x00, 0x01, 0x00]); // only 3 of 6 payload octets
        assert!(Frame::parse(&buf, DEFAULT_MAX_FRAME_SIZE)
            .unwrap()
            .is_none());
    }

    #[test]
    fn empty_buffer_returns_none() {
        assert!(Frame::parse(&[], DEFAULT_MAX_FRAME_SIZE).unwrap().is_none());
    }

    #[test]
    fn oversized_frame_is_rejected() {
        // Declare a payload length of 100 with a max frame size of 50.
        let buf = [0x00, 0x00, 0x64, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01];
        let err = Frame::parse(&buf, 50).unwrap_err();
        assert!(matches!(err, Error::Protocol(m) if m.contains("FRAME_SIZE_ERROR")));
    }

    /// Helper: parse exactly one frame from a fully-supplied buffer.
    fn parse_one(buf: &[u8]) -> Frame {
        let (frame, consumed) = Frame::parse(buf, DEFAULT_MAX_FRAME_SIZE)
            .expect("parse must not error")
            .expect("a full frame must be available");
        assert_eq!(consumed, buf.len(), "the whole buffer should be consumed");
        frame
    }

    /// Helper: round-trip a frame through encode then parse.
    fn round_trip(frame: Frame) {
        let encoded = frame.encode();
        let parsed = parse_one(&encoded);
        assert_eq!(parsed, frame);
    }

    #[test]
    fn data_frame_round_trips() {
        round_trip(Frame::Data {
            stream_id: 3,
            data: Bytes::from_static(b"hello http2"),
            end_stream: true,
        });
        round_trip(Frame::Data {
            stream_id: 7,
            data: Bytes::new(),
            end_stream: false,
        });
    }

    #[test]
    fn padded_data_frame_parses_with_padding_stripped() {
        // Hand-build a padded DATA frame: Pad Length = 4, data = "abc",
        // then 4 zero padding octets. Payload length = 1 + 3 + 4 = 8.
        let mut payload = vec![4u8];
        payload.extend_from_slice(b"abc");
        payload.extend_from_slice(&[0u8; 4]);
        let header = FrameHeader {
            length: payload.len() as u32,
            frame_type: FrameType::Data as u8,
            flags: flags::PADDED | flags::END_STREAM,
            stream_id: 1,
        };
        let mut buf = header.encode().to_vec();
        buf.extend_from_slice(&payload);

        let frame = parse_one(&buf);
        match frame {
            Frame::Data {
                stream_id,
                data,
                end_stream,
            } => {
                assert_eq!(stream_id, 1);
                assert_eq!(&data[..], b"abc");
                assert!(end_stream);
            }
            other => panic!("expected DATA, got {other:?}"),
        }
    }

    #[test]
    fn padded_frame_with_bad_pad_length_is_rejected() {
        // Pad Length = 200 but only 3 further octets: PROTOCOL_ERROR.
        let mut payload = vec![200u8];
        payload.extend_from_slice(b"abc");
        let header = FrameHeader {
            length: payload.len() as u32,
            frame_type: FrameType::Data as u8,
            flags: flags::PADDED,
            stream_id: 1,
        };
        let mut buf = header.encode().to_vec();
        buf.extend_from_slice(&payload);
        let err = Frame::parse(&buf, DEFAULT_MAX_FRAME_SIZE).unwrap_err();
        assert!(matches!(err, Error::Protocol(m) if m.contains("PROTOCOL_ERROR")));
    }

    #[test]
    fn headers_frame_round_trips_without_priority() {
        round_trip(Frame::Headers {
            stream_id: 5,
            block: Bytes::from_static(b"\x82\x86\x84"), // fake HPACK bytes
            end_stream: false,
            end_headers: true,
            priority: None,
        });
    }

    #[test]
    fn headers_frame_round_trips_with_priority() {
        round_trip(Frame::Headers {
            stream_id: 9,
            block: Bytes::from_static(b"hpack-fragment"),
            end_stream: true,
            end_headers: true,
            priority: Some(Priority {
                exclusive: true,
                stream_dependency: 3,
                weight: 201,
            }),
        });
    }

    #[test]
    fn padded_headers_with_priority_parses() {
        // Build: Pad Length(1) + priority block(5) + block + padding.
        let block = b"the-header-block";
        let prio = Priority {
            exclusive: false,
            stream_dependency: 1,
            weight: 16,
        };
        let mut payload = vec![3u8]; // Pad Length = 3
        prio.encode_into(&mut payload);
        payload.extend_from_slice(block);
        payload.extend_from_slice(&[0u8; 3]); // padding
        let header = FrameHeader {
            length: payload.len() as u32,
            frame_type: FrameType::Headers as u8,
            flags: flags::PADDED | flags::PRIORITY | flags::END_HEADERS,
            stream_id: 11,
        };
        let mut buf = header.encode().to_vec();
        buf.extend_from_slice(&payload);

        match parse_one(&buf) {
            Frame::Headers {
                stream_id,
                block: parsed_block,
                end_stream,
                end_headers,
                priority,
            } => {
                assert_eq!(stream_id, 11);
                assert_eq!(&parsed_block[..], block);
                assert!(!end_stream);
                assert!(end_headers);
                assert_eq!(priority, Some(prio));
            }
            other => panic!("expected HEADERS, got {other:?}"),
        }
    }

    #[test]
    fn priority_frame_round_trips() {
        round_trip(Frame::Priority {
            stream_id: 3,
            priority: Priority {
                exclusive: false,
                stream_dependency: 1,
                weight: 0,
            },
        });
    }

    #[test]
    fn priority_frame_with_wrong_length_is_rejected() {
        // PRIORITY must be exactly 5 octets; declare 4.
        let buf = [
            0x00,
            0x00,
            0x04,
            FrameType::Priority as u8,
            0x00,
            0x00,
            0x00,
            0x00,
            0x03, // header
            0x00,
            0x00,
            0x00,
            0x01, // 4-octet payload
        ];
        let err = Frame::parse(&buf, DEFAULT_MAX_FRAME_SIZE).unwrap_err();
        assert!(matches!(err, Error::Protocol(m) if m.contains("FRAME_SIZE_ERROR")));
    }

    #[test]
    fn rst_stream_round_trips() {
        round_trip(Frame::RstStream {
            stream_id: 7,
            error_code: error_codes::CANCEL,
        });
    }

    #[test]
    fn rst_stream_with_wrong_length_is_rejected() {
        let buf = [
            0x00,
            0x00,
            0x03,
            FrameType::RstStream as u8,
            0x00,
            0x00,
            0x00,
            0x00,
            0x01,
            0x00,
            0x00,
            0x00,
        ];
        let err = Frame::parse(&buf, DEFAULT_MAX_FRAME_SIZE).unwrap_err();
        assert!(matches!(err, Error::Protocol(m) if m.contains("FRAME_SIZE_ERROR")));
    }

    #[test]
    fn settings_frame_round_trips() {
        round_trip(Frame::Settings {
            ack: false,
            params: vec![
                (settings::HEADER_TABLE_SIZE, 4096),
                (settings::MAX_CONCURRENT_STREAMS, 128),
                (settings::INITIAL_WINDOW_SIZE, 65_535),
            ],
        });
    }

    #[test]
    fn settings_ack_round_trips() {
        round_trip(Frame::Settings {
            ack: true,
            params: vec![],
        });
    }

    #[test]
    fn settings_with_bad_length_is_rejected() {
        // Declare a 5-octet payload — not a multiple of 6.
        let buf = [
            0x00,
            0x00,
            0x05,
            FrameType::Settings as u8,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x01,
            0x00,
            0x00,
            0x00,
        ];
        let err = Frame::parse(&buf, DEFAULT_MAX_FRAME_SIZE).unwrap_err();
        assert!(matches!(err, Error::Protocol(m) if m.contains("FRAME_SIZE_ERROR")));
    }

    #[test]
    fn settings_ack_with_nonempty_payload_is_rejected() {
        let buf = [
            0x00,
            0x00,
            0x06,
            FrameType::Settings as u8,
            flags::ACK,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x01,
            0x00,
            0x00,
            0x00,
            0x01,
        ];
        let err = Frame::parse(&buf, DEFAULT_MAX_FRAME_SIZE).unwrap_err();
        assert!(matches!(err, Error::Protocol(m) if m.contains("FRAME_SIZE_ERROR")));
    }

    #[test]
    fn ping_frame_round_trips() {
        round_trip(Frame::Ping {
            ack: false,
            payload: [1, 2, 3, 4, 5, 6, 7, 8],
        });
        round_trip(Frame::Ping {
            ack: true,
            payload: [0; 8],
        });
    }

    #[test]
    fn ping_with_wrong_payload_length_is_rejected() {
        // PING must carry exactly 8 octets; declare 7.
        let buf = [
            0x00,
            0x00,
            0x07,
            FrameType::Ping as u8,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        let err = Frame::parse(&buf, DEFAULT_MAX_FRAME_SIZE).unwrap_err();
        assert!(matches!(err, Error::Protocol(m) if m.contains("FRAME_SIZE_ERROR")));
    }

    #[test]
    fn goaway_round_trips_with_and_without_debug() {
        round_trip(Frame::GoAway {
            last_stream_id: 42,
            error_code: error_codes::NO_ERROR,
            debug: Bytes::new(),
        });
        round_trip(Frame::GoAway {
            last_stream_id: 99,
            error_code: error_codes::PROTOCOL_ERROR,
            debug: Bytes::from_static(b"something went wrong"),
        });
    }

    #[test]
    fn goaway_too_short_is_rejected() {
        // GOAWAY needs at least 8 octets; declare 4.
        let buf = [
            0x00,
            0x00,
            0x04,
            FrameType::GoAway as u8,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
        ];
        let err = Frame::parse(&buf, DEFAULT_MAX_FRAME_SIZE).unwrap_err();
        assert!(matches!(err, Error::Protocol(m) if m.contains("FRAME_SIZE_ERROR")));
    }

    #[test]
    fn window_update_round_trips() {
        round_trip(Frame::WindowUpdate {
            stream_id: 0,
            increment: 65_535,
        });
        round_trip(Frame::WindowUpdate {
            stream_id: 5,
            increment: 1,
        });
    }

    #[test]
    fn window_update_zero_increment_is_rejected() {
        let buf = [
            0x00,
            0x00,
            0x04,
            FrameType::WindowUpdate as u8,
            0x00,
            0x00,
            0x00,
            0x00,
            0x01,
            0x00,
            0x00,
            0x00,
            0x00,
        ];
        let err = Frame::parse(&buf, DEFAULT_MAX_FRAME_SIZE).unwrap_err();
        assert!(matches!(err, Error::Protocol(m) if m.contains("PROTOCOL_ERROR")));
    }

    #[test]
    fn window_update_with_wrong_length_is_rejected() {
        let buf = [
            0x00,
            0x00,
            0x03,
            FrameType::WindowUpdate as u8,
            0x00,
            0x00,
            0x00,
            0x00,
            0x01,
            0x00,
            0x00,
            0x01,
        ];
        let err = Frame::parse(&buf, DEFAULT_MAX_FRAME_SIZE).unwrap_err();
        assert!(matches!(err, Error::Protocol(m) if m.contains("FRAME_SIZE_ERROR")));
    }

    #[test]
    fn continuation_round_trips() {
        round_trip(Frame::Continuation {
            stream_id: 5,
            block: Bytes::from_static(b"more-header-bytes"),
            end_headers: true,
        });
        round_trip(Frame::Continuation {
            stream_id: 5,
            block: Bytes::new(),
            end_headers: false,
        });
    }

    #[test]
    fn unknown_frame_type_is_skipped() {
        // An extension frame (type 0xEF) followed by a real PING frame. The
        // parser must transparently skip the unknown frame and return the PING.
        let unknown = FrameHeader {
            length: 3,
            frame_type: 0xEF,
            flags: 0,
            stream_id: 1,
        };
        let mut buf = unknown.encode().to_vec();
        buf.extend_from_slice(b"xyz");
        let ping = Frame::Ping {
            ack: false,
            payload: [9; 8],
        };
        let ping_bytes = ping.encode();
        buf.extend_from_slice(&ping_bytes);

        let (frame, consumed) = Frame::parse(&buf, DEFAULT_MAX_FRAME_SIZE).unwrap().unwrap();
        assert_eq!(frame, ping);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn unknown_frame_type_alone_returns_none_for_next() {
        // Only an unknown frame in the buffer: it is skipped, and since no more
        // bytes follow, the parser reports `None` (needs more data).
        let unknown = FrameHeader {
            length: 2,
            frame_type: 0xAB,
            flags: 0,
            stream_id: 0,
        };
        let mut buf = unknown.encode().to_vec();
        buf.extend_from_slice(b"hi");
        assert!(Frame::parse(&buf, DEFAULT_MAX_FRAME_SIZE)
            .unwrap()
            .is_none());
    }

    #[test]
    fn parse_consumes_only_one_frame_and_reports_offset() {
        // Two PING frames back to back; parse should return the first and an
        // offset pointing at the start of the second.
        let first = Frame::Ping {
            ack: false,
            payload: [1; 8],
        };
        let second = Frame::Ping {
            ack: true,
            payload: [2; 8],
        };
        let mut buf = first.encode();
        let first_len = buf.len();
        buf.extend_from_slice(&second.encode());

        let (frame, consumed) = Frame::parse(&buf, DEFAULT_MAX_FRAME_SIZE).unwrap().unwrap();
        assert_eq!(frame, first);
        assert_eq!(consumed, first_len);

        let (frame2, consumed2) = Frame::parse(&buf[consumed..], DEFAULT_MAX_FRAME_SIZE)
            .unwrap()
            .unwrap();
        assert_eq!(frame2, second);
        assert_eq!(consumed2, buf.len() - first_len);
    }

    #[test]
    fn stream_id_accessor_reports_connection_frames_as_zero() {
        assert_eq!(
            Frame::Settings {
                ack: true,
                params: vec![]
            }
            .stream_id(),
            0
        );
        assert_eq!(
            Frame::Ping {
                ack: false,
                payload: [0; 8]
            }
            .stream_id(),
            0
        );
        assert_eq!(
            Frame::GoAway {
                last_stream_id: 1,
                error_code: 0,
                debug: Bytes::new()
            }
            .stream_id(),
            0
        );
        assert_eq!(
            Frame::Data {
                stream_id: 13,
                data: Bytes::new(),
                end_stream: false
            }
            .stream_id(),
            13
        );
    }

    #[test]
    fn frame_type_from_u8_round_trips_known_types() {
        for v in 0u8..=9 {
            let ft = FrameType::from_u8(v).expect("0..=9 are all known");
            assert_eq!(ft as u8, v);
        }
        assert!(FrameType::from_u8(10).is_none());
        assert!(FrameType::from_u8(0xFF).is_none());
    }

    #[test]
    fn push_promise_is_rejected_as_protocol_error() {
        let header = FrameHeader {
            length: 4,
            frame_type: FrameType::PushPromise as u8,
            flags: 0,
            stream_id: 1,
        };
        let mut buf = header.encode().to_vec();
        buf.extend_from_slice(&[0, 0, 0, 2]);
        let err = Frame::parse(&buf, DEFAULT_MAX_FRAME_SIZE).unwrap_err();
        assert!(matches!(err, Error::Protocol(m) if m.contains("PROTOCOL_ERROR")));
    }
}
