//! Apache/Tomcat-style HTTP access logging.
//!
//! This module is fully working. It models a single logged request as an
//! [`AccessLogEntry`], formats it with either the **Common Log Format** or the
//! **Combined Log Format** ([`AccessLogFormat`]), and offers an [`AccessLog`]
//! writer that appends formatted lines to any [`std::io::Write`] sink behind a
//! `parking_lot::Mutex`.
//!
//! The two formats correspond exactly to Tomcat's `AccessLogValve` patterns
//! `common` and `combined`:
//!
//! ```text
//! common   = %h %l %u %t "%r" %>s %b
//! combined = %h %l %u %t "%r" %>s %b "%{Referer}i" "%{User-Agent}i"
//! ```

use std::io::{self, Write};

use parking_lot::Mutex;

/// Which Apache/Tomcat log pattern to emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessLogFormat {
    /// The Common Log Format: `%h %l %u %t "%r" %>s %b`.
    Common,
    /// The Combined Log Format: Common plus `"%{Referer}i" "%{User-Agent}i"`.
    Combined,
}

impl AccessLogFormat {
    /// Format a single entry as one log line (no trailing newline).
    ///
    /// Absent optional fields are rendered as `-`, matching Apache's
    /// convention for "no value".
    pub fn format(&self, entry: &AccessLogEntry) -> String {
        // The Common Log Format prefix, shared by both patterns.
        let common = format!(
            "{} {} {} [{}] \"{}\" {} {}",
            dash(&entry.remote_addr),
            dash(&entry.ident),
            dash(&entry.user),
            entry.timestamp,
            entry.request_line,
            entry.status,
            // A response body of zero bytes is conventionally logged as `-`.
            if entry.bytes == 0 {
                "-".to_string()
            } else {
                entry.bytes.to_string()
            },
        );

        match self {
            AccessLogFormat::Common => common,
            AccessLogFormat::Combined => format!(
                "{} \"{}\" \"{}\"",
                common,
                dash(&entry.referer),
                dash(&entry.user_agent),
            ),
        }
    }
}

/// Render an empty string as Apache's `-` placeholder.
fn dash(value: &str) -> &str {
    if value.is_empty() {
        "-"
    } else {
        value
    }
}

/// One HTTP request worth of access-log data.
///
/// Field names follow the Apache log-format directives they map to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccessLogEntry {
    /// `%h` — the remote client's IP address (or hostname).
    pub remote_addr: String,
    /// `%l` — the RFC 1413 identity of the client. Almost always `-`.
    pub ident: String,
    /// `%u` — the authenticated user name, if any.
    pub user: String,
    /// `%t` — the request timestamp, pre-formatted in CLF style, e.g.
    /// `10/Oct/2000:13:55:36 -0700`. The brackets are added by the formatter.
    pub timestamp: String,
    /// `%r` — the request line, e.g. `GET /index.html HTTP/1.1`.
    pub request_line: String,
    /// `%>s` — the final HTTP response status code.
    pub status: u16,
    /// `%b` — the size of the response body in bytes.
    pub bytes: u64,
    /// `%{Referer}i` — the `Referer` request header (Combined format only).
    pub referer: String,
    /// `%{User-Agent}i` — the `User-Agent` request header (Combined only).
    pub user_agent: String,
}

impl AccessLogEntry {
    /// Start a builder-ish entry with just the mandatory request fields set.
    /// Optional fields default to empty (rendered as `-`).
    pub fn new(
        remote_addr: impl Into<String>,
        request_line: impl Into<String>,
        status: u16,
    ) -> Self {
        AccessLogEntry {
            remote_addr: remote_addr.into(),
            request_line: request_line.into(),
            status,
            ..Default::default()
        }
    }
}

/// A thread-safe access-log writer.
///
/// Wraps any [`Write`] sink (a file, a buffer, stdout, …) in a
/// `parking_lot::Mutex` so it can be shared across connection-handling tasks.
/// Each [`AccessLog::log`] call formats one entry and appends it followed by a
/// newline.
pub struct AccessLog<W: Write> {
    format: AccessLogFormat,
    sink: Mutex<W>,
}

impl<W: Write> AccessLog<W> {
    /// Create an access log that writes `format`-style lines to `sink`.
    pub fn new(format: AccessLogFormat, sink: W) -> Self {
        AccessLog {
            format,
            sink: Mutex::new(sink),
        }
    }

    /// The log format this writer emits.
    pub fn format(&self) -> AccessLogFormat {
        self.format
    }

    /// Format `entry` and append it (plus a newline) to the sink.
    pub fn log(&self, entry: &AccessLogEntry) -> io::Result<()> {
        let line = self.format.format(entry);
        let mut sink = self.sink.lock();
        sink.write_all(line.as_bytes())?;
        sink.write_all(b"\n")?;
        Ok(())
    }

    /// Flush the underlying sink.
    pub fn flush(&self) -> io::Result<()> {
        self.sink.lock().flush()
    }

    /// Consume the log and return the wrapped sink.
    pub fn into_inner(self) -> W {
        self.sink.into_inner()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry() -> AccessLogEntry {
        AccessLogEntry {
            remote_addr: "127.0.0.1".to_string(),
            ident: String::new(),
            user: "frank".to_string(),
            timestamp: "10/Oct/2000:13:55:36 -0700".to_string(),
            request_line: "GET /apache_pb.gif HTTP/1.0".to_string(),
            status: 200,
            bytes: 2326,
            referer: "http://www.example.com/start.html".to_string(),
            user_agent: "Mozilla/4.08 [en] (Win98; I ;Nav)".to_string(),
        }
    }

    #[test]
    fn common_format_matches_apache_example() {
        let line = AccessLogFormat::Common.format(&sample_entry());
        assert_eq!(
            line,
            "127.0.0.1 - frank [10/Oct/2000:13:55:36 -0700] \"GET /apache_pb.gif HTTP/1.0\" 200 2326"
        );
    }

    #[test]
    fn combined_format_matches_apache_example() {
        let line = AccessLogFormat::Combined.format(&sample_entry());
        assert_eq!(
            line,
            "127.0.0.1 - frank [10/Oct/2000:13:55:36 -0700] \"GET /apache_pb.gif HTTP/1.0\" 200 2326 \
             \"http://www.example.com/start.html\" \"Mozilla/4.08 [en] (Win98; I ;Nav)\""
        );
    }

    #[test]
    fn missing_fields_render_as_dash() {
        let entry = AccessLogEntry::new("10.0.0.1", "GET / HTTP/1.1", 404);
        let line = AccessLogFormat::Combined.format(&entry);
        // remote_addr present; ident/user/referer/user-agent empty -> '-';
        // status 404, zero body -> '-'.
        assert_eq!(line, "10.0.0.1 - - [] \"GET / HTTP/1.1\" 404 - \"-\" \"-\"");
    }

    #[test]
    fn access_log_writer_appends_lines() {
        let log = AccessLog::new(AccessLogFormat::Common, Vec::<u8>::new());
        log.log(&sample_entry()).unwrap();
        log.log(&AccessLogEntry::new("10.0.0.2", "POST /x HTTP/1.1", 500))
            .unwrap();
        let buf = log.into_inner();
        let text = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("127.0.0.1 - frank"));
        assert!(lines[1].ends_with("\"POST /x HTTP/1.1\" 500 -"));
    }
}
