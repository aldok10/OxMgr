//! OxMgr metrics crate (layer 2): host/process/container metric collection
//! and platform helpers.
//!
//! Extracted from the monolith in workspace-crate-layout phase 4. Modules are
//! re-exported wholesale by the binary crate via shims, reproducing the prior
//! public surface — no item becomes newly reachable.

// Panic-family lints are allowed in test-only builds (same policy as the
// other workspace crates, rust-panic-discipline): `unwrap()`/`expect()` are
// how a test fails, and forbidding them in tests produces noise that gets
// suppressed wholesale — which is how a deny-level lint becomes decoration.
// Production code keeps the deny.
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

pub mod cgroup;
pub mod container;
pub mod env_expand;
pub mod host_consumers;
pub mod host_metrics;
pub mod platform;
pub mod process;

/// Test-only helpers shared by this crate's unit tests (`EnvGuard`,
/// `env_lock`). Mirrors the copy in the binary crate; both are
/// `cfg(test)`-scoped so neither affects any public surface.
#[cfg(test)]
pub mod test_utils;
