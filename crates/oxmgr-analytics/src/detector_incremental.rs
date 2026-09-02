//! Incremental detectors for gradual drift, sustained shifts, and regime changes.
//!
//! Scaffold for OpenSpec change `process-intelligence`, tasks section 5b.
//! The contract is `openspec/changes/process-intelligence/specs/process-anomaly-detection/`;
//! read it before adding to this file.
//!
//! Pure logic on purpose: nothing here reaches for the process manager, the HTTP layer or the
//! dashboard, so it can be unit-tested without a running daemon.
//!
//! # What these detectors answer
//!
//! Three questions about a metric series, each complementary to the level detector
//! (`detector_level.rs`) and to each other:
//!
//! - **EWMA**: is the metric drifting gradually in one direction, faster than the level
//!   detector's windowed baseline can track?
//! - **CUSUM**: is the metric holding slightly away from its baseline for long enough that
//!   the cumulative evidence matters, even though no single sample crosses a large-departure
//!   line?
//! - **Change point**: did the metric's behaviour fundamentally change — step to a new normal —
//!   rather than merely spike or drift temporarily?
//!
//! # Determinism
//!
//! Every detector is a pure function of its own state and the incoming sample. Given identical
//! inputs, the verdict, the finding, and the evidence are identical every time. No wall clock,
//! no RNG, no network, no side channel.
//!
//! # Incremental state only
//!
//! None of these detectors rescans history on a tick. EWMA carries an exponential moving
//! average and variance; CUSUM carries two accumulators; the change-point detector carries a
//! fixed-capacity ring for the current window. Per-sample cost is O(1).
//!
//! # Every threshold here is uncalibrated
//!
//! The defaults are chosen to be conservative against synthetic series in this module's tests.
//! None has been measured against a real workload. `tasks.md` 13.6 schedules trace-based
//! calibration as the honest blocker, and the observe-only default is what makes shipping
//! uncalibrated defaults defensible.

use oxmgr_core::numeric::usize_to_f64;

// ---------------------------------------------------------------------------
// EWMA (Exponentially Weighted Moving Average) — Task 5b.1
// ---------------------------------------------------------------------------

/// Default smoothing factor for the EWMA detector.
///
/// 0.3 means each new sample contributes 30% to the running mean and 70% is retained.
/// Higher = faster reaction to change but more noise; lower = smoother but slower to detect.
/// **Uncalibrated.**
pub const DEFAULT_EWMA_ALPHA: f64 = 0.3;

/// Default threshold on the normalised EWMA deviation for drift to be reported.
///
/// The normalised deviation is `|x − mean| / max(std_dev, std_floor)`. A value of 3.0
/// is roughly the level detector's departure threshold, but applied to the EWMA's own
/// estimate of centre and spread rather than to a windowed baseline.
/// **Uncalibrated.**
pub const DEFAULT_EWMA_THRESHOLD: f64 = 3.0;

/// Default number of consecutive elevated normalised-deviation samples needed before
/// reporting a drift finding.
///
/// 3 samples at the daemon's 2s tick is ~6s of sustained drift evidence.
/// **Uncalibrated.**
pub const DEFAULT_EWMA_MIN_CONSECUTIVE: u32 = 3;

/// Minimum number of samples before the EWMA detector can produce a verdict at all.
///
/// 10 samples (~20s at 2s tick) is enough for the exponential average to settle from its
/// seed but not so many that a slow drift takes minutes to surface.
/// **Uncalibrated.**
pub const DEFAULT_EWMA_MIN_SAMPLES: u32 = 10;

/// Floor on the EWMA's standard deviation estimate, in the metric's own units.
///
/// Without this floor a perfectly flat metric produces std_dev = 0 and any single-unit
/// deviation gives an infinite normalised deviation, which is the same failure mode MAD
/// handles in the level detector.
/// **Uncalibrated.** Callers that know their metric's measurement granularity should set
/// this to a fraction of it.
pub const DEFAULT_EWMA_STD_FLOOR: f64 = 1e-12;

/// Configuration for the EWMA drift detector.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EwmaConfig {
    /// Exponential smoothing factor in (0, 1].
    pub alpha: f64,
    /// Normalised-deviation threshold at or above which a sample counts as drifting.
    pub threshold: f64,
    /// Number of consecutive elevated samples needed before reporting.
    pub min_consecutive: u32,
    /// Minimum samples before the detector is willing to fire.
    pub min_samples: u32,
    /// Floor on the standard deviation estimate.
    pub std_floor: f64,
}

impl Default for EwmaConfig {
    fn default() -> Self {
        Self {
            alpha: DEFAULT_EWMA_ALPHA,
            threshold: DEFAULT_EWMA_THRESHOLD,
            min_consecutive: DEFAULT_EWMA_MIN_CONSECUTIVE,
            min_samples: DEFAULT_EWMA_MIN_SAMPLES,
            std_floor: DEFAULT_EWMA_STD_FLOOR,
        }
    }
}

/// Verdict of the EWMA detector on one sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EwmaVerdict {
    /// Not enough samples yet to evaluate.
    Warming { have: u32, need: u32 },
    /// Sample is within normal range of the EWMA.
    Normal,
    /// Drift detected: sustained elevated deviation from the EWMA.
    Drift(EwmaFinding),
}

/// Evidence behind an EWMA drift finding.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EwmaFinding {
    /// The normalised deviation of this sample from the EWMA centre.
    pub deviation: f64,
    /// The EWMA centre at the time of the finding.
    pub ewma_mean: f64,
    /// The EWMA standard deviation estimate at the time of the finding.
    pub ewma_std: f64,
    /// The sample value that triggered the finding.
    pub sample_value: f64,
    /// Number of consecutive elevated samples leading to this finding.
    pub consecutive: u32,
}

/// State for the EWMA detector.
#[derive(Debug, Clone)]
pub struct EwmaDetector {
    /// Exponential moving average.
    mean: f64,
    /// Exponential moving variance (Welford's online variance via EWMA).
    m2: f64,
    /// Total samples seen.
    sample_count: u32,
    /// Consecutive samples with normalised deviation >= threshold.
    consecutive: u32,
    /// Whether this is the very first sample (used to seed the mean).
    seeded: bool,
}

impl Default for EwmaDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl EwmaDetector {
    /// A new detector, with all state at zero.
    pub fn new() -> Self {
        Self {
            mean: 0.0,
            m2: 0.0,
            sample_count: 0,
            consecutive: 0,
            seeded: false,
        }
    }

    /// Feed one sample and return the verdict. Pure function of self + value.
    pub fn observe(&mut self, value: f64, config: &EwmaConfig) -> EwmaVerdict {
        self.sample_count += 1;

        if !self.seeded {
            self.mean = value;
            self.m2 = 0.0;
            self.seeded = true;
            self.consecutive = 0;
            return EwmaVerdict::Warming {
                have: self.sample_count,
                need: config.min_samples,
            };
        }

        // Update EWMA mean and variance.
        let alpha = config.alpha;
        let delta = value - self.mean;
        self.mean += alpha * delta;
        // Exponential moving variance: M2_n = (1-α)(M2_{n-1} + α * delta²)
        self.m2 = (1.0 - alpha) * (self.m2 + alpha * delta * delta);

        // Normalised deviation.
        // `powi` requires i32; sample_count is u32. For realistic sample counts
        // (< 2^31), try_from succeeds. For astronomically large counts, the
        // exponent saturates — the EWMA variance is essentially 1.0 at that
        // point anyway, so the exact exponent doesn't matter.
        let exponent = i32::try_from(self.sample_count)
            .unwrap_or(i32::MAX)
            .saturating_sub(1);
        let std_dev = (self.m2 / (1.0 - (1.0 - alpha).powi(exponent))).sqrt();
        let effective_std = std_dev.max(config.std_floor);
        let deviation = (value - self.mean).abs() / effective_std;

        // Gate: minimum samples.
        if self.sample_count < config.min_samples {
            self.consecutive = 0;
            return EwmaVerdict::Warming {
                have: self.sample_count,
                need: config.min_samples,
            };
        }

        // Gate: consecutive elevated.
        if deviation >= config.threshold {
            self.consecutive += 1;
        } else {
            self.consecutive = 0;
        }

        if self.consecutive >= config.min_consecutive {
            EwmaVerdict::Drift(EwmaFinding {
                deviation,
                ewma_mean: self.mean,
                ewma_std: effective_std,
                sample_value: value,
                consecutive: self.consecutive,
            })
        } else {
            EwmaVerdict::Normal
        }
    }
}

// ---------------------------------------------------------------------------
// CUSUM (Cumulative Sum) — Task 5b.2
// ---------------------------------------------------------------------------

/// Default slack (allowance) for the CUSUM detector.
///
/// Deviations smaller than `slack` are considered noise and do not accumulate.
/// Set to 1.0 here as a placeholder — roughly half the level detector's z threshold,
/// so a sustained deviation of half that magnitude is the smallest the CUSUM will
/// catch. **Uncalibrated.**
pub const DEFAULT_CUSUM_SLACK: f64 = 1.0;

/// Default alarm threshold for the CUSUM accumulators.
///
/// When `max(S+, S−)` reaches this value, a finding is reported. 5.0 is a rough
/// published starting point for the tabular CUSUM with k=1; at the daemon's 2s tick
/// a sustained 2-unit deviation would reach 5.0 after ~3 samples (6s).
/// **Uncalibrated.**
pub const DEFAULT_CUSUM_THRESHOLD: f64 = 5.0;

/// Minimum samples before the CUSUM can produce a verdict.
/// Same reasoning as EWMA's minimum: ~20s at a 2s tick. **Uncalibrated.**
pub const DEFAULT_CUSUM_MIN_SAMPLES: u32 = 10;

/// Configuration for the CUSUM detector.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CusumConfig {
    /// Slack: deviations within ±slack of the target are considered noise.
    pub slack: f64,
    /// Alarm threshold: max(S+, S−) reaching this triggers a finding.
    pub threshold: f64,
    /// Minimum samples before the detector is willing to fire.
    pub min_samples: u32,
}

impl Default for CusumConfig {
    fn default() -> Self {
        Self {
            slack: DEFAULT_CUSUM_SLACK,
            threshold: DEFAULT_CUSUM_THRESHOLD,
            min_samples: DEFAULT_CUSUM_MIN_SAMPLES,
        }
    }
}

/// Verdict of the CUSUM detector on one sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CusumVerdict {
    /// Not enough samples yet.
    Warming { have: u32, need: u32 },
    /// No sustained shift detected.
    Normal,
    /// Sustained shift detected.
    SustainedShift(CusumFinding),
}

/// Evidence behind a CUSUM sustained-shift finding.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CusumFinding {
    /// Which accumulator fired: `true` for positive (above target), `false` for negative.
    pub is_positive: bool,
    /// The accumulator value at the time of the alarm.
    pub accumulator: f64,
    /// The alarm threshold that was reached.
    pub threshold: f64,
    /// The deviation of the triggering sample from the target (before subtracting slack).
    pub deviation: f64,
    /// The sample value that triggered the alarm.
    pub sample_value: f64,
}

/// State for the CUSUM detector.
#[derive(Debug, Clone)]
pub struct CusumDetector {
    /// Cumulative sum for above-target shifts.
    cusum_plus: f64,
    /// Cumulative sum for below-target shifts.
    cusum_minus: f64,
    /// Total samples seen.
    sample_count: u32,
    /// After an alarm, the accumulators stay zeroed until the deviation returns
    /// below the slack. Without this, a continuing shift re-crosses the alarm
    /// threshold on the very next sample and reports one finding per tick — the
    /// "one shift, one finding" contract in `design.md` would be a lie.
    recovering: bool,
}

impl Default for CusumDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl CusumDetector {
    /// A new detector.
    pub fn new() -> Self {
        Self {
            cusum_plus: 0.0,
            cusum_minus: 0.0,
            sample_count: 0,
            recovering: false,
        }
    }

    /// Feed one sample and return the verdict.
    ///
    /// `target` is the expected baseline value (e.g. EWMA mean, or the last known normal
    /// level). The CUSUM accumulates signed deviations from this target beyond the slack.
    pub fn observe(&mut self, value: f64, target: f64, config: &CusumConfig) -> CusumVerdict {
        self.sample_count += 1;

        if self.sample_count < config.min_samples {
            return CusumVerdict::Warming {
                have: self.sample_count,
                need: config.min_samples,
            };
        }

        let deviation = value - target;
        let above = deviation - config.slack;
        let below = -deviation - config.slack;

        // While recovering from an alarm, accumulate nothing: the accumulators were
        // zeroed on alarm and stay zeroed until the deviation is back inside the
        // slack band. A shift that never ends reports once, not once per sample.
        if self.recovering {
            if above <= 0.0 && below <= 0.0 {
                self.recovering = false;
            }
            return CusumVerdict::Normal;
        }

        // Accumulate only positive excursions (tabular CUSUM).
        self.cusum_plus = (self.cusum_plus + above).max(0.0);
        self.cusum_minus = (self.cusum_minus + below).max(0.0);

        let (fired_positive, accumulator) = if self.cusum_plus >= config.threshold {
            (true, self.cusum_plus)
        } else if self.cusum_minus >= config.threshold {
            (false, self.cusum_minus)
        } else {
            return CusumVerdict::Normal;
        };

        // Reset on alarm — one shift, one finding. `recovering` keeps the accumulators
        // at zero until the deviation returns below the slack, so a sustained shift
        // cannot re-alarm on the next sample.
        self.cusum_plus = 0.0;
        self.cusum_minus = 0.0;
        self.recovering = true;

        CusumVerdict::SustainedShift(CusumFinding {
            is_positive: fired_positive,
            accumulator,
            threshold: config.threshold,
            deviation,
            sample_value: value,
        })
    }
}

// ---------------------------------------------------------------------------
// Change-Point Detector — Task 5b.3
// ---------------------------------------------------------------------------

/// Default minimum number of samples in each window for the change-point detector.
///
/// 20 samples at a 2s tick is ~40s per window, so the total lookback is ~80s. This
/// is deliberately shorter than the trend detector's window (600s) because a regime
/// change is a step, not a slow drift. **Uncalibrated.**
pub const DEFAULT_CHANGE_POINT_WINDOW: usize = 20;

/// Default threshold on the absolute mean difference between two adjacent windows
/// (as a fraction of the pooled standard deviation) for a change point to be
/// reported.
///
/// 1.5 standard deviations is a Cohen's d in the "medium-to-large" range. The intent
/// is to catch a real regime shift while ignoring normal noise. **Uncalibrated.**
pub const DEFAULT_CHANGE_POINT_EFFECT_SIZE: f64 = 1.5;

/// Minimum samples in each window before the change-point detector evaluates.
/// **Uncalibrated.**
pub const DEFAULT_CHANGE_POINT_MIN_WINDOW_SAMPLES: usize = 8;

/// Configuration for the change-point detector.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChangePointConfig {
    /// Number of samples in each of the two adjacent windows.
    pub window_size: usize,
    /// Minimum samples in each window before evaluation is attempted.
    pub min_window_samples: usize,
    /// Minimum Cohen's d (standardised mean difference) for a change point.
    pub effect_size_threshold: f64,
}

impl Default for ChangePointConfig {
    fn default() -> Self {
        Self {
            window_size: DEFAULT_CHANGE_POINT_WINDOW,
            min_window_samples: DEFAULT_CHANGE_POINT_MIN_WINDOW_SAMPLES,
            effect_size_threshold: DEFAULT_CHANGE_POINT_EFFECT_SIZE,
        }
    }
}

/// Verdict of the change-point detector on one sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChangePointVerdict {
    /// Not enough samples to form two windows.
    Warming { have: usize, need: usize },
    /// No regime change detected.
    Normal,
    /// A regime change was detected between the two most recent windows.
    RegimeChange(ChangePointFinding),
}

/// Evidence behind a change-point finding.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChangePointFinding {
    /// Mean of the earlier window.
    pub prior_mean: f64,
    /// Mean of the later window.
    pub later_mean: f64,
    /// Standardised effect size (Cohen's d).
    pub effect_size: f64,
    /// Pooled standard deviation.
    pub pooled_std: f64,
    /// Sample value that completed the detection window.
    pub sample_value: f64,
}

/// A fired finding from any of the incremental detectors, so the analysis engine can
/// route all three through one registry path.
///
/// `Warming` and `Normal` verdicts are deliberately absent: they are the anti-spike
/// suppression, not findings, and the engine writes nothing for them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IncrementalVerdict {
    /// EWMA drift: sustained elevated normalised deviation from the EWMA centre.
    EwmaDrift(EwmaFinding),
    /// CUSUM sustained shift: one accumulator crossed the alarm threshold.
    CusumShift(CusumFinding),
    /// Change point: a step to a new sustained regime between adjacent windows.
    ChangePoint(ChangePointFinding),
}

impl From<EwmaVerdict> for Option<IncrementalVerdict> {
    fn from(verdict: EwmaVerdict) -> Self {
        match verdict {
            EwmaVerdict::Drift(finding) => Some(IncrementalVerdict::EwmaDrift(finding)),
            EwmaVerdict::Warming { .. } | EwmaVerdict::Normal => None,
        }
    }
}

impl From<CusumVerdict> for Option<IncrementalVerdict> {
    fn from(verdict: CusumVerdict) -> Self {
        match verdict {
            CusumVerdict::SustainedShift(finding) => Some(IncrementalVerdict::CusumShift(finding)),
            CusumVerdict::Warming { .. } | CusumVerdict::Normal => None,
        }
    }
}

impl From<ChangePointVerdict> for Option<IncrementalVerdict> {
    fn from(verdict: ChangePointVerdict) -> Self {
        match verdict {
            ChangePointVerdict::RegimeChange(finding) => {
                Some(IncrementalVerdict::ChangePoint(finding))
            }
            ChangePointVerdict::Warming { .. } | ChangePointVerdict::Normal => None,
        }
    }
}

/// State for the change-point detector, using a fixed-capacity ring buffer.
#[derive(Debug, Clone)]
pub struct ChangePointDetector {
    /// Ring buffer for the current window.
    ring: Vec<f64>,
    /// Write position in the ring.
    pos: usize,
    /// Number of samples written (capped at ring capacity).
    filled: usize,
}

impl ChangePointDetector {
    /// A new detector with the configured window size.
    pub fn new(window_size: usize) -> Self {
        Self {
            ring: vec![0.0; window_size],
            pos: 0,
            filled: 0,
        }
    }

    fn mean_and_std(window: &[f64]) -> (f64, f64) {
        let n = window.len();
        if n == 0 {
            return (0.0, 0.0);
        }
        let n_f64 = usize_to_f64(n);
        let mean = window.iter().sum::<f64>() / n_f64;
        let variance = window.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / n_f64;
        (mean, variance.sqrt())
    }

    /// Feed one sample and return the verdict.
    ///
    /// Internally maintains a sliding window of the most recent `window_size` samples.
    /// When enough data is available, it splits the window in half and compares the
    /// two halves.
    pub fn observe(&mut self, value: f64, config: &ChangePointConfig) -> ChangePointVerdict {
        self.ring[self.pos] = value;
        self.pos = (self.pos + 1) % self.ring.len();
        self.filled += 1;
        if self.filled < self.ring.len() {
            return ChangePointVerdict::Warming {
                have: self.filled,
                need: self.ring.len(),
            };
        }

        // Read the ring in order (oldest first).
        let window_size = self.ring.len();
        let half = window_size / 2;
        let mut ordered = Vec::with_capacity(window_size);
        for i in 0..window_size {
            let idx = (self.pos + i) % window_size;
            ordered.push(self.ring[idx]);
        }

        if half < config.min_window_samples || window_size - half < config.min_window_samples {
            return ChangePointVerdict::Warming {
                have: self.filled,
                need: self.ring.len(),
            };
        }

        let (prior_mean, prior_std) = Self::mean_and_std(&ordered[..half]);
        let (later_mean, later_std) = Self::mean_and_std(&ordered[half..]);

        // Pooled standard deviation.
        let n1 = usize_to_f64(half);
        let n2 = usize_to_f64(window_size - half);
        let var1 = prior_std * prior_std;
        let var2 = later_std * later_std;
        let pooled_var = ((n1 - 1.0) * var1 + (n2 - 1.0) * var2) / (n1 + n2 - 2.0);
        let pooled_std = pooled_var.max(0.0).sqrt();

        // Cohen's d.
        let effect_size = if pooled_std > 1e-12 {
            (later_mean - prior_mean).abs() / pooled_std
        } else if (later_mean - prior_mean).abs() > 1e-12 {
            // Non-zero difference but zero variance: maximum effect.
            f64::INFINITY
        } else {
            0.0
        };

        if effect_size >= config.effect_size_threshold {
            ChangePointVerdict::RegimeChange(ChangePointFinding {
                prior_mean,
                later_mean,
                effect_size,
                pooled_std,
                sample_value: value,
            })
        } else {
            ChangePointVerdict::Normal
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- EWMA tests: fires on its own shape, silent on the shapes the others own ----

    #[test]
    fn ewma_detects_gradual_drift() {
        // Alpha 0.1, drift 0.2/sample, noise 0.05: the drift eventually exceeds the
        // baseline noise and the EWMA lag, triggering a finding. The noise comes from
        // a fixed LCG so the test is deterministic — the detector contract demands it.
        let config = EwmaConfig {
            alpha: 0.1,
            min_samples: 5,
            min_consecutive: 3,
            threshold: 2.0,
            ..EwmaConfig::default()
        };
        let mut det = EwmaDetector::new();
        // Deterministic noise: x_{n+1} = (1103515245*x + 12345) mod 2^32, mapped to ±0.05.
        let mut noise_state: u64 = 42;
        let mut noise = || {
            noise_state =
                noise_state.wrapping_mul(1_103_515_245).wrapping_add(12_345) & 0xFFFF_FFFF;
            oxmgr_core::numeric::u64_to_f64(noise_state >> 8)
                / oxmgr_core::numeric::u64_to_f64(1u64 << 24)
                * 0.1
                - 0.05
        };
        // Warmup with noise.
        for _ in 0..40 {
            det.observe(10.0 + noise(), &config);
        }
        // Ramp: each sample 0.2 higher than last.
        let mut found = false;
        for i in 0..40 {
            let v = 10.0 + 0.2 * usize_to_f64(i + 1) + noise();
            if let EwmaVerdict::Drift(_) = det.observe(v, &config) {
                found = true;
                break;
            }
        }
        assert!(found, "EWMA must detect gradual drift");
    }

    #[test]
    fn ewma_silent_on_noise_around_stable_level() {
        let config = EwmaConfig {
            min_samples: 5,
            min_consecutive: 3,
            threshold: 3.0,
            ..EwmaConfig::default()
        };
        let mut det = EwmaDetector::new();
        for v in [100.0; 20] {
            let result = det.observe(v, &config);
            assert!(
                matches!(result, EwmaVerdict::Warming { .. } | EwmaVerdict::Normal),
                "stable noise must not produce a finding: {result:?}"
            );
        }
    }

    #[test]
    fn ewma_alternating_deviations_do_not_accumulate() {
        let config = EwmaConfig {
            min_samples: 5,
            min_consecutive: 3,
            threshold: 2.0,
            ..EwmaConfig::default()
        };
        let mut det = EwmaDetector::new();
        // Warm up.
        for v in [50.0; 10] {
            det.observe(v, &config);
        }
        // Alternate above and below.
        let mut any_drift = false;
        for i in 0..20 {
            let v = if i % 2 == 0 { 52.0 } else { 48.0 };
            if let EwmaVerdict::Drift(_) = det.observe(v, &config) {
                any_drift = true;
                break;
            }
        }
        assert!(
            !any_drift,
            "alternating deviations must not trigger EWMA drift"
        );
    }

    // ---- CUSUM tests ----

    #[test]
    fn cusum_detects_sustained_shift() {
        let config = CusumConfig {
            slack: 0.5,
            threshold: 3.0,
            min_samples: 5,
        };
        let mut det = CusumDetector::new();
        // Warm up: feed samples at the target.
        for _ in 0..5 {
            det.observe(100.0, 100.0, &config);
        }
        // Feed a sustained shift above the target.
        let mut found = false;
        for _ in 0..10 {
            if let CusumVerdict::SustainedShift(_) = det.observe(102.0, 100.0, &config) {
                found = true;
                break;
            }
        }
        assert!(found, "CUSUM must detect a sustained shift");
    }

    #[test]
    fn cusum_alternating_deviations_do_not_accumulate() {
        let config = CusumConfig {
            slack: 0.5,
            threshold: 3.0,
            min_samples: 5,
        };
        let mut det = CusumDetector::new();
        for _ in 0..5 {
            det.observe(100.0, 100.0, &config);
        }
        // Alternate above and below the target.
        let mut any_alarm = false;
        for i in 0..20 {
            let v = if i % 2 == 0 { 102.0 } else { 98.0 };
            if let CusumVerdict::SustainedShift(_) = det.observe(v, 100.0, &config) {
                any_alarm = true;
                break;
            }
        }
        assert!(!any_alarm, "alternating deviations must not trigger CUSUM");
    }

    #[test]
    fn cusum_resets_after_alarm() {
        let config = CusumConfig {
            slack: 0.5,
            threshold: 2.0,
            min_samples: 5,
        };
        let mut det = CusumDetector::new();
        for _ in 0..5 {
            det.observe(100.0, 100.0, &config);
        }
        // Trigger first alarm.
        let mut found_first = false;
        for _ in 0..10 {
            if let CusumVerdict::SustainedShift(_) = det.observe(103.0, 100.0, &config) {
                found_first = true;
                break;
            }
        }
        assert!(found_first, "first CUSUM alarm must fire");
        // Continue at same shifted level: must NOT re-alarm immediately (recovery).
        for _ in 0..5 {
            let v = det.observe(103.0, 100.0, &config);
            assert_eq!(v, CusumVerdict::Normal, "must not re-alarm during recovery");
        }
        // Return to normal: resets recovery, allowing a new alarm.
        det.observe(100.0, 100.0, &config);
        // Feed shifted level again: a second alarm must fire.
        let mut found_third = false;
        for _ in 0..10 {
            if let CusumVerdict::SustainedShift(_) = det.observe(103.0, 100.0, &config) {
                found_third = true;
                break;
            }
        }
        assert!(
            found_third,
            "CUSUM must allow new alarm after shift recovery"
        );
    }

    // ---- Change-point tests ----

    #[test]
    fn change_point_detects_regime_shift() {
        let config = ChangePointConfig {
            window_size: 20,
            min_window_samples: 8,
            effect_size_threshold: 1.5,
        };
        let mut det = ChangePointDetector::new(config.window_size);
        // 20 samples at level 100.
        for _ in 0..20 {
            det.observe(100.0, &config);
        }
        // 20 samples at level 120 (step).
        let mut found = false;
        for _ in 0..20 {
            if let ChangePointVerdict::RegimeChange(_) = det.observe(120.0, &config) {
                found = true;
                break;
            }
        }
        assert!(found, "change-point detector must detect regime shift");
    }

    #[test]
    fn change_point_silent_on_temporary_excursion() {
        let config = ChangePointConfig {
            window_size: 20,
            min_window_samples: 8,
            effect_size_threshold: 1.5,
        };
        let mut det = ChangePointDetector::new(config.window_size);
        // 30 samples at level 100.
        for _ in 0..30 {
            det.observe(100.0, &config);
        }
        // 10 elevated samples — less than window_size, so the excursion is not a
        // sustained regime.
        for _ in 0..10 {
            det.observe(120.0, &config);
        }
        // 20 samples at level 100: enough to flush the excursion out of the ring
        // completely. Once the window is fully back to 100, there must be no
        // finding — the detector must not keep re-reporting a transition that
        // already ended.
        for _ in 0..20 {
            det.observe(100.0, &config);
        }
        // Now every observation must be silent.
        for _ in 0..10 {
            let v = det.observe(100.0, &config);
            assert!(
                matches!(
                    v,
                    ChangePointVerdict::Warming { .. } | ChangePointVerdict::Normal
                ),
                "excursion return must not produce change-point: {v:?}"
            );
        }
    }

    #[test]
    fn change_point_silent_on_flat_series() {
        let config = ChangePointConfig::default();
        let mut det = ChangePointDetector::new(config.window_size);
        for _ in 0..40 {
            let result = det.observe(42.0, &config);
            assert!(
                matches!(
                    result,
                    ChangePointVerdict::Warming { .. } | ChangePointVerdict::Normal
                ),
                "flat series must not trigger change-point: {result:?}"
            );
        }
    }
}
