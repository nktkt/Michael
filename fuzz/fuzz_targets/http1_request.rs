//! Fuzz target: the HTTP/1.1 request parser.
//!
//! Feeds arbitrary bytes into [`tomcatrs_coyote::http1::serve_connection`] over
//! an in-memory cursor. `serve_connection` is the only public entry point into
//! the HTTP/1.1 parser; internally it drives request-line parsing, header-block
//! parsing, body framing (`Content-Length` / `Transfer-Encoding: chunked`), and
//! limit enforcement.
//!
//! Invariant under test: **no input may make the parser panic.** Malformed,
//! truncated, or adversarial bytes must surface as a `4xx`/`5xx` response or a
//! clean connection close — never an `unwrap` panic, slice-index panic, integer
//! overflow, or unbounded allocation.
//!
//! The cursor presents the fuzz input as a client that has already sent
//! everything and then closed (EOF). Because the reader is a finite in-memory
//! buffer, every `read` resolves immediately, so the `async` plumbing completes
//! synchronously on a single poll — no real timers or sockets are involved.

#![no_main]

use std::net::SocketAddr;
use std::sync::Arc;

use libfuzzer_sys::fuzz_target;

use tomcatrs_config::RequestLimits;
use tomcatrs_coyote::{Adapter, Request, Response};

/// A trivial adapter: whatever request the parser manages to construct, answer
/// with a fixed 200. The adapter body is irrelevant — we are fuzzing the
/// parser, not request handling — but it must exist for `serve_connection`.
struct NullAdapter;

#[async_trait::async_trait]
impl Adapter for NullAdapter {
    async fn service(&self, _req: Request) -> Response {
        Response::new(200)
    }
}

fuzz_target!(|data: &[u8]| {
    // A current-thread runtime with no I/O driver is enough: the transport is
    // an in-memory `Cursor`, so reads never actually block.
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("building a current-thread runtime cannot fail");

    rt.block_on(async {
        // The "socket" is the fuzz input followed by EOF. A `Cursor<Vec<u8>>`
        // is `AsyncRead + AsyncWrite + Unpin`, exactly what `serve_connection`
        // requires; writes (the response) just accumulate in the cursor.
        let mut stream = std::io::Cursor::new(data.to_vec());
        let peer: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let adapter = NullAdapter;
        let limits = RequestLimits::default();

        // The contract: this must return without panicking for *any* input.
        // An `Err` (I/O failure while writing) is a perfectly acceptable
        // outcome and is simply discarded.
        let _ = tomcatrs_coyote::http1::serve_connection(
            &mut stream,
            peer,
            &adapter as &dyn Adapter,
            &limits,
        )
        .await;
    });
});
