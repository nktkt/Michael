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

use crate::transport::{CloseFrame, Message};

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

    /// Build the [`WebSocketEvent::OnOpen`] event for a freshly opened
    /// connection dispatched to `endpoint`.
    pub fn on_open(endpoint: WebSocketEndpointRef) -> Self {
        WebSocketEvent::OnOpen { endpoint }
    }

    /// Translate a transport-level [`Message`] into the matching lifecycle
    /// event for `endpoint`, or `None` for messages the container does not
    /// observe directly.
    ///
    /// The mapping is:
    ///
    /// * [`Message::Text`] / [`Message::Binary`] → [`WebSocketEvent::OnMessage`]
    /// * [`Message::Close`] → [`WebSocketEvent::OnClose`]
    /// * [`Message::Ping`] / [`Message::Pong`] → `None` — the transport answers
    ///   pings itself and tracks pongs for liveness; the Jakarta layer has no
    ///   `@OnPing` callback, so these never cross the bridge.
    pub fn from_message(endpoint: WebSocketEndpointRef, message: &Message) -> Option<Self> {
        match message {
            Message::Text(text) => Some(WebSocketEvent::OnMessage {
                endpoint,
                is_text: true,
                payload: Bytes::from(text.clone().into_bytes()),
            }),
            Message::Binary(data) => Some(WebSocketEvent::OnMessage {
                endpoint,
                is_text: false,
                payload: data.clone(),
            }),
            Message::Close(close) => Some(WebSocketEvent::on_close(endpoint, close.as_ref())),
            Message::Ping(_) | Message::Pong(_) => None,
        }
    }

    /// Build the [`WebSocketEvent::OnClose`] event from an optional
    /// [`CloseFrame`].
    pub fn on_close(endpoint: WebSocketEndpointRef, close: Option<&CloseFrame>) -> Self {
        WebSocketEvent::OnClose {
            endpoint,
            code: close.map(|c| c.code),
            reason: close.map(|c| c.reason.clone()),
        }
    }

    /// Build the [`WebSocketEvent::OnError`] event for a transport failure.
    pub fn on_error(endpoint: WebSocketEndpointRef, message: impl Into<String>) -> Self {
        WebSocketEvent::OnError {
            endpoint,
            message: message.into(),
        }
    }
}

/// The container-side contract a Jakarta WebSocket implementation fulfils.
///
/// The Rust transport ([`crate::transport::WebSocketConnection`]) drives a
/// connection and, at each lifecycle point, calls the corresponding method on a
/// `WebSocketEndpoint`. The production implementation forwards every call across
/// the JNI bridge to the JVM, where it becomes an invocation of an
/// `@OnOpen`/`@OnMessage`/`@OnClose`/`@OnError` method (or the equivalent
/// programmatic `Endpoint` / `MessageHandler`). Tests, meanwhile, can implement
/// this trait in pure Rust to assert on the event stream without a JVM.
///
/// All methods take `&self`: an endpoint is shared and must be cheaply callable
/// from the connection task. Implementations that need interior mutability
/// should use their own synchronization.
pub trait WebSocketEndpoint: Send + Sync {
    /// Invoked once, after the opening handshake, before any message.
    fn on_open(&self, endpoint: WebSocketEndpointRef);

    /// Invoked for each fully-reassembled application message. `is_text`
    /// distinguishes a UTF-8 text message from a binary one; `payload` is the
    /// complete, unmasked message body.
    fn on_message(&self, endpoint: WebSocketEndpointRef, is_text: bool, payload: Bytes);

    /// Invoked once when the connection closes, carrying the peer's close code
    /// and reason if it sent them.
    fn on_close(&self, endpoint: WebSocketEndpointRef, code: Option<u16>, reason: Option<String>);

    /// Invoked when a transport-level error terminates the connection.
    fn on_error(&self, endpoint: WebSocketEndpointRef, message: String);

    /// Dispatch a pre-built [`WebSocketEvent`] to the matching callback.
    ///
    /// This is the single entry point the transport uses; it fans the event out
    /// to [`WebSocketEndpoint::on_open`] / `on_message` / `on_close` /
    /// `on_error`. Implementors normally do not override it.
    fn dispatch(&self, event: WebSocketEvent) {
        match event {
            WebSocketEvent::OnOpen { endpoint } => self.on_open(endpoint),
            WebSocketEvent::OnMessage {
                endpoint,
                is_text,
                payload,
            } => self.on_message(endpoint, is_text, payload),
            WebSocketEvent::OnClose {
                endpoint,
                code,
                reason,
            } => self.on_close(endpoint, code, reason),
            WebSocketEvent::OnError { endpoint, message } => self.on_error(endpoint, message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

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

    #[test]
    fn message_maps_to_lifecycle_events() {
        let ep = WebSocketEndpointRef::new(1, 2);

        let text = WebSocketEvent::from_message(ep, &Message::Text("hi".into())).unwrap();
        assert_eq!(
            text,
            WebSocketEvent::OnMessage {
                endpoint: ep,
                is_text: true,
                payload: Bytes::from_static(b"hi"),
            }
        );

        let close =
            WebSocketEvent::from_message(ep, &Message::Close(Some(CloseFrame::new(1000, "bye"))))
                .unwrap();
        assert_eq!(
            close,
            WebSocketEvent::OnClose {
                endpoint: ep,
                code: Some(1000),
                reason: Some("bye".to_string()),
            }
        );

        // Ping/Pong are handled by the transport and never cross the bridge.
        assert!(WebSocketEvent::from_message(ep, &Message::Ping(Bytes::new())).is_none());
        assert!(WebSocketEvent::from_message(ep, &Message::Pong(Bytes::new())).is_none());
    }

    #[test]
    fn endpoint_trait_dispatch_fans_out() {
        #[derive(Default)]
        struct Recorder {
            log: Mutex<Vec<String>>,
        }
        impl WebSocketEndpoint for Recorder {
            fn on_open(&self, _ep: WebSocketEndpointRef) {
                self.log.lock().unwrap().push("open".into());
            }
            fn on_message(&self, _ep: WebSocketEndpointRef, is_text: bool, _p: Bytes) {
                self.log.lock().unwrap().push(format!("message:{is_text}"));
            }
            fn on_close(&self, _ep: WebSocketEndpointRef, code: Option<u16>, _r: Option<String>) {
                self.log.lock().unwrap().push(format!("close:{code:?}"));
            }
            fn on_error(&self, _ep: WebSocketEndpointRef, msg: String) {
                self.log.lock().unwrap().push(format!("error:{msg}"));
            }
        }

        let ep = WebSocketEndpointRef::new(3, 4);
        let rec = Recorder::default();
        rec.dispatch(WebSocketEvent::on_open(ep));
        rec.dispatch(WebSocketEvent::from_message(ep, &Message::Text("x".into())).unwrap());
        rec.dispatch(WebSocketEvent::on_close(
            ep,
            Some(&CloseFrame::new(1001, "gone")),
        ));
        rec.dispatch(WebSocketEvent::on_error(ep, "boom"));

        let log = rec.log.lock().unwrap();
        assert_eq!(
            *log,
            vec![
                "open".to_string(),
                "message:true".to_string(),
                "close:Some(1001)".to_string(),
                "error:boom".to_string(),
            ]
        );
    }
}
