//! RFC 6455 §5–§7 — the WebSocket *transport* layer.
//!
//! [`frame`](crate::frame) turns bytes into individual [`Frame`]s; this module
//! builds the rest of a working connection on top of that codec:
//!
//! * **Message reassembly** — fragmented data messages (an initial Text/Binary
//!   frame followed by `Continuation` frames) are stitched back into a single
//!   [`Message`], with control frames transparently handled even when they are
//!   interleaved within a fragmented data stream (RFC 6455 §5.4).
//! * **Control-frame semantics** — incoming `Ping` frames are answered with a
//!   `Pong` automatically; the most recent `Pong` payload is tracked for
//!   liveness checks.
//! * **The close handshake** — [`WebSocketConnection::close`] sends a `Close`
//!   frame, waits (bounded by [`WebSocketConfig::close_timeout`]) for the
//!   peer's `Close`, and only then marks the connection [`WebSocketState::Closed`]
//!   (RFC 6455 §7).
//! * **A state machine** — writes are rejected once the connection has begun
//!   closing, so application code cannot smuggle data past a `Close`.
//! * **Extension negotiation** — [`negotiate_permessage_deflate`] parses a
//!   client's `Sec-WebSocket-Extensions` offer and decides, per the configured
//!   [`CompressionPolicy`], whether to accept `permessage-deflate`.
//!
//! # Backpressure
//!
//! [`WebSocketConnection::write_message`] performs no buffering of its own: it
//! encodes each frame and immediately `write_all` + `flush`es it to the
//! underlying [`AsyncWrite`]. That means a slow or stalled peer naturally
//! propagates backpressure — the `write_message` future simply does not resolve
//! until the socket has accepted the bytes. There is no unbounded queue that
//! could grow without limit; the cost of a slow consumer is paid as latency on
//! the producing task, not as memory.
//!
//! # Server role
//!
//! This is a *server-side* transport. Per RFC 6455 §5.1 the server sends
//! unmasked frames and expects masked frames from the client; both directions
//! are handled here (the [`frame`](crate::frame) codec unmasks on parse, and
//! [`WebSocketConnection::write_message`] emits unmasked frames).

use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;
use tomcatrs_core::{Error, Result};

use crate::frame::{Frame, Opcode};

/// RFC 6455 §7.4.1 — defined close status codes.
///
/// These are the subset of registered codes the transport produces or
/// recognises directly. Applications may of course send other registered or
/// private-use codes through [`CloseFrame`].
pub mod close_code {
    /// `1000` — normal closure; the purpose for which the connection was
    /// established has been fulfilled.
    pub const NORMAL: u16 = 1000;
    /// `1001` — an endpoint is "going away" (server shutdown, browser navigated
    /// away from the page).
    pub const GOING_AWAY: u16 = 1001;
    /// `1002` — the connection is being terminated due to a protocol error.
    pub const PROTOCOL_ERROR: u16 = 1002;
    /// `1003` — the endpoint received a data type it cannot accept (e.g. a
    /// binary-only endpoint received text).
    pub const UNSUPPORTED: u16 = 1003;
    /// `1007` — a message was received containing data inconsistent with its
    /// type (e.g. non-UTF-8 inside a text message).
    pub const INVALID_PAYLOAD: u16 = 1007;
    /// `1008` — a message was received that violates the endpoint's policy.
    pub const POLICY_VIOLATION: u16 = 1008;
    /// `1009` — a message was received that is too big to process.
    pub const TOO_BIG: u16 = 1009;
    /// `1011` — the server encountered an unexpected condition that prevented it
    /// from fulfilling the request.
    pub const INTERNAL_ERROR: u16 = 1011;
}

/// Tunable limits and timing for a [`WebSocketConnection`].
///
/// [`WebSocketConfig::default`] provides values appropriate for a general
/// purpose server: 64 MiB whole-message ceiling, 1 MiB outbound fragment size,
/// no automatic keep-alive ping, a 10-second close-handshake timeout, and
/// strict masking enforcement.
#[derive(Debug, Clone)]
pub struct WebSocketConfig {
    /// The largest fully-reassembled message [`WebSocketConnection::read_message`]
    /// will accept. A message whose accumulated fragments exceed this is
    /// rejected with a `1009` (too big) protocol error.
    pub max_message_size: usize,
    /// The largest *outbound* frame [`WebSocketConnection::write_message`] will
    /// emit. Messages larger than this are split across multiple frames using
    /// the continuation mechanism. It also bounds the size of any single
    /// inbound frame's payload.
    pub max_frame_size: usize,
    /// If set, the interval at which a keep-alive `Ping` *should* be sent. The
    /// transport itself does not own a timer task — this value is advisory
    /// configuration that a connection-driving loop can consult — but it is
    /// carried here so the policy lives in one place.
    pub ping_interval: Option<Duration>,
    /// How long [`WebSocketConnection::close`] waits for the peer's responding
    /// `Close` frame before giving up and marking the connection closed anyway.
    pub close_timeout: Duration,
    /// If `true`, inbound frames that are *not* masked are tolerated rather than
    /// rejected. RFC 6455 §5.1 requires client frames to be masked, so this
    /// should stay `false` for spec-compliant peers; it exists only for interop
    /// with non-conformant clients and test harnesses.
    pub accept_unmasked_frames: bool,
}

impl Default for WebSocketConfig {
    fn default() -> Self {
        Self {
            max_message_size: 64 * 1024 * 1024,
            max_frame_size: 1024 * 1024,
            ping_interval: None,
            close_timeout: Duration::from_secs(10),
            accept_unmasked_frames: false,
        }
    }
}

/// The lifecycle state of a [`WebSocketConnection`] (RFC 6455 §4 / §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WebSocketState {
    /// The HTTP upgrade handshake has not yet completed.
    Connecting,
    /// The connection is open; data may flow in both directions.
    Open,
    /// A `Close` frame has been sent or received; the close handshake is in
    /// progress and no new application data may be written.
    Closing,
    /// The connection is fully closed; no further I/O is possible.
    Closed,
}

/// An RFC 6455 §5.5.1 close frame body: a status code plus an optional reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseFrame {
    /// The numeric status code; see [`close_code`] for the well-known values.
    pub code: u16,
    /// A human-readable, UTF-8 reason. May be empty.
    pub reason: String,
}

impl CloseFrame {
    /// Construct a close frame from a code and reason.
    pub fn new(code: u16, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
        }
    }

    /// Encode this close frame as a `Close` control-frame payload: a 2-byte
    /// big-endian status code followed by the UTF-8 reason bytes.
    fn to_payload(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(2 + self.reason.len());
        buf.extend_from_slice(&self.code.to_be_bytes());
        buf.extend_from_slice(self.reason.as_bytes());
        buf.freeze()
    }

    /// Decode a `Close` control-frame payload.
    ///
    /// An empty payload is legal (RFC 6455 §5.5.1) and yields `Ok(None)`. A
    /// 1-byte payload is malformed. The reason, if present, must be valid
    /// UTF-8.
    fn from_payload(payload: &[u8]) -> Result<Option<CloseFrame>> {
        match payload.len() {
            0 => Ok(None),
            1 => Err(Error::protocol(
                "websocket close frame: 1-byte payload is malformed (need 0 or >=2)",
            )),
            _ => {
                let code = u16::from_be_bytes([payload[0], payload[1]]);
                let reason = std::str::from_utf8(&payload[2..])
                    .map_err(|_| {
                        Error::protocol("websocket close frame: reason is not valid UTF-8")
                    })?
                    .to_owned();
                Ok(Some(CloseFrame { code, reason }))
            }
        }
    }
}

/// A whole, application-level WebSocket message.
///
/// `read_message` produces these *after* reassembling fragments; `write_message`
/// consumes them, fragmenting large [`Message::Text`]/[`Message::Binary`]
/// payloads as needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// A UTF-8 text message. The string is guaranteed valid UTF-8 (validated on
    /// receipt, trivially true on send).
    Text(String),
    /// A binary message.
    Binary(Bytes),
    /// A `Ping` control message. The payload is at most 125 bytes.
    Ping(Bytes),
    /// A `Pong` control message. The payload is at most 125 bytes.
    Pong(Bytes),
    /// A `Close` control message, optionally carrying a [`CloseFrame`].
    Close(Option<CloseFrame>),
}

impl Message {
    /// `true` for the three control message kinds (Ping, Pong, Close).
    pub fn is_control(&self) -> bool {
        matches!(
            self,
            Message::Ping(_) | Message::Pong(_) | Message::Close(_)
        )
    }
}

/// A live, framed WebSocket connection over an arbitrary byte stream `S`.
///
/// `S` is any Tokio [`AsyncRead`] + [`AsyncWrite`] — a TCP socket, a TLS
/// stream, or a [`tokio::io::duplex`] pipe in tests. The connection owns a small
/// read buffer for incremental frame parsing and otherwise holds no unbounded
/// state.
///
/// Construct one with [`WebSocketConnection::new`] *after* the HTTP upgrade
/// handshake (see [`crate::handshake`]) has been written to the stream.
#[derive(Debug)]
pub struct WebSocketConnection<S> {
    stream: S,
    config: WebSocketConfig,
    state: WebSocketState,
    /// Bytes read from the socket but not yet consumed by the frame parser.
    read_buf: BytesMut,
    /// Accumulator for a partially-received fragmented data message.
    fragment: Option<FragmentBuffer>,
    /// Payload of the most recently received `Pong`, for liveness tracking.
    last_pong: Option<Bytes>,
    /// `true` once we have sent a `Close` frame.
    close_sent: bool,
    /// `true` once we have received a `Close` frame.
    close_received: bool,
}

/// In-progress reassembly of a fragmented data message.
#[derive(Debug)]
struct FragmentBuffer {
    /// `true` if the message started as Text, `false` if Binary. Continuation
    /// frames carry opcode `0x0` and inherit this.
    is_text: bool,
    /// The concatenated payload bytes received so far.
    data: BytesMut,
}

impl<S> WebSocketConnection<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Wrap an already-upgraded byte stream as an open WebSocket connection.
    ///
    /// The HTTP `101 Switching Protocols` response must already have been sent;
    /// this constructor begins in [`WebSocketState::Open`].
    pub fn new(stream: S, config: WebSocketConfig) -> Self {
        Self {
            stream,
            config,
            state: WebSocketState::Open,
            read_buf: BytesMut::with_capacity(8 * 1024),
            fragment: None,
            last_pong: None,
            close_sent: false,
            close_received: false,
        }
    }

    /// The current lifecycle [`WebSocketState`].
    pub fn state(&self) -> WebSocketState {
        self.state
    }

    /// The active [`WebSocketConfig`].
    pub fn config(&self) -> &WebSocketConfig {
        &self.config
    }

    /// The payload of the most recently received `Pong`, if any.
    ///
    /// A connection-health checker can send a `Ping` with a known payload via
    /// [`WebSocketConnection::send_ping`] and later compare it against this to
    /// confirm the peer is still responsive.
    pub fn last_pong(&self) -> Option<&Bytes> {
        self.last_pong.as_ref()
    }

    /// Read and return the next *complete* application message.
    ///
    /// This method:
    ///
    /// * reads as many frames from the socket as needed;
    /// * reassembles fragmented Text/Binary messages across `Continuation`
    ///   frames (RFC 6455 §5.4);
    /// * transparently handles control frames — including ones interleaved
    ///   *within* a fragmented data message — auto-replying to `Ping` with
    ///   `Pong` and recording `Pong` payloads for liveness;
    /// * validates that a received Text message (or text fragment sequence) is
    ///   valid UTF-8, failing with a `1007` protocol error otherwise;
    /// * enforces [`WebSocketConfig::max_frame_size`] per frame and
    ///   [`WebSocketConfig::max_message_size`] for the reassembled whole.
    ///
    /// Returns `Ok(None)` on a clean end-of-stream (the peer closed the
    /// underlying transport with no more frames pending). A received `Close`
    /// frame is surfaced as `Ok(Some(Message::Close(..)))` so the caller can run
    /// the responding half of the close handshake.
    pub async fn read_message(&mut self) -> Result<Option<Message>> {
        if self.state == WebSocketState::Closed {
            return Ok(None);
        }

        loop {
            // Try to extract a frame from whatever is already buffered.
            match self.try_parse_frame()? {
                Some(frame) => {
                    if let Some(msg) = self.handle_frame(frame).await? {
                        return Ok(Some(msg));
                    }
                    // Frame was consumed internally (e.g. a Ping we answered,
                    // or a non-final fragment); keep going.
                }
                None => {
                    // Need more bytes from the socket.
                    if !self.fill_read_buf().await? {
                        // Clean EOF. If we were mid-fragment that is a protocol
                        // violation, but most callers just want `None`.
                        if self.fragment.is_some() {
                            return Err(Error::protocol(
                                "websocket: stream ended in the middle of a fragmented message",
                            ));
                        }
                        self.state = WebSocketState::Closed;
                        return Ok(None);
                    }
                }
            }
        }
    }

    /// Send a complete application message.
    ///
    /// [`Message::Text`] and [`Message::Binary`] payloads larger than
    /// [`WebSocketConfig::max_frame_size`] are automatically fragmented: the
    /// first frame carries the real opcode with `FIN=0`, subsequent frames use
    /// `Continuation`, and the last carries `FIN=1`. Control messages
    /// ([`Message::Ping`]/[`Message::Pong`]/[`Message::Close`]) are never
    /// fragmented and their payloads must be ≤125 bytes.
    ///
    /// All frames are written unmasked (server role) and flushed before the
    /// returned future resolves — see the [module docs](self#backpressure) on
    /// backpressure.
    ///
    /// Writing after the connection has entered [`WebSocketState::Closing`] or
    /// [`WebSocketState::Closed`] is rejected with a protocol error; sending a
    /// [`Message::Close`] is the one exception that *drives* that transition and
    /// is handled by [`WebSocketConnection::close`].
    pub async fn write_message(&mut self, msg: Message) -> Result<()> {
        // Sending Close is allowed while Open; everything else requires Open.
        match self.state {
            WebSocketState::Open => {}
            WebSocketState::Connecting => {
                return Err(Error::protocol(
                    "websocket: cannot write before the connection is open",
                ));
            }
            WebSocketState::Closing | WebSocketState::Closed => {
                return Err(Error::protocol(
                    "websocket: cannot write a message after the connection has started closing",
                ));
            }
        }

        match msg {
            Message::Text(text) => {
                self.write_data_frames(Opcode::Text, Bytes::from(text.into_bytes()))
                    .await
            }
            Message::Binary(data) => self.write_data_frames(Opcode::Binary, data).await,
            Message::Ping(payload) => self.write_control_frame(Opcode::Ping, payload).await,
            Message::Pong(payload) => self.write_control_frame(Opcode::Pong, payload).await,
            Message::Close(close) => self.send_close_frame(close).await,
        }
    }

    /// Send a `Ping` control frame with the given payload (≤125 bytes).
    ///
    /// The peer is expected to answer with a `Pong` echoing the payload;
    /// [`WebSocketConnection::read_message`] records that echo, retrievable via
    /// [`WebSocketConnection::last_pong`].
    pub async fn send_ping(&mut self, payload: impl Into<Bytes>) -> Result<()> {
        self.write_message(Message::Ping(payload.into())).await
    }

    /// Send a `Pong` control frame with the given payload (≤125 bytes).
    ///
    /// Unsolicited pongs are permitted by RFC 6455 §5.5.3 and may be used as a
    /// unidirectional keep-alive.
    pub async fn send_pong(&mut self, payload: impl Into<Bytes>) -> Result<()> {
        self.write_message(Message::Pong(payload.into())).await
    }

    /// Run the closing handshake (RFC 6455 §7).
    ///
    /// This sends a `Close` frame carrying `code`/`reason`, transitions to
    /// [`WebSocketState::Closing`], then waits — bounded by
    /// [`WebSocketConfig::close_timeout`] — for the peer's responding `Close`.
    /// Whether or not that response arrives in time, the connection ends in
    /// [`WebSocketState::Closed`].
    ///
    /// If a `Close` frame was *already* received from the peer before this call
    /// (the peer initiated the handshake), this simply sends the acknowledging
    /// `Close` and returns without waiting.
    pub async fn close(&mut self, code: u16, reason: impl Into<String>) -> Result<()> {
        if self.state == WebSocketState::Closed {
            return Ok(());
        }

        let peer_already_closed = self.close_received;

        if !self.close_sent {
            self.send_close_frame(Some(CloseFrame::new(code, reason)))
                .await?;
        }

        if peer_already_closed {
            // Peer started it; our Close above is the acknowledgement. Done.
            self.state = WebSocketState::Closed;
            return Ok(());
        }

        // We initiated: wait for the peer's responding Close, but don't hang.
        let deadline = self.config.close_timeout;
        let _ = timeout(deadline, async {
            while !self.close_received {
                match self.try_parse_frame() {
                    Ok(Some(frame)) => {
                        // Drain frames until we see the peer's Close. Data
                        // frames arriving during close are discarded per
                        // RFC 6455 §7.1.6.
                        if frame.opcode == Opcode::Close {
                            self.close_received = true;
                        }
                    }
                    Ok(None) => {
                        if !self.fill_read_buf().await.unwrap_or(false) {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .await;

        self.state = WebSocketState::Closed;
        let _ = self.stream.shutdown().await;
        Ok(())
    }

    /// Consume the connection and hand back the underlying stream.
    pub fn into_inner(self) -> S {
        self.stream
    }

    // --- internals -------------------------------------------------------

    /// Attempt to parse one frame out of `read_buf`, draining it on success.
    /// Also enforces the per-frame size limit and the masking policy.
    fn try_parse_frame(&mut self) -> Result<Option<Frame>> {
        match Frame::parse(&self.read_buf) {
            Ok(Some((frame, consumed))) => {
                let _ = self.read_buf.split_to(consumed);
                if !frame.opcode.is_control() && frame.payload.len() > self.config.max_frame_size {
                    return Err(Error::protocol(format!(
                        "websocket: frame payload {} exceeds max_frame_size {}",
                        frame.payload.len(),
                        self.config.max_frame_size
                    )));
                }
                Ok(Some(frame))
            }
            Ok(None) => Ok(None),
            Err(e) => {
                // The codec rejects unmasked client frames; honour the
                // `accept_unmasked_frames` escape hatch by retrying a lenient
                // parse. For any other error, propagate.
                if self.config.accept_unmasked_frames {
                    if let Some((frame, consumed)) = parse_lenient(&self.read_buf)? {
                        let _ = self.read_buf.split_to(consumed);
                        return Ok(Some(frame));
                    }
                    // Lenient parse needs more bytes.
                    return Ok(None);
                }
                Err(e)
            }
        }
    }

    /// Read more bytes from the socket into `read_buf`.
    ///
    /// Returns `Ok(true)` if bytes were read, `Ok(false)` on clean EOF.
    async fn fill_read_buf(&mut self) -> Result<bool> {
        let n = self.stream.read_buf(&mut self.read_buf).await?;
        Ok(n != 0)
    }

    /// Process a freshly parsed frame.
    ///
    /// Returns `Ok(Some(message))` when the frame completes a deliverable
    /// message, or `Ok(None)` when the frame was handled internally (an
    /// auto-answered `Ping`, a recorded `Pong`, or a non-final data fragment).
    async fn handle_frame(&mut self, frame: Frame) -> Result<Option<Message>> {
        // Control frames may legally appear between fragments of a data
        // message, so they are handled first and independently of `fragment`.
        if frame.opcode.is_control() {
            // The codec already enforces "not fragmented" and "<=125 bytes" for
            // control frames, but assert the invariants defensively.
            debug_assert!(frame.fin);
            debug_assert!(frame.payload.len() <= 125);
            return self.handle_control_frame(frame).await;
        }

        match frame.opcode {
            Opcode::Text | Opcode::Binary => {
                if self.fragment.is_some() {
                    return Err(Error::protocol(
                        "websocket: received a new data frame while a fragmented \
                         message was still in progress",
                    ));
                }
                let is_text = frame.opcode == Opcode::Text;
                if frame.fin {
                    // A complete, unfragmented data message.
                    self.finish_data_message(is_text, frame.payload)
                } else {
                    // Start of a fragmented message.
                    self.check_message_size(frame.payload.len())?;
                    let mut data = BytesMut::with_capacity(frame.payload.len());
                    data.extend_from_slice(&frame.payload);
                    self.fragment = Some(FragmentBuffer { is_text, data });
                    Ok(None)
                }
            }
            Opcode::Continuation => {
                let frag = self.fragment.as_mut().ok_or_else(|| {
                    Error::protocol(
                        "websocket: received a continuation frame with no message in progress",
                    )
                })?;
                let new_len = frag.data.len() + frame.payload.len();
                if new_len > self.config.max_message_size {
                    return Err(Error::protocol(format!(
                        "websocket: reassembled message size {} exceeds max_message_size {}",
                        new_len, self.config.max_message_size
                    )));
                }
                frag.data.extend_from_slice(&frame.payload);
                if frame.fin {
                    let FragmentBuffer { is_text, data } = self.fragment.take().unwrap();
                    self.finish_data_message(is_text, data.freeze())
                } else {
                    Ok(None)
                }
            }
            // Control opcodes were handled above.
            Opcode::Close | Opcode::Ping | Opcode::Pong => unreachable!(),
        }
    }

    /// Finalize a reassembled (or single-frame) data message, validating UTF-8
    /// for text and the overall size limit.
    fn finish_data_message(&mut self, is_text: bool, payload: Bytes) -> Result<Option<Message>> {
        self.check_message_size(payload.len())?;
        if is_text {
            match String::from_utf8(payload.to_vec()) {
                Ok(text) => Ok(Some(Message::Text(text))),
                Err(_) => Err(Error::protocol(
                    "websocket: text message payload is not valid UTF-8 (close code 1007)",
                )),
            }
        } else {
            Ok(Some(Message::Binary(payload)))
        }
    }

    /// Enforce [`WebSocketConfig::max_message_size`].
    fn check_message_size(&self, len: usize) -> Result<()> {
        if len > self.config.max_message_size {
            return Err(Error::protocol(format!(
                "websocket: message size {} exceeds max_message_size {} (close code 1009)",
                len, self.config.max_message_size
            )));
        }
        Ok(())
    }

    /// Handle a `Ping`/`Pong`/`Close` control frame.
    async fn handle_control_frame(&mut self, frame: Frame) -> Result<Option<Message>> {
        match frame.opcode {
            Opcode::Ping => {
                // RFC 6455 §5.5.3: respond to a Ping with a Pong carrying the
                // same payload, unless we have already started closing.
                if self.state == WebSocketState::Open {
                    self.write_control_frame(Opcode::Pong, frame.payload.clone())
                        .await?;
                }
                Ok(Some(Message::Ping(frame.payload)))
            }
            Opcode::Pong => {
                self.last_pong = Some(frame.payload.clone());
                Ok(Some(Message::Pong(frame.payload)))
            }
            Opcode::Close => {
                self.close_received = true;
                let close = CloseFrame::from_payload(&frame.payload)?;
                if self.state == WebSocketState::Open {
                    self.state = WebSocketState::Closing;
                }
                Ok(Some(Message::Close(close)))
            }
            Opcode::Text | Opcode::Binary | Opcode::Continuation => unreachable!(),
        }
    }

    /// Write a (possibly fragmented) data message to the socket.
    async fn write_data_frames(&mut self, opcode: Opcode, payload: Bytes) -> Result<()> {
        let max = self.config.max_frame_size.max(1);
        if payload.len() <= max {
            let frame = Frame {
                fin: true,
                opcode,
                payload,
            };
            return self.write_frame(&frame).await;
        }

        // Fragment: first frame carries the real opcode with FIN=0, the rest
        // are Continuation frames, the last has FIN=1.
        let mut offset = 0;
        let total = payload.len();
        let mut first = true;
        while offset < total {
            let end = (offset + max).min(total);
            let is_last = end == total;
            let frame = Frame {
                fin: is_last,
                opcode: if first { opcode } else { Opcode::Continuation },
                payload: payload.slice(offset..end),
            };
            self.write_frame(&frame).await?;
            offset = end;
            first = false;
        }
        Ok(())
    }

    /// Write a single control frame, validating the 125-byte payload limit.
    async fn write_control_frame(&mut self, opcode: Opcode, payload: Bytes) -> Result<()> {
        if payload.len() > 125 {
            return Err(Error::protocol(format!(
                "websocket: control frame payload {} exceeds 125 bytes",
                payload.len()
            )));
        }
        let frame = Frame {
            fin: true,
            opcode,
            payload,
        };
        self.write_frame(&frame).await
    }

    /// Send a `Close` frame and update the close-handshake bookkeeping. Unlike
    /// [`WebSocketConnection::write_message`], this is permitted while `Open`
    /// and drives the `Open -> Closing` transition.
    async fn send_close_frame(&mut self, close: Option<CloseFrame>) -> Result<()> {
        if self.close_sent {
            return Ok(());
        }
        let payload = close.map(|c| c.to_payload()).unwrap_or_default();
        if payload.len() > 125 {
            return Err(Error::protocol(
                "websocket: close frame payload exceeds 125 bytes",
            ));
        }
        let frame = Frame {
            fin: true,
            opcode: Opcode::Close,
            payload,
        };
        self.write_frame(&frame).await?;
        self.close_sent = true;
        if self.state == WebSocketState::Open {
            self.state = WebSocketState::Closing;
        }
        Ok(())
    }

    /// Encode and write one frame, flushing it so backpressure is honoured.
    async fn write_frame(&mut self, frame: &Frame) -> Result<()> {
        let bytes = frame.encode();
        self.stream.write_all(&bytes).await?;
        // Flush immediately: no unbounded buffering, and a stalled peer simply
        // leaves this future unresolved (see the module-level backpressure note).
        self.stream.flush().await?;
        Ok(())
    }
}

/// Leniently parse a frame, tolerating an unmasked client frame.
///
/// This mirrors [`Frame::parse`] but is only reached when
/// [`WebSocketConfig::accept_unmasked_frames`] is set. It handles the common
/// case (small-to-medium payloads, with or without a mask) needed by
/// non-conformant clients and test fixtures.
fn parse_lenient(buf: &[u8]) -> Result<Option<(Frame, usize)>> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let b0 = buf[0];
    let b1 = buf[1];
    let fin = b0 & 0x80 != 0;
    if b0 & 0x70 != 0 {
        return Err(Error::protocol(
            "websocket frame: reserved RSV bits must be zero",
        ));
    }
    let opcode = Opcode::from_u8(b0 & 0x0F)?;
    let masked = b1 & 0x80 != 0;
    let len7 = (b1 & 0x7F) as usize;

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
            offset += 8;
            u64::from_be_bytes(raw) as usize
        }
        other => other,
    };

    if opcode.is_control() && (!fin || payload_len > 125) {
        return Err(Error::protocol(
            "websocket frame: control frames must be unfragmented and <=125 bytes",
        ));
    }

    let mask_key = if masked {
        if buf.len() < offset + 4 {
            return Ok(None);
        }
        let k = [
            buf[offset],
            buf[offset + 1],
            buf[offset + 2],
            buf[offset + 3],
        ];
        offset += 4;
        Some(k)
    } else {
        None
    };

    if buf.len() < offset + payload_len {
        return Ok(None);
    }
    let mut payload = BytesMut::with_capacity(payload_len);
    match mask_key {
        Some(key) => {
            for (i, &byte) in buf[offset..offset + payload_len].iter().enumerate() {
                payload.extend_from_slice(&[byte ^ key[i & 3]]);
            }
        }
        None => payload.extend_from_slice(&buf[offset..offset + payload_len]),
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

// === permessage-deflate negotiation (RFC 7692) ==========================

/// Policy governing whether the server accepts the `permessage-deflate`
/// extension during the opening handshake.
///
/// Negotiation *parsing* is always performed; this policy only decides what to
/// do with a parsed offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompressionPolicy {
    /// Never accept `permessage-deflate`; every offer is declined. This is the
    /// behaviour when the crate is built without the `deflate` feature, since
    /// no compressor is then available.
    #[default]
    Disabled,
    /// Accept `permessage-deflate` when offered, using the negotiated
    /// parameters. Only meaningful with the `deflate` feature enabled.
    Enabled,
}

/// One parsed extension offer from a `Sec-WebSocket-Extensions` header.
///
/// An extensions header is a comma-separated list of extensions, each being a
/// token optionally followed by `;`-separated parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionOffer {
    /// The extension token, e.g. `permessage-deflate`.
    pub name: String,
    /// The `;`-separated parameters as `(name, optional value)` pairs.
    pub params: Vec<(String, Option<String>)>,
}

/// The four `permessage-deflate` parameters from RFC 7692 §7.1, as understood
/// from a client offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerMessageDeflateParams {
    /// Client requested that the *server* not use context takeover.
    pub server_no_context_takeover: bool,
    /// Client offered that *it* will not use context takeover.
    pub client_no_context_takeover: bool,
    /// The `server_max_window_bits` value the client is willing to accept
    /// (8–15), if it constrained it.
    pub server_max_window_bits: Option<u8>,
    /// The `client_max_window_bits` the client offered to use (8–15). The bare
    /// parameter with no value is represented as `Some(15)`.
    pub client_max_window_bits: Option<u8>,
}

/// The outcome of negotiating `permessage-deflate` against a client offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeflateNegotiation {
    /// The extension was not offered, or policy / build configuration declined
    /// it. No `Sec-WebSocket-Extensions` response header should be sent.
    Rejected,
    /// The extension was accepted with these parameters; the contained string
    /// is the exact `Sec-WebSocket-Extensions` response header *value* to echo.
    Accepted {
        /// The agreed parameters.
        params: PerMessageDeflateParams,
        /// The response header value, e.g.
        /// `permessage-deflate; client_max_window_bits=15`.
        response_header: String,
    },
}

/// Parse a `Sec-WebSocket-Extensions` header value into individual offers.
///
/// The grammar (RFC 6455 §9.1, RFC 7692 §7) is a comma-separated list of
/// `extension-token (";" parameter)*`. Parameter values may be bare tokens or
/// quoted strings; surrounding whitespace is ignored. Malformed fragments are
/// skipped rather than failing the whole parse, mirroring lenient header
/// handling elsewhere in the runtime.
pub fn parse_extension_offers(header_value: &str) -> Vec<ExtensionOffer> {
    let mut offers = Vec::new();
    for ext in header_value.split(',') {
        let mut parts = ext.split(';');
        let name = match parts.next() {
            Some(n) => n.trim(),
            None => continue,
        };
        if name.is_empty() {
            continue;
        }
        let mut params = Vec::new();
        for param in parts {
            let param = param.trim();
            if param.is_empty() {
                continue;
            }
            match param.split_once('=') {
                Some((k, v)) => {
                    let v = v.trim().trim_matches('"').to_owned();
                    params.push((k.trim().to_ascii_lowercase(), Some(v)));
                }
                None => params.push((param.to_ascii_lowercase(), None)),
            }
        }
        offers.push(ExtensionOffer {
            name: name.to_ascii_lowercase(),
            params,
        });
    }
    offers
}

/// Negotiate `permessage-deflate` against a client's `Sec-WebSocket-Extensions`
/// header, under the given [`CompressionPolicy`].
///
/// This performs the *negotiation* half of RFC 7692: it parses the offer,
/// validates the four parameters, and — when policy permits — produces the
/// response header value to echo back in the handshake. Actual DEFLATE
/// compression of frame payloads is gated behind the `deflate` Cargo feature
/// and is intentionally out of scope here; with the feature off,
/// [`CompressionPolicy::Disabled`] is the only sensible policy and every offer
/// is [`DeflateNegotiation::Rejected`].
///
/// Negotiation rules applied:
///
/// * Only the *first* `permessage-deflate` offer is considered (RFC 7692 §5.1
///   lets the server pick one; we pick the first).
/// * `server_max_window_bits` / `client_max_window_bits` must be in `8..=15`;
///   an out-of-range value rejects that offer.
/// * `client_max_window_bits` with no value means "client can accept any value"
///   and is treated as `15`.
/// * The `*_no_context_takeover` flags are boolean and must not carry a value.
pub fn negotiate_permessage_deflate(
    header_value: &str,
    policy: CompressionPolicy,
) -> DeflateNegotiation {
    let offers = parse_extension_offers(header_value);
    negotiate_permessage_deflate_from_offers(&offers, policy)
}

/// Like [`negotiate_permessage_deflate`] but operating on already-parsed
/// [`ExtensionOffer`]s — useful when the caller has the offers in hand.
pub fn negotiate_permessage_deflate_from_offers(
    offers: &[ExtensionOffer],
    policy: CompressionPolicy,
) -> DeflateNegotiation {
    if policy == CompressionPolicy::Disabled {
        return DeflateNegotiation::Rejected;
    }

    let offer = match offers.iter().find(|o| o.name == "permessage-deflate") {
        Some(o) => o,
        None => return DeflateNegotiation::Rejected,
    };

    let mut params = PerMessageDeflateParams {
        server_no_context_takeover: false,
        client_no_context_takeover: false,
        server_max_window_bits: None,
        client_max_window_bits: None,
    };

    for (key, value) in &offer.params {
        match key.as_str() {
            "server_no_context_takeover" => {
                if value.is_some() {
                    return DeflateNegotiation::Rejected;
                }
                params.server_no_context_takeover = true;
            }
            "client_no_context_takeover" => {
                if value.is_some() {
                    return DeflateNegotiation::Rejected;
                }
                params.client_no_context_takeover = true;
            }
            "server_max_window_bits" => match value.as_deref().map(parse_window_bits) {
                Some(Some(bits)) => params.server_max_window_bits = Some(bits),
                // RFC 7692: in an *offer* server_max_window_bits must have a
                // value. A missing or invalid value rejects the offer.
                _ => return DeflateNegotiation::Rejected,
            },
            "client_max_window_bits" => match value.as_deref() {
                None => params.client_max_window_bits = Some(15),
                Some(v) => match parse_window_bits(v) {
                    Some(bits) => params.client_max_window_bits = Some(bits),
                    None => return DeflateNegotiation::Rejected,
                },
            },
            // Unknown parameter — reject this offer (RFC 7692 §5.1).
            _ => return DeflateNegotiation::Rejected,
        }
    }

    let response_header = build_deflate_response_header(&params);
    DeflateNegotiation::Accepted {
        params,
        response_header,
    }
}

/// Parse a window-bits parameter value, accepting only `8..=15`.
fn parse_window_bits(value: &str) -> Option<u8> {
    match value.trim().parse::<u8>() {
        Ok(bits) if (8..=15).contains(&bits) => Some(bits),
        _ => None,
    }
}

/// Build the `Sec-WebSocket-Extensions` response header value for an accepted
/// `permessage-deflate` negotiation.
fn build_deflate_response_header(params: &PerMessageDeflateParams) -> String {
    let mut out = String::from("permessage-deflate");
    if params.server_no_context_takeover {
        out.push_str("; server_no_context_takeover");
    }
    if params.client_no_context_takeover {
        out.push_str("; client_no_context_takeover");
    }
    if let Some(bits) = params.server_max_window_bits {
        out.push_str("; server_max_window_bits=");
        out.push_str(&bits.to_string());
    }
    if let Some(bits) = params.client_max_window_bits {
        out.push_str("; client_max_window_bits=");
        out.push_str(&bits.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    /// Mask `payload` with `key`, producing a raw client frame body.
    fn mask(payload: &[u8], key: [u8; 4]) -> Vec<u8> {
        payload
            .iter()
            .enumerate()
            .map(|(i, &b)| b ^ key[i & 3])
            .collect()
    }

    /// Build a complete masked *client* frame (the form the server reads).
    fn client_frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let key = [0xAB, 0xCD, 0xEF, 0x12];
        let masked = mask(payload, key);
        let b0 = if fin { 0x80 } else { 0x00 } | opcode;
        let mut buf = Vec::new();
        let len = payload.len();
        if len < 126 {
            buf.push(b0);
            buf.push(0x80 | len as u8);
        } else if len <= u16::MAX as usize {
            buf.push(b0);
            buf.push(0x80 | 126);
            buf.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            buf.push(b0);
            buf.push(0x80 | 127);
            buf.extend_from_slice(&(len as u64).to_be_bytes());
        }
        buf.extend_from_slice(&key);
        buf.extend_from_slice(&masked);
        buf
    }

    #[tokio::test]
    async fn round_trips_a_text_message() {
        let (client, server) = duplex(4096);
        let mut conn = WebSocketConnection::new(server, WebSocketConfig::default());

        // Client -> server.
        let frame = client_frame(true, 0x1, b"hello world");
        {
            use tokio::io::AsyncWriteExt;
            let mut client = client;
            client.write_all(&frame).await.unwrap();

            let msg = conn.read_message().await.unwrap().unwrap();
            assert_eq!(msg, Message::Text("hello world".to_string()));

            // Server -> client.
            conn.write_message(Message::Text("hi back".to_string()))
                .await
                .unwrap();
            let mut buf = vec![0u8; 64];
            let n = client.read(&mut buf).await.unwrap();
            // FIN+text, len 7, unmasked.
            assert_eq!(buf[0], 0x81);
            assert_eq!(buf[1], 7);
            assert_eq!(&buf[2..n], b"hi back");
        }
    }

    #[tokio::test]
    async fn round_trips_a_binary_message() {
        let (mut client, server) = duplex(4096);
        let mut conn = WebSocketConnection::new(server, WebSocketConfig::default());

        let payload: Vec<u8> = (0u8..200).collect();
        client
            .write_all(&client_frame(true, 0x2, &payload))
            .await
            .unwrap();
        let msg = conn.read_message().await.unwrap().unwrap();
        assert_eq!(msg, Message::Binary(Bytes::from(payload.clone())));

        conn.write_message(Message::Binary(Bytes::from(payload.clone())))
            .await
            .unwrap();
        let mut buf = vec![0u8; 512];
        let n = client.read(&mut buf).await.unwrap();
        // FIN+binary, extended 16-bit length.
        assert_eq!(buf[0], 0x82);
        assert_eq!(buf[1], 126);
        let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        assert_eq!(len, 200);
        assert_eq!(&buf[4..n], &payload[..]);
    }

    #[tokio::test]
    async fn reassembles_a_fragmented_message() {
        let (mut client, server) = duplex(4096);
        let mut conn = WebSocketConnection::new(server, WebSocketConfig::default());

        // Three fragments: Text(FIN=0), Continuation(FIN=0), Continuation(FIN=1).
        client
            .write_all(&client_frame(false, 0x1, b"Hello, "))
            .await
            .unwrap();
        client
            .write_all(&client_frame(false, 0x0, b"fragmented "))
            .await
            .unwrap();
        client
            .write_all(&client_frame(true, 0x0, b"world!"))
            .await
            .unwrap();

        let msg = conn.read_message().await.unwrap().unwrap();
        assert_eq!(msg, Message::Text("Hello, fragmented world!".to_string()));
    }

    #[tokio::test]
    async fn ping_is_auto_ponged() {
        let (mut client, server) = duplex(4096);
        let mut conn = WebSocketConnection::new(server, WebSocketConfig::default());

        client
            .write_all(&client_frame(true, 0x9, b"liveness"))
            .await
            .unwrap();
        let msg = conn.read_message().await.unwrap().unwrap();
        assert_eq!(msg, Message::Ping(Bytes::from_static(b"liveness")));

        // The server must have auto-sent a Pong with the same payload.
        let mut buf = vec![0u8; 64];
        let n = client.read(&mut buf).await.unwrap();
        assert_eq!(buf[0], 0x8A); // FIN + Pong
        assert_eq!(buf[1], 8); // unmasked, len 8
        assert_eq!(&buf[2..n], b"liveness");
    }

    #[tokio::test]
    async fn control_frame_interleaved_within_fragments() {
        let (mut client, server) = duplex(4096);
        let mut conn = WebSocketConnection::new(server, WebSocketConfig::default());

        // Start a fragmented text message, slip a Ping in between fragments.
        client
            .write_all(&client_frame(false, 0x1, b"part-one "))
            .await
            .unwrap();
        client
            .write_all(&client_frame(true, 0x9, b"midping"))
            .await
            .unwrap();
        client
            .write_all(&client_frame(true, 0x0, b"part-two"))
            .await
            .unwrap();

        // The Ping surfaces first (and is auto-ponged), then the reassembled text.
        let m1 = conn.read_message().await.unwrap().unwrap();
        assert_eq!(m1, Message::Ping(Bytes::from_static(b"midping")));
        let m2 = conn.read_message().await.unwrap().unwrap();
        assert_eq!(m2, Message::Text("part-one part-two".to_string()));

        // And the auto-pong went out.
        let mut buf = vec![0u8; 64];
        let n = client.read(&mut buf).await.unwrap();
        assert_eq!(buf[0], 0x8A);
        assert_eq!(&buf[2..n], b"midping");
    }

    #[tokio::test]
    async fn oversize_message_is_rejected() {
        let (mut client, server) = duplex(64 * 1024);
        let config = WebSocketConfig {
            max_message_size: 100,
            max_frame_size: 64 * 1024,
            ..WebSocketConfig::default()
        };
        let mut conn = WebSocketConnection::new(server, config);

        let big = vec![0x41u8; 500];
        client
            .write_all(&client_frame(true, 0x2, &big))
            .await
            .unwrap();
        let err = conn.read_message().await.unwrap_err();
        assert!(matches!(err, Error::Protocol(_)));
    }

    #[tokio::test]
    async fn oversize_frame_is_rejected() {
        let (mut client, server) = duplex(64 * 1024);
        let config = WebSocketConfig {
            max_message_size: 64 * 1024,
            max_frame_size: 50,
            ..WebSocketConfig::default()
        };
        let mut conn = WebSocketConnection::new(server, config);

        let big = vec![0x41u8; 200];
        client
            .write_all(&client_frame(true, 0x2, &big))
            .await
            .unwrap();
        let err = conn.read_message().await.unwrap_err();
        assert!(matches!(err, Error::Protocol(_)));
    }

    #[tokio::test]
    async fn invalid_utf8_text_is_rejected() {
        let (mut client, server) = duplex(4096);
        let mut conn = WebSocketConnection::new(server, WebSocketConfig::default());

        // 0xFF is never valid in UTF-8.
        client
            .write_all(&client_frame(true, 0x1, &[0x48, 0x69, 0xFF]))
            .await
            .unwrap();
        let err = conn.read_message().await.unwrap_err();
        match err {
            Error::Protocol(msg) => assert!(msg.contains("1007")),
            other => panic!("expected protocol error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_after_close_is_rejected() {
        let (client, server) = duplex(4096);
        // A short close timeout: the peer never answers, so close() falls
        // through to the timeout path and still ends up Closed.
        let config = WebSocketConfig {
            close_timeout: Duration::from_millis(50),
            ..WebSocketConfig::default()
        };
        let mut conn = WebSocketConnection::new(server, config);

        // Keep `client` alive (so the Close frame write succeeds) but never
        // respond to the close handshake.
        conn.close(close_code::NORMAL, "done").await.unwrap();
        assert_eq!(conn.state(), WebSocketState::Closed);

        let err = conn
            .write_message(Message::Text("too late".to_string()))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Protocol(_)));

        drop(client);
    }

    #[tokio::test]
    async fn close_handshake_peer_initiated() {
        let (mut client, server) = duplex(4096);
        let mut conn = WebSocketConnection::new(server, WebSocketConfig::default());

        // Peer sends a Close (code 1000, reason "bye").
        let mut close_payload = Vec::new();
        close_payload.extend_from_slice(&1000u16.to_be_bytes());
        close_payload.extend_from_slice(b"bye");
        client
            .write_all(&client_frame(true, 0x8, &close_payload))
            .await
            .unwrap();

        let msg = conn.read_message().await.unwrap().unwrap();
        assert_eq!(msg, Message::Close(Some(CloseFrame::new(1000, "bye"))));
        assert_eq!(conn.state(), WebSocketState::Closing);

        // Server completes the handshake by sending its own Close.
        conn.close(close_code::NORMAL, "bye back").await.unwrap();
        assert_eq!(conn.state(), WebSocketState::Closed);

        // Client should have received the server's Close frame.
        let mut buf = vec![0u8; 64];
        let n = client.read(&mut buf).await.unwrap();
        assert_eq!(buf[0], 0x88); // FIN + Close
        let code = u16::from_be_bytes([buf[2], buf[3]]);
        assert_eq!(code, 1000);
        assert_eq!(&buf[4..n], b"bye back");
    }

    #[tokio::test]
    async fn close_handshake_server_initiated() {
        let (mut client, server) = duplex(4096);
        let mut conn = WebSocketConnection::new(server, WebSocketConfig::default());

        // Server initiates; spawn a task to act as the responding peer.
        let peer = tokio::spawn(async move {
            // Read the server's Close frame.
            let mut buf = vec![0u8; 64];
            let n = client.read(&mut buf).await.unwrap();
            assert_eq!(buf[0], 0x88);
            // Respond with our own Close.
            let mut payload = Vec::new();
            payload.extend_from_slice(&1000u16.to_be_bytes());
            client
                .write_all(&client_frame(true, 0x8, &payload))
                .await
                .unwrap();
            n
        });

        conn.close(close_code::GOING_AWAY, "shutdown")
            .await
            .unwrap();
        assert_eq!(conn.state(), WebSocketState::Closed);
        assert!(conn.close_received);
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn server_fragments_large_outbound_message() {
        let (mut client, server) = duplex(64 * 1024);
        let config = WebSocketConfig {
            max_frame_size: 10,
            ..WebSocketConfig::default()
        };
        let mut conn = WebSocketConnection::new(server, config);

        conn.write_message(Message::Text("0123456789ABCDEFGHIJ".to_string()))
            .await
            .unwrap();

        // 20 bytes / 10 per frame => first Text(FIN=0), then Continuation(FIN=1).
        let mut buf = vec![0u8; 256];
        let n = client.read(&mut buf).await.unwrap();
        // First frame: opcode text (0x1), FIN clear => 0x01.
        assert_eq!(buf[0], 0x01);
        assert_eq!(buf[1], 10);
        // Somewhere later a continuation frame with FIN set: 0x80 | 0x0 = 0x80.
        assert!(buf[..n].windows(1).any(|w| w[0] == 0x80) || n > 12);
        assert_eq!(buf[12], 0x80); // second frame header: FIN + Continuation
        assert_eq!(buf[13], 10);
    }

    #[tokio::test]
    async fn clean_eof_returns_none() {
        let (client, server) = duplex(4096);
        let mut conn = WebSocketConnection::new(server, WebSocketConfig::default());
        drop(client);
        assert!(conn.read_message().await.unwrap().is_none());
        assert_eq!(conn.state(), WebSocketState::Closed);
    }

    #[tokio::test]
    async fn accepts_unmasked_frames_when_configured() {
        let (mut client, server) = duplex(4096);
        let config = WebSocketConfig {
            accept_unmasked_frames: true,
            ..WebSocketConfig::default()
        };
        let mut conn = WebSocketConnection::new(server, config);

        // An unmasked text frame — illegal from a conformant client, but
        // tolerated here.
        let mut buf = vec![0x81u8, 5];
        buf.extend_from_slice(b"plain");
        client.write_all(&buf).await.unwrap();

        let msg = conn.read_message().await.unwrap().unwrap();
        assert_eq!(msg, Message::Text("plain".to_string()));
    }

    // --- permessage-deflate negotiation ---------------------------------

    #[test]
    fn parses_a_simple_extension_offer() {
        let offers = parse_extension_offers(
            "permessage-deflate; client_max_window_bits; server_no_context_takeover",
        );
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].name, "permessage-deflate");
        assert_eq!(
            offers[0].params,
            vec![
                ("client_max_window_bits".to_string(), None),
                ("server_no_context_takeover".to_string(), None),
            ]
        );
    }

    #[test]
    fn parses_multiple_extensions_and_valued_params() {
        let offers = parse_extension_offers(
            "permessage-deflate; server_max_window_bits=12, x-custom; q=\"hi\"",
        );
        assert_eq!(offers.len(), 2);
        assert_eq!(offers[0].name, "permessage-deflate");
        assert_eq!(
            offers[0].params,
            vec![("server_max_window_bits".to_string(), Some("12".to_string()))]
        );
        assert_eq!(offers[1].name, "x-custom");
        assert_eq!(
            offers[1].params,
            vec![("q".to_string(), Some("hi".to_string()))]
        );
    }

    #[test]
    fn deflate_offer_rejected_when_policy_disabled() {
        let neg = negotiate_permessage_deflate(
            "permessage-deflate; client_max_window_bits",
            CompressionPolicy::Disabled,
        );
        assert_eq!(neg, DeflateNegotiation::Rejected);
    }

    #[test]
    fn deflate_offer_accepted_when_policy_enabled() {
        let neg = negotiate_permessage_deflate(
            "permessage-deflate; client_max_window_bits=15; server_no_context_takeover",
            CompressionPolicy::Enabled,
        );
        match neg {
            DeflateNegotiation::Accepted {
                params,
                response_header,
            } => {
                assert!(params.server_no_context_takeover);
                assert_eq!(params.client_max_window_bits, Some(15));
                assert!(response_header.starts_with("permessage-deflate"));
                assert!(response_header.contains("server_no_context_takeover"));
                assert!(response_header.contains("client_max_window_bits=15"));
            }
            DeflateNegotiation::Rejected => panic!("expected acceptance"),
        }
    }

    #[test]
    fn deflate_offer_with_bad_window_bits_is_rejected() {
        // 7 is below the legal 8..=15 range.
        let neg = negotiate_permessage_deflate(
            "permessage-deflate; server_max_window_bits=7",
            CompressionPolicy::Enabled,
        );
        assert_eq!(neg, DeflateNegotiation::Rejected);
    }

    #[test]
    fn deflate_offer_with_unknown_param_is_rejected() {
        let neg = negotiate_permessage_deflate(
            "permessage-deflate; bogus_param",
            CompressionPolicy::Enabled,
        );
        assert_eq!(neg, DeflateNegotiation::Rejected);
    }

    #[test]
    fn no_deflate_offer_is_rejected() {
        let neg = negotiate_permessage_deflate("x-other-extension", CompressionPolicy::Enabled);
        assert_eq!(neg, DeflateNegotiation::Rejected);
    }

    #[test]
    fn close_frame_payload_round_trip() {
        let cf = CloseFrame::new(1000, "normal");
        let payload = cf.to_payload();
        let decoded = CloseFrame::from_payload(&payload).unwrap().unwrap();
        assert_eq!(decoded, cf);

        // Empty payload => no close frame body.
        assert!(CloseFrame::from_payload(&[]).unwrap().is_none());
        // 1-byte payload is malformed.
        assert!(CloseFrame::from_payload(&[0x03]).is_err());
    }
}
