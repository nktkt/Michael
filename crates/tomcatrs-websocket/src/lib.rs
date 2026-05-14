//! `tomcatrs-websocket` — the RFC 6455 WebSocket transport for the
//! **Tomcat-RS Compatibility Runtime**.
//!
//! This crate owns the *wire-level* concerns of WebSocket connections:
//!
//! * [`handshake`] — the HTTP/1.1 `Upgrade` handshake defined by
//!   [RFC 6455 §4](https://datatracker.ietf.org/doc/html/rfc6455#section-4),
//!   including computation of the `Sec-WebSocket-Accept` response header.
//! * [`frame`] — the binary framing protocol from
//!   [RFC 6455 §5](https://datatracker.ietf.org/doc/html/rfc6455#section-5):
//!   parsing client frames (with unmasking) and encoding server frames.
//! * [`transport`] — the connection layer built on the codec: message
//!   reassembly, control-frame handling, the close handshake, the connection
//!   state machine, and `permessage-deflate` extension negotiation
//!   ([RFC 6455 §5.4–§7](https://datatracker.ietf.org/doc/html/rfc6455#section-5.4),
//!   [RFC 7692](https://datatracker.ietf.org/doc/html/rfc7692)).
//! * [`jakarta_bridge`] — a thin, documented scaffold describing how the Rust
//!   transport hands decoded messages to the JVM-side Jakarta WebSocket
//!   implementation (`@ServerEndpoint`, `Session`, `MessageHandler`, …).
//!
//! The split mirrors Tomcat's own architecture: the connector handles bytes,
//! while the container/application layer handles endpoint semantics. Here the
//! "container" is the JVM, reached through [`jakarta_bridge`].

pub mod frame;
pub mod handshake;
pub mod jakarta_bridge;
pub mod transport;

pub use frame::{Frame, Opcode};
pub use handshake::{accept_key, handshake_response, HandshakeResponse, WEBSOCKET_GUID};
pub use jakarta_bridge::{WebSocketEndpoint, WebSocketEndpointRef, WebSocketEvent};
pub use transport::{
    close_code, negotiate_permessage_deflate, negotiate_permessage_deflate_from_offers,
    parse_extension_offers, CloseFrame, CompressionPolicy, DeflateNegotiation, ExtensionOffer,
    Message, PerMessageDeflateParams, WebSocketConfig, WebSocketConnection, WebSocketState,
};

/// The WebSocket protocol version implemented by this crate, as carried in the
/// `Sec-WebSocket-Version` header. RFC 6455 fixes this at `13`.
pub const WEBSOCKET_VERSION: u8 = 13;
