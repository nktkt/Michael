//! AJP (Apache JServ Protocol) connector — **scaffold only** (not implemented
//! in v0.1.0).
//!
//! AJP is the binary protocol Tomcat speaks to a fronting `httpd`/`nginx` via
//! `mod_jk` / `mod_proxy_ajp`. This module reserves the type names; every entry
//! point currently returns [`tomcatrs_core::Error::Protocol`].
//!
//! # Planned design
//!
//! A real implementation will:
//!
//! 1. Read AJP packets framed as `0x12 0x34 <2-byte length> <payload>`.
//! 2. Decode the `Forward Request` message (type `0x02`): method code, the
//!    common-header table, and AJP attributes, into a [`crate::Request`].
//! 3. Stream the [`crate::Response`] back as `Send Headers` (`0x04`) +
//!    `Send Body Chunk` (`0x03`) packets, finishing with `End Response`
//!    (`0x05`).
//! 4. Enforce the configured `secret` and `allowedRequestAttributes` so a
//!    compromised reverse proxy cannot smuggle privileged attributes — the
//!    lesson of CVE-2020-1938 ("Ghostcat").
//!
//! Because AJP is only safe on a trusted network, the implementation will also
//! require an explicit bind address rather than defaulting to `0.0.0.0`.

use tomcatrs_core::Error;

/// AJP packet type codes, from the server's point of view.
///
/// Defined now so packet-handling code can be written against named constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AjpMessageType {
    /// `0x02` — web server → container: a forwarded request.
    ForwardRequest,
    /// `0x07` — web server → container: a chunk of request body.
    BodyChunk,
    /// `0x08` — container → web server: "send me more request body".
    GetBodyChunk,
    /// `0x04` — container → web server: response status + headers.
    SendHeaders,
    /// `0x03` — container → web server: a chunk of response body.
    SendBodyChunk,
    /// `0x05` — container → web server: the response is complete.
    EndResponse,
}

impl AjpMessageType {
    /// The on-the-wire byte for this message type.
    pub fn code(self) -> u8 {
        match self {
            AjpMessageType::ForwardRequest => 0x02,
            AjpMessageType::BodyChunk => 0x07,
            AjpMessageType::GetBodyChunk => 0x08,
            AjpMessageType::SendHeaders => 0x04,
            AjpMessageType::SendBodyChunk => 0x03,
            AjpMessageType::EndResponse => 0x05,
        }
    }
}

/// Placeholder for the future AJP connection driver.
#[derive(Debug)]
pub struct AjpConnection {
    /// The shared secret a trusted proxy must present, if configured.
    _secret: Option<String>,
}

/// The canonical "AJP is not available yet" error.
pub fn unsupported() -> Error {
    Error::protocol("AJP not implemented in v0.1.0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_is_a_protocol_error() {
        assert!(matches!(unsupported(), Error::Protocol(_)));
    }

    #[test]
    fn message_type_codes() {
        assert_eq!(AjpMessageType::ForwardRequest.code(), 0x02);
        assert_eq!(AjpMessageType::EndResponse.code(), 0x05);
    }
}
