//! OxMgr analytics crate (layer 3): deterministic detectors, per-process
//! baselines, metric-history retention, and failure-pattern analysis.
//!
//! Extracted from the monolith in workspace-crate-layout phase 5. Modules are
//! re-exported wholesale by the binary crate via shims, reproducing the prior
//! public surface — no item becomes newly reachable.

// Test builds are exempt from the panic-freedom lints (§D3 of
// rust-panic-discipline): `unwrap()` is how a test fails, and forbidding it in
//! tests would produce noise that gets suppressed wholesale — which is how a
//! deny-level lint becomes decoration. Production code keeps the deny.
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

pub mod analysis;
pub mod baseline;
pub mod detector_incremental;
pub mod detector_level;
pub mod detector_trend;
pub mod failure_patterns;
pub mod metrics_history;
