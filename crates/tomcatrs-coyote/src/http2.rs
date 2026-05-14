//! HTTP/2 connector — **scaffold only** (not implemented in v0.1.0).
//!
//! This module documents the shape of the eventual HTTP/2 implementation so
//! that dependent crates can compile against stable type names, but every entry
//! point currently returns [`tomcatrs_core::Error::Protocol`].
//!
//! # Planned design
//!
//! A real implementation will:
//!
//! 1. Negotiate `h2` via ALPN during the TLS handshake (see [`crate::tls`]), or
//!    accept the HTTP/1.1 `Upgrade: h2c` prior-knowledge path (see
//!    [`crate::upgrade`]).
//! 2. Exchange `SETTINGS` frames and run the connection-level flow-control
//!    window.
//! 3. Decode `HEADERS` frames with an HPACK decoder into a [`crate::Request`],
//!    multiplexing concurrent streams onto independent tokio tasks.
//! 4. Re-encode each [`crate::Response`] as `HEADERS` + `DATA` frames, honoring
//!    per-stream flow control.
//!
//! The intent is to layer this on the `h2` crate rather than hand-rolling frame
//! parsing.

use tomcatrs_core::Error;

/// Connection-level HTTP/2 settings, as carried in a `SETTINGS` frame.
///
/// Present so the configuration and tuning surfaces can be designed ahead of
/// the implementation. Values are the RFC 9113 defaults.
#[derive(Debug, Clone)]
pub struct Http2Settings {
    /// `SETTINGS_HEADER_TABLE_SIZE` — HPACK dynamic table size, in bytes.
    pub header_table_size: u32,
    /// `SETTINGS_MAX_CONCURRENT_STREAMS` — server-imposed stream cap.
    pub max_concurrent_streams: u32,
    /// `SETTINGS_INITIAL_WINDOW_SIZE` — per-stream flow-control window.
    pub initial_window_size: u32,
    /// `SETTINGS_MAX_FRAME_SIZE` — largest frame payload we will accept.
    pub max_frame_size: u32,
}

impl Default for Http2Settings {
    fn default() -> Self {
        Http2Settings {
            header_table_size: 4096,
            max_concurrent_streams: 128,
            initial_window_size: 65_535,
            max_frame_size: 16_384,
        }
    }
}

/// Placeholder for the future HTTP/2 connection driver.
///
/// Construction is intentionally unavailable until the protocol is implemented.
#[derive(Debug)]
pub struct Http2Connection {
    _settings: Http2Settings,
}

/// The canonical "HTTP/2 is not available yet" error.
pub fn unsupported() -> Error {
    Error::protocol("HTTP/2 not implemented in v0.1.0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_is_a_protocol_error() {
        assert!(matches!(unsupported(), Error::Protocol(_)));
    }

    #[test]
    fn default_settings_match_rfc_9113() {
        let s = Http2Settings::default();
        assert_eq!(s.header_table_size, 4096);
        assert_eq!(s.initial_window_size, 65_535);
        assert_eq!(s.max_frame_size, 16_384);
    }
}
