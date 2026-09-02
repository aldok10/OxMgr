//! Trend, leak, and forecast analysis over retained history.
//!
//! Scaffold for OpenSpec change `process-intelligence`, tasks section(s) 6.
//! The contract is `openspec/changes/process-intelligence/specs/`; read it before adding to this file.
//!
//! Pure logic on purpose: nothing here reaches for the process manager, the HTTP layer or the
//! dashboard, so it can be unit-tested without a running daemon. Wiring it into those is a
//! separate integration step, which is why the module is currently unreferenced.
//!
//! # What this module answers
//!
//! One question: *is this series growing in the shape a resource leak makes, and if so, when
//! would it reach a limit?* It answers with a fit, a confidence, and its evidence — or it
//! refuses and names the reason. Refusal is the common case by design: the spec's
//! "fluctuating growth is not a leak" and "a short history is not enough" scenarios are
//! silence requirements, and silence that cannot explain itself is indistinguishable from a
//! broken detector.
//!
//! # Time is not a sample index
//!
//! Every statistic here uses the sample's own timestamp, never its position in the slice. The
//! daemon's maintenance tick is 2s with `MissedTickBehavior::Skip` (`src/daemon.rs`), so under
//! load a tick is dropped rather than queued and the spacing stretches; retained history is
//! also downsampled into coarser tiers, so one slice can mix 2s and 60s spacing. A regression
//! over `0, 1, 2, …` would silently rescale the slope by whatever the spacing happened to be,
//! and the forecast — a slope divided into a remaining distance — would be wrong by the same
//! factor. Slope is therefore always *per second*.
//!
//! # Three shapes that are not leaks, and are handled explicitly
//!
//! - **Sawtooth** (grow, restart or free, grow again). Fitting across the teeth understates
//!   the slope; fitting only the last tooth manufactures a clean leak out of normal
//!   behaviour. Handled by [`segment_series`]: one large drop splits the series and analysis
//!   continues on the newest piece; two or more large drops are a recurring pattern and the
//!   answer is [`NoLeakReason::RepeatedDrops`], not a finding.
//! - **A counter reset.** `process-io-metrics` records disk I/O as monotonic counters that
//!   restart at zero when the process does. A reset reads as one enormous negative delta, so
//!   an unguarded regression would report a *downward* trend on a process that is fine. The
//!   drop split above removes that: no fit ever spans a reset, and no downward slope is ever
//!   reported as anything at all — this detector reports growth or nothing.
//! - **A gap.** A stopped process, a skipped tick, or a downsampling boundary leaves a hole.
//!   Nothing is interpolated across it and the hole is not read as a value: the series splits
//!   there too, and if the newest piece is then too short the answer is silence.
//!
//! # Every threshold here is uncalibrated
//!
//! The defaults in [`TrendConfig`] are chosen to be conservative against the synthetic series
//! in this module's tests. None of them has been measured against a real workload, which is
//! the only thing that can set them: `design.md` puts trace-based calibration in a later
//! phase and this module cannot pre-empt it. Read them as "not yet wrong" rather than right.

/// One retained sample of one metric for one process.
///
/// Deliberately minimal and owned by this module: the history tiers are being written
/// separately, so depending on their type here would couple two unfinished things. Whatever
/// shape retention settles on, projecting it into a slice of these is a `map`.
///
/// `at_unix_ms` is milliseconds since the Unix epoch. Milliseconds rather than seconds because
/// the 2s tick would quantise badly at second resolution over a short window; integer rather
/// than float so equal timestamps compare exactly.
use oxmgr_core::numeric::{u64_to_f64, usize_to_f64};
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrendSample {
    /// When the sample was taken, in milliseconds since the Unix epoch.
    pub at_unix_ms: u64,
    /// The metric value. Absence is expressed by omitting the sample, never by a zero.
    pub value: f64,
}

impl TrendSample {
    /// Convenience constructor, mostly for tests and call sites mapping from retention.
    pub fn new(at_unix_ms: u64, value: f64) -> Self {
        Self { at_unix_ms, value }
    }
}

/// Gates a leak finding must clear, and the shape of "growing" this module recognises.
///
/// Every field is a threshold, and **every one is uncalibrated** — see the module docs. They
/// are grouped here rather than as constants so a per-process override is a value, not a
/// rebuild, which is what the spec's "thresholds can be overridden per process" requires.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrendConfig {
    /// Fewest samples that may produce any answer.
    ///
    /// Ordinary least squares needs 3 points before it has a residual degree of freedom at
    /// all, and 2 points fit any line perfectly — which would report `r² = 1` on pure noise.
    /// 8 is well clear of that while still reachable inside a few minutes of 2s ticks.
    pub min_samples: usize,
    /// Shortest span, in seconds, that may produce a leak finding.
    ///
    /// A leak is a statement about hours. 8 samples 2 seconds apart is 14 seconds of evidence
    /// and describes a burst, so sample count alone is not enough of a gate. 600s is a
    /// deliberately conservative starting point and is the threshold most likely to be wrong.
    pub min_window_secs: u64,
    /// Least coefficient of determination for the fitted line to count as describing the data.
    ///
    /// This is the spec's "a poor fit is not reported" gate, and 0.85 is not a round number
    /// chosen for feel. A clean step — flat at one level for half the window, flat at a higher
    /// level for the other half — fits a straight line with r² of **exactly 0.75**, which
    /// `a_step_is_a_poor_fit_and_not_a_leak` measures. That is a regime change (a deploy, a
    /// cache filling) and another detector's business, so the gate has to sit clear above it or
    /// every step reads as a leak. 0.85 leaves margin; it still admits a genuinely noisy ramp,
    /// which the fluctuating-growth test confirms fits at about 0.95 and is then refused on
    /// monotonicity instead.
    pub min_r_squared: f64,
    /// Least fraction of consecutive deltas that must be non-negative.
    ///
    /// The spec's separator between a leak and load-driven growth: both can fit a rising line
    /// well, but load falls back along the way and a leak mostly does not. Computed within one
    /// segment only, so it never counts a reset or a gap as a fall.
    pub min_monotonic_ratio: f64,
    /// Fraction of the previous value a fall must exceed to be treated as a reset or a free.
    ///
    /// 0.5 — a halving. Large enough that ordinary variation does not split the series,
    /// small enough to catch a counter reset to zero and a heap released back.
    pub reset_drop_fraction: f64,
    /// Multiple of the median interval above which a gap is assumed rather than a slow tick.
    ///
    /// `MissedTickBehavior::Skip` means one missed tick roughly doubles an interval, so the
    /// factor has to sit clear of that. 4x tolerates a couple of skipped ticks and still
    /// splits at a process that was stopped for a while.
    pub max_gap_factor: f64,
    /// Least Student *t* on the slope for the growth to count as real rather than noise.
    ///
    /// The spec's "statistically significant" gate. 3.0 is roughly the two-sided 1% critical
    /// value once there are a dozen-odd points, and this module compares against a fixed
    /// number rather than a per-dof table: the table would make the gate tighter on short
    /// windows, which [`Self::min_samples`] already refuses.
    pub min_slope_t: f64,
    /// How far past the observed window a forecast may reach, as a multiple of that window.
    ///
    /// Extrapolating an hour of data three days forward is not a forecast, it is arithmetic
    /// wearing a forecast's clothes: nothing in the data speaks to that horizon. 3.0 means an
    /// hour of evidence may talk about the next three hours and no further.
    pub max_horizon_window_multiple: f64,
    /// Sample count at which the maturity term of the confidence score reaches 1.
    ///
    /// Not a gate — [`Self::min_samples`] is the gate. This only stops a finding from the
    /// shortest admissible window scoring as high as one from a long one.
    pub confident_samples: usize,
}

impl Default for TrendConfig {
    fn default() -> Self {
        Self {
            min_samples: 8,
            min_window_secs: 600,
            min_r_squared: 0.85,
            min_monotonic_ratio: 0.8,
            reset_drop_fraction: 0.5,
            max_gap_factor: 4.0,
            min_slope_t: 3.0,
            max_horizon_window_multiple: 3.0,
            confident_samples: 40,
        }
    }
}

/// Running sums for a least-squares fit of `y` against time in seconds.
///
/// Task 6.1: the five sums (plus `Σy²`, needed for `r²`) are all a slope, an intercept, a fit
/// quality and a standard error require, and each is a multiply-add per sample. That is what
/// keeps per-tick analysis cost independent of retained history — the constraint `design.md`
/// states as hard, because analysis shares the maintenance tick with restarts and health
/// checks. The struct is public so a caller can carry it incrementally instead of re-walking
/// a window; [`analyse_trend`] fills a fresh one, which costs one pass over a bounded slice.
///
/// `x` is seconds relative to a caller-chosen origin, not an absolute epoch: `Σx²` on epoch
/// seconds is around 3e18 and the `Sxx = Σx² − (Σx)²/n` cancellation loses most of its
/// significant digits. [`analyse_trend`] uses the segment's first timestamp as the origin.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RegressionSums {
    n: u64,
    sum_x: f64,
    sum_y: f64,
    sum_xy: f64,
    sum_xx: f64,
    sum_yy: f64,
}

impl RegressionSums {
    /// An empty accumulator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds one point in. Constant cost, no allocation.
    pub fn push(&mut self, x_secs: f64, y: f64) {
        self.n += 1;
        self.sum_x += x_secs;
        self.sum_y += y;
        self.sum_xy += x_secs * y;
        self.sum_xx += x_secs * x_secs;
        self.sum_yy += y * y;
    }

    /// How many points have been folded in.
    #[cfg(test)]
    pub fn count(&self) -> u64 {
        self.n
    }

    /// `Σ(x − x̄)²`. Zero when every `x` is identical, which is the degenerate case every
    /// caller below has to check before dividing by it.
    fn sxx(&self) -> f64 {
        let n = u64_to_f64(self.n);
        (self.sum_xx - self.sum_x * self.sum_x / n).max(0.0)
    }

    /// `Σ(y − ȳ)²`. Zero for a perfectly flat series.
    fn syy(&self) -> f64 {
        let n = u64_to_f64(self.n);
        (self.sum_yy - self.sum_y * self.sum_y / n).max(0.0)
    }

    /// `Σ(x − x̄)(y − ȳ)`. Signed; its sign is the sign of the slope.
    fn sxy(&self) -> f64 {
        let n = u64_to_f64(self.n);
        self.sum_xy - self.sum_x * self.sum_y / n
    }

    /// Least-squares slope, in units of `y` per second. `None` when undefined.
    pub fn slope(&self) -> Option<f64> {
        if self.n < 2 {
            return None;
        }
        let sxx = self.sxx();
        if sxx <= 0.0 {
            return None;
        }
        Some(self.sxy() / sxx)
    }

    /// Value the fitted line takes at `x = 0`. `None` whenever the slope is.
    pub fn intercept(&self) -> Option<f64> {
        let slope = self.slope()?;
        let n = u64_to_f64(self.n);
        Some((self.sum_y - slope * self.sum_x) / n)
    }

    /// Residual sum of squares, `Σ(y − ŷ)²`, in the algebraic form `Syy − Sxy²/Sxx`.
    ///
    /// Clamped at zero: the subtraction can land a hair below it in floating point on a
    /// perfect fit, and a negative residual would poison the square roots downstream.
    fn residual_ss(&self) -> Option<f64> {
        if self.n < 2 {
            return None;
        }
        let sxx = self.sxx();
        if sxx <= 0.0 {
            return None;
        }
        let sxy = self.sxy();
        Some((self.syy() - sxy * sxy / sxx).max(0.0))
    }

    /// Coefficient of determination. `None` when undefined.
    ///
    /// A flat series has `Syy = 0`: nothing to explain, so "how much is explained" has no
    /// answer and this returns `None` rather than the 1.0 that `1 − 0/0` would suggest. That
    /// distinction is what stops a constant metric from passing the fit-quality gate.
    pub fn r_squared(&self) -> Option<f64> {
        let syy = self.syy();
        if syy <= 0.0 {
            return None;
        }
        let rss = self.residual_ss()?;
        Some((1.0 - rss / syy).clamp(0.0, 1.0))
    }

    /// Standard error of the slope, `sqrt(RSS / (n − 2) / Sxx)`.
    ///
    /// `None` below 3 points, where there is no residual degree of freedom. Exactly `0.0` for
    /// a perfectly straight series — real, not an error, and handled where it is divided by.
    pub fn slope_std_error(&self) -> Option<f64> {
        if self.n < 3 {
            return None;
        }
        let rss = self.residual_ss()?;
        let dof = u64_to_f64(self.n - 2);
        Some((rss / dof / self.sxx()).sqrt())
    }

    /// Student *t* for the null hypothesis "slope is zero".
    ///
    /// Infinite when the standard error is zero and the slope is not: a perfectly straight
    /// series is the strongest possible evidence of a trend, not a division error. `None` when
    /// both are zero, since a flat line is no evidence of anything.
    pub fn slope_t_statistic(&self) -> Option<f64> {
        let slope = self.slope()?;
        let se = self.slope_std_error()?;
        if se > 0.0 {
            Some(slope / se)
        } else if slope != 0.0 {
            Some(f64::INFINITY * slope.signum())
        } else {
            None
        }
    }
}

/// Why the series was cut, at the point it was cut.
///
/// Kept in the result so a refusal can name its cause: "no leak" and "the counter reset 30
/// seconds ago so there is not enough clean history yet" are different operational facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentBreak {
    /// The value fell by more than the configured fraction of the previous value.
    ///
    /// Covers both a counter reset on restart and a heap actually released. This module cannot
    /// tell those apart from the series alone and does not pretend to: either way the fit must
    /// not span the discontinuity, which is the only decision that depends on it.
    LargeDrop,
    /// The interval to the next sample exceeded the median interval by more than the gap
    /// factor. Nothing is interpolated across it.
    Gap,
}

/// A contiguous run of samples with no reset and no gap inside it.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment<'a> {
    /// The samples, in the order given.
    pub samples: &'a [TrendSample],
    /// What ended the previous segment, or `None` for the first.
    pub preceded_by: Option<SegmentBreak>,
}

impl Segment<'_> {
    /// Seconds from the first sample to the last. Zero for a single sample.
    pub fn window_secs(&self) -> f64 {
        match (self.samples.first(), self.samples.last()) {
            (Some(first), Some(last)) => {
                u64_to_f64(last.at_unix_ms.saturating_sub(first.at_unix_ms)) / 1000.0
            }
            _ => 0.0,
        }
    }
}

/// Median of the consecutive intervals, in milliseconds. `None` below two samples.
///
/// Median rather than mean because `MissedTickBehavior::Skip` produces outlier intervals by
/// design, and one 40-second stall would drag a mean far enough that a real gap no longer looks
/// like one. Allocates a small vector: intervals must be sorted, and the alternative — a
/// running quantile estimate — would be approximate for no useful saving on a bounded window.
fn median_interval_ms(samples: &[TrendSample]) -> Option<f64> {
    if samples.len() < 2 {
        return None;
    }
    let mut deltas: Vec<u64> = samples
        .windows(2)
        .map(|w| w[1].at_unix_ms.saturating_sub(w[0].at_unix_ms))
        .collect();
    deltas.sort_unstable();
    let mid = deltas.len() / 2;
    // Median of u64 ms deltas — values < 2^53 by construction (wall-clock differences); exact.
    Some(if deltas.len().is_multiple_of(2) {
        (u64_to_f64(deltas[mid - 1]) + u64_to_f64(deltas[mid])) / 2.0
    } else {
        u64_to_f64(deltas[mid])
    })
}

/// Splits a series at counter resets, large frees, and gaps in coverage.
///
/// This is the function that keeps the three not-a-leak shapes from being reported as leaks,
/// and it runs before any arithmetic:
///
/// - A fall past `reset_drop_fraction` of the previous value ends a segment. `process-io-metrics`
///   made per-process I/O a monotonic counter that restarts at zero with the process, so
///   without this a restart would fit as a steep *downward* trend. It also stops a sawtooth
///   from being fitted across its teeth, where the resulting slope describes neither the rises
///   nor the resets.
/// - An interval more than `max_gap_factor` times the median ends a segment. The missing time
///   is left missing: joining across it would invent a straight line through a period when the
///   process may have been stopped, and the spec is explicit that an empty window is empty
///   rather than zero.
///
/// Samples must be ordered oldest-first; out-of-order input is not reordered here, and a
/// backwards timestamp reads as a zero-length interval rather than a negative one.
pub fn segment_series<'a>(samples: &'a [TrendSample], config: &TrendConfig) -> Vec<Segment<'a>> {
    if samples.is_empty() {
        return Vec::new();
    }
    let gap_threshold_ms = median_interval_ms(samples)
        .map(|median| median * config.max_gap_factor)
        .filter(|t| *t > 0.0);

    // Upper bound: a segment can only break between samples, so at most one per sample.
    let mut segments = Vec::with_capacity(samples.len());
    let mut start = 0usize;
    let mut preceded_by: Option<SegmentBreak> = None;

    for i in 1..samples.len() {
        let previous = samples[i - 1];
        let current = samples[i];

        // A drop is judged against the magnitude of what came before, so the same rule works
        // for a 4 GiB counter and a 30 MiB heap. `abs` keeps a negative-valued metric — none
        // exist today, but nothing here requires non-negativity — from inverting the test.
        let dropped = current.value < previous.value
            && (previous.value - current.value)
                >= config.reset_drop_fraction * previous.value.abs()
            && previous.value.abs() > 0.0;

        let gapped = gap_threshold_ms.is_some_and(|threshold| {
            // Wall-clock ms difference — < 2^53 by construction; exact in f64.
            let ms_diff = current.at_unix_ms.saturating_sub(previous.at_unix_ms);
            let ms_diff_f64 = u64_to_f64(ms_diff);
            ms_diff_f64 > threshold
        });

        // Drop takes precedence in the report when both hold: a restart usually produces both
        // a gap and a reset, and "the counter went back to zero" is the more useful cause.
        let breakage = if dropped {
            Some(SegmentBreak::LargeDrop)
        } else if gapped {
            Some(SegmentBreak::Gap)
        } else {
            None
        };

        if let Some(breakage) = breakage {
            segments.push(Segment {
                samples: &samples[start..i],
                preceded_by,
            });
            preceded_by = Some(breakage);
            start = i;
        }
    }
    segments.push(Segment {
        samples: &samples[start..],
        preceded_by,
    });
    segments
}

/// Fraction of consecutive deltas within a segment that do not fall.
///
/// The spec's leak-versus-load separator. A leak's series is dominated by rises with flat
/// stretches between allocations; load-driven growth gives back what it took, repeatedly. Ties
/// (`delta == 0`) count as non-negative — a flat stretch in a growing series is not evidence
/// against a leak — which is why a genuinely flat series scores 1.0 here and has to be
/// excluded by the fit-quality and significance gates instead, not by this one.
///
/// `None` below two samples. Only ever called within a segment, so it cannot count a reset or
/// a gap as a fall.
pub fn monotonic_ratio(samples: &[TrendSample]) -> Option<f64> {
    if samples.len() < 2 {
        return None;
    }
    let non_negative = samples
        .windows(2)
        .filter(|w| w[1].value >= w[0].value)
        .count();
    // Counts bounded by ring capacity ≪ 2^53; exact in f64.
    Some(usize_to_f64(non_negative) / usize_to_f64(samples.len() - 1))
}

/// The fitted line and its quality, for the analysed segment.
///
/// This is the evidence half of a finding: every gate the detector applied can be re-checked
/// by hand from these numbers, which is what the spec's "a finding can be recomputed from its
/// evidence" scenario asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrendFit {
    /// Slope in metric units **per second**. Signed.
    pub slope_per_sec: f64,
    /// Fitted value at the segment's first sample, in metric units.
    pub intercept_at_window_start: f64,
    /// Coefficient of determination in `[0,1]`. `None` for a flat series, where it is undefined.
    pub r_squared: Option<f64>,
    /// Standard error of the slope. `0.0` on a perfect fit.
    pub slope_std_error: f64,
    /// `slope / std_error`. Infinite on a perfect non-flat fit.
    pub slope_t: f64,
    /// Fraction of within-segment deltas that did not fall.
    pub monotonic_ratio: f64,
    /// Seconds from the segment's first sample to its last.
    pub window_secs: f64,
    /// How many samples were fitted.
    pub sample_count: usize,
    /// Unix ms of the segment's first sample.
    pub window_start_unix_ms: u64,
    /// Unix ms of the segment's last sample.
    pub window_end_unix_ms: u64,
    /// The last observed value, which is where a forecast starts from.
    pub last_value: f64,
    /// What ended the segment before this one, if any.
    pub preceded_by: Option<SegmentBreak>,
}

impl TrendFit {
    /// The fitted value at `seconds_from_window_start`, extrapolating past the window.
    #[cfg(test)]
    pub fn value_at(&self, seconds_from_window_start: f64) -> f64 {
        self.intercept_at_window_start + self.slope_per_sec * seconds_from_window_start
    }
}

/// Why no leak was reported. Every variant is a distinct operational statement.
///
/// The spec requires silence for most of these; this type is what makes the silence
/// explainable, which is the difference between "nothing is wrong" and "the detector had
/// nothing to work with". `design.md` calls that distinction out as the thing that stops a
/// detector becoming folklore.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NoLeakReason {
    /// Fewer samples than `min_samples`, after segmentation.
    ///
    /// Reported as its own reason and never as a flat trend: too few points means no answer.
    TooFewSamples { have: usize, need: usize },
    /// The usable segment spans less time than `min_window_secs`.
    WindowTooShort { have_secs: f64, need_secs: f64 },
    /// Every timestamp in the segment is identical, so no line is defined.
    ///
    /// Reachable when a caller feeds several metrics recorded in one tick with one timestamp.
    NoTimeSpan,
    /// The series does not vary, so `r²` is undefined and there is no trend to test.
    FlatSeries,
    /// The fitted slope is zero or negative.
    ///
    /// Includes the counter-reset case by construction: the reset ends a segment, so a negative
    /// slope here is a genuine decline within clean data, and a decline is not this detector's
    /// business. It is never reported as a downward *trend finding*, only as the absence of a
    /// leak.
    NotGrowing { slope_per_sec: f64 },
    /// The slope is positive but indistinguishable from noise at `min_slope_t`.
    NotSignificant { slope_t: f64, need_t: f64 },
    /// The line does not describe the data well enough — the spec's "poor fit" refusal.
    PoorFit { r_squared: f64, need_r_squared: f64 },
    /// Growth that falls back too often to be a leak — the spec's "fluctuating growth" refusal.
    ///
    /// This is the load-driven-growth case, and the one gate that a rising, well-fitting,
    /// significant series can still fail.
    NotMonotonic { ratio: f64, need_ratio: f64 },
    /// Two or more large drops: a sawtooth, not a leak.
    ///
    /// A single drop is treated as one discontinuity and analysis continues on the newest
    /// segment. A recurring one is a *pattern*, and a process that grows and is reset
    /// repeatedly is behaving as it always has. Reporting the last tooth as a leak would fire
    /// once per tooth forever.
    RepeatedDrops { drops: usize },
    /// No samples at all.
    NoSamples,
}

/// The named terms behind a confidence score, and the score itself.
///
/// The spec requires confidence in `[0,1]` from a documented formula with visible terms. The
/// formula is the arithmetic mean of the four terms below — equal weights, because there is no
/// evidence yet on which term predicts a true positive best, and inventing weights would dress
/// a guess as a measurement.
///
/// The score is **ordinal, not a probability**. It exists so two findings can be ranked and so
/// the ranking is reproducible. It is not calibrated against any observed leak rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrendConfidence {
    /// How far past the significance gate the *t* statistic sits, saturating at twice the gate.
    pub significance_term: f64,
    /// Fit quality, rescaled so the `min_r_squared` gate maps to 0 and a perfect fit to 1.
    pub fit_term: f64,
    /// Monotonicity, rescaled so the `min_monotonic_ratio` gate maps to 0 and 1.0 maps to 1.
    pub monotonicity_term: f64,
    /// Sample count against `confident_samples`, capped at 1 — the spec's maturity term.
    pub maturity_term: f64,
    /// Arithmetic mean of the four terms, in `[0,1]`.
    pub score: f64,
}

impl TrendConfidence {
    /// Computes the four terms and their mean.
    ///
    /// Each term is normalised so that a finding sitting exactly on its gate contributes 0 —
    /// a barely-qualifying finding should score near the bottom, not in the middle. The
    /// saturation on the significance term matters because `slope_t` is infinite for a
    /// perfectly straight series, which would otherwise make the mean `NaN`.
    fn compute(fit: &TrendFit, config: &TrendConfig) -> Self {
        let significance_term = if config.min_slope_t > 0.0 {
            ((fit.slope_t - config.min_slope_t) / config.min_slope_t).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let fit_term = match fit.r_squared {
            Some(r2) if config.min_r_squared < 1.0 => {
                ((r2 - config.min_r_squared) / (1.0 - config.min_r_squared)).clamp(0.0, 1.0)
            }
            Some(_) => 1.0,
            None => 0.0,
        };
        let monotonicity_term = if config.min_monotonic_ratio < 1.0 {
            ((fit.monotonic_ratio - config.min_monotonic_ratio)
                / (1.0 - config.min_monotonic_ratio))
                .clamp(0.0, 1.0)
        } else {
            1.0
        };
        let maturity_term = if config.confident_samples > 0 {
            // Counts bounded by ring capacity ≪ 2^53; exact in f64.
            (usize_to_f64(fit.sample_count) / usize_to_f64(config.confident_samples))
                .clamp(0.0, 1.0)
        } else {
            1.0
        };
        let score = (significance_term + fit_term + monotonicity_term + maturity_term) / 4.0;
        Self {
            significance_term,
            fit_term,
            monotonicity_term,
            maturity_term,
            score,
        }
    }
}

/// A sustained-growth finding: the leak shape, with the evidence that produced it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LeakFinding {
    /// The fitted line and every gate value behind the decision.
    pub fit: TrendFit,
    /// Confidence and its named terms.
    pub confidence: TrendConfidence,
    /// Growth per hour, in metric units. The same slope, in the unit an operator reads.
    pub growth_per_hour: f64,
}

/// The outcome of one evaluation. Either a leak with evidence, or a named reason there is none.
///
/// Deliberately not `Option<LeakFinding>`: the spec's clearing scenario ("growth that levels off
/// clears") needs a caller to tell *why* a previously active finding no longer holds, and a bare
/// `None` cannot. The fit is carried on the negative arm too where one was computable, so an
/// operator can see the numbers that fell short.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrendAnalysis {
    /// Growth met every gate.
    Leak(LeakFinding),
    /// No leak, with the reason and the fit if one was computed.
    NoLeak {
        reason: NoLeakReason,
        fit: Option<TrendFit>,
    },
}

impl TrendAnalysis {
    /// The finding, if there is one. For callers that only need the positive arm.
    pub fn leak(&self) -> Option<&LeakFinding> {
        match self {
            Self::Leak(finding) => Some(finding),
            Self::NoLeak { .. } => None,
        }
    }

    /// Whether an active finding raised from an earlier evaluation should now be cleared.
    ///
    /// Task 6.4. True for every negative outcome including the refusals: if the history that
    /// supported a finding has been reset, gapped, or shortened out of admissibility, the
    /// finding is no longer supported and keeping it active would mean asserting something the
    /// current evidence cannot show.
    pub fn clears_active_finding(&self) -> bool {
        matches!(self, Self::NoLeak { .. })
    }
}

/// Analyses one metric's series for the leak shape.
///
/// Order of work, and why:
///
/// 1. Segment first ([`segment_series`]), so no statistic is ever computed across a counter
///    reset, a free, or a gap. Analysis then continues on the **newest** segment: it is the
///    only one describing the process's current behaviour.
/// 2. Refuse two or more drops outright as a sawtooth. Doing this before the fit means a
///    repeatedly-restarted process cannot produce a finding from its latest tooth.
/// 3. Admissibility gates — sample count, then window length — before any fit is trusted.
/// 4. Fit, then the three spec gates in the order slope sign → significance → fit quality →
///    monotonicity. The order only affects which reason is reported, not whether a finding is
///    produced; it runs cheapest-and-most-decisive first.
///
/// `samples` must be ordered oldest-first. Cost is one pass for the median interval, one for
/// segmentation and one for the sums: linear in the slice, with no dependence on how much
/// history exists outside it. Bounding the slice is the caller's job, and is what keeps
/// per-tick cost flat.
pub fn analyse_trend(samples: &[TrendSample], config: &TrendConfig) -> TrendAnalysis {
    if samples.is_empty() {
        return TrendAnalysis::NoLeak {
            reason: NoLeakReason::NoSamples,
            fit: None,
        };
    }

    let segments = segment_series(samples, config);
    let drops = segments
        .iter()
        .filter(|s| s.preceded_by == Some(SegmentBreak::LargeDrop))
        .count();
    if drops >= 2 {
        return TrendAnalysis::NoLeak {
            reason: NoLeakReason::RepeatedDrops { drops },
            fit: None,
        };
    }

    // `segment_series` always pushes a final segment for a non-empty input (line 433),
    // and `analyse_trend` returned early for empty input above, so `last()` should
    // always be `Some`. Using `if let` to avoid an expect: defensive against future
    // refactors that might change the early-return or segmentation logic.
    let Some(segment) = segments.last() else {
        return TrendAnalysis::NoLeak {
            reason: NoLeakReason::NoSamples,
            fit: None,
        };
    };

    if segment.samples.len() < config.min_samples {
        return TrendAnalysis::NoLeak {
            reason: NoLeakReason::TooFewSamples {
                have: segment.samples.len(),
                need: config.min_samples,
            },
            fit: None,
        };
    }

    let window_secs = segment.window_secs();
    if window_secs < u64_to_f64(config.min_window_secs) {
        return TrendAnalysis::NoLeak {
            reason: NoLeakReason::WindowTooShort {
                have_secs: window_secs,
                need_secs: u64_to_f64(config.min_window_secs),
            },
            fit: None,
        };
    }

    let Some(fit) = fit_segment(segment) else {
        return TrendAnalysis::NoLeak {
            reason: NoLeakReason::NoTimeSpan,
            fit: None,
        };
    };

    if fit.r_squared.is_none() {
        // `Syy == 0`: every value identical. Flat, not trending, and not a division error.
        return TrendAnalysis::NoLeak {
            reason: NoLeakReason::FlatSeries,
            fit: Some(fit),
        };
    }
    if fit.slope_per_sec <= 0.0 {
        return TrendAnalysis::NoLeak {
            reason: NoLeakReason::NotGrowing {
                slope_per_sec: fit.slope_per_sec,
            },
            fit: Some(fit),
        };
    }
    if fit.slope_t < config.min_slope_t {
        return TrendAnalysis::NoLeak {
            reason: NoLeakReason::NotSignificant {
                slope_t: fit.slope_t,
                need_t: config.min_slope_t,
            },
            fit: Some(fit),
        };
    }
    let r_squared = fit.r_squared.unwrap_or(0.0);
    if r_squared < config.min_r_squared {
        return TrendAnalysis::NoLeak {
            reason: NoLeakReason::PoorFit {
                r_squared,
                need_r_squared: config.min_r_squared,
            },
            fit: Some(fit),
        };
    }
    if fit.monotonic_ratio < config.min_monotonic_ratio {
        return TrendAnalysis::NoLeak {
            reason: NoLeakReason::NotMonotonic {
                ratio: fit.monotonic_ratio,
                need_ratio: config.min_monotonic_ratio,
            },
            fit: Some(fit),
        };
    }

    TrendAnalysis::Leak(LeakFinding {
        fit,
        confidence: TrendConfidence::compute(&fit, config),
        growth_per_hour: fit.slope_per_sec * 3600.0,
    })
}

/// Fits one segment, with time measured in seconds from its first sample.
///
/// The relative origin is not cosmetic: `Σx²` over epoch seconds is around 3e18, and
/// `Sxx = Σx² − (Σx)²/n` then subtracts two nearly equal enormous numbers, losing most of the
/// precision the slope depends on. Rebasing keeps `x` inside the window's own duration.
///
/// `None` only when no line is defined — fewer than two samples, or every timestamp identical.
fn fit_segment(segment: &Segment<'_>) -> Option<TrendFit> {
    let first = segment.samples.first()?;
    let last = segment.samples.last()?;
    let origin_ms = first.at_unix_ms;

    let mut sums = RegressionSums::new();
    for sample in segment.samples {
        // Rebasing keeps x small: it is a within-window offset in ms, < 2^53 by construction.
        let x = u64_to_f64(sample.at_unix_ms.saturating_sub(origin_ms)) / 1000.0;
        sums.push(x, sample.value);
    }

    let slope = sums.slope()?;
    let intercept = sums.intercept()?;
    let std_error = sums.slope_std_error().unwrap_or(0.0);
    let slope_t = sums.slope_t_statistic().unwrap_or(0.0);

    Some(TrendFit {
        slope_per_sec: slope,
        intercept_at_window_start: intercept,
        r_squared: sums.r_squared(),
        slope_std_error: std_error,
        slope_t,
        monotonic_ratio: monotonic_ratio(segment.samples).unwrap_or(0.0),
        window_secs: segment.window_secs(),
        sample_count: segment.samples.len(),
        window_start_unix_ms: first.at_unix_ms,
        window_end_unix_ms: last.at_unix_ms,
        last_value: last.value,
        preceded_by: segment.preceded_by,
    })
}

/// Why no forecast was produced.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NoForecastReason {
    /// The series did not qualify as growing. Carries the trend's own reason.
    ///
    /// The spec's "a metric stable or falling produces no forecast" — enforced by requiring the
    /// leak gates first, so a forecast can never rest on a fit the leak detector rejected.
    NotGrowing(NoLeakReason),
    /// The threshold is already at or below the last observed value.
    ///
    /// Nothing to forecast: the answer is "now", and reporting a time-to-reach for something
    /// already reached would be misleading rather than merely useless.
    AlreadyReached { last_value: f64, threshold: f64 },
    /// The projected time lies further ahead than `max_horizon_window_multiple` allows.
    ///
    /// A slow leak with a distant limit genuinely may take days; the data does not reach that
    /// far, so the honest answer is "not from this evidence".
    BeyondHorizon { seconds: f64, horizon_secs: f64 },
    /// The interval is unbounded, so the forecast carries no usable uncertainty.
    ///
    /// Happens when the slope's lower confidence bound is non-positive: the data admits a
    /// flat trend, so "may never reach it" is inside the interval, and a point estimate
    /// without a finite upper bound would be false precision. The spec requires an interval;
    /// where the interval cannot be stated, the forecast is withheld.
    UnboundedInterval { slope_lower_bound: f64 },
}

/// A time-to-threshold projection with the interval that qualifies it.
///
/// Read the point estimate as **"if the current trend continues"** — the spec requires the
/// forecast be stated as conditional, and `design.md` is blunt that linear extrapolation is
/// right for a steady leak and wrong for anything accelerating or self-limiting. The interval
/// expresses uncertainty *in the fitted slope only*: it says nothing about the workload
/// changing, which is the larger uncertainty and is not quantifiable from the series.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThresholdForecast {
    /// The threshold projected toward, in metric units.
    pub threshold: f64,
    /// Seconds from the last sample until the fitted line reaches the threshold.
    pub seconds_to_threshold: f64,
    /// Earliest arrival, from the steepest slope in the confidence interval.
    pub earliest_secs: f64,
    /// Latest arrival, from the shallowest slope in the interval. Finite by construction.
    pub latest_secs: f64,
    /// Two-sided confidence level the interval was computed at, e.g. `0.95`.
    pub interval_confidence: f64,
    /// The fit the projection came from.
    pub fit: TrendFit,
    /// Confidence in the underlying growth finding, carried through unchanged.
    pub confidence: TrendConfidence,
}

/// Either a forecast with its interval, or a named reason for withholding one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ForecastOutcome {
    /// A projection that cleared every gate.
    Forecast(ThresholdForecast),
    /// No projection, and why.
    Withheld(NoForecastReason),
}

impl ForecastOutcome {
    /// The forecast, if there is one.
    pub fn forecast(&self) -> Option<&ThresholdForecast> {
        match self {
            Self::Forecast(f) => Some(f),
            Self::Withheld(_) => None,
        }
    }
}

/// Two-sided 95% Student *t* critical values, indexed by degrees of freedom from 1.
///
/// A table rather than a formula: the inverse *t* CDF needs an incomplete beta function, which
/// is a dependency and a numerical-accuracy argument for a value that only has to be roughly
/// right. Entries beyond the table use the last one (dof 30, 2.042), which is conservative —
/// wider than the true value, so the interval never claims more precision than it has.
const T_CRITICAL_95: [f64; 30] = [
    12.706, 4.303, 3.182, 2.776, 2.571, 2.447, 2.365, 2.306, 2.262, 2.228, 2.201, 2.179, 2.160,
    2.145, 2.131, 2.120, 2.110, 2.101, 2.093, 2.086, 2.080, 2.074, 2.069, 2.064, 2.060, 2.056,
    2.052, 2.048, 2.045, 2.042,
];

/// Critical value for `dof` degrees of freedom at two-sided 95%.
fn t_critical_95(dof: usize) -> f64 {
    if dof == 0 {
        return f64::INFINITY;
    }
    let idx = dof.min(T_CRITICAL_95.len()) - 1;
    T_CRITICAL_95[idx]
}

/// Confidence level the interval in [`ThresholdForecast`] is computed at.
const FORECAST_INTERVAL_CONFIDENCE: f64 = 0.95;

/// Projects when a growing metric would reach `threshold`, or refuses and says why.
///
/// Task 6.5. The gate is deliberately the *leak* gate: [`analyse_trend`] must return a finding
/// before any projection happens, so a forecast can never be built on a fit that was too poor,
/// too short, too noisy, or not growing. That single dependency covers four of the spec's five
/// forecast scenarios; "no threshold means no forecast" is covered by the signature — a caller
/// with no threshold has nothing to pass and does not call this.
///
/// The interval comes from the slope's own confidence interval,
/// `slope ± t(0.975, n−2) × SE(slope)`. Time is inversely proportional to slope, so the
/// *steeper* bound gives the earliest arrival and the *shallower* bound the latest. When the
/// lower bound is at or below zero the data admits no growth at all, the latest arrival is
/// infinite, and the forecast is withheld: an interval of "somewhere between 40 minutes and
/// never" is not a forecast, and reporting only its midpoint would be a lie by omission.
///
/// The distance to the threshold is measured from the **last observed value**, not from the
/// fitted value at that time. An operator checking the claim reads the current value, and a
/// projection that starts somewhere else looks wrong even when the arithmetic is right.
pub fn forecast_time_to_threshold(
    samples: &[TrendSample],
    threshold: f64,
    config: &TrendConfig,
) -> ForecastOutcome {
    let analysis = analyse_trend(samples, config);
    let finding = match &analysis {
        TrendAnalysis::Leak(finding) => finding,
        TrendAnalysis::NoLeak { reason, .. } => {
            return ForecastOutcome::Withheld(NoForecastReason::NotGrowing(*reason));
        }
    };
    let fit = finding.fit;

    let remaining = threshold - fit.last_value;
    if remaining <= 0.0 {
        return ForecastOutcome::Withheld(NoForecastReason::AlreadyReached {
            last_value: fit.last_value,
            threshold,
        });
    }

    // `analyse_trend` guarantees a positive slope past the significance gate, so this division
    // is safe and its result positive.
    let seconds_to_threshold = remaining / fit.slope_per_sec;

    let dof = fit.sample_count.saturating_sub(2);
    let margin = t_critical_95(dof) * fit.slope_std_error;
    let slope_upper = fit.slope_per_sec + margin;
    let slope_lower = fit.slope_per_sec - margin;

    if slope_lower <= 0.0 {
        return ForecastOutcome::Withheld(NoForecastReason::UnboundedInterval {
            slope_lower_bound: slope_lower,
        });
    }

    let earliest_secs = remaining / slope_upper;
    let latest_secs = remaining / slope_lower;

    // Judged on the latest arrival, not the point estimate: an interval whose far end runs off
    // past the horizon is as unsupported as a point estimate there.
    let horizon_secs = fit.window_secs * config.max_horizon_window_multiple;
    if latest_secs > horizon_secs {
        return ForecastOutcome::Withheld(NoForecastReason::BeyondHorizon {
            seconds: latest_secs,
            horizon_secs,
        });
    }

    ForecastOutcome::Forecast(ThresholdForecast {
        threshold,
        seconds_to_threshold,
        earliest_secs,
        latest_secs,
        interval_confidence: FORECAST_INTERVAL_CONFIDENCE,
        fit,
        confidence: finding.confidence,
    })
}

#[cfg(test)]
/// Test code casts are bounded and exact.
mod tests {
    use super::*;

    /// Arbitrary but fixed epoch base for readable timestamps: 2024-01-01T00:00:00Z.
    const BASE_MS: u64 = 1_704_067_200_000;

    /// A series at a fixed spacing. `values[i]` sits at `BASE_MS + i * step_secs`.
    fn series(step_secs: u64, values: &[f64]) -> Vec<TrendSample> {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| {
                TrendSample::new(
                    BASE_MS
                        + u64::try_from(i)
                            .unwrap_or(0)
                            .saturating_mul(step_secs)
                            .saturating_mul(1000),
                    *v,
                )
            })
            .collect()
    }

    /// A series from explicit `(offset_secs, value)` pairs, for non-uniform spacing.
    fn series_at(points: &[(u64, f64)]) -> Vec<TrendSample> {
        points
            .iter()
            .map(|(t, v)| TrendSample::new(BASE_MS + t * 1000, *v))
            .collect()
    }

    /// A clean linear rise: `start + rate_per_sec * t`, sampled every `step_secs`.
    fn linear_rise(
        step_secs: u64,
        count: usize,
        start: f64,
        rate_per_sec: f64,
    ) -> Vec<TrendSample> {
        (0..count)
            .map(|i| {
                let i_u64 = u64::try_from(i).unwrap_or(0);
                let t = i_u64 * step_secs;
                let t_f64 = u64_to_f64(t);
                TrendSample::new(BASE_MS + t * 1000, start + rate_per_sec * t_f64)
            })
            .collect()
    }

    /// Config with the window gate lowered, so a test can exercise the statistical gates
    /// without building ten minutes of samples. The window gate itself is tested separately at
    /// its default.
    fn test_config() -> TrendConfig {
        TrendConfig {
            min_window_secs: 60,
            ..TrendConfig::default()
        }
    }

    /// Hand-checkable arithmetic for the regression, so the rest of the suite rests on a
    /// verified fit rather than on the assertion that the maths is right.
    ///
    /// Points: (0,1) (1,3) (2,5) (3,7). n=4, Σx=6, Σy=16, Σxy=34, Σx²=14, Σy²=84.
    ///   Sxx = 14 − 36/4 = 5
    ///   Sxy = 34 − 6·16/4 = 34 − 24 = 10
    ///   Syy = 84 − 256/4 = 84 − 64 = 20
    ///   slope = Sxy/Sxx = 10/5 = 2
    ///   intercept = (16 − 2·6)/4 = 1
    ///   RSS = Syy − Sxy²/Sxx = 20 − 100/5 = 0  → r² = 1 − 0/20 = 1, SE = 0, t = +∞
    #[test]
    fn regression_sums_match_hand_computed_values() {
        let mut sums = RegressionSums::new();
        for (x, y) in [(0.0, 1.0), (1.0, 3.0), (2.0, 5.0), (3.0, 7.0)] {
            sums.push(x, y);
        }
        assert_eq!(sums.count(), 4);
        assert!((sums.sxx() - 5.0).abs() < 1e-12, "Sxx = {}", sums.sxx());
        assert!((sums.sxy() - 10.0).abs() < 1e-12, "Sxy = {}", sums.sxy());
        assert!((sums.syy() - 20.0).abs() < 1e-12, "Syy = {}", sums.syy());
        assert!((sums.slope().unwrap() - 2.0).abs() < 1e-12);
        assert!((sums.intercept().unwrap() - 1.0).abs() < 1e-12);
        assert!(sums.residual_ss().unwrap() < 1e-12);
        assert!((sums.r_squared().unwrap() - 1.0).abs() < 1e-12);
        assert_eq!(sums.slope_std_error().unwrap(), 0.0);
        assert_eq!(sums.slope_t_statistic(), Some(f64::INFINITY));
    }

    /// Second hand-checked case, this time with a real residual, so `r²` and SE are exercised
    /// away from their degenerate values.
    ///
    /// Points: (0,1) (1,2) (2,5) (3,6). n=4, Σx=6, Σy=14,
    ///   Σxy = 0·1 + 1·2 + 2·5 + 3·6 = 30,  Σx² = 14,  Σy² = 1+4+25+36 = 66.
    ///   Sxx = 14 − 36/4 = 5
    ///   Sxy = 30 − 6·14/4 = 30 − 21 = 9
    ///   Syy = 66 − 196/4 = 66 − 49 = 17
    ///   slope = 9/5 = 1.8
    ///   intercept = (14 − 1.8·6)/4 = (14 − 10.8)/4 = 0.8
    ///   RSS = 17 − 81/5 = 17 − 16.2 = 0.8  → r² = 1 − 0.8/17 = 0.9529411…
    ///   SE = sqrt(0.8/2/5) = sqrt(0.08) = 0.2828427…;  t = 1.8/0.2828427 = 6.363961…
    ///
    /// The first draft of this comment had Σxy = 29 and asserted a slope of 1.6; the test failed
    /// and the code was right. Left recorded because it is the argument for hand-checking the
    /// arithmetic instead of asserting that the arithmetic is correct.
    #[test]
    fn regression_with_residual_matches_hand_computed_values() {
        let mut sums = RegressionSums::new();
        for (x, y) in [(0.0, 1.0), (1.0, 2.0), (2.0, 5.0), (3.0, 6.0)] {
            sums.push(x, y);
        }
        assert!((sums.sxx() - 5.0).abs() < 1e-12);
        assert!((sums.sxy() - 9.0).abs() < 1e-12);
        assert!((sums.syy() - 17.0).abs() < 1e-12);
        assert!((sums.slope().unwrap() - 1.8).abs() < 1e-12);
        assert!((sums.intercept().unwrap() - 0.8).abs() < 1e-12);
        assert!((sums.residual_ss().unwrap() - 0.8).abs() < 1e-12);
        assert!((sums.r_squared().unwrap() - (1.0 - 0.8 / 17.0)).abs() < 1e-12);
        assert!((sums.slope_std_error().unwrap() - 0.08_f64.sqrt()).abs() < 1e-12);
        assert!((sums.slope_t_statistic().unwrap() - 1.8 / 0.08_f64.sqrt()).abs() < 1e-12);
    }

    /// A flat series has nothing to explain, so `r²` is undefined rather than 1.
    ///
    /// This is the guard behind "a constant metric produces no findings": if `r²` returned 1
    /// here, a flat line would pass the fit-quality gate on every evaluation.
    #[test]
    fn flat_series_has_undefined_r_squared_and_zero_slope() {
        let mut sums = RegressionSums::new();
        for x in 0..6 {
            sums.push(usize_to_f64(x), 42.0);
        }
        assert_eq!(sums.slope(), Some(0.0));
        assert_eq!(sums.r_squared(), None);
        assert_eq!(sums.slope_t_statistic(), None);
    }

    /// Every `x` identical: no line is defined, and nothing divides by zero.
    #[test]
    fn identical_timestamps_define_no_slope() {
        let mut sums = RegressionSums::new();
        for y in [1.0, 2.0, 3.0] {
            sums.push(5.0, y);
        }
        assert_eq!(sums.slope(), None);
        assert_eq!(sums.intercept(), None);
        assert_eq!(sums.slope_std_error(), None);
    }

    /// Two points fit any line perfectly, so SE is undefined with no residual dof. This is why
    /// `min_samples` is 8 and not 2.
    #[test]
    fn two_points_give_a_slope_but_no_standard_error() {
        let mut sums = RegressionSums::new();
        sums.push(0.0, 1.0);
        sums.push(10.0, 3.0);
        assert_eq!(sums.slope(), Some(0.2));
        assert_eq!(sums.slope_std_error(), None);
        assert_eq!(sums.slope_t_statistic(), None);
    }

    /// Segmentation on clean data leaves one segment: nothing is split without cause.
    #[test]
    fn clean_series_is_one_segment() {
        let samples = linear_rise(2, 20, 100.0, 1.0);
        let segments = segment_series(&samples, &TrendConfig::default());
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].samples.len(), 20);
        assert_eq!(segments[0].preceded_by, None);
    }

    /// A counter reset splits the series and is labelled as a drop.
    ///
    /// This is the `process-io-metrics` case: disk I/O is a monotonic counter that restarts at
    /// zero with the process. Values 1000..1400 then 0..300.
    #[test]
    fn counter_reset_splits_the_series() {
        let samples = series(
            2,
            &[
                1000.0, 1100.0, 1200.0, 1300.0, 1400.0, 0.0, 100.0, 200.0, 300.0,
            ],
        );
        let segments = segment_series(&samples, &TrendConfig::default());
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].samples.len(), 5);
        assert_eq!(segments[1].samples.len(), 4);
        assert_eq!(segments[1].preceded_by, Some(SegmentBreak::LargeDrop));
    }

    /// A gap far longer than the median interval splits the series, and the missing time is
    /// left missing rather than interpolated.
    ///
    /// Median interval here is 2s; the 400s hole is 200x it.
    #[test]
    fn a_gap_splits_the_series_without_interpolating() {
        let samples = series_at(&[
            (0, 10.0),
            (2, 11.0),
            (4, 12.0),
            (6, 13.0),
            (406, 14.0),
            (408, 15.0),
            (410, 16.0),
        ]);
        let segments = segment_series(&samples, &TrendConfig::default());
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[1].preceded_by, Some(SegmentBreak::Gap));
        // No synthesised samples: the two segments together hold exactly the input.
        assert_eq!(
            segments.iter().map(|s| s.samples.len()).sum::<usize>(),
            samples.len()
        );
    }

    /// A skipped maintenance tick is not a gap.
    ///
    /// `MissedTickBehavior::Skip` roughly doubles an interval under load, which must not split
    /// the series — otherwise every busy period would destroy the window a leak needs. Median
    /// stays 2s, one interval is 6s, and the 4x factor tolerates it.
    #[test]
    fn a_skipped_tick_is_not_a_gap() {
        let samples = series_at(&[
            (0, 10.0),
            (2, 11.0),
            (4, 12.0),
            (10, 13.0),
            (12, 14.0),
            (14, 15.0),
        ]);
        let segments = segment_series(&samples, &TrendConfig::default());
        assert_eq!(
            segments.len(),
            1,
            "a skipped tick must not split the series"
        );
    }

    /// Monotonicity counts flat steps as non-negative and falls as negative.
    ///
    /// Deltas for 1,2,2,3,1,2: +1, 0, +1, -2, +1 → 4 of 5 non-negative = 0.8.
    #[test]
    fn monotonic_ratio_counts_flat_steps_as_non_negative() {
        let samples = series(2, &[1.0, 2.0, 2.0, 3.0, 1.0, 2.0]);
        assert_eq!(monotonic_ratio(&samples), Some(0.8));
        assert_eq!(monotonic_ratio(&samples[..1]), None);
    }

    // ---- Leak detection: fires on its own shape ----

    /// Steady growth over a long-enough window is reported, with rate and window.
    ///
    /// 2 KiB/s for 20 minutes at 10s spacing: 121 samples, window 1200s, perfect fit. Slope is
    /// checked in per-second and per-hour units because the per-hour figure is what an operator
    /// reads and a unit slip there would be invisible otherwise.
    #[test]
    fn steady_growth_is_reported_as_a_leak() {
        let samples = linear_rise(10, 121, 50_000.0, 2048.0);
        let analysis = analyse_trend(&samples, &TrendConfig::default());
        let finding = analysis.leak().expect("steady growth must be reported");
        assert!((finding.fit.slope_per_sec - 2048.0).abs() < 1e-6);
        assert!((finding.growth_per_hour - 2048.0 * 3600.0).abs() < 1e-3);
        assert!((finding.fit.window_secs - 1200.0).abs() < 1e-9);
        assert_eq!(finding.fit.sample_count, 121);
        assert_eq!(finding.fit.monotonic_ratio, 1.0);
        assert!(finding.confidence.score > 0.0 && finding.confidence.score <= 1.0);
        assert!(!analysis.clears_active_finding());
    }

    /// Non-uniform spacing gives the same per-second slope as uniform spacing.
    ///
    /// The whole reason this module uses timestamps rather than sample indices. Both series
    /// grow at exactly 10 units/s over 1000s; the second is sampled irregularly, as a
    /// downsampled or load-stretched series is. An index-based regression would report the
    /// second's slope as units-per-sample and be wrong by the spacing ratio.
    #[test]
    fn slope_is_per_second_regardless_of_spacing() {
        let uniform = linear_rise(10, 101, 0.0, 10.0);
        // Intervals cycle 10/20/30/40s: median 25s, so the widest is inside the 4x gap factor
        // and nothing splits. This is the shape a load-stretched tick and a downsampled tier
        // produce — same growth rate, uneven spacing.
        let mut offset = 0u64;
        let mut irregular = Vec::new();
        for step in [10u64, 20, 30, 40].iter().cycle().take(40) {
            let offset_f64 = u64_to_f64(offset);
            irregular.push(TrendSample::new(BASE_MS + offset * 1000, 10.0 * offset_f64));
            offset += step;
        }
        assert_eq!(segment_series(&irregular, &TrendConfig::default()).len(), 1);
        let a = analyse_trend(&uniform, &TrendConfig::default());
        let b = analyse_trend(&irregular, &TrendConfig::default());
        let sa = a.leak().expect("uniform").fit.slope_per_sec;
        let sb = b.leak().expect("irregular").fit.slope_per_sec;
        assert!((sa - 10.0).abs() < 1e-9, "uniform slope {sa}");
        assert!((sb - 10.0).abs() < 1e-9, "irregular slope {sb}");
    }

    /// Identical inputs give an identical finding, confidence and evidence.
    #[test]
    fn analysis_is_deterministic() {
        let samples = linear_rise(10, 80, 1000.0, 5.0);
        let first = analyse_trend(&samples, &TrendConfig::default());
        let second = analyse_trend(&samples, &TrendConfig::default());
        assert_eq!(first, second);
    }

    // ---- Leak detection: stays silent on everything else ----

    /// A flat line is reported as flat, not as a trend of zero slope dressed as a finding.
    #[test]
    fn a_flat_series_is_not_a_leak() {
        let samples = series(10, &[512.0; 121]);
        match analyse_trend(&samples, &TrendConfig::default()) {
            TrendAnalysis::NoLeak {
                reason: NoLeakReason::FlatSeries,
                ..
            } => {}
            other => panic!("expected FlatSeries, got {other:?}"),
        }
    }

    /// A constant metric that then moves a little produces no finding.
    ///
    /// The spec's "a constant metric produces no findings" case: 120 samples at 512 then one at
    /// 513. The slope is real but tiny and the line describes the data badly, so a gate refuses
    /// it. Which gate is not the point; producing nothing is.
    #[test]
    fn a_quiet_metric_with_a_small_change_is_not_a_leak() {
        let mut values = vec![512.0; 120];
        values.push(513.0);
        let samples = series(10, &values);
        let analysis = analyse_trend(&samples, &TrendConfig::default());
        assert!(analysis.leak().is_none(), "got {analysis:?}");
    }

    /// A single spike on a flat series is not a leak.
    #[test]
    fn a_single_spike_is_not_a_leak() {
        let mut values = vec![100.0; 121];
        values[60] = 100_000.0;
        let samples = series(10, &values);
        let analysis = analyse_trend(&samples, &TrendConfig::default());
        assert!(analysis.leak().is_none(), "got {analysis:?}");
    }

    /// Too few points is its own answer, not a flat trend.
    #[test]
    fn too_few_samples_gives_no_answer() {
        let samples = linear_rise(10, 5, 100.0, 10.0);
        match analyse_trend(&samples, &TrendConfig::default()) {
            TrendAnalysis::NoLeak {
                reason: NoLeakReason::TooFewSamples { have, need },
                fit,
            } => {
                assert_eq!((have, need), (5, 8));
                assert!(
                    fit.is_none(),
                    "no fit should be computed below the sample gate"
                );
            }
            other => panic!("expected TooFewSamples, got {other:?}"),
        }
        match analyse_trend(&[], &TrendConfig::default()) {
            TrendAnalysis::NoLeak {
                reason: NoLeakReason::NoSamples,
                ..
            } => {}
            other => panic!("expected NoSamples, got {other:?}"),
        }
    }

    /// Enough samples but not enough elapsed time is refused: 20 samples 2s apart is a 38s
    /// burst, and a leak is a claim about hours.
    #[test]
    fn a_short_window_is_refused_even_with_enough_samples() {
        let samples = linear_rise(2, 20, 100.0, 10.0);
        match analyse_trend(&samples, &TrendConfig::default()) {
            TrendAnalysis::NoLeak {
                reason:
                    NoLeakReason::WindowTooShort {
                        have_secs,
                        need_secs,
                    },
                ..
            } => {
                assert!((have_secs - 38.0).abs() < 1e-9);
                assert!((need_secs - 600.0).abs() < 1e-9);
            }
            other => panic!("expected WindowTooShort, got {other:?}"),
        }
    }

    /// A sawtooth is not a leak, however steep its teeth.
    ///
    /// Four teeth of 30 samples: rise from 100 to ~1000 at 10s spacing, reset to 100, repeat.
    /// Each tooth on its own fits a perfect rising line, which is exactly the trap — fitting
    /// only the newest tooth would report a leak once per tooth forever. Three drops, so the
    /// repeated-drop refusal fires before any fit.
    #[test]
    fn a_sawtooth_is_not_a_leak() {
        let mut values = Vec::new();
        for _tooth in 0..4 {
            for i in 0..30 {
                values.push(100.0 + usize_to_f64(i) * 30.0);
            }
        }
        let samples = series(10, &values);
        match analyse_trend(&samples, &TrendConfig::default()) {
            TrendAnalysis::NoLeak {
                reason: NoLeakReason::RepeatedDrops { drops },
                ..
            } => {
                assert_eq!(drops, 3, "three resets between four teeth");
            }
            other => panic!("expected RepeatedDrops, got {other:?}"),
        }
    }

    /// One restart mid-series is a discontinuity, not a downward trend.
    ///
    /// The `process-io-metrics` regression this change must not re-break. A counter runs
    /// 100_000 → 220_000 then resets and climbs 0 → 20_000. Fitted whole, the slope is steeply
    /// negative; segmented, the newest piece is a short clean rise. The assertion is that
    /// nothing downward is ever reported — the newest segment is too short to be a finding, and
    /// that refusal names the shortness, not a decline.
    #[test]
    fn a_counter_reset_is_never_reported_as_a_downward_trend() {
        let mut values: Vec<f64> = (0..60)
            .map(|i| 100_000.0 + usize_to_f64(i) * 2000.0)
            .collect();
        values.extend((0..20).map(|i| usize_to_f64(i) * 1000.0));
        let samples = series(10, &values);

        // Control: an unsegmented fit really does slope downward, so the guard is load-bearing.
        let mut naive = RegressionSums::new();
        for (i, v) in values.iter().enumerate() {
            let i_f64 = usize_to_f64(i);
            naive.push(i_f64 * 10.0, *v);
        }
        assert!(
            naive.slope().unwrap() < 0.0,
            "naive fit across the reset must be negative"
        );

        let analysis = analyse_trend(&samples, &TrendConfig::default());
        assert!(analysis.leak().is_none());
        match analysis {
            TrendAnalysis::NoLeak {
                reason: NoLeakReason::WindowTooShort { have_secs, .. },
                ..
            } => {
                assert!(
                    (have_secs - 190.0).abs() < 1e-9,
                    "only the post-reset segment is used"
                );
            }
            other => panic!("expected the post-reset segment to be judged, got {other:?}"),
        }
    }

    /// After one reset, a long clean rise is still a leak — and its rate is the post-reset rate.
    ///
    /// The complement of the test above: segmentation must not make a leaking process
    /// undetectable just because it restarted once. Growth is 500/s after the reset.
    #[test]
    fn growth_after_a_single_reset_is_still_detected() {
        let mut values: Vec<f64> = (0..30)
            .map(|i| 900_000.0 + usize_to_f64(i) * 100.0)
            .collect();
        values.extend((0..121).map(|i| usize_to_f64(i) * 5000.0));
        let samples = series(10, &values);
        let finding = analyse_trend(&samples, &TrendConfig::default())
            .leak()
            .copied()
            .expect("post-reset growth is a leak");
        assert!((finding.fit.slope_per_sec - 500.0).abs() < 1e-6);
        assert_eq!(finding.fit.sample_count, 121);
        assert_eq!(finding.fit.preceded_by, Some(SegmentBreak::LargeDrop));
    }

    /// Load-driven growth — up overall, but falling back repeatedly — is refused.
    ///
    /// A rising ramp with a 30% sawtooth wobble on top: the trend is genuinely upward and fits
    /// tolerably, but a third of the deltas are falls, so monotonicity refuses it. This is the
    /// spec's "fluctuating growth is not reported as a leak".
    #[test]
    fn fluctuating_growth_is_not_a_leak() {
        let values: Vec<f64> = (0..121)
            .map(|i| {
                let ramp = 100_000.0 + usize_to_f64(i) * 500.0;
                // Falls of ~6000 on a value near 100_000 — nowhere near the 50% that would read
                // as a reset, so this stays one segment and is judged on monotonicity, which is
                // the gate under test.
                let wobble = match i % 3 {
                    0 => 0.0,
                    1 => 6000.0,
                    _ => -4000.0,
                };
                ramp + wobble
            })
            .collect();
        let samples = series(10, &values);
        match analyse_trend(&samples, &TrendConfig::default()) {
            TrendAnalysis::NoLeak {
                reason: NoLeakReason::NotMonotonic { ratio, need_ratio },
                ..
            } => {
                assert!(
                    ratio < need_ratio,
                    "ratio {ratio} must fall short of {need_ratio}"
                );
            }
            other => panic!("expected NotMonotonic, got {other:?}"),
        }
    }

    /// A poor fit is refused even when the series is monotonic.
    ///
    /// A step function: flat at 100 for half the window, flat at 5000 for the rest. Never falls,
    /// so monotonicity is 1.0, and the slope is significant — but a straight line describes a
    /// step badly. This is the spec's "a poor fit is not reported", and it is a change point
    /// rather than a leak, which is another detector's business.
    #[test]
    fn a_step_is_a_poor_fit_and_not_a_leak() {
        let mut values = vec![100.0; 60];
        values.extend(vec![5000.0; 61]);
        let samples = series(10, &values);
        match analyse_trend(&samples, &TrendConfig::default()) {
            TrendAnalysis::NoLeak {
                reason:
                    NoLeakReason::PoorFit {
                        r_squared,
                        need_r_squared,
                    },
                ..
            } => {
                assert!(
                    r_squared < need_r_squared,
                    "r² {r_squared} vs gate {need_r_squared}"
                );
            }
            other => panic!("expected PoorFit, got {other:?}"),
        }
    }

    /// A decline within clean data is not a leak, and is reported as not growing.
    #[test]
    fn a_falling_series_is_not_a_leak() {
        let samples = linear_rise(10, 121, 100_000.0, -20.0);
        match analyse_trend(&samples, &TrendConfig::default()) {
            TrendAnalysis::NoLeak {
                reason: NoLeakReason::NotGrowing { slope_per_sec },
                ..
            } => {
                assert!((slope_per_sec + 20.0).abs() < 1e-6);
            }
            other => panic!("expected NotGrowing, got {other:?}"),
        }
    }

    /// Growth that levels off clears the finding.
    ///
    /// The spec's clearing scenario, and task 6.4. The same series grows for 600s and is then
    /// flat for 1200s; judged over the whole window the line no longer describes the data.
    #[test]
    fn growth_that_levels_off_clears() {
        let growing = linear_rise(10, 61, 1000.0, 100.0);
        assert!(growing.len() >= 8);
        let mut levelled = growing.clone();
        let plateau = growing.last().unwrap().value;
        for i in 1..=120 {
            levelled.push(TrendSample::new(BASE_MS + (600 + i * 10) * 1000, plateau));
        }
        let before = analyse_trend(&growing, &TrendConfig::default());
        assert!(
            before.leak().is_some(),
            "the growing phase is a leak: {before:?}"
        );
        let after = analyse_trend(&levelled, &TrendConfig::default());
        assert!(
            after.leak().is_none(),
            "levelling off must clear: {after:?}"
        );
        assert!(after.clears_active_finding());
    }

    /// Every timestamp identical: no line, no answer, no panic.
    #[test]
    fn identical_timestamps_are_refused() {
        let samples: Vec<TrendSample> = (0..10)
            .map(|i| TrendSample::new(BASE_MS, 100.0 + usize_to_f64(i)))
            .collect();
        let config = TrendConfig {
            min_window_secs: 0,
            ..TrendConfig::default()
        };
        match analyse_trend(&samples, &config) {
            TrendAnalysis::NoLeak {
                reason: NoLeakReason::NoTimeSpan,
                ..
            } => {}
            other => panic!("expected NoTimeSpan, got {other:?}"),
        }
    }

    // ---- Forecasting ----

    /// A forecast is produced for a growing metric, with a finite interval around it.
    ///
    /// Arithmetic that can be checked by hand: growth of 1000 units/s over a 1200s window from
    /// 100_000, so the last value is 100_000 + 1000·1200 = 1_300_000. Against a threshold of
    /// 2_000_000 the remaining distance is 700_000, and 700_000 / 1000 = 700s. The horizon is
    /// 3 × 1200 = 3600s, so 700s is inside it. The fit is perfect, so SE is 0 and the interval
    /// collapses onto the point estimate — the degenerate but correct case.
    #[test]
    fn a_forecast_is_produced_for_a_growing_metric() {
        let samples = linear_rise(10, 121, 100_000.0, 1000.0);
        let outcome = forecast_time_to_threshold(&samples, 2_000_000.0, &TrendConfig::default());
        let forecast = outcome
            .forecast()
            .expect("growth toward a threshold forecasts");
        assert!((forecast.fit.last_value - 1_300_000.0).abs() < 1e-6);
        assert!((forecast.seconds_to_threshold - 700.0).abs() < 1e-6);
        assert!(
            forecast.latest_secs.is_finite(),
            "the interval must be bounded"
        );
        assert!(forecast.earliest_secs <= forecast.seconds_to_threshold);
        assert!(forecast.latest_secs >= forecast.seconds_to_threshold);
        assert_eq!(forecast.interval_confidence, 0.95);
    }

    /// Noise widens the interval around the point estimate rather than being hidden.
    ///
    /// The interval is the part of the forecast that makes it honest, so a noisy-but-qualifying
    /// series must produce a strictly wider one than a clean series with the same slope.
    #[test]
    fn noise_widens_the_forecast_interval() {
        let clean = linear_rise(10, 121, 0.0, 1000.0);
        let noisy: Vec<TrendSample> = clean
            .iter()
            .enumerate()
            .map(|(i, s)| {
                // Deterministic zig-zag, small enough to keep monotonicity and fit above their
                // gates: values still never fall, since 1000/sample dwarfs the 400 offset.
                let offset = if i % 2 == 0 { 0.0 } else { 400.0 };
                TrendSample::new(s.at_unix_ms, s.value + offset)
            })
            .collect();
        let a = forecast_time_to_threshold(&clean, 2_000_000.0, &TrendConfig::default());
        let b = forecast_time_to_threshold(&noisy, 2_000_000.0, &TrendConfig::default());
        let clean_f = a.forecast().expect("clean forecast");
        let noisy_f = b.forecast().expect("noisy forecast");
        let clean_width = clean_f.latest_secs - clean_f.earliest_secs;
        let noisy_width = noisy_f.latest_secs - noisy_f.earliest_secs;
        assert!(
            noisy_width > clean_width,
            "noisy width {noisy_width} must exceed clean width {clean_width}"
        );
    }

    /// A stable metric produces no forecast, and the refusal names the trend's reason.
    #[test]
    fn a_stable_metric_produces_no_forecast() {
        let samples = series(10, &[512.0; 121]);
        match forecast_time_to_threshold(&samples, 100_000.0, &TrendConfig::default()) {
            ForecastOutcome::Withheld(NoForecastReason::NotGrowing(NoLeakReason::FlatSeries)) => {}
            other => panic!("expected NotGrowing(FlatSeries), got {other:?}"),
        }
    }

    /// A falling metric produces no forecast toward the threshold.
    #[test]
    fn a_falling_metric_produces_no_forecast() {
        let samples = linear_rise(10, 121, 500_000.0, -50.0);
        let outcome = forecast_time_to_threshold(&samples, 1_000_000.0, &TrendConfig::default());
        assert!(outcome.forecast().is_none(), "got {outcome:?}");
    }

    /// A poor fit produces no forecast, because the leak gate refuses it first.
    #[test]
    fn a_poor_fit_produces_no_forecast() {
        let mut values = vec![100.0; 60];
        values.extend(vec![5000.0; 61]);
        let samples = series(10, &values);
        match forecast_time_to_threshold(&samples, 10_000.0, &TrendConfig::default()) {
            ForecastOutcome::Withheld(NoForecastReason::NotGrowing(NoLeakReason::PoorFit {
                ..
            })) => {}
            other => panic!("expected a poor-fit refusal, got {other:?}"),
        }
    }

    /// Too few points produces no forecast — the noisy-5-point case that must never yield
    /// "40 minutes to the limit".
    #[test]
    fn too_few_points_produces_no_forecast() {
        let samples = series_at(&[
            (0, 100.0),
            (10, 400.0),
            (20, 300.0),
            (30, 900.0),
            (40, 1200.0),
        ]);
        match forecast_time_to_threshold(&samples, 5000.0, &TrendConfig::default()) {
            ForecastOutcome::Withheld(NoForecastReason::NotGrowing(
                NoLeakReason::TooFewSamples { .. },
            )) => {}
            other => panic!("expected a too-few-samples refusal, got {other:?}"),
        }
    }

    /// A threshold already passed is not forecast.
    #[test]
    fn a_threshold_already_reached_is_not_forecast() {
        let samples = linear_rise(10, 121, 100_000.0, 1000.0);
        match forecast_time_to_threshold(&samples, 1_000.0, &TrendConfig::default()) {
            ForecastOutcome::Withheld(NoForecastReason::AlreadyReached {
                last_value,
                threshold,
            }) => {
                assert!((last_value - 1_300_000.0).abs() < 1e-6);
                assert_eq!(threshold, 1_000.0);
            }
            other => panic!("expected AlreadyReached, got {other:?}"),
        }
    }

    /// A threshold too far off to be spoken to from the observed window is withheld.
    ///
    /// 20 minutes of data at 1000 units/s reaching 1_300_000; a threshold of 100_000_000 is
    /// about 27 hours away, far past the 3 × 1200s = 3600s horizon.
    #[test]
    fn a_distant_threshold_is_beyond_the_horizon() {
        let samples = linear_rise(10, 121, 100_000.0, 1000.0);
        match forecast_time_to_threshold(&samples, 100_000_000.0, &TrendConfig::default()) {
            ForecastOutcome::Withheld(NoForecastReason::BeyondHorizon {
                seconds,
                horizon_secs,
            }) => {
                assert!(seconds > horizon_secs);
                assert!((horizon_secs - 3600.0).abs() < 1e-9);
            }
            other => panic!("expected BeyondHorizon, got {other:?}"),
        }
    }

    /// The *t* table is monotonically decreasing and saturates rather than indexing past its end.
    #[test]
    fn t_critical_table_behaves_at_its_edges() {
        assert_eq!(t_critical_95(0), f64::INFINITY);
        assert_eq!(t_critical_95(1), 12.706);
        assert_eq!(t_critical_95(30), 2.042);
        assert_eq!(
            t_critical_95(5000),
            2.042,
            "beyond the table, the last value is reused"
        );
        for dof in 2..=30 {
            assert!(t_critical_95(dof) < t_critical_95(dof - 1));
        }
    }

    /// Confidence terms sit in `[0,1]`, the score is their mean, and stronger evidence scores
    /// higher — the spec's ranking requirement.
    #[test]
    fn confidence_terms_are_bounded_and_ordered() {
        let config = test_config();
        let long = linear_rise(10, 121, 0.0, 100.0);
        let short = linear_rise(10, 12, 0.0, 100.0);
        let long_c = analyse_trend(&long, &config)
            .leak()
            .expect("long")
            .confidence;
        let short_c = analyse_trend(&short, &config)
            .leak()
            .expect("short")
            .confidence;
        for c in [long_c, short_c] {
            for term in [
                c.significance_term,
                c.fit_term,
                c.monotonicity_term,
                c.maturity_term,
            ] {
                assert!((0.0..=1.0).contains(&term), "term {term} out of range");
            }
            let mean =
                (c.significance_term + c.fit_term + c.monotonicity_term + c.maturity_term) / 4.0;
            assert!((c.score - mean).abs() < 1e-12);
            assert!((0.0..=1.0).contains(&c.score));
        }
        assert!(
            long_c.maturity_term > short_c.maturity_term,
            "more samples must raise the maturity term"
        );
        assert!(long_c.score > short_c.score);
    }

    /// A fit is recomputable from its own evidence: the reported line reproduces the reported
    /// last value at the window's end.
    #[test]
    fn a_fit_can_be_rechecked_from_its_evidence() {
        let samples = linear_rise(10, 121, 2000.0, 25.0);
        let finding = analyse_trend(&samples, &TrendConfig::default())
            .leak()
            .copied()
            .expect("leak");
        let recomputed = finding.fit.value_at(finding.fit.window_secs);
        assert!(
            (recomputed - finding.fit.last_value).abs() < 1e-6,
            "line gives {recomputed}, evidence says {}",
            finding.fit.last_value
        );
    }
}
