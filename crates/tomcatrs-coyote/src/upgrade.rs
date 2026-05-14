//! Protocol upgrade / WebSocket handoff — **scaffold only** (not implemented in
//! v0.1.0).
//!
//! HTTP/1.1 defines the `Upgrade` mechanism (RFC 9110 §7.8) that lets a
//! connection switch protocols mid-stream. Tomcat uses it for two things this
//! module will eventually support:
//!
//! * **WebSocket** (`Upgrade: websocket`) — handing the raw connection to the
//!   `tomcatrs-websocket` crate after the `101 Switching Protocols` response.
//! * **HTTP/2 cleartext** (`Upgrade: h2c`) — handing the connection to
//!   [`crate::http2`].
//!
//! # Planned design
//!
//! [`detect_upgrade`] already inspects a parsed [`crate::Request`] and reports
//! the requested target protocol. The future handoff path will:
//!
//! 1. Validate the upgrade-specific headers (for WebSocket: `Sec-WebSocket-Key`,
//!    `Sec-WebSocket-Version`).
//! 2. Write the `101 Switching Protocols` response.
//! 3. Surrender the still-open byte stream to the target subsystem instead of
//!    looping back into the HTTP/1.1 keep-alive reader.

use crate::Request;

/// The protocol a client asked to upgrade to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeTarget {
    /// `Upgrade: websocket` — a WebSocket handshake.
    WebSocket,
    /// `Upgrade: h2c` — HTTP/2 over cleartext.
    H2c,
    /// An `Upgrade` token the connector does not recognize.
    Unknown(String),
}

/// Inspect a request for a `Connection: upgrade` + `Upgrade:` header pair and
/// report the requested [`UpgradeTarget`].
///
/// Returns `None` when the request is an ordinary one with no upgrade intent.
/// This function is real and tested; the *handoff* it would feed is the part
/// still scaffolded.
pub fn detect_upgrade(req: &Request) -> Option<UpgradeTarget> {
    let connection = req.header("connection")?;
    let mentions_upgrade = connection
        .split(',')
        .any(|tok| tok.trim().eq_ignore_ascii_case("upgrade"));
    if !mentions_upgrade {
        return None;
    }
    let upgrade = req.header("upgrade")?.trim();
    Some(match upgrade.to_ascii_lowercase().as_str() {
        "websocket" => UpgradeTarget::WebSocket,
        "h2c" => UpgradeTarget::H2c,
        _ => UpgradeTarget::Unknown(upgrade.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn req_with(headers: Vec<(&str, &str)>) -> Request {
        Request {
            method: "GET".into(),
            uri: "/chat".into(),
            path: "/chat".into(),
            query: None,
            version: "HTTP/1.1".into(),
            headers: headers
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: Bytes::new(),
            peer_addr: "127.0.0.1:0".parse().unwrap(),
        }
    }

    #[test]
    fn detects_websocket_upgrade() {
        let req = req_with(vec![("Connection", "Upgrade"), ("Upgrade", "websocket")]);
        assert_eq!(detect_upgrade(&req), Some(UpgradeTarget::WebSocket));
    }

    #[test]
    fn detects_h2c_upgrade() {
        let req = req_with(vec![
            ("Connection", "keep-alive, Upgrade"),
            ("Upgrade", "h2c"),
        ]);
        assert_eq!(detect_upgrade(&req), Some(UpgradeTarget::H2c));
    }

    #[test]
    fn no_upgrade_header_yields_none() {
        let req = req_with(vec![("Connection", "keep-alive")]);
        assert_eq!(detect_upgrade(&req), None);
    }

    #[test]
    fn unknown_upgrade_token_is_reported() {
        let req = req_with(vec![("Connection", "Upgrade"), ("Upgrade", "smtp")]);
        assert_eq!(
            detect_upgrade(&req),
            Some(UpgradeTarget::Unknown("smtp".to_string()))
        );
    }
}
