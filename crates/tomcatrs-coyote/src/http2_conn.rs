//! The HTTP/2 connection state machine.
//!
//! This module implements [RFC 9113] on top of two lower-level building blocks
//! supplied by sibling modules:
//!
//! * [`crate::http2`] — the frame layer: `Frame` parsing/encoding, frame flags,
//!   error codes, and settings identifiers.
//! * [`crate::hpack`] — header compression: `HpackDecoder` / `HpackEncoder`.
//!
//! [`Http2Connection`] owns one client connection. It validates the connection
//! preface, exchanges `SETTINGS`, then runs a single-threaded frame loop that
//! demultiplexes the wire into per-stream state. When a request is fully
//! received it is handed to the [`Adapter`] on a spawned task; the resulting
//! [`Response`] is serialized back as `HEADERS` + `DATA` frames, honoring the
//! peer's advertised `MAX_FRAME_SIZE` and both the connection-level and
//! per-stream flow-control windows.
//!
//! # What is implemented
//!
//! * Connection preface validation and `SETTINGS` exchange (+ `ACK`).
//! * `HEADERS` (+ `CONTINUATION`) reassembly and HPACK decode into a
//!   [`Request`].
//! * `DATA` frame intake with connection + per-stream flow control, emitting
//!   `WINDOW_UPDATE` as the server consumes received `DATA`.
//! * `PING` / `PING ACK`, `RST_STREAM`, `WINDOW_UPDATE`, `PRIORITY` handling.
//! * `MAX_CONCURRENT_STREAMS` enforcement (`REFUSED_STREAM`).
//! * `GOAWAY` on connection-level protocol errors and on graceful shutdown.
//! * The stream state machine ([`StreamState`]) with the legal transitions of
//!   RFC 9113 §5.1.
//!
//! # What is intentionally out of scope
//!
//! Server push (`PUSH_PROMISE`) is never used — Tomcat does not push. Stream
//! prioritization is parsed and accepted but not acted upon, which RFC 9113
//! §5.3 explicitly permits.
//!
//! [RFC 9113]: https://www.rfc-editor.org/rfc/rfc9113

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use tomcatrs_core::{Error, Result};

use crate::hpack::{HpackDecoder, HpackEncoder};
use crate::http2::{error_codes, settings as settings_ids, Frame, FRAME_HEADER_LEN};
use crate::normalize::normalize_target;
use crate::{Adapter, Request, Response};

/// The largest `WINDOW_UPDATE`/initial-window value permitted by RFC 9113:
/// `2^31 - 1`. A flow-control window may never exceed this.
const MAX_WINDOW: i64 = 0x7FFF_FFFF;

/// Hard cap on the number of bytes we will buffer for a single in-progress
/// header block (`HEADERS` + `CONTINUATION`) before declaring a protocol error.
/// Independent of `SETTINGS_MAX_HEADER_LIST_SIZE`, which bounds the *decoded*
/// size; this bounds the *compressed* bytes so a peer cannot exhaust memory by
/// never sending `END_HEADERS`.
const HEADER_BLOCK_HARD_CAP: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// One side's HTTP/2 connection settings.
///
/// A [`Http2Connection`] keeps two of these: `local` (what this server
/// advertises and enforces against the peer) and `peer` (what the client
/// advertised, which constrains what the server may send). All fields start at
/// the RFC 9113 §6.5.2 defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Http2Settings {
    /// `SETTINGS_HEADER_TABLE_SIZE` — HPACK dynamic table size, in bytes.
    pub header_table_size: u32,
    /// `SETTINGS_MAX_CONCURRENT_STREAMS` — cap on concurrently open streams.
    pub max_concurrent_streams: u32,
    /// `SETTINGS_INITIAL_WINDOW_SIZE` — initial per-stream flow-control window.
    pub initial_window_size: u32,
    /// `SETTINGS_MAX_FRAME_SIZE` — largest frame payload accepted.
    pub max_frame_size: u32,
    /// `SETTINGS_MAX_HEADER_LIST_SIZE` — advisory cap on the decoded header
    /// list size, in bytes. `u32::MAX` means "unlimited" (the RFC default).
    pub max_header_list_size: u32,
}

impl Default for Http2Settings {
    fn default() -> Self {
        // RFC 9113 §6.5.2 defaults. MAX_CONCURRENT_STREAMS has no protocol
        // default ("unlimited"); we pick a sane finite cap for the server side.
        Http2Settings {
            header_table_size: 4096,
            max_concurrent_streams: 128,
            initial_window_size: 65_535,
            max_frame_size: 16_384,
            max_header_list_size: u32::MAX,
        }
    }
}

impl Http2Settings {
    /// Encode the settings this server wants to advertise as `(id, value)`
    /// pairs suitable for a non-`ACK` `SETTINGS` frame.
    fn as_params(&self) -> Vec<(u16, u32)> {
        vec![
            (settings_ids::HEADER_TABLE_SIZE, self.header_table_size),
            (
                settings_ids::MAX_CONCURRENT_STREAMS,
                self.max_concurrent_streams,
            ),
            (settings_ids::INITIAL_WINDOW_SIZE, self.initial_window_size),
            (settings_ids::MAX_FRAME_SIZE, self.max_frame_size),
            (
                settings_ids::MAX_HEADER_LIST_SIZE,
                self.max_header_list_size,
            ),
        ]
    }

    /// Apply one `(id, value)` pair received from the peer.
    ///
    /// Unknown identifiers are ignored, as RFC 9113 §6.5.2 requires. Returns an
    /// error for values that violate the spec's allowed ranges.
    fn apply(&mut self, id: u16, value: u32) -> std::result::Result<(), Http2Error> {
        match id {
            settings_ids::HEADER_TABLE_SIZE => self.header_table_size = value,
            settings_ids::ENABLE_PUSH => {
                // The server never pushes, but the client may only send 0 or 1.
                if value > 1 {
                    return Err(Http2Error::connection(
                        error_codes::PROTOCOL_ERROR,
                        "ENABLE_PUSH must be 0 or 1",
                    ));
                }
            }
            settings_ids::MAX_CONCURRENT_STREAMS => self.max_concurrent_streams = value,
            settings_ids::INITIAL_WINDOW_SIZE => {
                if value as i64 > MAX_WINDOW {
                    return Err(Http2Error::connection(
                        error_codes::FLOW_CONTROL_ERROR,
                        "INITIAL_WINDOW_SIZE exceeds 2^31-1",
                    ));
                }
                self.initial_window_size = value;
            }
            settings_ids::MAX_FRAME_SIZE => {
                // Permitted range is 2^14..=2^24-1 (RFC 9113 §6.5.2).
                if !(16_384..=16_777_215).contains(&value) {
                    return Err(Http2Error::connection(
                        error_codes::PROTOCOL_ERROR,
                        "MAX_FRAME_SIZE out of range",
                    ));
                }
                self.max_frame_size = value;
            }
            settings_ids::MAX_HEADER_LIST_SIZE => self.max_header_list_size = value,
            _ => {} // Unknown setting — ignore.
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Stream state machine
// ---------------------------------------------------------------------------

/// The lifecycle state of a single HTTP/2 stream (RFC 9113 §5.1).
///
/// The server side only ever drives client-initiated (odd-numbered) streams, so
/// the `reserved` states are unreachable here and deliberately omitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    /// No frames have been exchanged for this stream id yet.
    Idle,
    /// `HEADERS` received; the stream is fully active in both directions.
    Open,
    /// This endpoint has sent `END_STREAM`; it may still receive frames.
    HalfClosedLocal,
    /// The peer has sent `END_STREAM`; this endpoint may still send frames.
    HalfClosedRemote,
    /// The stream is finished (or was reset). No further frames are valid.
    Closed,
}

impl StreamState {
    /// Transition for receiving `HEADERS` (request start) on this stream.
    ///
    /// `end_stream` is the frame's `END_STREAM` flag. Returns the new state, or
    /// an error if `HEADERS` is illegal in the current state.
    fn on_recv_headers(self, end_stream: bool) -> std::result::Result<StreamState, Http2Error> {
        match self {
            StreamState::Idle => Ok(if end_stream {
                StreamState::HalfClosedRemote
            } else {
                StreamState::Open
            }),
            // Trailers: HEADERS on an already-open stream must carry END_STREAM.
            StreamState::Open if end_stream => Ok(StreamState::HalfClosedRemote),
            _ => Err(Http2Error::connection(
                error_codes::STREAM_CLOSED,
                "HEADERS not valid in this stream state",
            )),
        }
    }

    /// Transition for receiving a `DATA` frame on this stream.
    fn on_recv_data(self, end_stream: bool) -> std::result::Result<StreamState, Http2Error> {
        match self {
            StreamState::Open => Ok(if end_stream {
                StreamState::HalfClosedRemote
            } else {
                StreamState::Open
            }),
            StreamState::HalfClosedLocal => Ok(if end_stream {
                StreamState::Closed
            } else {
                StreamState::HalfClosedLocal
            }),
            _ => Err(Http2Error::stream(
                error_codes::STREAM_CLOSED,
                "DATA not valid in this stream state",
            )),
        }
    }

    /// Transition for *this endpoint* sending `END_STREAM` (on the response).
    fn on_send_end_stream(self) -> StreamState {
        match self {
            StreamState::Open => StreamState::HalfClosedLocal,
            StreamState::HalfClosedRemote => StreamState::Closed,
            other => other,
        }
    }

    /// Whether the stream has reached a terminal state.
    fn is_closed(self) -> bool {
        matches!(self, StreamState::Closed)
    }
}

/// Per-stream bookkeeping for an in-flight request/response exchange.
#[derive(Debug)]
pub struct Stream {
    /// The stream identifier (odd, client-initiated).
    pub id: u32,
    /// Current lifecycle state.
    pub state: StreamState,
    /// Flow-control window for `DATA` we may *receive* on this stream. Starts
    /// at our advertised `INITIAL_WINDOW_SIZE`.
    pub recv_window: i64,
    /// Flow-control window for `DATA` we may *send* on this stream. Starts at
    /// the peer's advertised `INITIAL_WINDOW_SIZE`.
    pub send_window: i64,
    /// Compressed header-block fragments accumulated across `HEADERS` +
    /// `CONTINUATION` until `END_HEADERS` is seen.
    pub header_block: Vec<u8>,
    /// Whether a `HEADERS` frame has been seen but `END_HEADERS` has not yet —
    /// i.e. we are mid-block and only `CONTINUATION` for this stream is legal.
    pub awaiting_continuation: bool,
    /// `END_STREAM` flag observed on the `HEADERS` frame, remembered until the
    /// block is complete so we know whether a body follows.
    pub headers_end_stream: bool,
    /// Decoded request headers (populated once the block is decoded).
    pub headers: Vec<(String, String)>,
    /// Whether the request headers have been decoded yet.
    pub headers_decoded: bool,
    /// Accumulated request body bytes from `DATA` frames.
    pub body: Vec<u8>,
    /// Whether the request is fully received (`END_STREAM` seen) and has been
    /// dispatched to the adapter.
    pub dispatched: bool,
}

impl Stream {
    /// Create a fresh `Idle` stream with windows seeded from both settings.
    fn new(id: u32, local_initial_window: u32, peer_initial_window: u32) -> Self {
        Stream {
            id,
            state: StreamState::Idle,
            recv_window: local_initial_window as i64,
            send_window: peer_initial_window as i64,
            header_block: Vec::new(),
            awaiting_continuation: false,
            headers_end_stream: false,
            headers: Vec::new(),
            headers_decoded: false,
            body: Vec::new(),
            dispatched: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// An HTTP/2 protocol error, scoped to either a single stream or the whole
/// connection.
#[derive(Debug, Clone)]
struct Http2Error {
    /// One of [`crate::http2::error_codes`].
    code: u32,
    /// Human-readable detail, used for `GOAWAY` debug data and logging.
    detail: String,
    /// `true` if only the offending stream must be reset; `false` if the whole
    /// connection must be torn down with `GOAWAY`.
    stream_only: Option<u32>,
}

impl Http2Error {
    /// A connection-level error: the connection will be closed with `GOAWAY`.
    fn connection(code: u32, detail: impl Into<String>) -> Self {
        Http2Error {
            code,
            detail: detail.into(),
            stream_only: None,
        }
    }

    /// A stream-level error: only the affected stream is reset. The stream id
    /// is filled in by the dispatch site via [`Http2Error::on_stream`].
    fn stream(code: u32, detail: impl Into<String>) -> Self {
        Http2Error {
            code,
            detail: detail.into(),
            stream_only: Some(0),
        }
    }

    /// Bind a stream-scoped error to a concrete stream id.
    fn on_stream(mut self, id: u32) -> Self {
        if self.stream_only.is_some() {
            self.stream_only = Some(id);
        }
        self
    }
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// A response produced by a per-stream adapter task, routed back to the writer.
struct StreamResponse {
    /// The stream the response belongs to.
    stream_id: u32,
    /// The adapter's response.
    response: Response,
}

/// An HTTP/2 connection driver over an arbitrary async byte stream `S`.
///
/// One instance services exactly one client connection for its whole lifetime.
/// Construct-and-run is the only supported usage: see [`Http2Connection::serve`].
pub struct Http2Connection<S> {
    /// The underlying duplex byte stream (a `TcpStream`, a TLS stream, or — in
    /// tests — a `tokio::io::duplex` half).
    io: S,
    /// The adapter every fully-received request is dispatched to.
    adapter: Arc<dyn Adapter>,
    /// The remote peer address, copied into every [`Request`].
    peer_addr: SocketAddr,
    /// Settings this server advertises and enforces.
    local_settings: Http2Settings,
    /// Settings the peer advertised; constrains what we may send.
    peer_settings: Http2Settings,
    /// Whether the peer has acknowledged our initial `SETTINGS` frame.
    peer_acked_settings: bool,
    /// Connection-level flow-control window for `DATA` we may *receive*.
    conn_recv_window: i64,
    /// Connection-level flow-control window for `DATA` we may *send*.
    conn_send_window: i64,
    /// All streams that are not yet fully closed, keyed by stream id.
    streams: HashMap<u32, Stream>,
    /// The highest client-initiated stream id seen so far. Stream ids must be
    /// strictly increasing; a lower id is a protocol error.
    last_stream_id: u32,
    /// Count of streams currently in a non-idle, non-closed state — compared
    /// against `local_settings.max_concurrent_streams`.
    open_stream_count: u32,
    /// HPACK decoder. HPACK is stateful per direction and per connection, so
    /// this must persist across every `HEADERS` block on the connection.
    hpack_decoder: HpackDecoder,
    /// HPACK encoder for response headers.
    hpack_encoder: HpackEncoder,
    /// Raw inbound byte buffer; frames are parsed out of the front of this.
    inbuf: Vec<u8>,
    /// `true` once a `GOAWAY` has been sent and the loop should wind down.
    going_away: bool,
}

impl<S> Http2Connection<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    /// Serve one HTTP/2 connection to completion.
    ///
    /// This validates the client connection preface, exchanges `SETTINGS`, and
    /// then runs the frame loop until the peer closes, a `GOAWAY` is warranted,
    /// or an unrecoverable I/O error occurs.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] on an unrecoverable socket error. HTTP/2 protocol
    /// violations are handled in-band (a `RST_STREAM` or a `GOAWAY` frame) and
    /// do **not** surface as an `Err`.
    pub async fn serve(stream: S, adapter: Arc<dyn Adapter>, peer_addr: SocketAddr) -> Result<()> {
        let local_settings = Http2Settings::default();
        let mut conn = Http2Connection {
            io: stream,
            adapter,
            peer_addr,
            local_settings,
            peer_settings: Http2Settings::default(),
            peer_acked_settings: false,
            conn_recv_window: 65_535,
            conn_send_window: 65_535,
            streams: HashMap::new(),
            last_stream_id: 0,
            open_stream_count: 0,
            hpack_decoder: HpackDecoder::new(local_settings.header_table_size as usize),
            hpack_encoder: HpackEncoder::new(Http2Settings::default().header_table_size as usize),
            inbuf: Vec::with_capacity(16 * 1024),
            going_away: false,
        };
        // Bound the decoded header-list size with our advertised limit.
        if conn.local_settings.max_header_list_size != u32::MAX {
            conn.hpack_decoder
                .set_max_header_list_size(conn.local_settings.max_header_list_size as usize);
        }
        conn.run().await
    }

    /// The full connection lifecycle: preface → settings → frame loop → close.
    async fn run(&mut self) -> Result<()> {
        if let Err(e) = self.read_preface().await {
            // A bad preface means this almost certainly is not an HTTP/2 peer;
            // there is nothing useful to GOAWAY *to*. Log and drop.
            tracing::debug!(peer = %self.peer_addr, error = %e, "invalid HTTP/2 preface");
            return Ok(());
        }

        // RFC 9113 §3.4: the server's preface is a SETTINGS frame, sent
        // immediately, before processing any client frame.
        self.send_frame(&Frame::Settings {
            ack: false,
            params: self.local_settings.as_params(),
        })
        .await?;

        // Channel that per-stream adapter tasks use to hand finished responses
        // back to this single-threaded writer loop.
        let (resp_tx, mut resp_rx) = mpsc::unbounded_channel::<StreamResponse>();

        let mut read_chunk = [0u8; 16 * 1024];
        loop {
            // Drain any responses ready *now* before blocking on a read, so a
            // fast adapter does not wait on slow client I/O.
            while let Ok(sr) = resp_rx.try_recv() {
                self.write_response(sr).await?;
            }

            if self.going_away && self.streams.is_empty() {
                break;
            }

            // Try to parse and dispatch as many complete frames as the buffer
            // already holds before touching the socket again.
            match self.process_buffered_frames(&resp_tx).await {
                Ok(()) => {}
                Err(e) => {
                    self.handle_protocol_error(e).await?;
                    if self.going_away {
                        // After GOAWAY, let in-flight streams finish, then stop.
                        continue;
                    }
                }
            }

            // Block until either more bytes arrive or a response is ready.
            tokio::select! {
                read = self.io.read(&mut read_chunk) => {
                    match read {
                        Ok(0) => {
                            tracing::trace!(peer = %self.peer_addr, "HTTP/2 peer closed");
                            break;
                        }
                        Ok(n) => self.inbuf.extend_from_slice(&read_chunk[..n]),
                        Err(e) => return Err(Error::Io(e)),
                    }
                }
                Some(sr) = resp_rx.recv() => {
                    self.write_response(sr).await?;
                }
            }
        }

        // Best-effort graceful GOAWAY if we have not already sent one.
        if !self.going_away {
            let _ = self
                .send_frame(&Frame::GoAway {
                    last_stream_id: self.last_stream_id,
                    error_code: error_codes::NO_ERROR,
                    debug: Bytes::new(),
                })
                .await;
        }
        let _ = self.io.flush().await;
        Ok(())
    }

    /// Read and validate the 24-byte client connection preface
    /// (`PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n`).
    async fn read_preface(&mut self) -> Result<()> {
        let preface = crate::http2::PREFACE;
        while self.inbuf.len() < preface.len() {
            let mut chunk = [0u8; 64];
            let n = self.io.read(&mut chunk).await.map_err(Error::Io)?;
            if n == 0 {
                return Err(Error::protocol("connection closed during HTTP/2 preface"));
            }
            self.inbuf.extend_from_slice(&chunk[..n]);
        }
        if &self.inbuf[..preface.len()] != preface {
            return Err(Error::protocol("malformed HTTP/2 connection preface"));
        }
        // Consume the preface; whatever follows is the first frame(s).
        self.inbuf.drain(..preface.len());
        Ok(())
    }

    /// Parse and dispatch every complete frame currently sitting in `inbuf`.
    ///
    /// Stops when the buffer holds only a partial frame. A protocol error from
    /// any single frame aborts the batch and propagates to the caller.
    async fn process_buffered_frames(
        &mut self,
        resp_tx: &mpsc::UnboundedSender<StreamResponse>,
    ) -> std::result::Result<(), Http2Error> {
        loop {
            let parsed =
                Frame::parse(&self.inbuf, self.local_settings.max_frame_size).map_err(|e| {
                    Http2Error::connection(error_codes::PROTOCOL_ERROR, format!("frame parse: {e}"))
                })?;
            let (frame, consumed) = match parsed {
                Some(pair) => pair,
                None => return Ok(()), // Need more bytes.
            };
            // Defensive: a zero-length consume would loop forever.
            debug_assert!(consumed >= FRAME_HEADER_LEN);
            self.inbuf.drain(..consumed);
            self.dispatch_frame(frame, resp_tx).await?;
        }
    }

    /// Route one parsed [`Frame`] to its handler.
    async fn dispatch_frame(
        &mut self,
        frame: Frame,
        resp_tx: &mpsc::UnboundedSender<StreamResponse>,
    ) -> std::result::Result<(), Http2Error> {
        // Once mid-header-block, RFC 9113 §6.2 allows *only* CONTINUATION for
        // the same stream — any other frame is a connection error.
        if let Some(sid) = self.continuation_expected_on() {
            let ok = matches!(&frame, Frame::Continuation { stream_id, .. } if *stream_id == sid);
            if !ok {
                return Err(Http2Error::connection(
                    error_codes::PROTOCOL_ERROR,
                    "expected CONTINUATION frame",
                ));
            }
        }

        match frame {
            Frame::Settings { ack, params } => self.on_settings(ack, params).await,
            Frame::Ping { ack, payload } => self.on_ping(ack, payload).await,
            Frame::WindowUpdate {
                stream_id,
                increment,
            } => self.on_window_update(stream_id, increment),
            Frame::Headers {
                stream_id,
                block,
                end_stream,
                end_headers,
                priority: _,
            } => {
                self.on_headers(stream_id, block, end_stream, end_headers, resp_tx)
                    .await
            }
            Frame::Continuation {
                stream_id,
                block,
                end_headers,
            } => {
                self.on_continuation(stream_id, block, end_headers, resp_tx)
                    .await
            }
            Frame::Data {
                stream_id,
                data,
                end_stream,
            } => self
                .on_data(stream_id, data, end_stream, resp_tx)
                .await
                .map_err(|e| e.on_stream(stream_id)),
            Frame::RstStream {
                stream_id,
                error_code,
            } => self.on_rst_stream(stream_id, error_code),
            Frame::Priority { .. } => {
                // Accepted and ignored — RFC 9113 §5.3.2 permits this.
                Ok(())
            }
            Frame::GoAway {
                last_stream_id,
                error_code,
                ..
            } => {
                tracing::debug!(
                    peer = %self.peer_addr,
                    last_stream_id,
                    error_code,
                    "received GOAWAY from peer"
                );
                self.going_away = true;
                Ok(())
            }
        }
    }

    /// If a stream is mid-header-block, the id it is awaiting `CONTINUATION` on.
    fn continuation_expected_on(&self) -> Option<u32> {
        self.streams
            .values()
            .find(|s| s.awaiting_continuation)
            .map(|s| s.id)
    }

    // -- SETTINGS -----------------------------------------------------------

    /// Handle an inbound `SETTINGS` frame (or its `ACK`).
    async fn on_settings(
        &mut self,
        ack: bool,
        params: Vec<(u16, u32)>,
    ) -> std::result::Result<(), Http2Error> {
        if ack {
            if !params.is_empty() {
                return Err(Http2Error::connection(
                    error_codes::FRAME_SIZE_ERROR,
                    "SETTINGS ACK must be empty",
                ));
            }
            self.peer_acked_settings = true;
            return Ok(());
        }

        // A change to INITIAL_WINDOW_SIZE retroactively adjusts every open
        // stream's send window by the delta (RFC 9113 §6.9.2).
        let old_initial = self.peer_settings.initial_window_size as i64;
        for (id, value) in &params {
            self.peer_settings.apply(*id, *value)?;
        }
        let new_initial = self.peer_settings.initial_window_size as i64;
        if new_initial != old_initial {
            let delta = new_initial - old_initial;
            for stream in self.streams.values_mut() {
                stream.send_window += delta;
                if stream.send_window > MAX_WINDOW {
                    return Err(Http2Error::connection(
                        error_codes::FLOW_CONTROL_ERROR,
                        "INITIAL_WINDOW_SIZE change overflowed a stream window",
                    ));
                }
            }
        }

        // Acknowledge.
        self.send_frame(&Frame::Settings {
            ack: true,
            params: Vec::new(),
        })
        .await
        .map_err(io_to_h2)?;
        Ok(())
    }

    // -- PING ---------------------------------------------------------------

    /// Handle an inbound `PING`: reply to a non-`ACK` with a matching `ACK`.
    async fn on_ping(
        &mut self,
        ack: bool,
        payload: [u8; 8],
    ) -> std::result::Result<(), Http2Error> {
        if ack {
            // A PING ACK we never solicited; nothing to do but ignore it.
            return Ok(());
        }
        self.send_frame(&Frame::Ping { ack: true, payload })
            .await
            .map_err(io_to_h2)
    }

    // -- WINDOW_UPDATE ------------------------------------------------------

    /// Handle an inbound `WINDOW_UPDATE` for the connection (`stream_id == 0`)
    /// or for a specific stream.
    fn on_window_update(
        &mut self,
        stream_id: u32,
        increment: u32,
    ) -> std::result::Result<(), Http2Error> {
        if increment == 0 {
            // A zero increment is a protocol error (connection- or
            // stream-scoped depending on the target).
            return if stream_id == 0 {
                Err(Http2Error::connection(
                    error_codes::PROTOCOL_ERROR,
                    "WINDOW_UPDATE increment of 0",
                ))
            } else {
                Err(
                    Http2Error::stream(error_codes::PROTOCOL_ERROR, "WINDOW_UPDATE increment of 0")
                        .on_stream(stream_id),
                )
            };
        }
        if stream_id == 0 {
            self.conn_send_window += increment as i64;
            if self.conn_send_window > MAX_WINDOW {
                return Err(Http2Error::connection(
                    error_codes::FLOW_CONTROL_ERROR,
                    "connection send window overflow",
                ));
            }
        } else if let Some(stream) = self.streams.get_mut(&stream_id) {
            stream.send_window += increment as i64;
            if stream.send_window > MAX_WINDOW {
                return Err(Http2Error::stream(
                    error_codes::FLOW_CONTROL_ERROR,
                    "stream send window overflow",
                )
                .on_stream(stream_id));
            }
        }
        // A WINDOW_UPDATE for an unknown (already-closed) stream is ignored.
        Ok(())
    }

    // -- RST_STREAM ---------------------------------------------------------

    /// Handle an inbound `RST_STREAM`: abruptly close the named stream.
    fn on_rst_stream(
        &mut self,
        stream_id: u32,
        error_code: u32,
    ) -> std::result::Result<(), Http2Error> {
        if stream_id == 0 {
            return Err(Http2Error::connection(
                error_codes::PROTOCOL_ERROR,
                "RST_STREAM on stream 0",
            ));
        }
        if let Some(stream) = self.streams.get_mut(&stream_id) {
            tracing::debug!(stream_id, error_code, "stream reset by peer");
            stream.state = StreamState::Closed;
            self.retire_closed_streams();
        }
        Ok(())
    }

    // -- HEADERS / CONTINUATION --------------------------------------------

    /// Handle an inbound `HEADERS` frame: open (or trailer) a stream and begin
    /// (or complete, if `end_headers`) a compressed header block.
    async fn on_headers(
        &mut self,
        stream_id: u32,
        block: Bytes,
        end_stream: bool,
        end_headers: bool,
        resp_tx: &mpsc::UnboundedSender<StreamResponse>,
    ) -> std::result::Result<(), Http2Error> {
        if stream_id == 0 || stream_id % 2 == 0 {
            return Err(Http2Error::connection(
                error_codes::PROTOCOL_ERROR,
                "HEADERS must use an odd, non-zero stream id",
            ));
        }

        let is_new = !self.streams.contains_key(&stream_id);
        if is_new {
            // Stream ids must be strictly increasing (RFC 9113 §5.1.1).
            if stream_id <= self.last_stream_id {
                return Err(Http2Error::connection(
                    error_codes::PROTOCOL_ERROR,
                    "stream id did not increase",
                ));
            }
            self.last_stream_id = stream_id;

            // Enforce MAX_CONCURRENT_STREAMS: refuse, do not kill the
            // connection (RFC 9113 §5.1.2).
            if self.open_stream_count >= self.local_settings.max_concurrent_streams {
                self.send_frame(&Frame::RstStream {
                    stream_id,
                    error_code: error_codes::REFUSED_STREAM,
                })
                .await
                .map_err(io_to_h2)?;
                return Ok(());
            }

            let stream = Stream::new(
                stream_id,
                self.local_settings.initial_window_size,
                self.peer_settings.initial_window_size,
            );
            self.streams.insert(stream_id, stream);
            self.open_stream_count += 1;
        }

        // Advance the state machine for the HEADERS receipt.
        {
            let stream = self.streams.get_mut(&stream_id).expect("just inserted");
            stream.state = stream
                .state
                .on_recv_headers(end_stream)
                .map_err(|e| e.on_stream(stream_id))?;
            stream.headers_end_stream = end_stream;
            stream.header_block.extend_from_slice(&block);
            stream.awaiting_continuation = !end_headers;
            if stream.header_block.len() > HEADER_BLOCK_HARD_CAP {
                return Err(Http2Error::connection(
                    error_codes::PROTOCOL_ERROR,
                    "header block exceeds hard cap",
                ));
            }
        }

        if end_headers {
            self.finish_header_block(stream_id, resp_tx).await?;
        }
        Ok(())
    }

    /// Handle an inbound `CONTINUATION` frame: append to the in-progress block.
    async fn on_continuation(
        &mut self,
        stream_id: u32,
        block: Bytes,
        end_headers: bool,
        resp_tx: &mpsc::UnboundedSender<StreamResponse>,
    ) -> std::result::Result<(), Http2Error> {
        let stream = self.streams.get_mut(&stream_id).ok_or_else(|| {
            Http2Error::connection(
                error_codes::PROTOCOL_ERROR,
                "CONTINUATION for unknown stream",
            )
        })?;
        if !stream.awaiting_continuation {
            return Err(Http2Error::connection(
                error_codes::PROTOCOL_ERROR,
                "unexpected CONTINUATION",
            ));
        }
        stream.header_block.extend_from_slice(&block);
        if stream.header_block.len() > HEADER_BLOCK_HARD_CAP {
            return Err(Http2Error::connection(
                error_codes::PROTOCOL_ERROR,
                "header block exceeds hard cap",
            ));
        }
        if end_headers {
            stream.awaiting_continuation = false;
            self.finish_header_block(stream_id, resp_tx).await?;
        }
        Ok(())
    }

    /// HPACK-decode a now-complete header block, build the [`Request`] skeleton,
    /// and — if the request also carried `END_STREAM` — dispatch it.
    async fn finish_header_block(
        &mut self,
        stream_id: u32,
        resp_tx: &mpsc::UnboundedSender<StreamResponse>,
    ) -> std::result::Result<(), Http2Error> {
        let (block, end_stream) = {
            let stream = self.streams.get_mut(&stream_id).expect("stream exists");
            (
                std::mem::take(&mut stream.header_block),
                stream.headers_end_stream,
            )
        };

        // HPACK decode is connection-stateful: a failure corrupts the decoder
        // for the whole connection, so it is a connection-level error.
        let decoded = self.hpack_decoder.decode(&block).map_err(|e| {
            Http2Error::connection(
                error_codes::COMPRESSION_ERROR,
                format!("HPACK decode failed: {e}"),
            )
        })?;

        {
            let stream = self.streams.get_mut(&stream_id).expect("stream exists");
            // Trailers arrive as a second HEADERS block; we keep the request
            // headers and ignore trailer fields beyond appending them.
            if stream.headers_decoded {
                stream.headers.extend(decoded);
            } else {
                stream.headers = decoded;
                stream.headers_decoded = true;
            }
        }

        if end_stream {
            self.try_dispatch(stream_id, resp_tx)?;
        }
        Ok(())
    }

    // -- DATA ---------------------------------------------------------------

    /// Handle an inbound `DATA` frame: account flow control, append to the
    /// request body, emit `WINDOW_UPDATE`s, and dispatch on `END_STREAM`.
    async fn on_data(
        &mut self,
        stream_id: u32,
        data: Bytes,
        end_stream: bool,
        resp_tx: &mpsc::UnboundedSender<StreamResponse>,
    ) -> std::result::Result<(), Http2Error> {
        if stream_id == 0 {
            return Err(Http2Error::connection(
                error_codes::PROTOCOL_ERROR,
                "DATA on stream 0",
            ));
        }
        let len = data.len() as i64;

        // Connection-level flow control: the peer must never overrun the
        // window we advertised.
        if len > self.conn_recv_window {
            return Err(Http2Error::connection(
                error_codes::FLOW_CONTROL_ERROR,
                "DATA exceeds connection flow-control window",
            ));
        }
        self.conn_recv_window -= len;

        {
            let stream = self.streams.get_mut(&stream_id).ok_or_else(|| {
                Http2Error::stream(error_codes::STREAM_CLOSED, "DATA for unknown stream")
            })?;
            if len > stream.recv_window {
                return Err(Http2Error::stream(
                    error_codes::FLOW_CONTROL_ERROR,
                    "DATA exceeds stream flow-control window",
                ));
            }
            stream.recv_window -= len;
            stream.state = stream.state.on_recv_data(end_stream)?;
            stream.body.extend_from_slice(&data);
        }

        // The server "consumes" received DATA immediately (it is buffered into
        // the request body), so we can replenish both windows right away. This
        // keeps a large upload flowing without stalling.
        if len > 0 {
            self.conn_recv_window += len;
            self.send_frame(&Frame::WindowUpdate {
                stream_id: 0,
                increment: len as u32,
            })
            .await
            .map_err(io_to_h2)?;

            if let Some(stream) = self.streams.get_mut(&stream_id) {
                stream.recv_window += len;
            }
            self.send_frame(&Frame::WindowUpdate {
                stream_id,
                increment: len as u32,
            })
            .await
            .map_err(io_to_h2)?;
        }

        if end_stream {
            self.try_dispatch(stream_id, resp_tx)?;
        }
        Ok(())
    }

    // -- Dispatch -----------------------------------------------------------

    /// Build the [`Request`] for a fully-received stream and spawn a task that
    /// runs the adapter and ships the [`Response`] back over `resp_tx`.
    fn try_dispatch(
        &mut self,
        stream_id: u32,
        resp_tx: &mpsc::UnboundedSender<StreamResponse>,
    ) -> std::result::Result<(), Http2Error> {
        let stream = match self.streams.get_mut(&stream_id) {
            Some(s) => s,
            None => return Ok(()),
        };
        if stream.dispatched {
            return Ok(());
        }
        stream.dispatched = true;

        let request = build_request(
            stream_id,
            &stream.headers,
            std::mem::take(&mut stream.body),
            self.peer_addr,
        )
        .map_err(|e| e.on_stream(stream_id))?;

        let adapter = self.adapter.clone();
        let tx = resp_tx.clone();
        tokio::spawn(async move {
            let response = adapter.service(request).await;
            // If the connection writer is already gone the send simply fails;
            // there is nothing useful to do with the response then.
            let _ = tx.send(StreamResponse {
                stream_id,
                response,
            });
        });
        Ok(())
    }

    // -- Response writing ---------------------------------------------------

    /// Serialize one adapter [`Response`] back onto the wire as `HEADERS` plus
    /// flow-controlled `DATA`, then advance the stream's state.
    async fn write_response(&mut self, sr: StreamResponse) -> Result<()> {
        let StreamResponse {
            stream_id,
            response,
        } = sr;

        // The stream may have been reset by the peer while the adapter ran.
        if !self.streams.contains_key(&stream_id) {
            return Ok(());
        }

        // -- HEADERS ---------------------------------------------------------
        // RFC 9113 §8.3: the `:status` pseudo-header must come first, and only
        // lowercase field names are legal in HTTP/2.
        let mut fields: Vec<(String, String)> = Vec::with_capacity(response.headers.len() + 1);
        fields.push((":status".to_string(), response.status.to_string()));
        for (k, v) in &response.headers {
            let lower = k.to_ascii_lowercase();
            // Connection-specific headers are forbidden in HTTP/2 (§8.2.2).
            if matches!(
                lower.as_str(),
                "connection" | "transfer-encoding" | "keep-alive" | "proxy-connection" | "upgrade"
            ) {
                continue;
            }
            fields.push((lower, v.clone()));
        }
        let block = Bytes::from(self.hpack_encoder.encode(&fields));

        let body = response.body;
        let end_stream_on_headers = body.is_empty();
        self.send_frame(&Frame::Headers {
            stream_id,
            block,
            end_stream: end_stream_on_headers,
            end_headers: true,
            priority: None,
        })
        .await?;

        // -- DATA ------------------------------------------------------------
        if !body.is_empty() {
            self.write_body(stream_id, body).await?;
        }

        // Advance the stream state for our END_STREAM and retire it if closed.
        if let Some(stream) = self.streams.get_mut(&stream_id) {
            stream.state = stream.state.on_send_end_stream();
        }
        self.retire_closed_streams();
        self.io.flush().await.map_err(Error::Io)?;
        Ok(())
    }

    /// Stream a response body out as `DATA` frames, respecting the peer's
    /// `MAX_FRAME_SIZE` and both flow-control windows. Blocks (awaiting
    /// `WINDOW_UPDATE`s read by the frame loop is not possible from here, so we
    /// instead read more frames inline) until the whole body is sent.
    async fn write_body(&mut self, stream_id: u32, body: Bytes) -> Result<()> {
        let max_frame = self.peer_settings.max_frame_size as usize;
        let mut offset = 0usize;

        while offset < body.len() {
            // How much may we send right now? Bounded by: bytes left, the
            // peer's max frame size, the connection window, the stream window.
            let stream_window = self
                .streams
                .get(&stream_id)
                .map(|s| s.send_window)
                .unwrap_or(0);
            if stream_window <= 0 || self.conn_send_window <= 0 {
                // Out of credit — pump the connection until a WINDOW_UPDATE
                // (or anything else) arrives that might replenish it.
                if !self.read_more_for_flow_control().await? {
                    // Peer closed without granting credit; abandon the body.
                    tracing::debug!(stream_id, "peer closed before granting flow-control credit");
                    return Ok(());
                }
                continue;
            }

            let budget = (body.len() - offset)
                .min(max_frame)
                .min(stream_window.max(0) as usize)
                .min(self.conn_send_window.max(0) as usize);
            if budget == 0 {
                continue;
            }

            let chunk = body.slice(offset..offset + budget);
            offset += budget;
            let last = offset >= body.len();

            self.conn_send_window -= budget as i64;
            if let Some(stream) = self.streams.get_mut(&stream_id) {
                stream.send_window -= budget as i64;
            }

            self.send_frame(&Frame::Data {
                stream_id,
                data: chunk,
                end_stream: last,
            })
            .await?;
        }
        Ok(())
    }

    /// Read and process more inbound frames specifically so a blocked
    /// `write_body` can observe `WINDOW_UPDATE`s. Returns `false` if the peer
    /// closed the connection.
    async fn read_more_for_flow_control(&mut self) -> Result<bool> {
        let mut chunk = [0u8; 8192];
        let n = self.io.read(&mut chunk).await.map_err(Error::Io)?;
        if n == 0 {
            return Ok(false);
        }
        self.inbuf.extend_from_slice(&chunk[..n]);

        // Only WINDOW_UPDATE / SETTINGS / PING / RST_STREAM are relevant here;
        // applying them keeps flow control correct. Anything that errors is
        // handled by the outer loop on the next pass, so we tolerate failure.
        loop {
            let parsed = match Frame::parse(&self.inbuf, self.local_settings.max_frame_size) {
                Ok(Some(p)) => p,
                Ok(None) => break,
                Err(_) => break,
            };
            let (frame, consumed) = parsed;
            match &frame {
                Frame::WindowUpdate { .. }
                | Frame::Settings { .. }
                | Frame::Ping { .. }
                | Frame::RstStream { .. } => {
                    self.inbuf.drain(..consumed);
                    // Re-route through the normal handlers (no resp channel
                    // needed — none of these dispatch a request).
                    let (tx, _rx) = mpsc::unbounded_channel();
                    if self.dispatch_frame(frame, &tx).await.is_err() {
                        break;
                    }
                }
                _ => break, // Leave non-flow frames for the main loop.
            }
        }
        Ok(true)
    }

    // -- Errors / housekeeping ---------------------------------------------

    /// Act on a [`Http2Error`]: reset the offending stream, or send `GOAWAY`
    /// and begin winding the connection down.
    async fn handle_protocol_error(&mut self, e: Http2Error) -> Result<()> {
        match e.stream_only {
            Some(stream_id) if stream_id != 0 => {
                tracing::debug!(stream_id, code = e.code, detail = %e.detail, "stream error");
                self.send_frame(&Frame::RstStream {
                    stream_id,
                    error_code: e.code,
                })
                .await?;
                if let Some(stream) = self.streams.get_mut(&stream_id) {
                    stream.state = StreamState::Closed;
                }
                self.retire_closed_streams();
            }
            _ => {
                tracing::debug!(code = e.code, detail = %e.detail, "connection error → GOAWAY");
                self.send_frame(&Frame::GoAway {
                    last_stream_id: self.last_stream_id,
                    error_code: e.code,
                    debug: Bytes::from(e.detail.into_bytes()),
                })
                .await?;
                self.going_away = true;
            }
        }
        Ok(())
    }

    /// Drop every stream that has reached [`StreamState::Closed`], keeping
    /// `open_stream_count` accurate for `MAX_CONCURRENT_STREAMS` accounting.
    fn retire_closed_streams(&mut self) {
        let before = self.streams.len();
        self.streams.retain(|_, s| !s.state.is_closed());
        let removed = (before - self.streams.len()) as u32;
        self.open_stream_count = self.open_stream_count.saturating_sub(removed);
    }

    /// Encode and write one frame, flushing is left to the caller.
    async fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let bytes = frame.encode();
        self.io.write_all(&bytes).await.map_err(Error::Io)?;
        Ok(())
    }
}

/// Map an I/O `Error` surfacing from a frame write into a connection-level
/// [`Http2Error`] so it can flow through the protocol-error machinery.
fn io_to_h2(e: Error) -> Http2Error {
    Http2Error::connection(error_codes::INTERNAL_ERROR, format!("i/o error: {e}"))
}

/// Assemble a [`Request`] from a stream's decoded HPACK header list and body.
///
/// Validates the mandatory HTTP/2 pseudo-headers (`:method`, `:path`,
/// `:scheme`) per RFC 9113 §8.3.1 and rebuilds the conventional header view the
/// rest of the connector expects (e.g. synthesizing a `host` header from
/// `:authority`).
fn build_request(
    _stream_id: u32,
    decoded: &[(String, String)],
    body: Vec<u8>,
    peer_addr: SocketAddr,
) -> std::result::Result<Request, Http2Error> {
    let mut method: Option<String> = None;
    let mut path: Option<String> = None;
    let mut scheme: Option<String> = None;
    let mut authority: Option<String> = None;
    let mut headers: Vec<(String, String)> = Vec::new();

    let mut seen_regular = false;
    for (name, value) in decoded {
        if let Some(pseudo) = name.strip_prefix(':') {
            // All pseudo-headers must precede regular fields (§8.3).
            if seen_regular {
                return Err(Http2Error::stream(
                    error_codes::PROTOCOL_ERROR,
                    "pseudo-header after regular header",
                ));
            }
            match pseudo {
                "method" => method = Some(value.clone()),
                "path" => path = Some(value.clone()),
                "scheme" => scheme = Some(value.clone()),
                "authority" => authority = Some(value.clone()),
                other => {
                    return Err(Http2Error::stream(
                        error_codes::PROTOCOL_ERROR,
                        format!("unknown pseudo-header :{other}"),
                    ));
                }
            }
        } else {
            seen_regular = true;
            // Field names must be lowercase in HTTP/2 (§8.2.1).
            if name.bytes().any(|b| b.is_ascii_uppercase()) {
                return Err(Http2Error::stream(
                    error_codes::PROTOCOL_ERROR,
                    "uppercase header field name",
                ));
            }
            // Connection-specific headers are forbidden (§8.2.2).
            if matches!(
                name.as_str(),
                "connection" | "keep-alive" | "proxy-connection" | "transfer-encoding" | "upgrade"
            ) {
                return Err(Http2Error::stream(
                    error_codes::PROTOCOL_ERROR,
                    "connection-specific header in HTTP/2",
                ));
            }
            headers.push((name.clone(), value.clone()));
        }
    }

    let method = method.ok_or_else(|| {
        Http2Error::stream(error_codes::PROTOCOL_ERROR, "missing :method pseudo-header")
    })?;
    let raw_path = path.ok_or_else(|| {
        Http2Error::stream(error_codes::PROTOCOL_ERROR, "missing :path pseudo-header")
    })?;
    // :scheme is mandatory for non-CONNECT requests; default sensibly anyway.
    let _scheme = scheme.unwrap_or_else(|| "https".to_string());

    if raw_path.is_empty() {
        return Err(Http2Error::stream(
            error_codes::PROTOCOL_ERROR,
            ":path must not be empty",
        ));
    }

    // Surface :authority as a conventional `host` header so the downstream
    // host-routing logic (built for HTTP/1.1) keeps working unchanged.
    if let Some(auth) = authority {
        if !headers.iter().any(|(k, _)| k == "host") {
            headers.insert(0, ("host".to_string(), auth));
        }
    }

    // Normalize the :path into a clean path + query, exactly as HTTP/1.1 does.
    let normalized = normalize_target(&raw_path).map_err(|e| {
        Http2Error::stream(error_codes::PROTOCOL_ERROR, format!("invalid :path: {e}"))
    })?;

    Ok(Request {
        method,
        uri: raw_path,
        path: normalized.path,
        query: normalized.query,
        version: "HTTP/2.0".to_string(),
        headers,
        body: Bytes::from(body),
        peer_addr,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Settings -----------------------------------------------------------

    #[test]
    fn default_settings_match_rfc_9113() {
        let s = Http2Settings::default();
        assert_eq!(s.header_table_size, 4096);
        assert_eq!(s.initial_window_size, 65_535);
        assert_eq!(s.max_frame_size, 16_384);
        assert_eq!(s.max_header_list_size, u32::MAX);
    }

    #[test]
    fn settings_apply_rejects_bad_max_frame_size() {
        let mut s = Http2Settings::default();
        // Below the 2^14 minimum.
        assert!(s.apply(settings_ids::MAX_FRAME_SIZE, 1024).is_err());
        // Above the 2^24-1 maximum.
        assert!(s.apply(settings_ids::MAX_FRAME_SIZE, 1 << 25).is_err());
        // A valid value sticks.
        assert!(s.apply(settings_ids::MAX_FRAME_SIZE, 32_768).is_ok());
        assert_eq!(s.max_frame_size, 32_768);
    }

    #[test]
    fn settings_apply_ignores_unknown_ids() {
        let mut s = Http2Settings::default();
        let before = s;
        assert!(s.apply(0xABCD, 12345).is_ok());
        assert_eq!(s, before);
    }

    // -- Stream state transitions ------------------------------------------

    #[test]
    fn stream_state_idle_to_open_on_headers() {
        let s = StreamState::Idle;
        assert_eq!(s.on_recv_headers(false).unwrap(), StreamState::Open);
    }

    #[test]
    fn stream_state_idle_to_half_closed_remote_on_headers_end_stream() {
        let s = StreamState::Idle;
        assert_eq!(
            s.on_recv_headers(true).unwrap(),
            StreamState::HalfClosedRemote
        );
    }

    #[test]
    fn stream_state_headers_illegal_when_closed() {
        assert!(StreamState::Closed.on_recv_headers(false).is_err());
    }

    #[test]
    fn stream_state_data_transitions() {
        // Open + DATA(end) → HalfClosedRemote.
        assert_eq!(
            StreamState::Open.on_recv_data(true).unwrap(),
            StreamState::HalfClosedRemote
        );
        // Open + DATA(no end) → Open.
        assert_eq!(
            StreamState::Open.on_recv_data(false).unwrap(),
            StreamState::Open
        );
        // HalfClosedLocal + DATA(end) → Closed.
        assert_eq!(
            StreamState::HalfClosedLocal.on_recv_data(true).unwrap(),
            StreamState::Closed
        );
        // DATA on an Idle stream is illegal.
        assert!(StreamState::Idle.on_recv_data(false).is_err());
    }

    #[test]
    fn stream_state_send_end_stream_transitions() {
        assert_eq!(
            StreamState::Open.on_send_end_stream(),
            StreamState::HalfClosedLocal
        );
        assert_eq!(
            StreamState::HalfClosedRemote.on_send_end_stream(),
            StreamState::Closed
        );
        // Idempotent on terminal states.
        assert_eq!(
            StreamState::Closed.on_send_end_stream(),
            StreamState::Closed
        );
    }

    #[test]
    fn full_request_response_state_cycle() {
        // A no-body request: HEADERS(end_stream) then the server's response
        // with END_STREAM walks the stream Idle → HalfClosedRemote → Closed.
        let s = StreamState::Idle;
        let s = s.on_recv_headers(true).unwrap();
        assert_eq!(s, StreamState::HalfClosedRemote);
        let s = s.on_send_end_stream();
        assert_eq!(s, StreamState::Closed);
        assert!(s.is_closed());
    }

    // -- Flow-control window accounting ------------------------------------

    #[test]
    fn flow_control_window_accounting() {
        // A new stream's windows are seeded from the two initial-window
        // settings (local for recv, peer for send).
        let mut stream = Stream::new(1, 65_535, 100_000);
        assert_eq!(stream.recv_window, 65_535);
        assert_eq!(stream.send_window, 100_000);

        // Receiving 10_000 bytes of DATA spends the recv window...
        stream.recv_window -= 10_000;
        assert_eq!(stream.recv_window, 55_535);
        // ...and the server replenishes it after consuming the bytes.
        stream.recv_window += 10_000;
        assert_eq!(stream.recv_window, 65_535);

        // Sending response DATA spends the send window...
        stream.send_window -= 40_000;
        assert_eq!(stream.send_window, 60_000);
        // ...and a peer WINDOW_UPDATE replenishes it.
        stream.send_window += 25_000;
        assert_eq!(stream.send_window, 85_000);

        // The window must never be allowed past 2^31-1.
        stream.send_window = MAX_WINDOW;
        assert!(stream.send_window + 1 > MAX_WINDOW);
    }

    #[test]
    fn settings_initial_window_change_adjusts_open_streams() {
        // Simulates the RFC 9113 §6.9.2 retroactive adjustment: when the peer
        // changes INITIAL_WINDOW_SIZE, every open stream's send window moves by
        // the same signed delta.
        let mut stream = Stream::new(1, 65_535, 65_535);
        stream.send_window -= 5_000; // some response data already sent
        assert_eq!(stream.send_window, 60_535);

        let old_initial = 65_535i64;
        let new_initial = 32_768i64;
        let delta = new_initial - old_initial;
        stream.send_window += delta;
        assert_eq!(stream.send_window, 60_535 - 32_767);
    }

    // -- build_request ------------------------------------------------------

    #[test]
    fn build_request_maps_pseudo_headers() {
        let decoded = vec![
            (":method".to_string(), "GET".to_string()),
            (":scheme".to_string(), "https".to_string()),
            (":authority".to_string(), "example.com".to_string()),
            (":path".to_string(), "/app/x?q=1".to_string()),
            ("accept".to_string(), "*/*".to_string()),
        ];
        let req = build_request(1, &decoded, Vec::new(), "127.0.0.1:1".parse().unwrap())
            .expect("valid request");
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/app/x");
        assert_eq!(req.query.as_deref(), Some("q=1"));
        assert_eq!(req.version, "HTTP/2.0");
        assert_eq!(req.header("host"), Some("example.com"));
        assert_eq!(req.header("accept"), Some("*/*"));
    }

    #[test]
    fn build_request_rejects_missing_method() {
        let decoded = vec![(":path".to_string(), "/".to_string())];
        assert!(build_request(1, &decoded, Vec::new(), "127.0.0.1:1".parse().unwrap()).is_err());
    }

    #[test]
    fn build_request_rejects_pseudo_after_regular() {
        let decoded = vec![
            (":method".to_string(), "GET".to_string()),
            ("accept".to_string(), "*/*".to_string()),
            (":path".to_string(), "/".to_string()),
        ];
        assert!(build_request(1, &decoded, Vec::new(), "127.0.0.1:1".parse().unwrap()).is_err());
    }

    #[test]
    fn build_request_rejects_uppercase_header() {
        let decoded = vec![
            (":method".to_string(), "GET".to_string()),
            (":path".to_string(), "/".to_string()),
            ("Accept".to_string(), "*/*".to_string()),
        ];
        assert!(build_request(1, &decoded, Vec::new(), "127.0.0.1:1".parse().unwrap()).is_err());
    }

    // -- Integration: a full GET / over an in-memory duplex stream ----------
    //
    // This exercises the whole connection: preface validation, SETTINGS
    // exchange, HEADERS decode, adapter dispatch, and response serialization.
    // It depends on the final `http2.rs` frame layer and `hpack.rs`; if those
    // sibling modules expose the documented `Frame` / HPACK APIs the test runs
    // end-to-end. It is written against that documented contract.

    /// A trivial adapter that echoes the request path in a small body.
    struct EchoAdapter;

    #[async_trait::async_trait]
    impl Adapter for EchoAdapter {
        async fn service(&self, req: Request) -> Response {
            Response::with_body(200, format!("path={}", req.path))
        }
    }

    #[tokio::test]
    async fn http2_serves_a_get_over_duplex() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let peer: SocketAddr = "127.0.0.1:55555".parse().unwrap();

        let server_task = tokio::spawn(async move {
            Http2Connection::serve(server, Arc::new(EchoAdapter), peer).await
        });

        // --- client: send preface + SETTINGS + HEADERS(GET /) --------------
        client
            .write_all(crate::http2::PREFACE)
            .await
            .expect("write preface");
        client
            .write_all(
                &Frame::Settings {
                    ack: false,
                    params: Vec::new(),
                }
                .encode(),
            )
            .await
            .expect("write settings");

        // Encode the request pseudo-headers with HPACK.
        let mut enc = HpackEncoder::new(4096);
        let block = enc.encode(&[
            (":method".to_string(), "GET".to_string()),
            (":scheme".to_string(), "http".to_string()),
            (":authority".to_string(), "localhost".to_string()),
            (":path".to_string(), "/".to_string()),
        ]);
        client
            .write_all(
                &Frame::Headers {
                    stream_id: 1,
                    block: Bytes::from(block),
                    end_stream: true,
                    end_headers: true,
                    priority: None,
                }
                .encode(),
            )
            .await
            .expect("write headers");

        // --- client: read and decode the server's frames ------------------
        // Expect: server SETTINGS, SETTINGS ACK, then HEADERS + DATA for
        // stream 1. We read until we see a HEADERS frame carrying `:status`.
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let mut dec = HpackDecoder::new(4096);
        let mut saw_status_200 = false;
        let mut saw_body = false;

        // Bounded read loop so a contract mismatch fails fast instead of hanging.
        for _ in 0..64 {
            let n = tokio::time::timeout(std::time::Duration::from_secs(2), client.read(&mut tmp))
                .await
                .expect("server should respond before timeout")
                .expect("read");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);

            while let Some((frame, consumed)) =
                Frame::parse(&buf, 16_384).expect("server frames must parse")
            {
                buf.drain(..consumed);
                match frame {
                    Frame::Headers { block, .. } => {
                        let fields = dec.decode(&block).expect("decode response headers");
                        if fields.iter().any(|(k, v)| k == ":status" && v == "200") {
                            saw_status_200 = true;
                        }
                    }
                    Frame::Data { data, .. } => {
                        if data.as_ref() == b"path=/" {
                            saw_body = true;
                        }
                    }
                    _ => {}
                }
            }
            if saw_status_200 && saw_body {
                break;
            }
        }

        assert!(saw_status_200, "expected a HEADERS frame with :status 200");
        assert!(saw_body, "expected a DATA frame carrying the echoed body");

        drop(client);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server_task).await;
    }
}
