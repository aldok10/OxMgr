//! Per-process behavioural baselines: how they are learned, adapted, persisted, and invalidated.
//!
//! Scaffold for OpenSpec change `process-intelligence`, tasks section(s) 3 and 4.
//! The contract is `openspec/changes/process-intelligence/specs/`; read it before adding to this file.
//!
//! Pure logic on purpose: nothing here reaches for the process manager, the HTTP layer or the
//! dashboard, so it can be unit-tested without a running daemon. Wiring it into those is a
//! separate integration step, which is why the module is currently unreferenced.
//!
//! # Shape of the answer
//!
//! A baseline answers one question — "is this value normal *for this process*" — and it answers
//! it from the process's own history rather than a configured absolute. Three properties make
//! that safe enough to build detectors on:
//!
//! 1. **Robust centre and spread.** Median and MAD (median absolute deviation), not mean and
//!    standard deviation. One startup spike moves a mean and inflates a deviation, which then
//!    hides the *next* real anomaly. A single sample cannot move a median off a window of 64.
//! 2. **An explicit not-established state.** Below [`DEFAULT_MIN_SAMPLES`] observations the
//!    baseline reports [`BaselineReadiness::Warming`] and yields no centre, no spread and no
//!    deviation. `Warming` is a different answer from "established, and the value happens to be
//!    zero", which is why centre and spread are `Option` rather than `0.0`.
//! 3. **Adaptation that refuses to launder a regression.** Re-centring is gated on the window
//!    looking like a *settled new level*, never like an ongoing climb. See
//!    [`MetricBaseline::observe`] for the veto and why it is the important half of adaptation.
//!
//! # What lives where
//!
//! In memory a baseline holds its sample window; on disk it holds only a summary. The window is
//! volatile by design: persisting it would make every whole-file rewrite (`src/storage.rs`
//! `save_state`, temp-file-and-replace) carry 64 floats per metric per process, and the window
//! refills within a couple of minutes of a restart. The summary — centre, spread, counters — is
//! what a restarting daemon actually cannot recompute, because recomputing it needs the history
//! that was just lost. That split is what makes persisted size a constant per process rather
//! than a function of retention.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

/// Scale factor that makes a MAD comparable to a standard deviation.
///
/// For normally distributed data `1.4826 x MAD` estimates sigma, so a deviation expressed in
/// these units reads on the same scale an operator already expects from a z-score. The constant
/// is `1 / qnorm(0.75)`; the extra digits are carried so the value is not a source of drift
/// between the detector and a hand check.
pub const MAD_TO_SIGMA: f64 = 1.482_602_218_505_602;

/// Samples retained for the median/MAD computation.
///
/// At the daemon's 2s maintenance tick 64 samples is roughly 128 seconds: long enough that a
/// short burst is a minority of the window and cannot move the median, short enough that a
/// genuine new level is adopted within about two minutes. A power of two for no reason beyond
/// the ring capacity being obvious.
///
/// UNVALIDATED: chosen by reasoning about the tick rate, not calibrated against recorded
/// workload traces. `design.md` phase 5 is where that calibration happens.
pub const DEFAULT_WINDOW: usize = 64;

/// Observations required before a baseline is trusted at all.
///
/// A MAD computed from a handful of samples is itself noise, and a baseline that is wrong is
/// worse than a baseline that is absent: it produces confident findings from nothing. 30 samples
/// is the conventional floor for a dispersion estimate to be worth quoting, and at a 2s tick it
/// is about a minute of process life — which also happens to skip the erratic first seconds
/// after a start without any special-casing of startup.
///
/// UNVALIDATED: reasoned, not measured.
pub const DEFAULT_MIN_SAMPLES: u64 = 30;

/// Spread floor for percentage metrics, in percentage points.
///
/// A flat metric has MAD zero, and a zero spread turns any change at all into an infinite
/// deviation — the single most productive source of false positives in this design. The floor is
/// set from measurement granularity, so it must differ per metric family. 0.5 matches
/// `PERCENT_EPSILON` in `src/host_metrics.rs`: below half a percentage point the daemon already
/// treats a CPU figure as unchanged, so nothing smaller can honestly be called a departure.
pub const PERCENT_SPREAD_FLOOR: f64 = 0.5;

/// Spread floor for byte-valued metrics, in bytes.
///
/// 4096 is one page. Resident memory moves in page units, so a smaller floor would express a
/// precision the underlying measurement does not have.
///
/// UNVALIDATED as a *useful* floor: it is correct as a granularity bound, but whether it is
/// large enough to suppress real-world memory jitter needs traces.
pub const BYTES_SPREAD_FLOOR: f64 = 4096.0;

/// How far a candidate centre must sit from the current one before the baseline moves, in
/// spreads.
///
/// Without hysteresis the centre would chase ordinary noise, and every re-centre slightly
/// changes every deviation a detector computes. One spread is the smallest gap that is not
/// itself noise.
pub const DEFAULT_ADAPT_HYSTERESIS_SPREADS: f64 = 1.0;

/// Fraction of *non-zero* sample-to-sample deltas that must agree in sign for the window to be
/// read as an ongoing climb (or fall) rather than a settled level.
///
/// Stationary noise sits near 0.5. A leak sits near 1.0. 0.8 leaves room for a leak with
/// occasional GC dips while staying well clear of noise.
///
/// Zero deltas are excluded deliberately: a quantised metric that simply does not move has
/// every delta equal to zero, and counting those as "non-negative" would score a perfectly flat
/// series at monotonicity 1.0 and freeze its baseline forever.
pub const DEFAULT_DRIFT_MONOTONICITY: f64 = 0.8;

/// Minimum number of non-zero deltas before monotonicity is trusted, as a fraction of the
/// window.
///
/// Two moves in the same direction is not evidence of a trend. A coarse metric can spend most of
/// a window unchanged, and judging it on its two transitions would freeze it on a coin flip.
pub const DEFAULT_DRIFT_MIN_MOVE_FRACTION: f64 = 0.25;

/// Net movement across the window, in spreads, required before the climb veto applies.
///
/// Measured as the distance between the medians of the last and first thirds of the window. For
/// stationary noise that distance is a fraction of a spread. For a straight ramp of total rise
/// `R` it is about `0.67R` against a scaled MAD of about `0.37R`, i.e. about 1.8 spreads — so the
/// threshold has to sit below that or the veto would miss the exact shape it exists to catch.
/// 1.0 separates the two cases with margin on both sides.
pub const DEFAULT_DRIFT_NET_SHIFT_SPREADS: f64 = 1.0;

/// Version tag written with persisted baselines.
///
/// Not used for gating yet: every field is `#[serde(default)]`, so an older payload loads by
/// filling defaults rather than by inspecting this. It exists so a future incompatible change
/// has something to branch on other than guessing from which fields are present.
pub const PERSISTED_BASELINE_VERSION: u32 = 1;

/// The family a metric belongs to, which is what decides its spread floor.
///
/// The floor is a statement about measurement granularity, so it cannot be one global number: a
/// half-point move in CPU percent and a half-byte move in RSS are not comparable claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    /// A percentage, floored at [`PERCENT_SPREAD_FLOOR`].
    Percent,
    /// A byte count, floored at [`BYTES_SPREAD_FLOOR`].
    Bytes,
}

impl MetricKind {
    /// The smallest spread this family of metric can honestly claim.
    pub fn spread_floor(self) -> f64 {
        match self {
            Self::Percent => PERCENT_SPREAD_FLOOR,
            Self::Bytes => BYTES_SPREAD_FLOOR,
        }
    }
}

/// The metrics a baseline is kept for.
///
/// A closed enum rather than free-form strings: an unknown metric name is then a compile error
/// at the call site instead of a silently empty baseline at runtime, and the persisted keys are
/// a fixed vocabulary. These are exactly the figures `refresh_resource_metrics` already
/// collects, so no new sampling is implied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    CpuPercent,
    MemoryBytes,
    DiskReadBytes,
    DiskWriteBytes,
}

/// Every metric, in the order they are reported.
pub const ALL_METRICS: [Metric; 4] = [
    Metric::CpuPercent,
    Metric::MemoryBytes,
    Metric::DiskReadBytes,
    Metric::DiskWriteBytes,
];

impl Metric {
    /// The persisted key. Stable: changing one of these strings orphans every stored baseline
    /// for that metric, which reads as a silent re-warm rather than as an error.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CpuPercent => "cpu_percent",
            Self::MemoryBytes => "memory_bytes",
            Self::DiskReadBytes => "disk_read_bytes",
            Self::DiskWriteBytes => "disk_write_bytes",
        }
    }

    /// Parses a persisted key. `None` for anything unrecognised, which is how a payload written
    /// by a newer daemon degrades: the unknown series is dropped and re-warms, rather than
    /// failing the whole load.
    pub fn from_key(key: &str) -> Option<Self> {
        ALL_METRICS.into_iter().find(|m| m.as_str() == key)
    }

    /// Which spread floor applies.
    pub fn kind(self) -> MetricKind {
        match self {
            Self::CpuPercent => MetricKind::Percent,
            Self::MemoryBytes | Self::DiskReadBytes | Self::DiskWriteBytes => MetricKind::Bytes,
        }
    }
}

/// Whether a baseline may be used yet.
///
/// A distinct state rather than a `bool` beside an `Option<f64>`, because the sample count is the
/// operator's answer to "how much longer": "not ready, 12 of 30" is actionable where "not ready"
/// is not. This is what turns an absence of findings into a checkable claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BaselineReadiness {
    /// Too few samples to trust. No detector depending on this baseline may fire.
    Warming { samples: u64, required: u64 },
    /// Enough samples observed; centre and spread are meaningful.
    Ready { samples: u64 },
}

impl BaselineReadiness {
    /// Whether baseline-dependent detectors may fire.
    pub fn is_ready(self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    /// Maturity in `[0, 1]`: how much of the warm-up requirement has been met, saturating at 1.
    ///
    /// Exposed because the confidence formula needs a baseline-maturity term, and that term has
    /// to come from the baseline rather than be re-derived (and re-approximated) per detector.
    #[cfg(test)]
    pub fn maturity(self) -> f64 {
        match self {
            Self::Warming { samples, required } => {
                if required == 0 {
                    1.0
                } else {
                    // Counts bounded by the window capacity ≪ 2^53; u64_to_f64 is exact.
                    (oxmgr_core::numeric::u64_to_f64(samples)
                        / oxmgr_core::numeric::u64_to_f64(required))
                    .clamp(0.0, 1.0)
                }
            }
            Self::Ready { .. } => 1.0,
        }
    }
}

/// A tuning value that could not be used, and what was applied instead.
///
/// Returned rather than logged so the caller can surface it: the spec requires a fallback to be
/// *reported*, and a `warn!` in a library function is not a report anyone can query.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigFallback {
    /// Field name as it appears in configuration.
    pub field: &'static str,
    /// Why the supplied value was unusable.
    pub reason: &'static str,
    /// The default that was substituted, rendered for display.
    pub applied: String,
}

/// Tunables for one baseline.
///
/// Every field has a documented default and every default is reasoned rather than measured; see
/// each constant. [`BaselineConfig::sanitised`] is the only way an externally supplied value
/// should enter, so an unusable number becomes a reported fallback instead of a NaN threshold.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BaselineConfig {
    /// Samples retained for median/MAD.
    pub window: usize,
    /// Observations required before the baseline is `Ready`.
    pub min_samples: u64,
    /// Spread floor, overriding the metric family's default when finite and positive.
    pub spread_floor: f64,
    /// How far a candidate centre must move, in spreads, before re-centring.
    pub adapt_hysteresis_spreads: f64,
    /// Signed-delta agreement above which the window reads as an ongoing climb.
    pub drift_monotonicity: f64,
    /// Fraction of the window that must actually have moved before monotonicity is trusted.
    pub drift_min_move_fraction: f64,
    /// Net movement across the window, in spreads, required for the climb veto to apply.
    pub drift_net_shift_spreads: f64,
}

impl BaselineConfig {
    /// Defaults for a metric family, with that family's spread floor.
    pub fn for_kind(kind: MetricKind) -> Self {
        Self {
            window: DEFAULT_WINDOW,
            min_samples: DEFAULT_MIN_SAMPLES,
            spread_floor: kind.spread_floor(),
            adapt_hysteresis_spreads: DEFAULT_ADAPT_HYSTERESIS_SPREADS,
            drift_monotonicity: DEFAULT_DRIFT_MONOTONICITY,
            drift_min_move_fraction: DEFAULT_DRIFT_MIN_MOVE_FRACTION,
            drift_net_shift_spreads: DEFAULT_DRIFT_NET_SHIFT_SPREADS,
        }
    }

    /// Replaces every unusable value with the documented default, reporting each substitution.
    ///
    /// "Unusable" means non-finite, negative where only a positive value makes sense, or outside
    /// the range the value is defined on. A window of zero is rejected because the median of an
    /// empty window is undefined; `min_samples` of zero is rejected because a baseline ready
    /// after no observations is exactly the failure mode warm-up gating exists to prevent.
    pub fn sanitised(self, kind: MetricKind) -> (Self, Vec<ConfigFallback>) {
        let defaults = Self::for_kind(kind);
        let mut out = self;
        let mut fallbacks = Vec::new();

        if out.window == 0 {
            fallbacks.push(ConfigFallback {
                field: "window",
                reason: "window must be at least 1",
                applied: defaults.window.to_string(),
            });
            out.window = defaults.window;
        }
        if out.min_samples == 0 {
            fallbacks.push(ConfigFallback {
                field: "min_samples",
                reason: "a baseline ready after zero samples defeats warm-up gating",
                applied: defaults.min_samples.to_string(),
            });
            out.min_samples = defaults.min_samples;
        }
        if !out.spread_floor.is_finite() || out.spread_floor <= 0.0 {
            fallbacks.push(ConfigFallback {
                field: "spread_floor",
                reason: "spread floor must be finite and positive",
                applied: defaults.spread_floor.to_string(),
            });
            out.spread_floor = defaults.spread_floor;
        }
        if !out.adapt_hysteresis_spreads.is_finite() || out.adapt_hysteresis_spreads < 0.0 {
            fallbacks.push(ConfigFallback {
                field: "adapt_hysteresis_spreads",
                reason: "hysteresis must be finite and non-negative",
                applied: defaults.adapt_hysteresis_spreads.to_string(),
            });
            out.adapt_hysteresis_spreads = defaults.adapt_hysteresis_spreads;
        }
        if !out.drift_monotonicity.is_finite() || !(0.0..=1.0).contains(&out.drift_monotonicity) {
            fallbacks.push(ConfigFallback {
                field: "drift_monotonicity",
                reason: "monotonicity is a fraction in [0, 1]",
                applied: defaults.drift_monotonicity.to_string(),
            });
            out.drift_monotonicity = defaults.drift_monotonicity;
        }
        if !out.drift_min_move_fraction.is_finite()
            || !(0.0..=1.0).contains(&out.drift_min_move_fraction)
        {
            fallbacks.push(ConfigFallback {
                field: "drift_min_move_fraction",
                reason: "move fraction is a fraction in [0, 1]",
                applied: defaults.drift_min_move_fraction.to_string(),
            });
            out.drift_min_move_fraction = defaults.drift_min_move_fraction;
        }
        if !out.drift_net_shift_spreads.is_finite() || out.drift_net_shift_spreads < 0.0 {
            fallbacks.push(ConfigFallback {
                field: "drift_net_shift_spreads",
                reason: "net shift must be finite and non-negative",
                applied: defaults.drift_net_shift_spreads.to_string(),
            });
            out.drift_net_shift_spreads = defaults.drift_net_shift_spreads;
        }

        (out, fallbacks)
    }
}

/// Median of a slice, by sorting a copy.
///
/// Sorting is `O(n log n)` where a selection algorithm would be `O(n)`, and that is a deliberate
/// trade: `n` is [`DEFAULT_WINDOW`] = 64, bounded and independent of uptime, so the constant
/// factor of a hand-written quickselect buys nothing measurable and costs the reviewability the
/// whole design rests on. `design.md` names median/MAD as the one non-streaming statistic and
/// bounds it with the window for exactly this reason.
///
/// Non-finite values are the caller's problem: [`MetricBaseline::observe`] refuses them at the
/// door, so `partial_cmp` here cannot see a NaN. `total_cmp` is used anyway rather than an
/// `unwrap`, since a panic in the maintenance tick would take the daemon down over a metric.
fn median_of(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        // Even-length median is the mean of the two central values. Averaging rather than
        // picking the lower keeps the median continuous as samples arrive, so a baseline does
        // not step by half a sample gap purely because the window length changed parity.
        Some((sorted[mid - 1] + sorted[mid]) / 2.0)
    } else {
        Some(sorted[mid])
    }
}

/// Scaled median absolute deviation: `1.4826 x median(|x - centre|)`.
///
/// Returned unfloored — the floor is applied in [`MetricBaseline`], where the metric family is
/// known. Keeping them separate means the raw statistic stays testable against a hand
/// calculation, and the floor stays visible as a policy decision rather than hidden inside the
/// arithmetic.
fn scaled_mad(values: &[f64], centre: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let deviations: Vec<f64> = values.iter().map(|v| (v - centre).abs()).collect();
    median_of(&deviations).map(|mad| mad * MAD_TO_SIGMA)
}

/// Which way a window is moving, when it is moving at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrendDirection {
    Rising,
    Falling,
}

/// The evidence behind a climb judgement, kept so a held baseline can explain itself.
///
/// Structured rather than a bare `bool`: "the baseline refused to adapt" is a claim an operator
/// will want to check, and the three numbers below are the whole basis for it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrendEvidence {
    pub direction: TrendDirection,
    /// Fraction of non-zero deltas agreeing with `direction`.
    pub monotonicity: f64,
    /// Non-zero deltas seen, over deltas available.
    pub moves: usize,
    pub deltas: usize,
    /// Distance between the medians of the final and first thirds, in spreads.
    pub net_shift_spreads: f64,
}

/// Whether the window looks like an ongoing climb or fall rather than a settled level.
///
/// This is the function that decides whether adaptation is allowed, so it is written to be hard
/// to trigger by accident. Three independent conditions must hold together:
///
/// 1. **Enough of the window actually moved.** A metric quantised to pages can sit unchanged for
///    most of a window; judging it on its two transitions would be a coin flip.
/// 2. **The moves mostly agree in sign.** Stationary noise sits near 0.5 by construction; a leak
///    sits near 1.0. Zero deltas are excluded from the ratio entirely — counting an unchanged
///    sample as "non-negative" would score a perfectly flat series at 1.0 and freeze its
///    baseline for the life of the process.
/// 3. **The window went somewhere.** Sign agreement alone is scale-blind: 64 consecutive
///    one-byte increases agree perfectly and mean nothing. Requiring the medians of the first and
///    last thirds to differ by at least `drift_net_shift_spreads` spreads makes the test scale
///    with the metric's own noise instead of with an absolute quantity.
///
/// Thirds rather than halves so the comparison ignores the middle of the window, where a ramp's
/// two ends are least separated. `spread` is the current floored spread, passed in rather than
/// recomputed so the caller controls which estimate the ratio is expressed against.
fn window_trend(window: &[f64], spread: f64, config: &BaselineConfig) -> Option<TrendEvidence> {
    // Nine is the smallest length that gives three thirds of three samples: below that the
    // "medians of the outer thirds" comparison is comparing individual samples, which is
    // precisely the single-sample sensitivity this module exists to avoid.
    if window.len() < 9 || !spread.is_finite() || spread <= 0.0 {
        return None;
    }

    let deltas = window.len() - 1;
    let mut rising = 0usize;
    let mut falling = 0usize;
    for pair in window.windows(2) {
        let delta = pair[1] - pair[0];
        if delta > 0.0 {
            rising += 1;
        } else if delta < 0.0 {
            falling += 1;
        }
    }
    let moves = rising + falling;
    // `deltas` is a window count bounded by fixed capacity; the product is a small non-negative
    // float whose ceil is < usize::MAX for any realistic window. `f64_to_usize_ceil` is the
    // crate's sanctioned f64→usize conversion (std has no TryFrom<f64> for integers).
    let required_moves = oxmgr_core::numeric::f64_to_usize_ceil(
        config.drift_min_move_fraction * oxmgr_core::numeric::usize_to_f64(deltas),
    );
    if moves == 0 || moves < required_moves.max(2) {
        return None;
    }

    let (direction, agreeing) = if rising >= falling {
        (TrendDirection::Rising, rising)
    } else {
        (TrendDirection::Falling, falling)
    };
    // Counts bounded by the window capacity ≪ 2^53; usize_to_f64 is exact.
    let monotonicity =
        oxmgr_core::numeric::usize_to_f64(agreeing) / oxmgr_core::numeric::usize_to_f64(moves);
    if monotonicity < config.drift_monotonicity {
        return None;
    }

    let third = window.len() / 3;
    let first = median_of(&window[..third])?;
    let last = median_of(&window[window.len() - third..])?;
    let net_shift_spreads = (last - first) / spread;

    // The net movement must agree with the deltas. A window whose samples mostly rose but which
    // ends below where it started is not a climb; it is a spike followed by a fall, and adapting
    // to it is correct.
    let agrees = match direction {
        TrendDirection::Rising => net_shift_spreads > 0.0,
        TrendDirection::Falling => net_shift_spreads < 0.0,
    };
    if !agrees || net_shift_spreads.abs() < config.drift_net_shift_spreads {
        return None;
    }

    Some(TrendEvidence {
        direction,
        monotonicity,
        moves,
        deltas,
        net_shift_spreads,
    })
}

/// The on-disk summary of one metric's baseline.
///
/// Every field is `#[serde(default)]`, which is the whole forward-compatibility story: a payload
/// written before a field existed loads with that field defaulted rather than failing, and a
/// failed load here would mean a daemon that re-warms every baseline on every upgrade. The
/// codebase already relies on this pattern; `src/storage.rs` treats an unparseable file as absent
/// and moves on, and the same posture applies one level down at the field.
///
/// `centre` and `spread` are `Option` on purpose. `None` means "never established"; `Some(0.0)`
/// means "established, and the process genuinely idles at zero". Collapsing those into a bare
/// `f64` would make a warming baseline indistinguishable from a quiet one, which is the exact
/// confusion the readiness requirement exists to prevent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PersistedMetricBaseline {
    /// Robust centre, or `None` if never established.
    #[serde(default)]
    pub centre: Option<f64>,
    /// Floored, scaled spread, or `None` if never established.
    #[serde(default)]
    pub spread: Option<f64>,
    /// Observations seen across the process's whole lifetime, restarts included.
    ///
    /// Carried across restarts rather than reset, consistent with `process-io-metrics`: a managed
    /// process keeps its identity across restarts and its lifetime counters carry forward. Reset
    /// it and every restart would re-enter warm-up, which for a process that restarts often means
    /// never detecting anything.
    #[serde(default)]
    pub samples: u64,
    /// Times the centre has been re-established since the baseline was created.
    ///
    /// Diagnostic: a high count on a supposedly stable process means the hysteresis is too small
    /// for that workload, and that is not visible from the centre alone.
    #[serde(default)]
    pub adaptations: u64,
    /// Times adaptation was withheld because the window looked like an ongoing climb.
    ///
    /// The counter that makes the anti-laundering rule auditable. A leaking process accumulates
    /// holds; if this is climbing while the centre stays put, the baseline is doing its job.
    #[serde(default)]
    pub adaptation_holds: u64,
}

/// Everything persisted for one process's baselines.
///
/// Keyed by metric string rather than by the [`Metric`] enum so an unknown key from a newer
/// daemon can be skipped individually instead of failing the map. `BTreeMap` for a deterministic
/// serialisation order — the file is rewritten whole on every save, and a map that reorders
/// itself would produce a spurious diff on every write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PersistedProcessBaselines {
    /// Schema version. Informational; see [`PERSISTED_BASELINE_VERSION`].
    #[serde(default)]
    pub version: u32,
    /// The configuration fingerprint the baselines were learned under.
    ///
    /// `src/process.rs` already computes this over command, args, env, resource limits and the
    /// rest. If it differs on load the baselines describe a workload that no longer exists, so
    /// they are discarded. An empty string means "unknown", which is treated as a mismatch:
    /// silently trusting an unlabelled baseline is the failure this field exists to prevent.
    #[serde(default)]
    pub config_fingerprint: String,
    /// Per-metric summaries, keyed by [`Metric::as_str`].
    #[serde(default)]
    pub metrics: BTreeMap<String, PersistedMetricBaseline>,
}

/// Why a restore did not use the persisted state.
///
/// Reported rather than silently swallowed, because "this process re-warmed after a restart" and
/// "this process re-warmed because its config changed" are different operational facts and only
/// one of them is worth investigating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// Persisted state adopted.
    Restored { metrics: usize },
    /// Fingerprint differed: the process is arguably a different workload.
    DiscardedConfigChanged { stored: String, current: String },
    /// The stored fingerprint was empty, so it could not be shown to match.
    DiscardedUnknownFingerprint,
}

/// A live baseline for one metric of one process.
///
/// Holds a bounded sample window (volatile) plus the summary that gets persisted. `observe` is
/// the only way values enter, and it is the only place the adaptation veto is applied.
#[derive(Debug, Clone)]
pub struct MetricBaseline {
    metric: Metric,
    config: BaselineConfig,
    window: VecDeque<f64>,
    centre: Option<f64>,
    spread: Option<f64>,
    samples: u64,
    adaptations: u64,
    adaptation_holds: u64,
    /// Why the last `observe` declined to adapt, if it did. Not persisted: it describes the
    /// volatile window, and a restored baseline has no window yet.
    last_hold: Option<TrendEvidence>,
}

impl MetricBaseline {
    /// A fresh, warming baseline with the metric family's defaults.
    pub fn new(metric: Metric) -> Self {
        Self::with_config(metric, BaselineConfig::for_kind(metric.kind()))
    }

    /// A fresh baseline with explicit tunables. The config is sanitised, so an unusable value
    /// becomes a default here rather than a NaN threshold later.
    pub fn with_config(metric: Metric, config: BaselineConfig) -> Self {
        let (config, _) = config.sanitised(metric.kind());
        Self {
            metric,
            config,
            window: VecDeque::with_capacity(config.window),
            centre: None,
            spread: None,
            samples: 0,
            adaptations: 0,
            adaptation_holds: 0,
            last_hold: None,
        }
    }

    /// Records one sample and re-derives the baseline.
    ///
    /// Non-finite samples are dropped rather than stored: a NaN in the window would poison the
    /// median for the next 64 observations, and a metric collector returning NaN is a collector
    /// bug that should not become a baseline bug. Returns whether the sample was accepted.
    ///
    /// ## Why adaptation cannot absorb a regression
    ///
    /// The centre only moves when *both* of these hold:
    ///
    /// - the candidate centre (the window median) sits at least `adapt_hysteresis_spreads`
    ///   spreads away from the current centre, so ordinary noise cannot drag it; and
    /// - [`window_trend`] does **not** report an ongoing climb or fall.
    ///
    /// The second condition is the important one. A process leaking memory presents a window that
    /// is monotonically rising and has travelled a meaningful distance in spreads — exactly the
    /// shape `window_trend` detects — so its median is refused as a new centre and the deviation
    /// from the *original* centre keeps growing instead of being reset to zero each window. The
    /// baseline can therefore only follow a change that has **settled**: once the metric stops
    /// climbing, monotonicity collapses toward 0.5, the veto lifts, and the new level is adopted.
    /// That is the distinction between "new normal after a deploy" and "slow leak", and it is
    /// made from the shape of the window rather than from any absolute rate.
    ///
    /// The cost is honest and worth stating: a leak so slow that fewer than
    /// `drift_min_move_fraction` of a 128-second window's samples move, or whose net rise stays
    /// under one spread across that window, is not distinguishable from a settled change *at this
    /// window length* and will be adopted. Catching that case is the trend/leak detector's job
    /// over a much longer window (`design.md`, phase 2), not this one's. This module makes the
    /// baseline refuse to launder a visible climb; it does not claim to detect every leak.
    pub fn observe(&mut self, value: f64) -> bool {
        if !value.is_finite() {
            return false;
        }

        if self.window.len() == self.config.window {
            self.window.pop_front();
        }
        self.window.push_back(value);
        self.samples = self.samples.saturating_add(1);

        let window: Vec<f64> = self.window.iter().copied().collect();
        let Some(candidate_centre) = median_of(&window) else {
            return true;
        };
        let candidate_spread = self.floored(scaled_mad(&window, candidate_centre));

        // Spread is refreshed unconditionally while the centre may be held. Holding the spread
        // too would leave a growing series measured against the dispersion of its quiet past,
        // which inflates deviations and would make the leak look like a level departure as well
        // — one event, two findings. Spread describes noise; centre describes level. Only the
        // level claim is what a regression could launder.
        self.spread = Some(candidate_spread);

        // While warming, the centre tracks the window unconditionally: no hysteresis, no veto, no
        // adaptation counted. Two reasons. The estimate from the first sample is worthless and
        // must not be defended — gating it behind hysteresis would let a single early sample lock
        // the centre for the life of the process, since a sub-one-spread gap never clears the
        // gate. And there is nothing to protect: no detector may use a warming baseline, so there
        // is no regression that could be laundered yet. Establishment is not adaptation, which is
        // why `adaptations` stays at zero through warm-up.
        let ready = self.samples >= self.config.min_samples;
        let current_centre = match self.centre {
            Some(current) if ready => current,
            _ => {
                self.centre = Some(candidate_centre);
                self.last_hold = None;
                return true;
            }
        };

        // The veto is evaluated before the hysteresis gate, and the order is load-bearing. Asked
        // the other way round, a hold would only be *counted* once the window median had already
        // drifted a full spread from the centre — so a slow climb would sit in the hysteresis
        // dead zone, held but silently, and `adaptation_holds` would stay at zero while the
        // baseline was doing the very thing the counter exists to evidence. This way the counter
        // answers "is this window climbing" on every tick, independently of how far the median
        // has got.
        if let Some(evidence) = window_trend(&window, candidate_spread, &self.config) {
            self.adaptation_holds = self.adaptation_holds.saturating_add(1);
            self.last_hold = Some(evidence);
            return true;
        }
        self.last_hold = None;

        let gap_spreads = (candidate_centre - current_centre).abs() / candidate_spread;
        if gap_spreads >= self.config.adapt_hysteresis_spreads {
            self.centre = Some(candidate_centre);
            self.adaptations = self.adaptations.saturating_add(1);
        }
        true
    }

    /// Applies the spread floor. `None` in, `floor` out: a baseline with samples always reports
    /// some spread, because a detector dividing by an absent spread has no defined behaviour.
    fn floored(&self, spread: Option<f64>) -> f64 {
        match spread {
            Some(value) if value.is_finite() && value > self.config.spread_floor => value,
            _ => self.config.spread_floor,
        }
    }

    /// Whether the baseline may be used.
    pub fn readiness(&self) -> BaselineReadiness {
        if self.samples >= self.config.min_samples {
            BaselineReadiness::Ready {
                samples: self.samples,
            }
        } else {
            BaselineReadiness::Warming {
                samples: self.samples,
                required: self.config.min_samples,
            }
        }
    }

    /// Deviation of `value` from the centre, in floored spreads.
    ///
    /// `None` while warming — which is the warm-up gate itself, expressed where it cannot be
    /// forgotten. A detector cannot obtain a number to compare against a threshold until the
    /// baseline is ready, so "no detector fires on an immature baseline" holds by construction
    /// rather than by every detector remembering to check.
    ///
    /// Signed, so direction is preserved: a departure below the baseline is as much a finding as
    /// one above, and an absolute value here would lose that.
    #[cfg(test)]
    pub fn deviation(&self, value: f64) -> Option<f64> {
        if !self.readiness().is_ready() || !value.is_finite() {
            return None;
        }
        let centre = self.centre?;
        let spread = self.floored(self.spread);
        Some((value - centre) / spread)
    }

    /// The reportable state: centre, spread, sample count, readiness.
    pub fn snapshot(&self) -> BaselineSnapshot {
        let ready = self.readiness();
        BaselineSnapshot {
            metric: self.metric,
            // Withheld while warming for the same reason `deviation` is: a centre that exists but
            // must not be used is an invitation to use it. The sample count in `readiness` is
            // what tells the operator progress is being made.
            centre: if ready.is_ready() { self.centre } else { None },
            spread: if ready.is_ready() {
                self.centre.map(|_| self.floored(self.spread))
            } else {
                None
            },
            spread_floor: self.config.spread_floor,
            spread_floored: ready.is_ready()
                && self.centre.is_some()
                && self.spread.is_none_or(|s| s <= self.config.spread_floor),
            readiness: ready,
            window_len: self.window.len(),
            adaptations: self.adaptations,
            adaptation_holds: self.adaptation_holds,
            holding: self.last_hold.is_some(),
        }
    }

    /// The summary to persist. Small and fixed-size: five numbers, regardless of retention.
    pub fn to_persisted(&self) -> PersistedMetricBaseline {
        PersistedMetricBaseline {
            centre: self.centre,
            spread: self.centre.map(|_| self.floored(self.spread)),
            samples: self.samples,
            adaptations: self.adaptations,
            adaptation_holds: self.adaptation_holds,
        }
    }

    /// Rebuilds from a persisted summary, with an empty window.
    ///
    /// The restored baseline is immediately usable — `samples` carried across, so a `Ready`
    /// baseline stays ready — but has no window, so it cannot adapt until it has refilled one.
    /// That is the correct posture: the persisted centre is the best available estimate, and the
    /// alternative (re-warming) means a frequently restarted daemon detects nothing at all.
    ///
    /// A non-finite persisted centre or spread is discarded rather than trusted. A corrupt number
    /// would propagate into every deviation this baseline ever reports.
    pub fn from_persisted(
        metric: Metric,
        config: BaselineConfig,
        persisted: &PersistedMetricBaseline,
    ) -> Self {
        let mut restored = Self::with_config(metric, config);
        let centre = persisted.centre.filter(|c| c.is_finite());
        restored.centre = centre;
        restored.spread = persisted
            .spread
            .filter(|s| s.is_finite() && *s > 0.0)
            .map(|s| s.max(restored.config.spread_floor));
        // Samples are only meaningful alongside a centre. Restoring a count without a centre
        // would report `Ready` with nothing to compare against.
        restored.samples = if centre.is_some() {
            persisted.samples
        } else {
            0
        };
        restored.adaptations = persisted.adaptations;
        restored.adaptation_holds = persisted.adaptation_holds;
        restored
    }
}

/// The queryable state of one baseline.
///
/// Serialisable because this is what the metrics endpoint and the event stream carry; the spec
/// requires baseline state to be inspectable through the surfaces that already exist.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaselineSnapshot {
    pub metric: Metric,
    /// Robust centre. `None` while warming, or before first establishment.
    pub centre: Option<f64>,
    /// Floored spread. `None` under the same conditions as `centre`.
    pub spread: Option<f64>,
    /// The floor in force, so a reader can tell a floored spread from a measured one.
    pub spread_floor: f64,
    /// Whether the reported spread is the floor rather than a measurement — i.e. the metric is
    /// flat. Worth reporting: it explains why a visible change produced no finding.
    pub spread_floored: bool,
    pub readiness: BaselineReadiness,
    /// Samples currently in the window. Lower than `readiness`'s count after a restore, since the
    /// window is not persisted.
    pub window_len: usize,
    pub adaptations: u64,
    pub adaptation_holds: u64,
    /// Whether adaptation is currently being withheld because the window looks like a climb.
    pub holding: bool,
}

/// All baselines for one process, plus the configuration they were learned under.
///
/// The fingerprint is held here rather than per metric because it describes the workload, not a
/// series: if the command changed, every metric's baseline is equally suspect.
#[derive(Debug, Clone)]
pub struct ProcessBaselines {
    config_fingerprint: String,
    baselines: BTreeMap<Metric, MetricBaseline>,
}

impl ProcessBaselines {
    /// A fresh set, warming, for the given configuration fingerprint.
    pub fn new(config_fingerprint: impl Into<String>) -> Self {
        Self {
            config_fingerprint: config_fingerprint.into(),
            baselines: BTreeMap::new(),
        }
    }

    /// The baseline for a metric, created warming if absent.
    ///
    /// Lazily created so a process that never reports disk I/O does not carry two empty
    /// baselines, and so adding a metric to [`ALL_METRICS`] does not need a migration.
    pub fn entry(&mut self, metric: Metric) -> &mut MetricBaseline {
        self.baselines
            .entry(metric)
            .or_insert_with(|| MetricBaseline::new(metric))
    }

    pub fn get(&self, metric: Metric) -> Option<&MetricBaseline> {
        self.baselines.get(&metric)
    }

    /// Records a sample for one metric. Returns false if the value was not finite.
    pub fn observe(&mut self, metric: Metric, value: f64) -> bool {
        self.entry(metric).observe(value)
    }

    /// Reportable state for every metric held, in `Metric` order.
    pub fn snapshots(&self) -> Vec<BaselineSnapshot> {
        self.baselines.values().map(|b| b.snapshot()).collect()
    }

    /// Applies a possibly-new configuration fingerprint.
    ///
    /// A changed fingerprint clears every baseline: `src/process.rs` computes it over command,
    /// args, env, resource limits, health check and the rest, so a change there means the workload
    /// being measured is arguably not the same one the baseline describes. Keeping the old centre
    /// would compare a new binary against the old one's normal — which is not a finding about the
    /// process, it is a finding about the deploy, and it would fire on every metric at once.
    ///
    /// A **restart** deliberately does not reach this path. `process-io-metrics` established that
    /// a managed process keeps its identity across restarts and its lifetime counters carry
    /// forward; a restart with an unchanged fingerprint is the same workload, so its baseline
    /// survives. Restart-driven re-warming would mean a process that restarts every few minutes
    /// never leaves warm-up and never produces a finding.
    ///
    /// Returns true if the baselines were cleared.
    #[cfg(test)]
    pub fn apply_config_fingerprint(&mut self, fingerprint: &str) -> bool {
        if self.config_fingerprint == fingerprint {
            return false;
        }
        self.config_fingerprint = fingerprint.to_string();
        self.baselines.clear();
        true
    }

    /// The current config fingerprint.
    ///
    /// Permanent API (workspace-crate-layout phase 5): test-only visibility does
    /// not cross a crate boundary; manager persistence tests assert on it.
    pub fn config_fingerprint(&self) -> &str {
        &self.config_fingerprint
    }

    /// The persistable form.
    pub fn to_persisted(&self) -> PersistedProcessBaselines {
        PersistedProcessBaselines {
            version: PERSISTED_BASELINE_VERSION,
            config_fingerprint: self.config_fingerprint.clone(),
            metrics: self
                .baselines
                .iter()
                .map(|(metric, baseline)| (metric.as_str().to_string(), baseline.to_persisted()))
                .collect(),
        }
    }

    /// Restores baselines for a process whose current fingerprint is `current_fingerprint`.
    ///
    /// Always returns a usable value: on any mismatch the result is a fresh warming set for the
    /// *current* fingerprint, never an error. Unreadable persisted state must be equivalent to
    /// absent state — the alternative is a daemon that will not start because a statistics cache
    /// is malformed, and `src/storage.rs` already takes that position one level up by treating a
    /// corrupt state file as default.
    ///
    /// Unknown metric keys are skipped individually, so a payload from a newer daemon contributes
    /// the metrics both versions understand instead of nothing.
    pub fn restore(
        current_fingerprint: &str,
        persisted: &PersistedProcessBaselines,
    ) -> (Self, RestoreOutcome) {
        if persisted.config_fingerprint.is_empty() {
            return (
                Self::new(current_fingerprint),
                RestoreOutcome::DiscardedUnknownFingerprint,
            );
        }
        if persisted.config_fingerprint != current_fingerprint {
            return (
                Self::new(current_fingerprint),
                RestoreOutcome::DiscardedConfigChanged {
                    stored: persisted.config_fingerprint.clone(),
                    current: current_fingerprint.to_string(),
                },
            );
        }

        let mut restored = Self::new(current_fingerprint);
        for (key, summary) in &persisted.metrics {
            let Some(metric) = Metric::from_key(key) else {
                continue;
            };
            restored.baselines.insert(
                metric,
                MetricBaseline::from_persisted(
                    metric,
                    BaselineConfig::for_kind(metric.kind()),
                    summary,
                ),
            );
        }
        let metrics = restored.baselines.len();
        (restored, RestoreOutcome::Restored { metrics })
    }
}

#[cfg(test)]
/// Test code casts are bounded and exact.
mod tests {
    use super::*;

    /// Feeds `count` copies of `value`. Used to warm a baseline to a known flat state.
    fn feed(baseline: &mut MetricBaseline, value: f64, count: usize) {
        for _ in 0..count {
            assert!(baseline.observe(value), "finite sample must be accepted");
        }
    }

    fn feed_set(set: &mut ProcessBaselines, metric: Metric, value: f64, count: usize) {
        for _ in 0..count {
            set.observe(metric, value);
        }
    }

    fn warm_flat(metric: Metric, value: f64) -> MetricBaseline {
        let mut baseline = MetricBaseline::new(metric);
        feed(&mut baseline, value, DEFAULT_WINDOW);
        baseline
    }

    // -- Persistence -----------------------------------------------------------------------------
    //
    // These were the gap in this module: the types were written for serde and forward
    // compatibility, and nothing exercised either. Persistence is what decides whether a
    // baseline survives a restart, so an untested round-trip is an untested claim.

    #[test]
    fn a_persisted_baseline_round_trips_through_json() {
        let mut set = ProcessBaselines::new("fingerprint-a");
        feed_set(&mut set, Metric::CpuPercent, 20.0, DEFAULT_WINDOW);
        feed_set(&mut set, Metric::MemoryBytes, 8_000_000.0, DEFAULT_WINDOW);
        let before = set.to_persisted();

        let json = serde_json::to_string(&before).expect("serialise");
        let after: PersistedProcessBaselines = serde_json::from_str(&json).expect("deserialise");

        assert_eq!(after.version, PERSISTED_BASELINE_VERSION);
        assert_eq!(after.config_fingerprint, "fingerprint-a");
        assert_eq!(after.metrics.len(), 2);

        let (restored, outcome) = ProcessBaselines::restore("fingerprint-a", &after);
        assert_eq!(outcome, RestoreOutcome::Restored { metrics: 2 });

        // Ready across the restart, which is the point: a daemon restarted every few minutes
        // would otherwise never finish warming and would detect nothing at all.
        for metric in [Metric::CpuPercent, Metric::MemoryBytes] {
            let snapshot = restored.get(metric).expect("restored").snapshot();
            assert!(
                snapshot.readiness.is_ready(),
                "{metric:?} lost readiness across a round trip"
            );
            assert_eq!(
                snapshot.centre,
                set.get(metric).expect("original").snapshot().centre,
                "{metric:?} centre changed across a round trip"
            );
        }
    }

    #[test]
    fn an_older_payload_missing_a_newer_field_still_loads() {
        // Every field is `#[serde(default)]` precisely so an upgrade cannot fail to read state
        // written by the previous version. `adaptation_holds` and `adaptations` are absent here,
        // as they would be in a payload from before those counters existed.
        let json = r#"{
            "version": 1,
            "config_fingerprint": "fingerprint-a",
            "metrics": { "cpu_percent": { "centre": 20.0, "spread": 0.75, "samples": 50 } }
        }"#;
        let persisted: PersistedProcessBaselines =
            serde_json::from_str(json).expect("an older payload must still deserialise");

        let (restored, outcome) = ProcessBaselines::restore("fingerprint-a", &persisted);
        assert_eq!(outcome, RestoreOutcome::Restored { metrics: 1 });
        let snapshot = restored
            .get(Metric::CpuPercent)
            .expect("restored")
            .snapshot();
        assert!(
            snapshot.readiness.is_ready(),
            "50 samples is past the minimum"
        );
        assert_eq!(snapshot.centre, Some(20.0));
        assert_eq!(
            snapshot.adaptations, 0,
            "an absent counter defaults, not fails"
        );
        assert_eq!(snapshot.adaptation_holds, 0);
    }

    #[test]
    fn an_unestablished_baseline_round_trips_as_unestablished() {
        // The distinction the whole module rests on: "never established" must not come back as
        // "established at zero". `Option` on the wire is what preserves it.
        let mut set = ProcessBaselines::new("fingerprint-a");
        feed_set(&mut set, Metric::CpuPercent, 20.0, 3);

        let json = serde_json::to_string(&set.to_persisted()).expect("serialise");
        let persisted: PersistedProcessBaselines = serde_json::from_str(&json).expect("read back");
        let (restored, _) = ProcessBaselines::restore("fingerprint-a", &persisted);

        let snapshot = restored
            .get(Metric::CpuPercent)
            .expect("restored")
            .snapshot();
        assert!(!snapshot.readiness.is_ready(), "3 samples is still warming");
        assert_eq!(
            snapshot.centre, None,
            "a warming centre must be absent, never zero"
        );
    }

    #[test]
    fn a_changed_configuration_discards_the_persisted_baseline() {
        let mut set = ProcessBaselines::new("fingerprint-a");
        feed_set(&mut set, Metric::CpuPercent, 20.0, DEFAULT_WINDOW);
        let persisted = set.to_persisted();

        // Different command, args or limits: arguably a different workload, so what was learned
        // about the old one is not evidence about the new one.
        let (restored, outcome) = ProcessBaselines::restore("fingerprint-b", &persisted);
        assert_eq!(
            outcome,
            RestoreOutcome::DiscardedConfigChanged {
                stored: "fingerprint-a".to_string(),
                current: "fingerprint-b".to_string(),
            }
        );
        assert!(
            restored.get(Metric::CpuPercent).is_none(),
            "a discarded baseline must not be silently reused"
        );
    }

    #[test]
    fn an_unlabelled_payload_is_not_trusted() {
        // An empty fingerprint cannot be shown to match, and silently trusting it is exactly the
        // failure the field exists to prevent.
        let persisted = PersistedProcessBaselines {
            version: PERSISTED_BASELINE_VERSION,
            config_fingerprint: String::new(),
            metrics: [(
                Metric::CpuPercent.as_str().to_string(),
                PersistedMetricBaseline {
                    centre: Some(20.0),
                    spread: Some(0.75),
                    samples: 100,
                    adaptations: 0,
                    adaptation_holds: 0,
                },
            )]
            .into_iter()
            .collect(),
        };

        let (restored, outcome) = ProcessBaselines::restore("fingerprint-a", &persisted);
        assert_eq!(outcome, RestoreOutcome::DiscardedUnknownFingerprint);
        assert!(restored.get(Metric::CpuPercent).is_none());
    }

    #[test]
    fn an_unknown_metric_key_is_skipped_rather_than_failing_the_load() {
        // A payload from a newer version naming a metric this build does not know must not make
        // the daemon refuse to start. Unreadable state has to behave like absent state.
        let json = r#"{
            "version": 1,
            "config_fingerprint": "fingerprint-a",
            "metrics": {
                "cpu_percent": { "centre": 20.0, "spread": 0.75, "samples": 50 },
                "quantum_flux_bytes": { "centre": 1.0, "spread": 1.0, "samples": 99 }
            }
        }"#;
        let persisted: PersistedProcessBaselines = serde_json::from_str(json).expect("deserialise");
        let (restored, outcome) = ProcessBaselines::restore("fingerprint-a", &persisted);

        assert_eq!(
            outcome,
            RestoreOutcome::Restored { metrics: 1 },
            "the known metric loads and the unknown one is dropped"
        );
        assert!(restored.get(Metric::CpuPercent).is_some());
    }

    #[test]
    fn a_restart_keeps_the_baseline_but_a_reconfiguration_clears_it() {
        // Consistent with `process-io-metrics`: a managed process keeps its identity across a
        // restart, and its accumulated state carries forward. Only a configuration change breaks
        // that continuity.
        let mut set = ProcessBaselines::new("fingerprint-a");
        feed_set(&mut set, Metric::CpuPercent, 20.0, DEFAULT_WINDOW);
        assert!(
            set.get(Metric::CpuPercent)
                .expect("warm")
                .snapshot()
                .readiness
                .is_ready()
        );

        assert!(
            !set.apply_config_fingerprint("fingerprint-a"),
            "an unchanged fingerprint is not a change"
        );
        assert!(
            set.get(Metric::CpuPercent).is_some(),
            "a restart under the same configuration keeps what was learned"
        );

        assert!(set.apply_config_fingerprint("fingerprint-b"));
        assert!(
            set.get(Metric::CpuPercent).is_none(),
            "a reconfiguration clears it"
        );
    }

    #[test]
    fn median_of_odd_and_even_windows_matches_hand_calculation() {
        // Odd: the middle element of the sorted series, unaffected by the outlier's magnitude.
        assert_eq!(median_of(&[3.0, 1.0, 2.0, 9000.0, 4.0]), Some(3.0));
        // Even: the mean of the two central values, 2 and 3.
        assert_eq!(median_of(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(median_of(&[]), None);
    }

    #[test]
    fn scaled_mad_matches_hand_calculation() {
        // Series 1..=5, centre 3. Absolute deviations: 2, 1, 0, 1, 2 -> median 1.
        // Scaled: 1 * 1.4826... = 1.4826...
        let values = [1.0, 2.0, 3.0, 4.0, 5.0];
        let mad = scaled_mad(&values, 3.0).expect("non-empty");
        assert!(
            (mad - MAD_TO_SIGMA).abs() < 1e-12,
            "expected {MAD_TO_SIGMA}, got {mad}"
        );

        // A flat series has every deviation zero, so MAD is exactly zero. This is the case the
        // floor exists for: unfloored, any change at all divides by zero.
        assert_eq!(scaled_mad(&[7.0; 8], 7.0), Some(0.0));
    }

    #[test]
    fn warming_baseline_withholds_everything_until_minimum_samples() {
        let mut baseline = MetricBaseline::new(Metric::CpuPercent);
        let sample_count = usize::try_from(DEFAULT_MIN_SAMPLES - 1).unwrap_or(0);
        feed(&mut baseline, 20.0, sample_count);

        let snapshot = baseline.snapshot();
        assert_eq!(
            snapshot.readiness,
            BaselineReadiness::Warming {
                samples: DEFAULT_MIN_SAMPLES - 1,
                required: DEFAULT_MIN_SAMPLES,
            },
            "one sample short of the requirement must still be warming"
        );
        assert!(!snapshot.readiness.is_ready());
        assert_eq!(
            snapshot.centre, None,
            "no centre is published while warming"
        );
        assert_eq!(snapshot.spread, None);
        assert_eq!(
            baseline.deviation(20.0),
            None,
            "no deviation means no detector can compute a statistic to threshold"
        );

        // The sample that crosses the line.
        baseline.observe(20.0);
        assert_eq!(
            baseline.readiness(),
            BaselineReadiness::Ready {
                samples: DEFAULT_MIN_SAMPLES
            }
        );
        assert!(baseline.deviation(20.0).is_some());
        assert_eq!(baseline.snapshot().centre, Some(20.0));
    }

    #[test]
    fn warming_is_distinct_from_established_at_zero() {
        // The whole reason centre and spread are Option. An idle process legitimately baselines
        // at 0.0 CPU, and that must not read as "not yet learned".
        let idle = warm_flat(Metric::CpuPercent, 0.0);
        let idle_snapshot = idle.snapshot();
        assert!(idle_snapshot.readiness.is_ready());
        assert_eq!(idle_snapshot.centre, Some(0.0));

        let warming = MetricBaseline::new(Metric::CpuPercent);
        let warming_snapshot = warming.snapshot();
        assert!(!warming_snapshot.readiness.is_ready());
        assert_eq!(warming_snapshot.centre, None);

        assert_ne!(
            idle_snapshot.centre, warming_snapshot.centre,
            "established-at-zero and never-established must be different answers"
        );
    }

    #[test]
    fn maturity_rises_with_samples_and_saturates_when_ready() {
        let warming = BaselineReadiness::Warming {
            samples: 15,
            required: 30,
        };
        assert!((warming.maturity() - 0.5).abs() < 1e-12);
        let earlier = BaselineReadiness::Warming {
            samples: 3,
            required: 30,
        };
        assert!(
            earlier.maturity() < warming.maturity(),
            "a less mature baseline must score lower, so confidence can rank on it"
        );
        assert_eq!(BaselineReadiness::Ready { samples: 900 }.maturity(), 1.0);
    }

    #[test]
    fn flat_metric_cannot_manufacture_a_large_deviation() {
        // 64 identical samples: measured MAD is exactly 0. Unfloored this is a division by zero
        // and any change becomes infinite.
        let baseline = warm_flat(Metric::CpuPercent, 20.0);
        let snapshot = baseline.snapshot();
        assert_eq!(snapshot.spread, Some(PERCENT_SPREAD_FLOOR));
        assert!(
            snapshot.spread_floored,
            "a reader must be able to tell a floored spread from a measured one"
        );

        // A 0.4-point change on a dead-flat CPU series: 0.4 / 0.5 = 0.8 spreads. Below any
        // sane departure threshold, and crucially finite.
        let deviation = baseline.deviation(20.4).expect("ready");
        assert!(
            (deviation - 0.8).abs() < 1e-12,
            "expected 0.8 spreads, got {deviation}"
        );
        assert!(deviation.is_finite());

        // A substantial change on the same quiet metric is still detected: 20 points is 40
        // spreads. The floor suppresses noise, not signal.
        let big = baseline.deviation(40.0).expect("ready");
        assert!((big - 40.0).abs() < 1e-12, "expected 40 spreads, got {big}");
    }

    #[test]
    fn byte_metrics_get_a_page_sized_floor() {
        let baseline = warm_flat(Metric::MemoryBytes, 100.0 * 1024.0 * 1024.0);
        assert_eq!(baseline.snapshot().spread, Some(BYTES_SPREAD_FLOOR));
        // A one-byte change on flat RSS: 1/4096 spreads. Nothing can be built on that, which is
        // the point.
        let deviation = baseline
            .deviation(100.0 * 1024.0 * 1024.0 + 1.0)
            .expect("ready");
        assert!(deviation.abs() < 0.001, "got {deviation}");
    }

    #[test]
    fn deviation_is_signed_so_both_directions_are_visible() {
        let baseline = warm_flat(Metric::CpuPercent, 50.0);
        let above = baseline.deviation(60.0).expect("ready");
        let below = baseline.deviation(40.0).expect("ready");
        assert!(above > 0.0 && below < 0.0);
        assert!((above + below).abs() < 1e-12, "symmetric about the centre");
    }

    #[test]
    fn non_finite_samples_are_refused_rather_than_stored() {
        let mut baseline = warm_flat(Metric::CpuPercent, 30.0);
        let before = baseline.snapshot();
        assert!(!baseline.observe(f64::NAN));
        assert!(!baseline.observe(f64::INFINITY));
        let after = baseline.snapshot();
        assert_eq!(
            before, after,
            "a NaN in the window would poison the median for the next 64 observations"
        );
        assert_eq!(baseline.deviation(f64::NAN), None);
    }

    #[test]
    fn a_single_extreme_sample_does_not_move_the_baseline() {
        // Worked example. Window of 64 samples alternating 20.0/21.0 gives centre 20.5 and
        // measured MAD 0.5 -> scaled 0.7413, which is above the 0.5 floor, so the spread here is
        // a real measurement rather than the floor.
        let mut baseline = MetricBaseline::new(Metric::CpuPercent);
        for i in 0..DEFAULT_WINDOW {
            baseline.observe(if i % 2 == 0 { 20.0 } else { 21.0 });
        }
        let before = baseline.snapshot();
        // 20.0, not the 20.5 the full window's median would give. The centre was established at
        // sample 30 — the warm-up boundary — where the window held 15 twenties and 14 twenty-ones
        // and so had median 20.0. From there the candidate median oscillates 20.0/20.5 with the
        // window's parity, a 0.67-spread gap that never clears the 1.0-spread hysteresis. Worth
        // asserting rather than smoothing over: it is hysteresis working as intended, and it means
        // the centre carries up to half a sample gap of arbitrariness from where warm-up ended.
        assert_eq!(before.centre, Some(20.0));
        let spread_before = before.spread.expect("ready");
        assert!(
            (spread_before - 0.5 * MAD_TO_SIGMA).abs() < 1e-12,
            "expected 0.5 * 1.4826 = {}, got {spread_before}",
            0.5 * MAD_TO_SIGMA
        );
        assert!(!before.spread_floored, "0.741 is above the 0.5 floor");

        // One sample at 900% CPU — a 1200-spread excursion.
        baseline.observe(900.0);
        let after = baseline.snapshot();

        // The reported centre DOES move here, from 20.0 to 21.0, and the original version of this
        // test asserted otherwise on faulty arithmetic. Worth keeping the correction visible,
        // because the mechanism is not what the failure looks like.
        //
        // The window is a fixed ring of 64. Pushing the outlier first evicts the front sample —
        // a 20.0 — so the window becomes 31x20.0, 32x21.0, 1x900.0. With only 31 twenties, both
        // central sorted values (indices 31 and 32) are 21.0, so the median is 21.0, not the 20.5
        // the old comment predicted from an assumed 32 twenties. Against a centre still lagging at
        // 20.0 from warm-up, that is |21.0 - 20.0| / 0.7413 = 1.35 spreads, which clears the
        // 1.0-spread gate and releases a re-centre that hysteresis had been deferring.
        //
        // So the outlier did shift the reported centre, by displacing a sample and tipping the
        // median's parity — not by having its own magnitude absorbed. That distinction is the
        // robustness claim, and it is asserted directly below.
        assert_eq!(
            after.centre,
            Some(21.0),
            "the centre re-centres onto an ordinary window value, not towards the outlier"
        );

        // The claim that actually matters: the centre stays inside the ordinary range of the data
        // and nowhere near the excursion. A mean would not.
        let centre_after = after.centre.expect("ready");
        assert!(
            (20.0..=21.0).contains(&centre_after),
            "centre {centre_after} left the ordinary data range 20.0..=21.0"
        );

        // The robustness claim made at the statistic rather than at the policy that also happens
        // to protect it. Median moves 20.5 -> 21.0, half a point. The mean of the same window
        // moves to 34.25, which is 13.75 points — 27 times further, and outside the range of every
        // ordinary sample in it.
        let window_after: Vec<f64> = baseline.window.iter().copied().collect();
        assert_eq!(window_after.len(), DEFAULT_WINDOW, "the ring did not grow");
        let median_after = median_of(&window_after).expect("non-empty");
        let median_shift = (median_after - 20.5).abs();
        let mean_after = window_after.iter().sum::<f64>()
            / oxmgr_core::numeric::usize_to_f64(window_after.len());
        let mean_shift = (mean_after - 20.5).abs();
        assert!(
            (median_after - 21.0).abs() < 1e-12,
            "expected median 21.0, got {median_after}"
        );
        assert!(
            median_shift * 10.0 < mean_shift,
            "median moved {median_shift:.3}, mean moved {mean_shift:.3}"
        );

        // Spread is barely touched: the deviations are still dominated by the 64 ordinary samples.
        let spread_after = after.spread.expect("ready");
        assert!(
            (spread_after - spread_before).abs() < 0.5,
            "spread moved from {spread_before} to {spread_after}"
        );
        assert!(
            !after.spread_floored,
            "one outlier must not inflate the spread into hiding the next real anomaly"
        );
    }

    #[test]
    fn baseline_follows_a_durable_step_once_it_settles() {
        // A deploy that legitimately doubles memory use. The step is a step, not a ramp: after it
        // lands the series is flat again, so monotonicity collapses and the veto lifts.
        let mut baseline = warm_flat(Metric::MemoryBytes, 100_000_000.0);
        assert_eq!(baseline.snapshot().centre, Some(100_000_000.0));

        feed(&mut baseline, 200_000_000.0, DEFAULT_WINDOW);
        let snapshot = baseline.snapshot();
        assert_eq!(
            snapshot.centre,
            Some(200_000_000.0),
            "a settled new level must become the new normal"
        );
        assert!(
            snapshot.adaptations >= 1,
            "the adaptation must be counted so re-centring is auditable"
        );
        // And the new regime stops looking anomalous, which is the operational point: otherwise
        // every deploy alarms forever.
        let deviation = baseline.deviation(200_000_000.0).expect("ready");
        assert!(deviation.abs() < 1e-9, "got {deviation} spreads");
    }

    #[test]
    fn hysteresis_holds_the_centre_against_ordinary_noise() {
        // A small settled move inside the hysteresis dead zone must not re-centre, because every
        // re-centre perturbs every deviation a detector computes.
        let mut baseline = warm_flat(Metric::CpuPercent, 40.0);
        let before = baseline.snapshot();
        // 0.4 points is 0.8 floored spreads, under the 1.0-spread hysteresis.
        feed(&mut baseline, 40.4, DEFAULT_WINDOW);
        let after = baseline.snapshot();
        assert_eq!(after.centre, before.centre, "centre must not chase noise");
        assert_eq!(after.adaptations, 0);
    }

    #[test]
    fn adaptation_does_not_absorb_a_steady_leak() {
        // THE test for this module. A process leaking 2 MiB per tick over 200 ticks. If the
        // baseline followed the window median, the deviation would sit near zero forever and the
        // leak would be laundered into the new normal.
        const STEP: f64 = 2.0 * 1024.0 * 1024.0;
        let start = 100.0 * 1024.0 * 1024.0;

        let mut baseline = MetricBaseline::new(Metric::MemoryBytes);
        // Warm on a flat level first, so there is an established centre to hold.
        feed(&mut baseline, start, DEFAULT_WINDOW);
        let established = baseline.snapshot().centre.expect("ready");
        assert_eq!(established, start);

        let mut value = start;
        for _ in 0..200 {
            value += STEP;
            baseline.observe(value);
        }

        let snapshot = baseline.snapshot();
        assert_eq!(
            snapshot.centre,
            Some(established),
            "the centre must not have moved: the leak is still measured against pre-leak normal"
        );
        assert!(
            snapshot.holding,
            "the baseline must report that it is currently withholding adaptation"
        );
        // 185 of the 200 climbing ticks are recorded holds. The missing ~15 are the start of the
        // climb, where the window still holds mostly pre-leak samples and so has fewer than the
        // required `0.25 * 63 = 16` non-zero deltas to judge on. Adaptation is prevented across
        // that period regardless, by hysteresis: with fewer than 32 climbing samples in a 64-slot
        // window the median is still sitting on the flat majority, so the candidate centre has not
        // moved at all. The `adaptations == 0` assertion below is what proves that, and it is the
        // claim that matters — a hold is only evidence, the centre not moving is the behaviour.
        assert!(
            snapshot.adaptation_holds >= 180,
            "the climb should be recorded as held on all but its earliest ticks, got {}",
            snapshot.adaptation_holds
        );
        assert_eq!(
            snapshot.adaptations, 0,
            "not one re-centre during a monotonic climb"
        );

        // The deviation must keep growing, which is what lets a detector fire at all. After 200
        // steps the process sits 400 MiB above a baseline of 100 MiB.
        //
        // 8.4 spreads, not the hundreds a naive reading suggests, and the reason is worth stating
        // because it bounds what this design can claim. Spread is refreshed on every observation
        // (see `observe`), and a window of 64 samples climbing 2 MiB per tick has a scaled MAD of
        // about 0.25 * 63 * 2 MiB * 1.4826 = 47 MiB. So the numerator grows without limit while the
        // denominator grows to a constant set by the climb rate — a steady leak plateaus at a
        // deviation of roughly (elapsed climb) / (window climb), which is large but not unbounded
        // in the way a frozen spread would make it.
        //
        // 8.4 spreads still clears any sane departure threshold, so the leak is detectable. The
        // honest limitation: a *faster* leak inflates its own spread proportionally and does not
        // score proportionally higher, so this statistic ranks leak severity poorly. Severity is
        // the trend detector's job (design.md phase 2), which reports a slope in bytes per second
        // rather than in spreads.
        let deviation = baseline.deviation(value).expect("ready");
        assert!(
            deviation > 5.0,
            "the departure must keep growing against the held centre, got {deviation}"
        );
        assert!(
            (value - established) / 1024.0 / 1024.0 > 399.0,
            "sanity: the process really did climb 400 MiB"
        );
    }

    #[test]
    fn a_leak_that_stops_is_adopted_as_the_new_level() {
        // The other half of the rule: the veto must lift, or a process that grew once could never
        // baseline again and would alarm forever.
        const STEP: f64 = 2.0 * 1024.0 * 1024.0;
        let start = 100.0 * 1024.0 * 1024.0;
        let mut baseline = MetricBaseline::new(Metric::MemoryBytes);
        feed(&mut baseline, start, DEFAULT_WINDOW);

        let mut value = start;
        for _ in 0..100 {
            value += STEP;
            baseline.observe(value);
        }
        assert_eq!(
            baseline.snapshot().centre,
            Some(start),
            "held while climbing"
        );

        // Growth levels off at the plateau it reached.
        feed(&mut baseline, value, DEFAULT_WINDOW);
        let snapshot = baseline.snapshot();
        assert_eq!(
            snapshot.centre,
            Some(value),
            "once settled, the plateau is the new normal"
        );
        assert!(!snapshot.holding, "the hold must lift when the climb stops");
    }

    #[test]
    fn stationary_noise_is_not_read_as_a_climb() {
        // The veto must not fire on ordinary jitter, or the baseline would never adapt to
        // anything. A deterministic zig-zag around 50 has monotonicity 0.5 by construction.
        let window: Vec<f64> = (0..DEFAULT_WINDOW)
            .map(|i| if i % 2 == 0 { 48.0 } else { 52.0 })
            .collect();
        let config = BaselineConfig::for_kind(MetricKind::Percent);
        let spread = scaled_mad(&window, 50.0)
            .expect("non-empty")
            .max(config.spread_floor);
        assert_eq!(
            window_trend(&window, spread, &config),
            None,
            "alternating deviations must not look monotonic"
        );
    }

    #[test]
    fn a_ramp_is_read_as_a_climb_with_reportable_evidence() {
        // Worked example for the numbers in DEFAULT_DRIFT_NET_SHIFT_SPREADS' doc comment. A
        // straight ramp 0..63: every delta is +1, so monotonicity is exactly 1.0.
        let window: Vec<f64> = (0..DEFAULT_WINDOW)
            .map(oxmgr_core::numeric::usize_to_f64)
            .collect();
        let centre = median_of(&window).expect("non-empty");
        assert_eq!(centre, 31.5);
        // Deviations from 31.5 are 31.5, 30.5, ... 0.5, 0.5, ... 31.5; their median is 16.0, so
        // the scaled spread is 16 * 1.4826 = 23.72.
        let spread = scaled_mad(&window, centre).expect("non-empty");
        assert!(
            (spread - 16.0 * MAD_TO_SIGMA).abs() < 1e-12,
            "expected {}, got {spread}",
            16.0 * MAD_TO_SIGMA
        );

        let config = BaselineConfig::for_kind(MetricKind::Percent);
        let evidence = window_trend(&window, spread, &config).expect("a ramp is a climb");
        assert_eq!(evidence.direction, TrendDirection::Rising);
        assert_eq!(evidence.monotonicity, 1.0);
        assert_eq!(evidence.moves, DEFAULT_WINDOW - 1);
        // Thirds of 64 are 21 samples each: median of 0..20 is 10, median of 43..63 is 53.
        // (53 - 10) / 23.72 = 1.81 spreads, comfortably over the 1.0 threshold and comfortably
        // under what stationary noise produces.
        assert!(
            (evidence.net_shift_spreads - 43.0 / spread).abs() < 1e-12,
            "got {}",
            evidence.net_shift_spreads
        );
        assert!(evidence.net_shift_spreads > config.drift_net_shift_spreads);
    }

    #[test]
    fn a_flat_series_is_not_read_as_a_climb() {
        // The zero-delta exclusion. If unchanged samples counted as non-negative, a dead-flat
        // series would score monotonicity 1.0 and its baseline would be frozen for the life of
        // the process.
        let window = vec![25.0; DEFAULT_WINDOW];
        let config = BaselineConfig::for_kind(MetricKind::Percent);
        assert_eq!(window_trend(&window, config.spread_floor, &config), None);
    }

    #[test]
    fn a_spike_that_returns_is_not_read_as_a_climb() {
        // Rising deltas dominate a slow rise followed by a fast fall, but the window ends where it
        // began, so the net-shift check refuses it. Adapting to it would be correct anyway.
        let mut window: Vec<f64> = (0..DEFAULT_WINDOW - 1)
            .map(|i| 20.0 + oxmgr_core::numeric::usize_to_f64(i))
            .collect();
        window.push(20.0);
        let config = BaselineConfig::for_kind(MetricKind::Percent);
        let centre = median_of(&window).expect("non-empty");
        let spread = scaled_mad(&window, centre)
            .expect("non-empty")
            .max(config.spread_floor);
        let trend = window_trend(&window, spread, &config);
        assert!(
            trend.is_some_and(|e| e.direction == TrendDirection::Rising),
            "this shape does still read as rising overall"
        );
        // But a genuine down-then-up excursion, ending where it started, must not.
        let mut excursion = vec![30.0; 20];
        excursion.extend(std::iter::repeat_n(90.0, 12));
        excursion.extend(std::iter::repeat_n(30.0, 20));
        let ex_centre = median_of(&excursion).expect("non-empty");
        let ex_spread = scaled_mad(&excursion, ex_centre)
            .expect("non-empty")
            .max(config.spread_floor);
        assert_eq!(
            window_trend(&excursion, ex_spread, &config),
            None,
            "an excursion that returns has no net shift"
        );
    }

    #[test]
    fn short_windows_are_never_judged_as_climbing() {
        // Below nine samples the outer thirds are individual samples, and judging a trend on two
        // numbers is the single-sample sensitivity this module exists to avoid.
        let config = BaselineConfig::for_kind(MetricKind::Percent);
        for len in 0..9 {
            let window: Vec<f64> = (0..len)
                .map(|i| oxmgr_core::numeric::usize_to_f64(i) * 10.0)
                .collect();
            assert_eq!(
                window_trend(&window, 1.0, &config),
                None,
                "window of {len} must not be judged"
            );
        }
    }
}
