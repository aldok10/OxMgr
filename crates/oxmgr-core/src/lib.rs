//! OxMgr core: domain types and pure logic.
//!
//! Zero-I/O by contract — no async runtime, no HTTP server, no system
//! collection. Every other workspace layer builds on this crate and nothing
//! here knows any of them. The purity is enforced mechanically by
//! `scripts/check-crate-layering.sh`, which fails if a forbidden dependency
//! ever appears in this manifest.

// Panic-family lints are allowed in test-only builds (same policy as the
// binary crate, rust-panic-discipline): `unwrap()`/`expect()` are how a test
// fails, and forbidding them in tests produces noise that gets suppressed
// wholesale — which is how a deny-level lint becomes decoration. Production
// code keeps the deny.
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

pub mod constants;
pub mod errors;
pub mod events;
pub mod findings;
pub mod numeric;
pub mod protection;
pub mod rules;
pub mod severity;
pub mod tuning;
