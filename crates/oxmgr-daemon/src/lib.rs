//! OxMgr daemon crate (layer 5): HTTP/SSE surface, IPC socket protocol, and
//! the I/O-fused config loader.
//!
//! Extracted from the monolith in workspace-crate-layout phase 6. The binary
//! crate re-exports these modules wholesale via shims, reproducing the prior
//! public surface — no item becomes newly reachable.

// Test builds are exempt from the panic-freedom lints (§D3 of
// rust-panic-discipline): `unwrap()` is how a test fails, and forbidding it in
// tests would produce noise that gets suppressed wholesale — which is how a
// deny-level lint becomes decoration. Production code keeps the deny.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::let_underscore_must_use,
        reason = "test-only builds; a panicking assertion is the test failing loudly"
    )
)]

pub mod config;
pub mod daemon;
pub mod ipc;

/// Test-only helpers shared by this crate's unit tests (`EnvGuard`,
/// `env_lock`). Mirrors the copies in the binary and metrics crates; all are
/// `cfg(test)`-scoped so none affects any public surface.
#[cfg(test)]
pub mod test_utils;

/// A wrapper around a writer that ignores `BrokenPipe` errors.
pub struct SafeWriter<W>(pub W);

impl<W: std::io::Write> std::io::Write for SafeWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.0.write(buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(buf.len()),
            Err(e) => Err(e),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.0.flush() {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            Err(e) => Err(e),
        }
    }
}

// Removing the MakeWriter implementation because it depends on tracing-subscriber,
// which is not a dependency of oxmgr-daemon.
// The MakeWriter will be implemented in main.rs instead.

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_safe_writer_ignores_broken_pipe() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = std::net::TcpStream::connect(addr).unwrap();

        // Accept the connection and immediately drop it
        let (conn, _) = listener.accept().unwrap();
        drop(conn);

        let mut safe_writer = SafeWriter(stream);
        // The write should not fail even if the pipe is closed
        let result = safe_writer.write_all(b"hello");
        assert!(result.is_ok());
    }
}
