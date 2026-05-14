//! Scaffold: the boundary between the Rust WebSocket transport and the
//! JVM-side **Jakarta WebSocket** (`jakarta.websocket`) implementation.
//!
//! # Division of responsibility
//!
//! The Tomcat-RS rewrite is incremental. For WebSocket, the line is drawn at
//! the *transport vs. container* boundary:
//!
//! | Concern                                   | Owner            |
//! |-------------------------------------------|------------------|
//! | TCP/TLS, HTTP upgrade, frame codec        | **Rust** (this crate) |
//! | `@ServerEndpoint` discovery & lifecycle   | JVM (Jakarta)    |
//! | `Session` / `RemoteEndpoint` objects      | JVM (Jakarta)    |
//! | `MessageHandler` dispatch, encoders/decoders | JVM (Jakarta) |
//!
//! In other words: Rust turns bytes into [`crate::Frame`]s and assembles them
//! into whole messages; the JVM turns messages into method calls on annotated
//! endpoint classes. The two halves communicate over the JNI bridge using the
//! small, JNI-friendly types defined here.
//!
//! Nothing in this module performs JNI calls yet — that wiring is future work,
//! tracked alongside the `tomcatrs-servlet-bridge` crate. The types exist so
//! the transport layer can be built and tested against a stable shape.

use bytes::Bytes;

/// An opaque handle to a Jakarta WebSocket endpoint instance living on the JVM.
///
/// On the Java side this corresponds to the container-managed object behind a
/// `@ServerEndpoint`-annotated class (or a programmatic `Endpoint` subclass)
/// together with its associated `jakarta.websocket.Session`. The Rust transport
/// never dereferences the handle; it only passes it back across the bridge so
/// the JVM can route an event to the correct endpoint/session pair.
///
/// The numeric fields mirror the JNI registry indices the bridge will use; they
/// are deliberately plain integers so the struct is trivially copyable across
/// the FFI boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WebSocketEndpointRef {
    /// Identifier of the deployed endpoint *class* (the `@ServerEndpoint`),
    /// assigned by the JVM-side registry at deployment time.
    pub endpoint_id: u64,
    /// Identifier of the live `Session` for one connected peer.
    pub session_id: u64,
}

impl WebSocketEndpointRef {
    /// Construct a reference from its JVM-assigned identifiers.
    pub fn new(endpoint_id: u64, session_id: u64) -> Self {
        Self {
            endpoint_id,
            session_id,
        }
    }
}

/// A transport-level event the Rust side raises for the JVM container.
///
/// Each variant maps onto a Jakarta WebSocket lifecycle callback:
///
/// * [`WebSocketEvent::OnOpen`]   → `@OnOpen`    / `Endpoint.onOpen`
/// * [`WebSocketEvent::OnMessage`] → `@OnMessage` / a registered `MessageHandler`
/// * [`WebSocketEvent::OnClose`]  → `@OnClose`   / `Endpoint.onClose`
/// * [`WebSocketEvent::OnError`]  → `@OnError`   / `Endpoint.onError`
///
/// Conversely, the JVM's `RemoteEndpoint.sendText` / `sendBinary` calls come
/// back the other way and are encoded by [`crate::Frame::encode`]; that
/// outbound path is not modelled as an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebSocketEvent {
    /// The handshake completed and the connection is open.
    OnOpen {
        /// The endpoint/session this connection was dispatched to.
        endpoint: WebSocketEndpointRef,
    },
    /// A complete application message arrived (after de-fragmentation).
    OnMessage {
        /// The endpoint/session that received the message.
        endpoint: WebSocketEndpointRef,
        /// `true` if the payload is UTF-8 text, `false` if binary.
        is_text: bool,
        /// The fully assembled, unmasked message payload.
        payload: Bytes,
    },
    /// The connection closed (clean or otherwise).
    OnClose {
        /// The endpoint/session that closed.
        endpoint: WebSocketEndpointRef,
        /// The RFC 6455 §7.4 close code, if one was received.
        code: Option<u16>,
        /// The optional UTF-8 close reason.
        reason: Option<String>,
    },
    /// A transport-level error occurred on the connection.
    OnError {
        /// The endpoint/session the error is associated with.
        endpoint: WebSocketEndpointRef,
        /// A human-readable description of the failure.
        message: String,
    },
}

impl WebSocketEvent {
    /// The endpoint reference this event is addressed to.
    pub fn endpoint(&self) -> WebSocketEndpointRef {
        match self {
            WebSocketEvent::OnOpen { endpoint }
            | WebSocketEvent::OnMessage { endpoint, .. }
            | WebSocketEvent::OnClose { endpoint, .. }
            | WebSocketEvent::OnError { endpoint, .. } => *endpoint,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_exposes_its_endpoint() {
        let ep = WebSocketEndpointRef::new(7, 42);
        let evt = WebSocketEvent::OnOpen { endpoint: ep };
        assert_eq!(evt.endpoint(), ep);

        let msg = WebSocketEvent::OnMessage {
            endpoint: ep,
            is_text: true,
            payload: Bytes::from_static(b"hi"),
        };
        assert_eq!(msg.endpoint(), ep);
    }
}
