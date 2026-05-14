//! RFC 6455 §5 — the WebSocket framing protocol.
//!
//! Every WebSocket message is carried as one or more *frames*. This module
//! implements a fully working codec for the server side of a connection:
//!
//! * [`Frame::parse`] decodes a single frame from a byte buffer, transparently
//!   unmasking client payloads (clients MUST mask, RFC 6455 §5.1).
//! * [`Frame::encode`] serializes a frame for transmission; server frames are
//!   never masked.
//!
//! The parser is incremental: when the buffer does not yet hold a complete
//! frame it returns `Ok(None)` so the caller can read more bytes and retry.

use bytes::{BufMut, Bytes, BytesMut};
use tomcatrs_core::{Error, Result};

/// Hard ceiling on a single frame's payload length.
///
/// RFC 6455 permits payloads up to 2^63-1 bytes, but accepting that unbounded
/// is a denial-of-service hazard. 64 MiB is comfortably larger than any sane
/// control or application frame; larger messages should be fragmented.
pub const MAX_PAYLOAD_LEN: usize = 64 * 1024 * 1024;

/// The frame opcode (RFC 6455 §5.2), identifying the frame's interpretation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Opcode {
    /// `0x0` — a continuation of a fragmented message.
    Continuation = 0x0,
    /// `0x1` — a UTF-8 text message (fragment).
    Text = 0x1,
    /// `0x2` — a binary message (fragment).
    Binary = 0x2,
    /// `0x8` — a connection close control frame.
    Close = 0x8,
    /// `0x9` — a ping control frame.
    Ping = 0x9,
    /// `0xA` — a pong control frame.
    Pong = 0xA,
}

impl Opcode {
    /// Decode a 4-bit opcode nibble, rejecting the reserved values.
    pub fn from_u8(value: u8) -> Result<Self> {
        match value {
            0x0 => Ok(Opcode::Continuation),
            0x1 => Ok(Opcode::Text),
            0x2 => Ok(Opcode::Binary),
            0x8 => Ok(Opcode::Close),
            0x9 => Ok(Opcode::Ping),
            0xA => Ok(Opcode::Pong),
            other => Err(Error::protocol(format!(
                "websocket frame: reserved or invalid opcode 0x{other:X}"
            ))),
        }
    }

    /// `true` for the three control opcodes (Close, Ping, Pong).
    ///
    /// Control frames have tight constraints (RFC 6455 §5.5): they MUST NOT be
    /// fragmented and their payload MUST NOT exceed 125 bytes.
    pub fn is_control(self) -> bool {
        matches!(self, Opcode::Close | Opcode::Ping | Opcode::Pong)
    }
}

/// A single decoded WebSocket frame.
///
/// Payloads are always stored *unmasked* regardless of direction, so the rest
/// of the runtime never has to think about masking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The FIN bit: `true` if this frame is the final fragment of a message.
    pub fin: bool,
    /// The frame's opcode.
    pub opcode: Opcode,
    /// The (already unmasked) application payload.
    pub payload: Bytes,
}

impl Frame {
    /// Create a final (unfragmented) text frame.
    pub fn text(payload: impl Into<Bytes>) -> Self {
        Frame {
            fin: true,
            opcode: Opcode::Text,
            payload: payload.into(),
        }
    }

    /// Create a final (unfragmented) binary frame.
    pub fn binary(payload: impl Into<Bytes>) -> Self {
        Frame {
            fin: true,
            opcode: Opcode::Binary,
            payload: payload.into(),
        }
    }

    /// Try to decode one frame from the front of `buf`.
    ///
    /// Returns:
    ///
    /// * `Ok(Some((frame, consumed)))` — a frame was decoded; `consumed` bytes
    ///   should be drained from `buf`.
    /// * `Ok(None)` — `buf` does not yet contain a complete frame; read more.
    /// * `Err(_)` — the bytes are not a valid frame (oversized, bad opcode,
    ///   unmasked client frame, …).
    ///
    /// Per RFC 6455 §5.1, frames from the client MUST be masked; an unmasked
    /// frame is rejected with [`Error::Protocol`].
    pub fn parse(buf: &[u8]) -> Result<Option<(Frame, usize)>> {
        // Need at least the 2-byte fixed header.
        if buf.len() < 2 {
            return Ok(None);
        }

        let b0 = buf[0];
        let b1 = buf[1];

        let fin = b0 & 0x80 != 0;
        let rsv = b0 & 0x70;
        if rsv != 0 {
            return Err(Error::protocol(
                "websocket frame: reserved RSV bits must be zero (no extensions negotiated)",
            ));
        }
        let opcode = Opcode::from_u8(b0 & 0x0F)?;

        let masked = b1 & 0x80 != 0;
        let len7 = (b1 & 0x7F) as usize;

        // Decode the extended payload length.
        let mut offset = 2;
        let payload_len: usize = match len7 {
            126 => {
                if buf.len() < offset + 2 {
                    return Ok(None);
                }
                let len = u16::from_be_bytes([buf[offset], buf[offset + 1]]) as usize;
                offset += 2;
                len
            }
            127 => {
                if buf.len() < offset + 8 {
                    return Ok(None);
                }
                let mut raw = [0u8; 8];
                raw.copy_from_slice(&buf[offset..offset + 8]);
                let len = u64::from_be_bytes(raw);
                offset += 8;
                // On a 32-bit target `usize` may be narrower than u64.
                if len > usize::MAX as u64 {
                    return Err(Error::protocol(
                        "websocket frame: payload length exceeds addressable memory",
                    ));
                }
                len as usize
            }
            other => other,
        };

        if payload_len > MAX_PAYLOAD_LEN {
            return Err(Error::protocol(format!(
                "websocket frame: payload length {payload_len} exceeds limit {MAX_PAYLOAD_LEN}"
            )));
        }

        if opcode.is_control() {
            if !fin {
                return Err(Error::protocol(
                    "websocket frame: control frames must not be fragmented",
                ));
            }
            if payload_len > 125 {
                return Err(Error::protocol(
                    "websocket frame: control frame payload must not exceed 125 bytes",
                ));
            }
        }

        // Client frames MUST be masked.
        if !masked {
            return Err(Error::protocol(
                "websocket frame: client frames must be masked (RFC 6455 §5.1)",
            ));
        }

        // The 4-byte masking key follows the length.
        if buf.len() < offset + 4 {
            return Ok(None);
        }
        let mask_key = [
            buf[offset],
            buf[offset + 1],
            buf[offset + 2],
            buf[offset + 3],
        ];
        offset += 4;

        // Finally the payload itself.
        if buf.len() < offset + payload_len {
            return Ok(None);
        }
        let mut payload = BytesMut::with_capacity(payload_len);
        for (i, &byte) in buf[offset..offset + payload_len].iter().enumerate() {
            payload.put_u8(byte ^ mask_key[i & 3]);
        }
        offset += payload_len;

        Ok(Some((
            Frame {
                fin,
                opcode,
                payload: payload.freeze(),
            },
            offset,
        )))
    }

    /// Serialize this frame for transmission to the client.
    ///
    /// Server-to-client frames are never masked (RFC 6455 §5.1), so the MASK
    /// bit is clear and no masking key is emitted.
    pub fn encode(&self) -> Bytes {
        let len = self.payload.len();
        let mut out = BytesMut::with_capacity(len + 10);

        // Byte 0: FIN + RSV(0) + opcode.
        let b0 = if self.fin { 0x80 } else { 0x00 } | (self.opcode as u8);
        out.put_u8(b0);

        // Byte 1+: MASK bit clear, plus the (possibly extended) length.
        if len < 126 {
            out.put_u8(len as u8);
        } else if len <= u16::MAX as usize {
            out.put_u8(126);
            out.put_u16(len as u16);
        } else {
            out.put_u8(127);
            out.put_u64(len as u64);
        }

        out.extend_from_slice(&self.payload);
        out.freeze()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mask `payload` with `key`, producing a raw client frame body.
    fn mask(payload: &[u8], key: [u8; 4]) -> Vec<u8> {
        payload
            .iter()
            .enumerate()
            .map(|(i, &b)| b ^ key[i & 3])
            .collect()
    }

    #[test]
    fn parse_masked_client_text_frame() {
        let key = [0x37, 0xfa, 0x21, 0x3d];
        let body = b"Hello";
        let masked = mask(body, key);

        let mut buf = vec![0x81, 0x80 | body.len() as u8];
        buf.extend_from_slice(&key);
        buf.extend_from_slice(&masked);

        let (frame, consumed) = Frame::parse(&buf).unwrap().expect("complete frame");
        assert_eq!(consumed, buf.len());
        assert!(frame.fin);
        assert_eq!(frame.opcode, Opcode::Text);
        assert_eq!(&frame.payload[..], b"Hello");
    }

    #[test]
    fn encode_server_text_frame_is_unmasked() {
        let frame = Frame::text(Bytes::from_static(b"Hello"));
        let bytes = frame.encode();
        assert_eq!(bytes[0], 0x81); // FIN + text
        assert_eq!(bytes[1], 0x05); // no MASK bit, len 5
        assert_eq!(&bytes[2..], b"Hello");
    }

    #[test]
    fn text_frame_round_trips_through_a_masked_client_frame() {
        // Encode a server frame, re-mask it as if a client sent it, parse back.
        let original = Frame::text(Bytes::from_static(b"round trip payload"));
        let key = [0x01, 0x02, 0x03, 0x04];
        let masked = mask(&original.payload, key);

        let mut buf = vec![0x81, 0x80 | original.payload.len() as u8];
        buf.extend_from_slice(&key);
        buf.extend_from_slice(&masked);

        let (parsed, _) = Frame::parse(&buf).unwrap().unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn partial_buffer_returns_none() {
        // Header claims 5 bytes but only 2 payload bytes are present.
        let key = [0x37, 0xfa, 0x21, 0x3d];
        let mut buf = vec![0x81, 0x85];
        buf.extend_from_slice(&key);
        buf.extend_from_slice(&[0xAA, 0xBB]); // only 2 of 5 payload bytes
        assert!(Frame::parse(&buf).unwrap().is_none());

        // An empty buffer and a 1-byte buffer also need more data.
        assert!(Frame::parse(&[]).unwrap().is_none());
        assert!(Frame::parse(&[0x81]).unwrap().is_none());
    }

    #[test]
    fn unmasked_client_frame_is_rejected() {
        // FIN+text, len 0, MASK bit clear — illegal from a client.
        let buf = [0x81, 0x00];
        assert!(Frame::parse(&buf).is_err());
    }

    #[test]
    fn extended_16bit_length_round_trip() {
        let payload = vec![0x5A_u8; 300];
        let key = [0x11, 0x22, 0x33, 0x44];
        let masked = mask(&payload, key);

        let mut buf = vec![0x82, 0x80 | 126];
        buf.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        buf.extend_from_slice(&key);
        buf.extend_from_slice(&masked);

        let (frame, consumed) = Frame::parse(&buf).unwrap().unwrap();
        assert_eq!(consumed, buf.len());
        assert_eq!(frame.opcode, Opcode::Binary);
        assert_eq!(frame.payload.len(), 300);

        // And the server-side encoding uses the 126 extended form too.
        let encoded = Frame::binary(frame.payload.clone()).encode();
        assert_eq!(encoded[1], 126);
    }

    #[test]
    fn oversized_payload_is_rejected() {
        // 127 extended length with a value past MAX_PAYLOAD_LEN.
        let mut buf = vec![0x82, 0x80 | 127];
        buf.extend_from_slice(&((MAX_PAYLOAD_LEN as u64) + 1).to_be_bytes());
        buf.extend_from_slice(&[0, 0, 0, 0]); // mask key
        assert!(Frame::parse(&buf).is_err());
    }

    #[test]
    fn bad_opcode_is_rejected() {
        let buf = [0x83, 0x80, 0, 0, 0, 0]; // opcode 0x3 is reserved
        assert!(Frame::parse(&buf).is_err());
    }
}
