//! The findings contract: what a detector emits, its identity, lifecycle, and confidence.
//!
//! Lint-level cleanup: display-path casts in finding rendering and forecast formatting.
//!
//! Scaffold for OpenSpec change `process-intelligence`, tasks section(s) 7.
//! The contract is `openspec/changes/process-intelligence/specs/`; read it before adding to this file.
//!
//! Pure logic on purpose: nothing here reaches for the process manager, the HTTP layer or the
//! dashboard, so it can be unit-tested without a running daemon. Wiring it into those is a
//! separate integration step, which is why the module is currently unreferenced.
//!
//! # Identity
//!
//! A finding is identified by [`FindingKey`] — the managed process, the detector, the metric, and
//! an optional variant discriminator. Nothing time-dependent and nothing magnitude-dependent is in
//! the key, which is the whole point: the daemon re-evaluates every detector on a 2s maintenance
//! tick, so a key derived from the observed value or the evaluation time would mint a new finding
//! every tick and the findings list would be a log rather than a state. Because the key describes
//! only *what the condition is*, the second observation of one condition lands on the same key and
//! [`FindingRegistry::observe`] reports [`Transition::Held`] instead of raising again.
//!
//! Episodes are separated by [`Finding::occurrence`], not by the key: clearing a finding leaves the
//! per-key occurrence counter behind, so a condition that recurs after clearing raises occurrence
//! 2 with a fresh `id` and a fresh `raised_at_unix`. A consumer therefore distinguishes new from
//! ongoing by the transition it was handed, or — reading a snapshot cold — by `id`/`occurrence`
//! and `raised_at_unix`, which do not move while a finding is held.
//!
//! # Absent is not zero
//!
//! Following `host_metrics`, every quantity a detector may genuinely not have is `Option` and is
//! omitted from the JSON rather than serialised as 0: a fitted trend with `fit_quality: Some(0.0)`
//! describes the data terribly, whereas `None` means no trend was fitted at all. The same holds
//! for the confidence terms — an absent term is dropped from the weighted mean, never scored zero,
//! since "not applicable" must not read as "no support".

use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::BTreeSet;
use std::fmt;

use serde::de::Deserializer;
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::numeric::format_duration_compact;

/// Why a finding could not be constructed.
///
/// Construction is fallible because requirement *Every finding carries structured evidence* makes
/// evidence mandatory: "a finding without evidence SHALL NOT be produced". A detector that cannot
/// show its work gets an error rather than a finding with an empty body.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum FindingError {
    /// No samples were referenced, so an operator cannot check the finding by hand.
    #[error("evidence references no samples: a finding must name the samples it was derived from")]
    NoSamples,
    /// A window that covers no samples, or ends before it starts.
    #[error("evidence window is not a real window: {reason}")]
    InvalidWindow { reason: &'static str },
    /// `NaN` or an infinity reached a field that is compared or ranked.
    #[error("evidence field `{field}` is not finite: {value}")]
    NonFinite { field: &'static str, value: f64 },
    /// A threshold of zero (or negative) cannot be exceeded by a factor, so no exceedance term
    /// could be derived and the confidence score would be meaningless.
    #[error("evidence threshold must be positive, got {threshold}")]
    NonPositiveThreshold { threshold: f64 },
}

/// Which detector produced a finding. Part of the identity, so it must be stable across ticks.
///
/// The wire form is a snake_case string. `Other` preserves an unrecognised name verbatim instead of
/// collapsing it to a catch-all: findings are persisted and served over HTTP, and two different
/// future detectors folded into one variant would silently share an identity. Construct through
/// [`Detector::from_wire`] so a name that matches a known detector normalises to that variant.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Detector {
    /// Robust z-score: the metric sits far from its baseline centre for consecutive samples.
    LevelDeparture,
    /// EWMA: gradual movement a windowed baseline would absorb.
    Drift,
    /// CUSUM: a shift too small to depart, sustained long enough to matter.
    SustainedShift,
    /// Adjacent-window comparison: a step to a new sustained regime, not an excursion.
    ChangePoint,
    /// Significant, well-fitted, predominantly monotonic growth.
    ResourceLeak,
    /// Repeated failing exits inside the crash-loop window, from `failure_patterns`.
    CrashLoop,
    /// Failure rate in the recent half of the window exceeding the earlier half.
    RestartAcceleration,
    /// The same failing exit status, repeatedly: a deterministic fault.
    RepeatedExit,
    /// Several distinct processes failed inside the storm window. Keyed under the namespace.
    RestartStorm,
    /// A declared dependency failed shortly before this process did. NOT a causal claim.
    DependencyCorrelation,
    /// A detector this build does not know, kept under its own name.
    Other(String),
}

impl Detector {
    /// The canonical wire name.
    pub fn as_wire(&self) -> &str {
        match self {
            Self::LevelDeparture => "level_departure",
            Self::Drift => "drift",
            Self::SustainedShift => "sustained_shift",
            Self::ChangePoint => "change_point",
            Self::ResourceLeak => "resource_leak",
            Self::CrashLoop => "crash_loop",
            Self::RestartAcceleration => "restart_acceleration",
            Self::RepeatedExit => "repeated_exit",
            Self::RestartStorm => "restart_storm",
            Self::DependencyCorrelation => "dependency_correlation",
            Self::Other(name) => name.as_str(),
        }
    }

    /// Parses a wire name, normalising known detectors so `Other("drift")` cannot exist alongside
    /// [`Detector::Drift`] and split one condition into two identities.
    pub fn from_wire(raw: &str) -> Self {
        match raw {
            "level_departure" => Self::LevelDeparture,
            "drift" => Self::Drift,
            "sustained_shift" => Self::SustainedShift,
            "change_point" => Self::ChangePoint,
            "resource_leak" => Self::ResourceLeak,
            "crash_loop" => Self::CrashLoop,
            "restart_acceleration" => Self::RestartAcceleration,
            "repeated_exit" => Self::RepeatedExit,
            "restart_storm" => Self::RestartStorm,
            "dependency_correlation" => Self::DependencyCorrelation,
            other => Self::Other(other.to_string()),
        }
    }
}

impl fmt::Display for Detector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

impl Serialize for Detector {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_wire())
    }
}

impl<'de> Deserialize<'de> for Detector {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::from_wire(&raw))
    }
}

/// Which metric a finding is about. Part of the identity.
///
/// Same string-with-passthrough treatment as [`Detector`], for the same reason: a metric this build
/// does not know keeps its name rather than becoming indistinguishable from another unknown.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Metric {
    CpuPercent,
    MemoryBytes,
    DiskReadBytes,
    DiskWriteBytes,
    RestartRate,
    /// A metric this build does not know, kept under its own name.
    Other(String),
}

impl Metric {
    /// The canonical wire name.
    pub fn as_wire(&self) -> &str {
        match self {
            Self::CpuPercent => "cpu_percent",
            Self::MemoryBytes => "memory_bytes",
            Self::DiskReadBytes => "disk_read_bytes",
            Self::DiskWriteBytes => "disk_write_bytes",
            Self::RestartRate => "restart_rate",
            Self::Other(name) => name.as_str(),
        }
    }

    /// Parses a wire name, normalising known metrics.
    pub fn from_wire(raw: &str) -> Self {
        match raw {
            "cpu_percent" => Self::CpuPercent,
            "memory_bytes" => Self::MemoryBytes,
            "disk_read_bytes" => Self::DiskReadBytes,
            "disk_write_bytes" => Self::DiskWriteBytes,
            "restart_rate" => Self::RestartRate,
            other => Self::Other(other.to_string()),
        }
    }
}

impl fmt::Display for Metric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

impl Serialize for Metric {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_wire())
    }
}

impl<'de> Deserialize<'de> for Metric {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::from_wire(&raw))
    }
}

/// The stable identity of a condition.
///
/// Four parts, and deliberately nothing else. The process, because findings are per process and are
/// cleared when it is deleted. The detector and the metric, because "memory is leaking" and "memory
/// departed its baseline" are different claims about the same series and must not overwrite each
/// other. And `variant`, for a detector that can hold two distinguishable conditions on one metric
/// at once — `level_departure` uses it for direction, since a metric sitting above its baseline and
/// one sitting below are different conditions, not one condition that moved.
///
/// What is **not** in the key is the load-bearing decision: no timestamp, no observed value, no
/// confidence, no evaluation counter. Detection runs on the daemon's 2s maintenance tick, so any of
/// those would produce a fresh identity on every tick — the "reappears every 2s with a new id"
/// failure. Because the key names only the condition, the second tick of one condition maps onto the
/// existing finding and is reported as [`Transition::Held`].
///
/// The process is named rather than numbered: the name is what an operator, the HTTP surface and any
/// per-process suppression all use, and it survives a process restart, where the retained-history
/// and baseline state for that name is what detection continues from. A delete clears the findings
/// for that name ([`FindingRegistry::clear_process`]), so a later process reusing the name cannot
/// inherit a stale identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FindingKey {
    pub process: String,
    pub detector: Detector,
    pub metric: Metric,
    /// Discriminates two concurrent conditions from one detector on one metric. `None` when the
    /// detector can only hold one at a time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

impl FindingKey {
    /// A key with no variant.
    pub fn new(process: impl Into<String>, detector: Detector, metric: Metric) -> Self {
        Self {
            process: process.into(),
            detector,
            metric,
            variant: None,
        }
    }

    /// Adds the variant discriminator.
    pub fn with_variant(mut self, variant: impl Into<String>) -> Self {
        self.variant = Some(variant.into());
        self
    }

    /// The key rendered as a stable string, for logs and for [`Finding::id`].
    pub fn as_string(&self) -> String {
        match &self.variant {
            Some(variant) => format!(
                "{}/{}/{}/{}",
                self.process, self.detector, self.metric, variant
            ),
            None => format!("{}/{}/{}", self.process, self.detector, self.metric),
        }
    }
}

impl fmt::Display for FindingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_string())
    }
}

/// Which way the metric moved relative to what was expected.
///
/// Reported because *Departure in either direction is detected*: a finding that says only "far from
/// baseline" leaves an operator unable to tell a memory climb from a worker pool that has gone
/// quiet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Above,
    Below,
}

/// The span of history a detector examined.
///
/// Both bounds are Unix seconds, and `sample_count` is the number of retained samples inside them —
/// not the number of seconds, since the tiered history is downsampled and a one-hour window may hold
/// 60 aggregates rather than 1800 raw samples.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceWindow {
    pub start_unix: u64,
    pub end_unix: u64,
    pub sample_count: u32,
}

/// One sample behind a finding, so the arithmetic can be redone by hand.
///
/// Detectors are incremental and do not keep the whole window, so this is the subset they cite:
/// enough to check the comparison, not a copy of retention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SampleRef {
    pub at_unix: u64,
    pub value: f64,
}

impl SampleRef {
    pub fn new(at_unix: u64, value: f64) -> Self {
        Self { at_unix, value }
    }
}

/// The baseline state the comparison was made against.
///
/// `min_samples` travels with it so baseline maturity is *derivable from the evidence* rather than
/// dependent on whatever configuration happens to be loaded when the finding is read back. That is
/// what makes *A finding can be recomputed from its evidence* hold for the confidence score too.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaselineSummary {
    /// Robust centre (median).
    pub center: f64,
    /// Robust spread (scaled MAD), after the flat-series floor has been applied.
    pub spread: f64,
    /// Samples the baseline has observed.
    pub sample_count: u32,
    /// Samples required before the baseline is trusted.
    pub min_samples: u32,
}

impl BaselineSummary {
    /// Whether the baseline has observed enough samples to be trusted.
    ///
    /// A detector must not emit a baseline-dependent finding while this is false; the type cannot
    /// enforce that, so [`Finding`] construction rejects it — see [`FindingBuilder::build`].
    pub fn is_ready(&self) -> bool {
        self.min_samples > 0 && self.sample_count >= self.min_samples
    }
}

/// The fitted trend behind a leak or forecast.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrendFit {
    /// Slope in metric units per second. Signed: a falling metric has a negative slope.
    pub slope_per_second: f64,
    /// Coefficient of determination, 0 to 1. How well the line describes the data.
    pub fit_quality: f64,
    /// Fraction of sample-to-sample deltas that are non-negative, 0 to 1. `None` when the detector
    /// does not compute it — which is not the same as "no monotonicity", so it is not 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monotonicity_ratio: Option<f64>,
    /// Standard error of the slope, for significance. `None` when not computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slope_std_error: Option<f64>,
}

/// What a forecast is conditional on. Serialised so the wire form carries the caveat rather than
/// relying on a consumer having read the docs: *A forecast is stated as conditional*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForecastAssumption {
    /// The outcome if the currently fitted trend continues unchanged.
    CurrentTrendContinues,
}

/// Time until the fitted trend would reach a threshold.
///
/// The interval is mandatory, not optional: a bare point estimate reads as a prediction, and linear
/// extrapolation of a leak is not one. A detector with too poor a fit to bound the estimate omits
/// the whole forecast instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThresholdForecast {
    /// The threshold value being approached, in metric units.
    pub threshold_value: f64,
    /// Seconds until the fitted line reaches it.
    pub eta_seconds: f64,
    /// Earliest and latest arrival across the prediction interval.
    pub interval_low_seconds: f64,
    pub interval_high_seconds: f64,
    pub assumes: ForecastAssumption,
}

/// How many consecutive samples agreed with the condition, against how many were required.
///
/// Both halves are needed: 5 agreeing samples is strong when 3 were required and merely sufficient
/// when 5 were, and the confidence term is the ratio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agreement {
    pub consecutive_samples: u32,
    pub required_samples: u32,
}

/// Everything needed to check a finding by hand.
///
/// Mandatory in full — this is the evidence requirement's "the detector that produced it, the metric
/// and window examined, the observed and expected values, the threshold applied, and references to
/// the samples behind it". The detector and metric are not repeated here; they live in
/// [`FindingKey`], which every [`Finding`] carries, so there is one authority for them rather than
/// two fields that can disagree.
///
/// `statistic` and `threshold` are the comparison the detector actually made, in the detector's own
/// units: a z-score against a z threshold, a CUSUM accumulation against its limit, a slope
/// t-statistic against its cut-off. `observed`/`expected` are the same event in metric units, which
/// is what an operator reads. Keeping both is deliberate: metric units alone cannot say how far past
/// the line the statistic sat, and the statistic alone is not something anyone can sanity-check
/// against a dashboard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    /// The window of history examined.
    pub window: EvidenceWindow,
    /// The value the detector fired on, in metric units.
    pub observed: f64,
    /// What was expected instead, in metric units. Typically the baseline centre, or the EWMA.
    pub expected: f64,
    /// The detector's own test statistic. Non-negative by convention: direction is carried by
    /// [`Evidence::direction`], not by the sign here, so exceedance arithmetic needs no special case.
    pub statistic: f64,
    /// The value of `statistic` at which the detector fires. Must be positive.
    pub threshold: f64,
    /// Which way the metric moved. `None` for a detector where direction is not meaningful.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<Direction>,
    /// Consecutive-sample agreement. `None` for a detector with no persistence requirement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agreement: Option<Agreement>,
    /// The baseline compared against. `None` for a detector that uses none — a leak test needs no
    /// baseline. Absent means "not baseline-derived", never "an immature baseline".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<BaselineSummary>,
    /// The fitted trend, where one applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trend: Option<TrendFit>,
    /// Time-to-threshold, where a threshold is known and the fit supports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forecast: Option<ThresholdForecast>,
    /// The samples the finding rests on. Never empty: a finding whose samples cannot be named is not
    /// checkable, and construction rejects it.
    pub samples: Vec<SampleRef>,
    /// Detector-specific extras that do not deserve a field on this struct. A `BTreeMap` so the
    /// serialised order is deterministic, which the determinism requirement needs — a `HashMap`
    /// would emit the same evidence in a different key order between runs.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, f64>,
}

impl Evidence {
    /// How far past its threshold the statistic sat, as a ratio: 1.0 is exactly at the threshold,
    /// 2.0 is twice it.
    pub fn exceedance_ratio(&self) -> f64 {
        self.statistic / self.threshold
    }

    fn validate(&self) -> Result<(), FindingError> {
        if self.samples.is_empty() {
            return Err(FindingError::NoSamples);
        }
        if self.window.sample_count == 0 {
            return Err(FindingError::InvalidWindow {
                reason: "window covers no samples",
            });
        }
        if self.window.end_unix < self.window.start_unix {
            return Err(FindingError::InvalidWindow {
                reason: "window ends before it starts",
            });
        }
        check_finite("observed", self.observed)?;
        check_finite("expected", self.expected)?;
        check_finite("statistic", self.statistic)?;
        check_finite("threshold", self.threshold)?;
        if self.threshold <= 0.0 {
            return Err(FindingError::NonPositiveThreshold {
                threshold: self.threshold,
            });
        }
        for sample in &self.samples {
            check_finite("samples[].value", sample.value)?;
        }
        if let Some(trend) = &self.trend {
            check_finite("trend.slope_per_second", trend.slope_per_second)?;
            check_finite("trend.fit_quality", trend.fit_quality)?;
        }
        if let Some(forecast) = &self.forecast {
            check_finite("forecast.eta_seconds", forecast.eta_seconds)?;
        }
        Ok(())
    }
}

fn check_finite(field: &'static str, value: f64) -> Result<(), FindingError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(FindingError::NonFinite { field, value })
    }
}

/// The weight of each confidence term, fixed and documented.
///
/// Weights are compile-time constants rather than configuration precisely so that the same evidence
/// scores the same on every daemon: a tunable weight would make confidence incomparable between two
/// hosts, and unreproducible from persisted evidence after a config change.
const WEIGHT_EXCEEDANCE: f64 = 0.4;
const WEIGHT_AGREEMENT: f64 = 0.3;
const WEIGHT_MATURITY: f64 = 0.2;
const WEIGHT_FIT: f64 = 0.1;

/// Exceedance ratio that saturates the exceedance term. At 3x the threshold the term is 1.0 and
/// further excess adds nothing — the difference between 3x and 30x is not more certainty that the
/// condition is real, it is a bigger anomaly, which the evidence already reports.
const EXCEEDANCE_SATURATION: f64 = 3.0;

/// Agreement multiple that saturates the agreement term: twice the required consecutive samples.
const AGREEMENT_SATURATION: f64 = 2.0;

/// Baseline sample multiple that saturates the maturity term: four times the minimum.
const MATURITY_SATURATION: f64 = 4.0;

/// The named terms behind a confidence score, each in 0..=1.
///
/// Reported alongside the score because *Confidence terms are visible*. A term is `None` when it does
/// not apply to the detector — no baseline means no maturity term — and an absent term is dropped
/// from the weighted mean rather than scored 0. Scoring it 0 would penalise a leak finding for not
/// having a baseline it never needed, and "unknown" must not read as "no support".
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ConfidenceTerms {
    /// How far past threshold the statistic sat: `(ratio - 1) / (saturation - 1)`, clamped to 0..=1.
    /// Always present — every finding has a statistic and a threshold.
    pub exceedance: f64,
    /// Consecutive agreement against what was required, saturating at twice required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agreement: Option<f64>,
    /// Baseline samples against the minimum, saturating at four times the minimum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maturity: Option<f64>,
    /// Fit quality of the trend, where one was fitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit: Option<f64>,
}

/// A confidence score in 0..=1 with the terms that produced it.
///
/// # What the number means
///
/// It is a fixed weighted mean of the applicable terms below, and nothing more. It is **not** a
/// probability that the finding is a true positive; the daemon has no way to calibrate that, and
/// presenting one would be false precision. 0.5 means "the applicable terms averaged halfway to
/// saturation" — for a level departure with a baseline that means, roughly, the statistic sat around
/// twice its threshold with agreement and maturity in the same middling place. Its purpose is
/// ranking: two findings from the same detector are comparable, and the same evidence always yields
/// the same score, so an operator can order a list and argue about a specific term.
///
/// Weights, over the terms that apply: exceedance 0.4, agreement 0.3, maturity 0.2, fit 0.1. The
/// applicable weights are renormalised, so a detector with no baseline and no trend scores on
/// exceedance and agreement alone (0.4 and 0.3 renormalised to 4/7 and 3/7) rather than being capped
/// at 0.7 for terms it could never have.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Confidence {
    /// The score, 0..=1.
    pub score: f64,
    /// The contributing terms.
    pub terms: ConfidenceTerms,
}

impl Confidence {
    /// Derives the score and its terms from evidence.
    ///
    /// Pure and total: same evidence, same score, which is what *The same inputs give the same
    /// finding* and *A finding can be recomputed from its evidence* both require.
    pub fn from_evidence(evidence: &Evidence) -> Self {
        let exceedance = normalise(evidence.exceedance_ratio(), 1.0, EXCEEDANCE_SATURATION);

        let agreement = evidence.agreement.and_then(|agreement| {
            if agreement.required_samples == 0 {
                // Nothing was required, so agreement carries no information: drop the term rather
                // than divide by zero or award a free 1.0.
                return None;
            }
            let ratio =
                f64::from(agreement.consecutive_samples) / f64::from(agreement.required_samples);
            Some(normalise(ratio, 1.0, AGREEMENT_SATURATION))
        });

        let maturity = evidence.baseline.as_ref().and_then(|baseline| {
            if baseline.min_samples == 0 {
                return None;
            }
            let ratio = f64::from(baseline.sample_count) / f64::from(baseline.min_samples);
            Some(normalise(ratio, 1.0, MATURITY_SATURATION))
        });

        let fit = evidence
            .trend
            .as_ref()
            .map(|trend| trend.fit_quality.clamp(0.0, 1.0));

        let terms = ConfidenceTerms {
            exceedance,
            agreement,
            maturity,
            fit,
        };
        Self {
            score: weighted_score(&terms),
            terms,
        }
    }
}

/// Maps a ratio onto 0..=1: `floor` scores 0, `saturation` and beyond score 1.
///
/// A ratio below `floor` clamps to 0 rather than going negative — a detector should not have fired
/// there at all, and a negative term would drag an unrelated term's contribution below zero.
fn normalise(ratio: f64, floor: f64, saturation: f64) -> f64 {
    if !ratio.is_finite() {
        return 0.0;
    }
    let span = saturation - floor;
    if span <= 0.0 {
        return 1.0;
    }
    ((ratio - floor) / span).clamp(0.0, 1.0)
}

/// The weighted mean over applicable terms, with the applicable weights renormalised.
fn weighted_score(terms: &ConfidenceTerms) -> f64 {
    let mut weighted = WEIGHT_EXCEEDANCE * terms.exceedance.clamp(0.0, 1.0);
    let mut total = WEIGHT_EXCEEDANCE;
    for (weight, term) in [
        (WEIGHT_AGREEMENT, terms.agreement),
        (WEIGHT_MATURITY, terms.maturity),
        (WEIGHT_FIT, terms.fit),
    ] {
        if let Some(value) = term {
            weighted += weight * value.clamp(0.0, 1.0);
            total += weight;
        }
    }
    if total <= 0.0 {
        return 0.0;
    }
    (weighted / total).clamp(0.0, 1.0)
}

/// Where a finding is in its lifecycle.
///
/// Cleared findings are kept and marked rather than deleted, which is what lets a consumer tell "this
/// resolved" from "this never existed": *A finding clears when the condition ends* is only observable
/// if the clearing is itself visible for a while. Retention of cleared findings is the caller's
/// policy; this type only records the state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingStatus {
    /// The condition holds now.
    Active,
    /// The condition no longer holds.
    Cleared,
}

/// A single finding: one condition, one episode.
///
/// `id` is `"{key}#{occurrence}"` — derived, not random, so the same episode has the same id on every
/// daemon that replays the same observations, and so an id read from a persisted payload still points
/// at a key. `raised_at_unix` and `occurrence` are fixed for the life of an episode: holding a finding
/// updates its evidence, confidence and `last_seen_unix`, never its identity. That is the line a
/// consumer reads to tell ongoing from new.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    /// `"{process}/{detector}/{metric}[/{variant}]#{occurrence}"`.
    pub id: String,
    /// The stable identity of the underlying condition.
    pub key: FindingKey,
    /// Which episode of this condition, from 1. Increments only on a re-raise after clearing.
    pub occurrence: u32,
    pub status: FindingStatus,
    /// When this episode was first raised. Does not move while the finding is held.
    pub raised_at_unix: u64,
    /// When the condition was last observed to hold.
    pub last_seen_unix: u64,
    /// When the condition stopped holding. `None` while active — absent, not 0, since 0 is a real
    /// Unix time and would read as "cleared at the epoch".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleared_at_unix: Option<u64>,
    /// How many evaluations have observed this episode, including the one that raised it. 1 means it
    /// has been seen exactly once.
    pub observation_count: u32,
    pub confidence: Confidence,
    /// The evidence from the most recent observation. Mandatory.
    pub evidence: Evidence,
    /// A short operator-facing sentence. Never the only description of the finding: the evidence is
    /// the machine-readable form, this is the courtesy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl Finding {
    /// Starts a builder. Fallible construction is the point: [`FindingBuilder::build`] is the only way
    /// to get a `Finding`, and it refuses without usable evidence.
    pub fn builder(key: FindingKey, at_unix: u64, evidence: Evidence) -> FindingBuilder {
        FindingBuilder {
            key,
            at_unix,
            evidence,
            occurrence: 1,
            summary: None,
        }
    }

    /// Whether the condition holds now.
    pub fn is_active(&self) -> bool {
        matches!(self.status, FindingStatus::Active)
    }

    /// Whether this is the first episode of its condition. A re-occurrence has `occurrence > 1`.
    #[cfg(test)]
    pub fn is_recurrence(&self) -> bool {
        self.occurrence > 1
    }

    /// Seconds the condition has been held, from raise to last observation.
    pub fn duration_seconds(&self) -> u64 {
        self.last_seen_unix.saturating_sub(self.raised_at_unix)
    }

    /// Recomputes the confidence from the carried evidence and reports whether it matches the stored
    /// score.
    ///
    /// This is *A finding can be recomputed from its evidence* made checkable at runtime rather than
    /// only in a test: a payload whose score does not follow from its evidence has been tampered with
    /// or was written by a build with different weights.
    pub fn confidence_is_reproducible(&self) -> bool {
        Confidence::from_evidence(&self.evidence) == self.confidence
    }

    fn clear(&mut self, at_unix: u64) {
        self.status = FindingStatus::Cleared;
        self.cleared_at_unix = Some(at_unix.max(self.last_seen_unix));
    }
}

/// Generates guidance for an operator to act on this finding.
///
/// Pure logic: takes a finding and returns the human-readable steps. Logic mirrors
/// the dashboard's `findingGuidance` to ensure consistency.
pub fn guidance_for(finding: &Finding) -> Option<Vec<String>> {
    let key = &finding.key;
    let evidence = &finding.evidence;
    let detector_name = key.detector.as_wire();
    let is_memory = key.metric.as_wire() == "memory_bytes";
    let direction = evidence.direction.as_ref();
    // Dashboard parity: the JS resolves `evidence.direction ?? key.variant`, so a finding whose
    // detector left the direction unset still lands in the right branch when the key names it.
    let resolved_below = matches!(direction, Some(Direction::Below))
        || (direction.is_none() && key.variant.as_deref() == Some("below"));

    if detector_name == "resource_leak" {
        let mut steps = vec![
            "Growth is monotonic and well-fitted, which distinguishes a leak from load: load falls back, a leak does not.".to_string(),
            "Compare against a restart — if usage returns to its old level and climbs again at the same rate, it is retention rather than demand.".to_string(),
        ];
        match evidence.forecast.as_ref() {
            Some(forecast) if forecast.eta_seconds.is_finite() => steps.push(format!(
                "At the current rate the configured limit is reached in about {}, if the trend continues unchanged.",
                format_duration_compact(std::time::Duration::try_from_secs_f64(forecast.eta_seconds.max(0.0)).unwrap_or_default().as_secs())
            )),
            _ => steps.push("No arrival time is projected: either no memory limit is configured for this process, or the target is too far out to extrapolate honestly.".to_string()),
        }
        steps.push("A memory limit turns an unbounded leak into a bounded restart. Set one if this process has none.".to_string());
        return Some(steps);
    }

    if detector_name == "level_departure" {
        let is_below = resolved_below;
        if is_below {
            return Some(vec![
                format!(
                    "This metric has dropped well below its own baseline, which is unusual rather than bad — {} can mean work stopped arriving.",
                    if is_memory { "a memory drop" } else { "an idle CPU" }
                ),
                "Check whether upstream traffic, a queue, or a scheduled job has stopped feeding this process.".to_string(),
                "If the process was deliberately quietened, the baseline will re-learn the new level and the finding clears itself.".to_string(),
            ]);
        }
        return Some(vec![
            "This metric sits well above the level this process itself established, sustained across consecutive samples rather than a single spike.".to_string(),
            if is_memory {
                "Check for a workload change first: more concurrent requests, a larger payload, or a cache that has just filled.".to_string()
            } else {
                "Check for a workload change first: request volume, a retry storm, or a newly enabled feature.".to_string()
            },
            "Compare against its own history rather than against other processes — the baseline is per-process for exactly this reason.".to_string(),
            "If this is the new normal, the baseline re-learns it and the finding clears; if it keeps climbing, look for a leak or an unbounded queue.".to_string(),
        ]);
    }

    if ["drift", "sustained_shift", "change_point"].contains(&detector_name) {
        return Some(vec![
            "The change is gradual or stepwise rather than a spike, so it is easy to miss and rarely urgent.".to_string(),
            "Correlate with a deploy, a configuration change, or a traffic pattern shift around the window shown in the evidence.".to_string(),
            "No action is implied. This is a note that behaviour changed, not that anything is wrong.".to_string(),
        ]);
    }

    if detector_name == "crash_loop" {
        return Some(vec![
            "Process is restarting faster than the configured threshold.".to_string(),
            "Check logs for initialization errors, missing config, or immediate crashes at startup.".to_string(),
        ]);
    }

    if detector_name == "restart_acceleration" {
        return Some(vec![
            "Restart frequency is increasing, suggesting a degrading system or resource exhaustion.".to_string(),
            "Check if startup latency is growing or if dependencies are becoming slower.".to_string(),
        ]);
    }

    if detector_name == "repeated_exit" {
        return Some(vec![
            "Process is exiting with the same error code repeatedly, indicating a deterministic failure.".to_string(),
            "Check exit logs for common patterns like segmentation faults or specific library errors.".to_string(),
        ]);
    }

    if detector_name == "restart_storm" {
        return Some(vec![
            "Multiple processes in this namespace are failing simultaneously.".to_string(),
            "Check for a shared dependency update, configuration change, or resource limit trigger (e.g., node pressure).".to_string(),
        ]);
    }

    if detector_name == "dependency_correlation" {
        return Some(vec![
            "This process failure correlates with a dependency failure.".to_string(),
            "Check the health of the implicated dependencies listed in the findings evidence."
                .to_string(),
        ]);
    }

    None
}

/// Builds a [`Finding`], validating the evidence.
pub struct FindingBuilder {
    key: FindingKey,
    at_unix: u64,
    evidence: Evidence,
    occurrence: u32,
    summary: Option<String>,
}

impl FindingBuilder {
    /// Sets the episode number. Callers going through [`FindingRegistry`] never need this; it exists
    /// for reconstructing a finding outside the registry.
    pub fn occurrence(mut self, occurrence: u32) -> Self {
        self.occurrence = occurrence.max(1);
        self
    }

    /// Sets the operator-facing sentence.
    pub fn summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }

    /// Validates the evidence, derives the confidence, and produces the finding.
    ///
    /// Rejects an immature baseline: *No detector fires on an immature baseline* is a property of the
    /// finding, not just of each detector, so enforcing it here means a new detector cannot forget it.
    /// A detector with no baseline at all is unaffected — `baseline: None` is "not baseline-derived",
    /// which is a different claim from "baseline not ready".
    pub fn build(self) -> Result<Finding, FindingError> {
        self.evidence.validate()?;
        if let Some(baseline) = &self.evidence.baseline
            && !baseline.is_ready()
        {
            return Err(FindingError::InvalidWindow {
                reason: "baseline is not ready: no baseline-dependent finding may be raised",
            });
        }
        let confidence = Confidence::from_evidence(&self.evidence);
        let id = format!("{}#{}", self.key.as_string(), self.occurrence);
        Ok(Finding {
            id,
            key: self.key,
            occurrence: self.occurrence,
            status: FindingStatus::Active,
            raised_at_unix: self.at_unix,
            last_seen_unix: self.at_unix,
            cleared_at_unix: None,
            observation_count: 1,
            confidence,
            evidence: self.evidence,
            summary: self.summary,
        })
    }
}

/// What an observation did to the registry.
///
/// The return value of [`FindingRegistry::observe`] and [`FindingRegistry::sweep`], and the reason a
/// consumer does not have to diff two snapshots to tell new from ongoing. Map `Raised` onto an
/// `anomaly:detected` event and `Cleared` onto `anomaly:cleared`; `Held` is deliberately silent, which
/// is *An active finding SHALL NOT be re-raised on every evaluation*.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "transition")]
pub enum Transition {
    /// A first episode of this condition.
    Raised { id: String },
    /// A further episode after the previous one cleared. `occurrence` is the new episode number, so a
    /// re-occurrence is visible as such rather than looking like a first sighting.
    ReRaised { id: String, occurrence: u32 },
    /// The same condition observed again. Evidence and confidence were refreshed; identity was not.
    Held { id: String },
    /// The condition stopped holding.
    Cleared { id: String },
}

impl Transition {
    /// The finding id this transition concerns.
    pub fn id(&self) -> &str {
        match self {
            Self::Raised { id }
            | Self::ReRaised { id, .. }
            | Self::Held { id }
            | Self::Cleared { id } => id,
        }
    }

    /// Whether this transition started a new episode, first or repeat. The signal a notifier acts on.
    #[cfg(test)]
    pub fn is_new_episode(&self) -> bool {
        matches!(self, Self::Raised { .. } | Self::ReRaised { .. })
    }
}

/// Holds findings across evaluations and applies the raise / hold / clear lifecycle.
///
/// The registry is what makes identity load-bearing rather than decorative: detectors report a
/// condition each tick and the registry decides whether that is a new finding. Ordered maps
/// throughout, so listing and serialising a registry is deterministic — the determinism requirement
/// covers the findings list, not only each finding.
///
/// State per key outlives the episode. `occurrences` remembers how many episodes a key has had even
/// after the finding is dropped from `findings`, so a condition that recurs long after clearing is
/// still numbered as a recurrence rather than restarting at 1.
#[derive(Debug, Clone, Default)]
pub struct FindingRegistry {
    findings: BTreeMap<FindingKey, Finding>,
    occurrences: BTreeMap<FindingKey, u32>,
}

impl FindingRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that a condition holds, and reports what that did.
    ///
    /// First sighting raises. A sighting while active holds: identity, `raised_at_unix` and
    /// `occurrence` are preserved while evidence, confidence, `last_seen_unix` and
    /// `observation_count` are refreshed, so the finding reflects the latest observation without
    /// becoming a new finding. A sighting after clearing re-raises under the next occurrence number.
    ///
    /// `evidence` is taken by value and validated, so an unusable observation is an error rather than
    /// a silently degraded finding.
    pub fn observe(
        &mut self,
        key: FindingKey,
        at_unix: u64,
        evidence: Evidence,
    ) -> Result<Transition, FindingError> {
        if let Some(existing) = self.findings.get_mut(&key)
            && existing.is_active()
        {
            let confidence = Confidence::from_evidence(&evidence);
            evidence.validate()?;
            // `existing` is a live `&mut` into the map; mutations below only run
            // after validation succeeds, and the early return drops the borrow
            // without touching the entry — same guarantee as the previous
            // separate get/get_mut pair, without the get_mut expect.
            existing.last_seen_unix = at_unix.max(existing.last_seen_unix);
            existing.observation_count = existing.observation_count.saturating_add(1);
            existing.confidence = confidence;
            existing.evidence = evidence;
            return Ok(Transition::Held {
                id: existing.id.clone(),
            });
        }

        let previous = self.occurrences.get(&key).copied().unwrap_or(0);
        let occurrence = previous.saturating_add(1);
        let finding = Finding::builder(key.clone(), at_unix, evidence)
            .occurrence(occurrence)
            .build()?;
        let id = finding.id.clone();
        self.occurrences.insert(key.clone(), occurrence);
        self.findings.insert(key, finding);
        if occurrence > 1 {
            Ok(Transition::ReRaised { id, occurrence })
        } else {
            Ok(Transition::Raised { id })
        }
    }

    /// Records that a condition no longer holds.
    ///
    /// Returns `None` when there was nothing active to clear, so clearing twice is a no-op rather than
    /// a second `anomaly:cleared` event.
    pub fn clear(&mut self, key: &FindingKey, at_unix: u64) -> Option<Transition> {
        let finding = self.findings.get_mut(key)?;
        if !finding.is_active() {
            return None;
        }
        finding.clear(at_unix);
        Some(Transition::Cleared {
            id: finding.id.clone(),
        })
    }

    /// Clears every active finding for a process whose key is not in `still_holding`.
    ///
    /// This is how a detection cycle ends: the detectors report the conditions they still see, and
    /// everything else for that process clears. Without it, clearing depends on each detector
    /// remembering to report an absence — and a detector that stops firing because its baseline
    /// re-based would leave a finding active forever.
    #[cfg(test)]
    pub fn sweep(
        &mut self,
        process: &str,
        still_holding: &BTreeSet<FindingKey>,
        at_unix: u64,
    ) -> Vec<Transition> {
        let stale: Vec<FindingKey> = self
            .findings
            .values()
            .filter(|finding| {
                finding.key.process == process
                    && finding.is_active()
                    && !still_holding.contains(&finding.key)
            })
            .map(|finding| finding.key.clone())
            .collect();
        stale
            .iter()
            .filter_map(|key| self.clear(key, at_unix))
            .collect()
    }

    /// Drops every finding for a process, and its episode history.
    ///
    /// *Findings are cleared when the process is deleted.* Dropped rather than marked cleared: the
    /// process is gone, so there is nothing left for a cleared finding to describe, and keeping the
    /// occurrence counter would number a future process of the same name as a continuation of one that
    /// no longer exists. Returns how many findings were removed.
    pub fn clear_process(&mut self, process: &str) -> usize {
        let before = self.findings.len();
        self.findings.retain(|key, _| key.process != process);
        self.occurrences.retain(|key, _| key.process != process);
        before - self.findings.len()
    }

    /// Forgets cleared findings that cleared at or before `before_unix`, leaving their occurrence
    /// history. Retention policy belongs to the caller; this is the mechanism.
    #[cfg(test)]
    pub fn forget_cleared(&mut self, before_unix: u64) -> usize {
        let before = self.findings.len();
        self.findings
            .retain(|_, finding| match finding.cleared_at_unix {
                Some(cleared_at) => cleared_at > before_unix,
                None => true,
            });
        before - self.findings.len()
    }

    /// The finding for a key, active or cleared.
    pub fn get(&self, key: &FindingKey) -> Option<&Finding> {
        self.findings.get(key)
    }

    /// Every active finding, in key order.
    #[cfg(test)]
    pub fn active(&self) -> Vec<&Finding> {
        self.findings
            .values()
            .filter(|finding| finding.is_active())
            .collect()
    }

    /// Every active finding for one process, in key order.
    pub fn active_for(&self, process: &str) -> Vec<&Finding> {
        self.findings
            .values()
            .filter(|finding| finding.is_active() && finding.key.process == process)
            .collect()
    }

    /// Every finding held, active or cleared, in key order.
    pub fn all(&self) -> Vec<&Finding> {
        self.findings.values().collect()
    }

    /// How many episodes a key has had, including the current one. 0 if it has never fired.
    #[cfg(test)]
    pub fn occurrence_count(&self, key: &FindingKey) -> u32 {
        self.occurrences.get(key).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(start: u64, end: u64, count: u32) -> EvidenceWindow {
        EvidenceWindow {
            start_unix: start,
            end_unix: end,
            sample_count: count,
        }
    }

    fn ready_baseline() -> BaselineSummary {
        BaselineSummary {
            center: 100.0,
            spread: 10.0,
            sample_count: 60,
            min_samples: 30,
        }
    }

    /// A level-departure observation: statistic 4.5 against a threshold of 3.0.
    fn level_evidence() -> Evidence {
        Evidence {
            window: window(1_000, 1_010, 6),
            observed: 145.0,
            expected: 100.0,
            statistic: 4.5,
            threshold: 3.0,
            direction: Some(Direction::Above),
            agreement: Some(Agreement {
                consecutive_samples: 3,
                required_samples: 3,
            }),
            baseline: Some(ready_baseline()),
            trend: None,
            forecast: None,
            samples: vec![
                SampleRef::new(1_000, 143.0),
                SampleRef::new(1_005, 144.0),
                SampleRef::new(1_010, 145.0),
            ],
            extra: BTreeMap::new(),
        }
    }

    fn level_key() -> FindingKey {
        FindingKey::new("api", Detector::LevelDeparture, Metric::MemoryBytes).with_variant("above")
    }

    // ---- construction and mandatory evidence (7.1, 7.4) ----

    #[test]
    fn a_finding_cannot_be_built_without_samples() {
        let mut evidence = level_evidence();
        evidence.samples.clear();
        let error = Finding::builder(level_key(), 1_010, evidence)
            .build()
            .expect_err("evidence with no samples must be refused");
        assert_eq!(error, FindingError::NoSamples);
    }

    #[test]
    fn a_finding_cannot_be_built_from_an_unusable_threshold() {
        let mut evidence = level_evidence();
        evidence.threshold = 0.0;
        let error = Finding::builder(level_key(), 1_010, evidence)
            .build()
            .expect_err("a zero threshold cannot be exceeded");
        assert_eq!(error, FindingError::NonPositiveThreshold { threshold: 0.0 });
    }

    #[test]
    fn a_finding_cannot_be_built_from_a_non_finite_value() {
        let mut evidence = level_evidence();
        evidence.observed = f64::NAN;
        let error = Finding::builder(level_key(), 1_010, evidence)
            .build()
            .expect_err("NaN must not reach a ranked field");
        assert!(matches!(
            error,
            FindingError::NonFinite {
                field: "observed",
                ..
            }
        ));
    }

    #[test]
    fn an_empty_window_is_refused() {
        let mut evidence = level_evidence();
        evidence.window.sample_count = 0;
        assert!(matches!(
            Finding::builder(level_key(), 1_010, evidence).build(),
            Err(FindingError::InvalidWindow { .. })
        ));
    }

    #[test]
    fn an_immature_baseline_cannot_raise_a_finding() {
        let mut evidence = level_evidence();
        evidence.baseline = Some(BaselineSummary {
            sample_count: 29,
            min_samples: 30,
            ..ready_baseline()
        });
        assert!(matches!(
            Finding::builder(level_key(), 1_010, evidence).build(),
            Err(FindingError::InvalidWindow { .. })
        ));
    }

    #[test]
    fn a_detector_with_no_baseline_is_not_treated_as_immature() {
        let mut evidence = level_evidence();
        evidence.baseline = None;
        let finding = Finding::builder(
            FindingKey::new("api", Detector::ResourceLeak, Metric::MemoryBytes),
            1_010,
            evidence,
        )
        .build()
        .expect("a leak finding needs no baseline");
        assert!(finding.confidence.terms.maturity.is_none());
    }

    #[test]
    fn evidence_carries_the_detector_metric_window_and_comparison() {
        let finding = Finding::builder(level_key(), 1_010, level_evidence())
            .build()
            .expect("valid evidence");
        // Detector and metric come from the key, so there is one authority for them.
        assert_eq!(finding.key.detector, Detector::LevelDeparture);
        assert_eq!(finding.key.metric, Metric::MemoryBytes);
        assert_eq!(finding.evidence.window.sample_count, 6);
        assert_eq!(finding.evidence.observed, 145.0);
        assert_eq!(finding.evidence.expected, 100.0);
        assert_eq!(finding.evidence.threshold, 3.0);
        assert_eq!(finding.evidence.samples.len(), 3);
    }

    // ---- determinism (7.7) ----

    #[test]
    fn the_same_inputs_give_the_same_finding_and_confidence() {
        let first = Finding::builder(level_key(), 1_010, level_evidence())
            .build()
            .expect("valid");
        let second = Finding::builder(level_key(), 1_010, level_evidence())
            .build()
            .expect("valid");
        assert_eq!(first, second);
        assert_eq!(first.confidence, second.confidence);
        assert_eq!(first.evidence, second.evidence);
        assert_eq!(first.id, second.id);
    }

    #[test]
    fn confidence_recomputes_from_the_carried_evidence() {
        let finding = Finding::builder(level_key(), 1_010, level_evidence())
            .build()
            .expect("valid");
        assert!(finding.confidence_is_reproducible());
        let recomputed = Confidence::from_evidence(&finding.evidence);
        assert_eq!(recomputed, finding.confidence);
    }

    #[test]
    fn a_tampered_score_is_detectable() {
        let mut finding = Finding::builder(level_key(), 1_010, level_evidence())
            .build()
            .expect("valid");
        finding.confidence.score = 0.99;
        assert!(!finding.confidence_is_reproducible());
    }

    // ---- confidence ordering per term (7.2, 7.3) ----

    #[test]
    fn confidence_is_between_zero_and_one() {
        let mut extreme = level_evidence();
        extreme.statistic = 5_000.0;
        let high = Confidence::from_evidence(&extreme);
        let mut marginal = level_evidence();
        marginal.statistic = 3.0;
        let low = Confidence::from_evidence(&marginal);
        for confidence in [high, low] {
            assert!(
                (0.0..=1.0).contains(&confidence.score),
                "score out of range: {}",
                confidence.score
            );
        }
    }

    #[test]
    fn further_past_threshold_scores_higher() {
        let near = Confidence::from_evidence(&level_evidence());
        let mut further_evidence = level_evidence();
        further_evidence.statistic = 8.0;
        let further = Confidence::from_evidence(&further_evidence);
        assert!(
            further.score > near.score,
            "{} should exceed {}",
            further.score,
            near.score
        );
        assert!(further.terms.exceedance > near.terms.exceedance);
    }

    #[test]
    fn longer_agreement_scores_higher() {
        let short = Confidence::from_evidence(&level_evidence());
        let mut longer_evidence = level_evidence();
        longer_evidence.agreement = Some(Agreement {
            consecutive_samples: 6,
            required_samples: 3,
        });
        let longer = Confidence::from_evidence(&longer_evidence);
        assert!(
            longer.score > short.score,
            "{} should exceed {}",
            longer.score,
            short.score
        );
    }

    #[test]
    fn a_more_mature_baseline_scores_higher() {
        let young = Confidence::from_evidence(&level_evidence());
        let mut mature_evidence = level_evidence();
        mature_evidence.baseline = Some(BaselineSummary {
            sample_count: 240,
            ..ready_baseline()
        });
        let mature = Confidence::from_evidence(&mature_evidence);
        assert!(
            mature.score > young.score,
            "{} should exceed {}",
            mature.score,
            young.score
        );
    }

    #[test]
    fn a_better_fit_scores_higher() {
        let mut poor_evidence = level_evidence();
        poor_evidence.trend = Some(TrendFit {
            slope_per_second: 12.0,
            fit_quality: 0.4,
            monotonicity_ratio: Some(0.9),
            slope_std_error: Some(0.5),
        });
        let mut good_evidence = poor_evidence.clone();
        good_evidence.trend = Some(TrendFit {
            fit_quality: 0.98,
            ..good_evidence.trend.clone().expect("set above")
        });
        let poor = Confidence::from_evidence(&poor_evidence);
        let good = Confidence::from_evidence(&good_evidence);
        assert!(good.score > poor.score, "{} vs {}", good.score, poor.score);
    }

    #[test]
    fn an_absent_term_is_dropped_rather_than_scored_zero() {
        // Saturate every present term. A detector with no baseline and no trend must still be able
        // to reach 1.0, otherwise "not applicable" would read as "no support".
        let mut evidence = level_evidence();
        evidence.baseline = None;
        evidence.statistic = 9.0; // 3x threshold saturates exceedance
        evidence.agreement = Some(Agreement {
            consecutive_samples: 6,
            required_samples: 3,
        });
        let confidence = Confidence::from_evidence(&evidence);
        assert!(confidence.terms.maturity.is_none());
        assert!(confidence.terms.fit.is_none());
        assert!(
            (confidence.score - 1.0).abs() < 1e-9,
            "renormalised score should reach 1.0, got {}",
            confidence.score
        );
    }

    #[test]
    fn terms_are_reported_alongside_the_score() {
        let confidence = Confidence::from_evidence(&level_evidence());
        assert!(confidence.terms.exceedance > 0.0);
        assert!(confidence.terms.agreement.is_some());
        assert!(confidence.terms.maturity.is_some());
        let json = serde_json::to_string(&confidence).expect("serialise");
        assert!(json.contains("\"terms\""));
        assert!(json.contains("\"exceedance\""));
    }

    #[test]
    fn exceedance_saturates_rather_than_growing_without_bound() {
        let mut at_saturation = level_evidence();
        at_saturation.statistic = 9.0;
        let mut far_beyond = level_evidence();
        far_beyond.statistic = 900.0;
        assert_eq!(
            Confidence::from_evidence(&at_saturation).score,
            Confidence::from_evidence(&far_beyond).score
        );
    }

    // ---- identity and lifecycle (7.5, 7.6) ----

    #[test]
    fn identity_does_not_depend_on_time_or_magnitude() {
        // The same condition observed later, with a different observed value, is the same key.
        let first = Finding::builder(level_key(), 1_010, level_evidence())
            .build()
            .expect("valid");
        let mut later_evidence = level_evidence();
        later_evidence.window = window(2_000, 2_010, 6);
        later_evidence.observed = 190.0;
        later_evidence.statistic = 9.0;
        let later = Finding::builder(level_key(), 2_010, later_evidence)
            .build()
            .expect("valid");
        assert_eq!(first.key, later.key);
        assert_eq!(first.id, later.id);
    }

    #[test]
    fn two_detectors_on_one_metric_are_two_findings() {
        let level = FindingKey::new("api", Detector::LevelDeparture, Metric::MemoryBytes);
        let leak = FindingKey::new("api", Detector::ResourceLeak, Metric::MemoryBytes);
        assert_ne!(level, leak);
    }

    #[test]
    fn direction_variants_are_distinct_conditions() {
        let above = FindingKey::new("api", Detector::LevelDeparture, Metric::CpuPercent)
            .with_variant("above");
        let below = FindingKey::new("api", Detector::LevelDeparture, Metric::CpuPercent)
            .with_variant("below");
        assert_ne!(above, below);
    }

    #[test]
    fn the_same_condition_observed_twice_is_one_finding() {
        let mut registry = FindingRegistry::new();
        let raised = registry
            .observe(level_key(), 1_010, level_evidence())
            .expect("valid");
        assert!(matches!(raised, Transition::Raised { .. }));

        let mut second_evidence = level_evidence();
        second_evidence.window = window(1_010, 1_020, 6);
        second_evidence.observed = 150.0;
        let held = registry
            .observe(level_key(), 1_020, second_evidence)
            .expect("valid");

        assert!(matches!(held, Transition::Held { .. }));
        assert!(!held.is_new_episode());
        assert_eq!(raised.id(), held.id(), "identity must not change on a hold");
        assert_eq!(registry.active().len(), 1, "one finding, not two");

        let finding = registry.get(&level_key()).expect("present");
        assert_eq!(finding.occurrence, 1);
        assert_eq!(finding.raised_at_unix, 1_010, "raise time must not move");
        assert_eq!(finding.last_seen_unix, 1_020);
        assert_eq!(finding.observation_count, 2);
        assert_eq!(finding.evidence.observed, 150.0, "evidence refreshes");
        assert_eq!(finding.duration_seconds(), 10);
    }

    #[test]
    fn a_held_finding_is_not_re_raised_across_many_ticks() {
        let mut registry = FindingRegistry::new();
        let mut transitions = Vec::new();
        for tick in 0..20u64 {
            let at = 1_000 + tick * 2;
            let mut evidence = level_evidence();
            evidence.window = window(at, at + 2, 6);
            transitions.push(registry.observe(level_key(), at, evidence).expect("valid"));
        }
        let new_episodes = transitions
            .iter()
            .filter(|transition| transition.is_new_episode())
            .count();
        assert_eq!(new_episodes, 1, "20 ticks of one condition raise once");
        assert_eq!(registry.active().len(), 1);
        assert_eq!(
            registry
                .get(&level_key())
                .expect("present")
                .observation_count,
            20
        );
    }

    #[test]
    fn a_resolved_finding_is_distinguishable_from_an_active_one() {
        let mut registry = FindingRegistry::new();
        registry
            .observe(level_key(), 1_010, level_evidence())
            .expect("valid");
        let active = registry.get(&level_key()).expect("present").clone();
        assert!(active.is_active());
        assert_eq!(active.status, FindingStatus::Active);
        assert!(active.cleared_at_unix.is_none());

        let cleared_transition = registry.clear(&level_key(), 1_040).expect("was active");
        assert!(matches!(cleared_transition, Transition::Cleared { .. }));

        let cleared = registry.get(&level_key()).expect("retained after clearing");
        assert!(!cleared.is_active());
        assert_eq!(cleared.status, FindingStatus::Cleared);
        assert_eq!(cleared.cleared_at_unix, Some(1_040));
        assert_eq!(cleared.id, active.id, "clearing does not change identity");
        assert!(registry.active().is_empty(), "not listed as active");
    }

    #[test]
    fn clearing_twice_is_a_no_op() {
        let mut registry = FindingRegistry::new();
        registry
            .observe(level_key(), 1_010, level_evidence())
            .expect("valid");
        assert!(registry.clear(&level_key(), 1_040).is_some());
        assert!(
            registry.clear(&level_key(), 1_050).is_none(),
            "a second clear must not emit a second event"
        );
        assert_eq!(
            registry.get(&level_key()).expect("present").cleared_at_unix,
            Some(1_040),
            "the original clear time stands"
        );
    }

    #[test]
    fn a_re_occurrence_after_clearing_is_visible() {
        let mut registry = FindingRegistry::new();
        registry
            .observe(level_key(), 1_010, level_evidence())
            .expect("valid");
        let first = registry.get(&level_key()).expect("present").clone();
        registry.clear(&level_key(), 1_040).expect("was active");

        let mut recurrence_evidence = level_evidence();
        recurrence_evidence.window = window(5_000, 5_010, 6);
        let transition = registry
            .observe(level_key(), 5_010, recurrence_evidence)
            .expect("valid");

        assert!(
            matches!(transition, Transition::ReRaised { occurrence: 2, .. }),
            "expected a re-raise, got {transition:?}"
        );
        assert!(transition.is_new_episode());
        let second = registry.get(&level_key()).expect("present");
        assert_eq!(second.occurrence, 2);
        assert!(second.is_recurrence());
        assert!(!first.is_recurrence());
        assert_ne!(second.id, first.id, "a new episode has a new id");
        assert_eq!(second.raised_at_unix, 5_010);
        assert!(second.cleared_at_unix.is_none());
        assert_eq!(second.observation_count, 1);
        assert_eq!(registry.occurrence_count(&level_key()), 2);
    }

    #[test]
    fn a_re_occurrence_after_forgetting_still_counts_as_a_recurrence() {
        let mut registry = FindingRegistry::new();
        registry
            .observe(level_key(), 1_010, level_evidence())
            .expect("valid");
        registry.clear(&level_key(), 1_040).expect("was active");
        assert_eq!(registry.forget_cleared(2_000), 1);
        assert!(registry.get(&level_key()).is_none());

        registry
            .observe(level_key(), 9_000, level_evidence())
            .expect("valid");
        assert_eq!(
            registry.get(&level_key()).expect("present").occurrence,
            2,
            "episode history outlives the finding"
        );
    }

    #[test]
    fn a_rejected_observation_leaves_a_held_finding_untouched() {
        let mut registry = FindingRegistry::new();
        registry
            .observe(level_key(), 1_010, level_evidence())
            .expect("valid");
        let before = registry.get(&level_key()).expect("present").clone();

        let mut bad_evidence = level_evidence();
        bad_evidence.samples.clear();
        assert!(registry.observe(level_key(), 1_020, bad_evidence).is_err());

        assert_eq!(registry.get(&level_key()).expect("present"), &before);
    }

    #[test]
    fn a_sweep_clears_what_the_detectors_no_longer_report() {
        let mut registry = FindingRegistry::new();
        let leak_key = FindingKey::new("api", Detector::ResourceLeak, Metric::MemoryBytes);
        let other_process = FindingKey::new("worker", Detector::LevelDeparture, Metric::CpuPercent);
        for key in [level_key(), leak_key.clone(), other_process.clone()] {
            registry
                .observe(key, 1_010, level_evidence())
                .expect("valid");
        }

        let still_holding: BTreeSet<FindingKey> = [level_key()].into_iter().collect();
        let cleared = registry.sweep("api", &still_holding, 1_020);

        assert_eq!(cleared.len(), 1);
        assert_eq!(cleared[0].id(), format!("{}#1", leak_key.as_string()));
        assert!(registry.get(&level_key()).expect("present").is_active());
        assert!(!registry.get(&leak_key).expect("present").is_active());
        assert!(
            registry.get(&other_process).expect("present").is_active(),
            "a sweep of one process must not clear another's findings"
        );
    }

    #[test]
    fn a_sweep_does_not_re_clear_an_already_cleared_finding() {
        let mut registry = FindingRegistry::new();
        registry
            .observe(level_key(), 1_010, level_evidence())
            .expect("valid");
        registry.clear(&level_key(), 1_020).expect("was active");
        assert!(registry.sweep("api", &BTreeSet::new(), 1_030).is_empty());
    }

    #[test]
    fn deleting_a_process_clears_its_findings() {
        let mut registry = FindingRegistry::new();
        registry
            .observe(level_key(), 1_010, level_evidence())
            .expect("valid");
        registry
            .observe(
                FindingKey::new("worker", Detector::ResourceLeak, Metric::MemoryBytes),
                1_010,
                level_evidence(),
            )
            .expect("valid");

        assert_eq!(registry.clear_process("api"), 1);
        assert!(registry.active_for("api").is_empty());
        assert!(registry.get(&level_key()).is_none());
        assert_eq!(
            registry.occurrence_count(&level_key()),
            0,
            "a later process reusing the name starts fresh"
        );
        assert_eq!(registry.active_for("worker").len(), 1);
    }

    #[test]
    fn active_findings_are_listable_with_what_an_operator_needs() {
        let mut registry = FindingRegistry::new();
        registry
            .observe(level_key(), 1_010, level_evidence())
            .expect("valid");
        let listed = registry.active_for("api");
        assert_eq!(listed.len(), 1);
        let finding = listed[0];
        assert_eq!(finding.key.detector, Detector::LevelDeparture);
        assert!(finding.confidence.score > 0.0);
        assert!(!finding.evidence.samples.is_empty());
        assert_eq!(finding.raised_at_unix, 1_010);
    }

    #[test]
    fn listing_order_is_deterministic() {
        let keys = [
            FindingKey::new("api", Detector::ResourceLeak, Metric::MemoryBytes),
            FindingKey::new("api", Detector::LevelDeparture, Metric::CpuPercent),
            FindingKey::new("worker", Detector::Drift, Metric::CpuPercent),
        ];
        let mut forwards = FindingRegistry::new();
        for key in keys.iter().cloned() {
            forwards
                .observe(key, 1_010, level_evidence())
                .expect("valid");
        }
        let mut backwards = FindingRegistry::new();
        for key in keys.iter().rev().cloned() {
            backwards
                .observe(key, 1_010, level_evidence())
                .expect("valid");
        }
        let ids = |registry: &FindingRegistry| -> Vec<String> {
            registry.all().iter().map(|f| f.id.clone()).collect()
        };
        assert_eq!(ids(&forwards), ids(&backwards), "order is key order");
    }

    // ---- serde: round-trip, forward compatibility, absent-is-not-zero ----

    #[test]
    fn a_finding_round_trips_through_json() {
        let mut evidence = level_evidence();
        evidence.trend = Some(TrendFit {
            slope_per_second: 1_024.0,
            fit_quality: 0.94,
            monotonicity_ratio: Some(0.97),
            slope_std_error: Some(12.0),
        });
        evidence.forecast = Some(ThresholdForecast {
            threshold_value: 2_000_000_000.0,
            eta_seconds: 7_200.0,
            interval_low_seconds: 5_400.0,
            interval_high_seconds: 10_800.0,
            assumes: ForecastAssumption::CurrentTrendContinues,
        });
        evidence.extra.insert("cusum_slack".to_string(), 0.5);
        let finding = Finding::builder(level_key(), 1_010, evidence)
            .summary("memory 45% above its baseline for 3 consecutive samples")
            .build()
            .expect("valid");

        let json = serde_json::to_string(&finding).expect("serialise");
        let decoded: Finding = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(decoded, finding);
        assert!(decoded.confidence_is_reproducible());
    }

    #[test]
    fn the_wire_form_names_the_detector_metric_and_status_as_strings() {
        let finding = Finding::builder(level_key(), 1_010, level_evidence())
            .build()
            .expect("valid");
        let json = serde_json::to_string(&finding).expect("serialise");
        assert!(json.contains("\"detector\":\"level_departure\""), "{json}");
        assert!(json.contains("\"metric\":\"memory_bytes\""), "{json}");
        assert!(json.contains("\"status\":\"active\""), "{json}");
        assert!(json.contains("\"direction\":\"above\""), "{json}");
        assert!(
            json.contains("\"id\":\"api/level_departure/memory_bytes/above#1\""),
            "{json}"
        );
    }

    #[test]
    fn an_unknown_detector_or_metric_keeps_its_name() {
        let json = r#"{"process":"api","detector":"future_detector","metric":"gpu_percent"}"#;
        let key: FindingKey = serde_json::from_str(json).expect("deserialise");
        assert_eq!(key.detector, Detector::Other("future_detector".to_string()));
        assert_eq!(key.metric, Metric::Other("gpu_percent".to_string()));
        // And it survives a round-trip under the same name rather than collapsing.
        let round_tripped: FindingKey =
            serde_json::from_str(&serde_json::to_string(&key).expect("serialise"))
                .expect("deserialise");
        assert_eq!(round_tripped, key);
    }

    #[test]
    fn a_known_detector_name_normalises_rather_than_becoming_other() {
        assert_eq!(Detector::from_wire("drift"), Detector::Drift);
        assert_ne!(
            Detector::from_wire("drift"),
            Detector::Other("drift".into())
        );
        assert_eq!(Metric::from_wire("cpu_percent"), Metric::CpuPercent);
    }

    #[test]
    fn a_payload_missing_newer_optional_fields_still_deserialises() {
        // Every field this build added since the first release is omitted: variant, direction,
        // agreement, baseline, trend, forecast, extra, cleared_at_unix, summary, and the optional
        // confidence terms.
        let json = r#"{
            "id": "api/level_departure/memory_bytes#1",
            "key": { "process": "api", "detector": "level_departure", "metric": "memory_bytes" },
            "occurrence": 1,
            "status": "active",
            "raised_at_unix": 1000,
            "last_seen_unix": 1000,
            "observation_count": 1,
            "confidence": { "score": 0.5, "terms": { "exceedance": 0.5 } },
            "evidence": {
                "window": { "start_unix": 1000, "end_unix": 1010, "sample_count": 6 },
                "observed": 145.0,
                "expected": 100.0,
                "statistic": 4.5,
                "threshold": 3.0,
                "samples": [ { "at_unix": 1000, "value": 145.0 } ]
            }
        }"#;
        let finding: Finding = serde_json::from_str(json).expect("older payload must deserialise");
        assert_eq!(finding.key.variant, None);
        assert_eq!(finding.evidence.baseline, None);
        assert_eq!(finding.evidence.trend, None);
        assert!(finding.evidence.extra.is_empty());
        assert_eq!(finding.confidence.terms.agreement, None);
        assert_eq!(finding.summary, None);
        assert!(finding.is_active());
    }

    #[test]
    fn absent_is_not_zero_on_the_wire() {
        let mut without = level_evidence();
        without.trend = None;
        without.forecast = None;
        let plain = Finding::builder(level_key(), 1_010, without)
            .build()
            .expect("valid");

        let mut with = level_evidence();
        with.trend = Some(TrendFit {
            slope_per_second: 0.0,
            fit_quality: 0.0,
            monotonicity_ratio: Some(0.0),
            slope_std_error: Some(0.0),
        });
        let fitted = Finding::builder(level_key(), 1_010, with)
            .build()
            .expect("valid");

        let plain_json = serde_json::to_string(&plain).expect("serialise");
        let fitted_json = serde_json::to_string(&fitted).expect("serialise");

        // No trend at all omits the field entirely; a trend that fits terribly reports 0.
        assert!(!plain_json.contains("trend"), "{plain_json}");
        assert!(!plain_json.contains("cleared_at_unix"), "{plain_json}");
        assert!(fitted_json.contains("\"fit_quality\":0"), "{fitted_json}");
        assert_ne!(plain_json, fitted_json);

        // And the confidence term follows: absent, not 0.
        assert_eq!(plain.confidence.terms.fit, None);
        assert_eq!(fitted.confidence.terms.fit, Some(0.0));
        assert!(
            plain.confidence.score > fitted.confidence.score,
            "a terrible fit must not score the same as no fit: {} vs {}",
            plain.confidence.score,
            fitted.confidence.score
        );
    }

    #[test]
    fn a_cleared_finding_serialises_its_clear_time() {
        let mut registry = FindingRegistry::new();
        registry
            .observe(level_key(), 1_010, level_evidence())
            .expect("valid");
        registry.clear(&level_key(), 1_040).expect("was active");
        let json =
            serde_json::to_string(registry.get(&level_key()).expect("present")).expect("serialise");
        assert!(json.contains("\"status\":\"cleared\""), "{json}");
        assert!(json.contains("\"cleared_at_unix\":1040"), "{json}");
    }

    #[test]
    fn a_transition_serialises_under_a_tag() {
        let raised = Transition::Raised {
            id: "api/drift/cpu_percent#1".to_string(),
        };
        let json = serde_json::to_string(&raised).expect("serialise");
        assert!(json.contains("\"transition\":\"raised\""), "{json}");
        let decoded: Transition = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(decoded, raised);
    }

    #[test]
    fn a_forecast_states_its_assumption_on_the_wire() {
        let forecast = ThresholdForecast {
            threshold_value: 1_000.0,
            eta_seconds: 600.0,
            interval_low_seconds: 400.0,
            interval_high_seconds: 900.0,
            assumes: ForecastAssumption::CurrentTrendContinues,
        };
        let json = serde_json::to_string(&forecast).expect("serialise");
        assert!(
            json.contains("\"assumes\":\"current_trend_continues\""),
            "{json}"
        );
    }

    // ---- guidance (3.3): served by the daemon, one source for CLI and dashboard ----

    fn new_test_finding(
        detector: Detector,
        metric: Metric,
        direction: Option<Direction>,
        forecast: Option<ThresholdForecast>,
    ) -> Finding {
        let key = FindingKey::new("api", detector, metric);
        let mut evidence = level_evidence();
        evidence.direction = direction;
        evidence.forecast = forecast;
        Finding::builder(key, 1_000, evidence)
            .build()
            .expect("must build")
    }

    #[test]
    fn guidance_for_resource_leak_includes_forecast_when_finite() {
        let forecast = ThresholdForecast {
            threshold_value: 100.0,
            eta_seconds: 7200.0,
            interval_low_seconds: 7000.0,
            interval_high_seconds: 7400.0,
            assumes: ForecastAssumption::CurrentTrendContinues,
        };
        let finding = new_test_finding(
            Detector::ResourceLeak,
            Metric::MemoryBytes,
            None,
            Some(forecast),
        );
        let guidance = guidance_for(&finding).expect("known detector");
        assert_eq!(
            guidance[2],
            "At the current rate the configured limit is reached in about 2h 0m, if the trend continues unchanged."
        );
    }

    #[test]
    fn guidance_for_level_departure_above_memory() {
        let finding = new_test_finding(
            Detector::LevelDeparture,
            Metric::MemoryBytes,
            Some(Direction::Above),
            None,
        );
        let guidance = guidance_for(&finding).expect("known detector");
        assert_eq!(
            guidance[0],
            "This metric sits well above the level this process itself established, sustained across consecutive samples rather than a single spike."
        );
        assert!(guidance[1].contains("workload change"));
    }

    #[test]
    fn guidance_for_unknown_detector_is_none() {
        let finding = new_test_finding(
            Detector::Other("foo".to_string()),
            Metric::CpuPercent,
            None,
            None,
        );
        assert!(guidance_for(&finding).is_none());
    }
}
