//! OxMgr store: layer-independent persistence-adjacent services.
//!
//! Event retention over core bus events, content hashing, and ecosystem-file
//! JS config extraction. Depends only on `oxmgr-core` — the crate carries no
//! manager or analytics payloads, because persisted formats owned by higher
//! layers cannot be encoded here without an upward dependency edge (see
//! design.md, "storage/oxfile/bundle stay out of store").

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

pub mod event_retention;
pub mod hash;
pub mod js_config;
