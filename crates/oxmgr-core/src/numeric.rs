//! Numeric conversions shared across the crate.
//!
//! **This module is the crate's single sanctioned suppression site.** The lint
//! policy (`cast-suppression-discipline`) bans `#[allow]`/`#[expect]` everywhere
//! else, because a cast that can truncate, lose precision, or lose sign must be
//! auditable. The conversions that std provides a checked or lossless primitive
//! for are implemented below without any cast: `try_from` for narrowing integer
//! casts (`usize_to_f64`, `coord_u16`), `Duration` arithmetic for milliseconds
//! (`duration_millis`), and 32-bit decomposition for wide byte counters
//! (`u64_to_f64`).
//!
//! The remaining directions have **no checked primitive in std**: there is no
//! `TryFrom<f64>` for any integer type, no `From<u64> for f64`, no
//! `From<usize> for f64`, and no `From<u32> for f32` (verified against the
//! toolchain this crate builds with). For those, an `as` cast is unavoidable;
//! concentrating it here behind named helpers keeps that fact visible and
//! auditable instead of scattered through call sites.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "sole sanctioned suppression site: std offers no checked primitive for f64→int, u64→f64, usize→f64, or u32→f32 (see module doc)"
)]

use std::time::Duration;

/// Whole milliseconds of a `Duration` as `u64`, without the `u128` detour of
/// `as_millis()`.
///
/// `as_millis()` returns `u128`, so a later `as u64` truncates on paper even when
/// every real value fits. This computes in `u64` directly and saturates instead of
/// wrapping; an uptime that exceeds the `u64` millisecond range is meaningless anyway.
pub fn duration_millis(d: Duration) -> u64 {
    d.as_secs()
        .saturating_mul(1000)
        .saturating_add(u64::from(d.subsec_millis()))
}

/// `usize` to `f64` for counts bounded below `2^53` (window capacities, event
/// counts, fixture lengths). `f64::from(u32)` is exact for every `u32`; the
/// `u32::try_from` guard turns an unrepresentable count into a documented `0.0`
/// rather than a silent lossy conversion.
pub fn usize_to_f64(n: usize) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(0))
}

/// `u64` to `f64` decomposed into 32-bit halves, so the conversion is built from
/// exact `f64::from(u32)` pieces instead of one lossy `as`.
///
/// `n >> 32` and `n & 0xFFFF_FFFF` are bit operations, not casts; each half fits
/// `u32` by construction. For byte counters below `2^53` the result is exact,
/// and above `2^53` it is the nearest representable `f64` — the best any
/// conversion can do.
pub fn u64_to_f64(n: u64) -> f64 {
    let hi = u32::try_from(n >> 32).unwrap_or(0);
    let lo = u32::try_from(n & 0xFFFF_FFFF).unwrap_or(0);
    f64::from(hi) * 4294967296.0 + f64::from(lo)
}

/// Terminal coordinate: `usize` clamped to the largest representable cell.
///
/// crossterm addresses cells with `u16`. Values here come from `terminal::size()`
/// (already `u16`) or from lengths bounded by it, so the clamp only guards future
/// callers; `try_from` makes that guard a runtime check instead of a silent `as`.
pub fn coord_u16(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// Round an `f64` to the nearest whole `usize`, for display-only figures (bar
/// cells, KB conversions).
///
/// std offers no `TryFrom<f64>` for integers, so `as` is the only primitive.
/// Callers pass bounded non-negative figures; the saturating behaviour of `as`
/// (NaN/negative → 0, overflow → `usize::MAX`) is exactly the display contract
/// wanted here.
pub fn f64_to_usize_round(f: f64) -> usize {
    f.round() as usize
}

/// Round an `f64` to the nearest whole `u32`, for milli-unit figures (milli-cpus,
/// per-mille) whose value is bounded well below `u32::MAX`.
///
/// No `TryFrom<f64>` exists in std; `as` is the only primitive. The ceiling is
/// documented at each call site.
pub fn f64_to_u32_round(f: f64) -> u32 {
    f.round() as u32
}

/// Take the ceiling of an `f64` as `usize`, for thresholds derived from counts
/// (e.g. "at least this many moves in the window").
///
/// No `TryFrom<f64>` exists in std; `as` is the only primitive. Callers bound
/// the input to a realistic window count.
pub fn f64_to_usize_ceil(f: f64) -> usize {
    f.ceil() as usize
}

/// `u32` to `f32` for per-mille / percentage figures whose mantissa rounding is
/// the accepted display contract.
///
/// `From<u32> for f32` does not exist in std (u32 exceeds the 24-bit mantissa),
/// so `as` is the only primitive. Values are bounded per-mille ratios; the
/// rounding above 2^24 only matters for overcommit far beyond reality.
pub fn u32_to_f32(n: u32) -> f32 {
    n as f32
}

/// Compact duration: `2d 5h`, `3h 25m`, `25m 12s`, `12s`.
///
/// Lives in core because two layers render the same ETA — the daemon's finding
/// guidance (`/api/findings`) and the CLI table — and one formatter keeps the
/// surfaces from drifting like a duplicated copy would.
pub fn format_duration_compact(total_secs: u64) -> String {
    let days = total_secs / 86_400;
    let hours = (total_secs % 86_400) / 3_600;
    let mins = (total_secs % 3_600) / 60;
    let secs = total_secs % 60;

    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else if mins > 0 {
        format!("{mins}m {secs}s")
    } else {
        format!("{secs}s")
    }
}
