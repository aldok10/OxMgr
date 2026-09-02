//! The core level detector: is a current value abnormal against its own baseline.
//!
//! Scaffold for OpenSpec change `process-intelligence`, tasks section(s) 5.
//! The contract is `openspec/changes/process-intelligence/specs/`; read it before adding to this file.
//!
//! Pure logic on purpose: nothing here reaches for the process manager, the HTTP layer or the
//! dashboard, so it can be unit-tested without a running daemon. Wiring it into those is a
//! separate integration step, which is why the module is currently unreferenced.
//!
//! # What this detector answers
//!
//! One question, per process and per metric: *is the value I just measured far
//! enough from this process's own normal, for long enough, to be worth telling an
//! operator about?* The statistic is the robust z-score from `design.md`,
//! `|x − centre| / spread`, where centre is the baseline median and spread is its
//! MAD scaled by 1.4826.
//!
//! Deliberately one detector, not a suite. `design.md` records EWMA, CUSUM and
//! change-point as *conditional* on phase-2 calibration showing this one missing
//! real degradation, because every extra detector multiplies both the tuning
//! surface and the chance of four findings for one event. Section 5b of `tasks.md`
//! is not implemented here.
//!
//! # Three ways this stays quiet
//!
//! False positives are the failure mode that kills the feature: an operator who
//! learns to ignore findings has gained noise and lost nothing else. So:
//!
//! 1. **Warm-up gating.** Below [`LevelConfig::min_baseline_samples`] the answer is
//!    [`LevelVerdict::Undetermined`], never `Normal`. "Not enough data" and "fine"
//!    are different answers and the caller must be able to tell them apart —
//!    conflating them is how a dashboard ends up claiming a process it has watched
//!    for four seconds is healthy.
//! 2. **A persistence requirement.** A single crossing is a spike, not a
//!    departure. Nothing is reported until
//!    [`LevelConfig::min_consecutive_samples`] measured samples agree, *in the same
//!    direction*, with no gap between them.
//! 3. **A floored spread.** MAD is exactly zero for a perfectly flat series, which
//!    would make every one-unit change an infinite z-score. See
//!    [`LevelConfig::spread_floor_abs`].
//!
//! # Absent is not zero
//!
//! [`LevelDetector::observe`] takes an `Option<f64>`, and `None` is neither a
//! measurement of zero nor evidence of health, matching how
//! `ProcessMetrics::disk_read_rate_bps` treats a missing rate. An absent sample
//! breaks the consecutive run — a run interrupted by a sample nobody measured is
//! not consecutive evidence — but it does not clear an active departure, because
//! "I stopped being able to measure" is not "the problem went away".
//!
//! # Every threshold here is a guess
//!
//! The defaults below are starting points chosen from published rules of thumb and
//! this daemon's tick rate. None has been calibrated against a recorded workload,
//! which `design.md` schedules for phase 2 and states is the one part unit tests
//! cannot answer. Treat them as placeholders with reasoning, not as principled
//! values.

use serde::{Deserialize, Serialize};

/// Default z-score at or above which a sample counts as departing.
///
/// 3.5 is the Iglewicz-Hoaglin cutoff for the modified (MAD-based) z-score, which
/// is the same statistic this detector computes. For normally distributed data
/// that is roughly a 1-in-2000 sample, so at one sample per 2s maintenance tick a
/// stationary metric would produce a lone crossing about every 70 minutes — which
/// is precisely why a lone crossing is not reportable.
///
/// **Uncalibrated.** Process CPU and RSS are not normally distributed, so the
/// 1-in-2000 figure describes the arithmetic and not the workload.
pub const DEFAULT_DEPARTURE_Z: f64 = oxmgr_core::tuning::DEFAULT_DEPARTURE_Z;

/// Default number of consecutive measured samples that must agree before a
/// departure is reported.
///
/// Three samples at the daemon's 2s tick is ~6s of agreement. Two felt too easy to
/// reach by chance for a metric sampled continuously; much beyond three and a real
/// step change takes long enough to report that an operator watching a graph sees
/// it first, which makes the finding useless.
///
/// **Uncalibrated.** The right value depends on the sample interval and on how
/// bursty real workloads are, neither of which has been measured yet.
pub const DEFAULT_MIN_CONSECUTIVE_SAMPLES: u32 =
    oxmgr_core::tuning::DEFAULT_MIN_CONSECUTIVE_SAMPLES;

/// Default baseline sample count below which this detector refuses to fire.
///
/// A median and MAD over fewer samples than this describe the last few seconds
/// rather than the process's normal behaviour. 30 samples is ~60s at a 2s tick,
/// which clears the startup burst — JIT warm-up, cache fill, connection pools —
/// that `spec.md` names as the largest single class of false positives.
///
/// **Uncalibrated.** Chosen as "long enough to be past startup" for a 2s tick, not
/// derived from any measured convergence of the median.
pub const DEFAULT_MIN_BASELINE_SAMPLES: u32 = oxmgr_core::tuning::DEFAULT_MIN_BASELINE_SAMPLES;

/// Default fraction of `|centre|` used as a relative floor on the spread.
///
/// The absolute floor has to be in the metric's own units, which differ by three
/// orders of magnitude between percent-CPU and bytes-RSS, so an absolute default
/// cannot be right for both. This relative floor is unit-free: a metric whose
/// spread is under 1% of its own level is treated as flat. In practice it means a
/// 400MB RSS that never moves needs a ~14MB shift to reach z=3.5 rather than a
/// single page.
///
/// **Uncalibrated.** 1% is a round number picked to be small enough not to hide
/// real shifts and large enough to swallow sampling granularity.
pub const DEFAULT_SPREAD_FLOOR_REL: f64 = 0.01;

/// Default absolute floor on the spread, in the metric's own units.
///
/// Zero, meaning "rely on the relative floor". A caller that knows its metric's
/// measurement granularity — one page for RSS, one percentage point for CPU —
/// should set it, because the relative floor alone still permits a large z-score on
/// a metric whose centre is near zero.
pub const DEFAULT_SPREAD_FLOOR_ABS: f64 = 0.0;

/// Default fraction of the departure threshold a sample must fall back under
/// before an active departure clears.
///
/// Clearing at exactly the firing threshold makes a value parked on the line
/// oscillate raise/clear/raise, and each cycle is a fresh notification. Requiring
/// the z-score back under 0.75x the threshold gives the hysteresis that turns that
/// into one finding.
///
/// **Uncalibrated.** Standard hysteresis practice, no measurement behind the 0.75.
pub const DEFAULT_CLEAR_Z_FRACTION: f64 = 0.75;

/// What this detector needs to know about a baseline, and nothing more.
///
/// A local input shape, not the baseline module's type. `src/baseline.rs` owns
/// median/MAD/window state and is being written separately; depending on its API
/// from here would couple two independent pieces of work. The seam is expected to
/// be a cheap `impl From<&Baseline> for BaselineSummary`.
///
/// Note what is absent: no `ready` flag. Readiness is derived here from
/// `sample_count` against [`LevelConfig::min_baseline_samples`] so that one
/// configured number decides it, rather than two places holding an opinion that
/// can drift apart. If `baseline.rs` reports its own readiness for the API surface
/// in task 4.4, it must read the same configured minimum.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BaselineSummary {
    /// Robust centre of the retained window — the median, per `design.md`.
    pub centre: f64,
    /// Robust spread, already scaled by [`MAD_TO_SIGMA`]. May legitimately be
    /// `0.0` for a flat metric; the floor in [`LevelConfig`] handles that.
    pub spread: f64,
    /// How many samples the baseline has observed. Drives warm-up gating and is
    /// reported in evidence so an operator can see how mature the comparison was.
    pub sample_count: u32,
}

impl BaselineSummary {
    /// Builds a summary. Present so the field order cannot be mixed up silently at
    /// call sites, since `centre` and `spread` are both `f64`.
    pub fn new(centre: f64, spread: f64, sample_count: u32) -> Self {
        Self {
            centre,
            spread,
            sample_count,
        }
    }

    /// Whether every number in the summary is usable arithmetic.
    ///
    /// A NaN centre or a negative spread means the baseline itself is broken. The
    /// detector reports that as [`UndeterminedReason::BaselineUnusable`] rather
    /// than propagating NaN into a comparison, where `NaN >= threshold` is `false`
    /// and would look exactly like "normal".
    fn is_usable(&self) -> bool {
        self.centre.is_finite() && self.spread.is_finite() && self.spread >= 0.0
    }
}

/// Tuning for the level detector.
///
/// Every field has a documented default and [`LevelConfig::sanitised`] replaces an
/// unusable configured value with it, which is the spec's "an absent or unusable
/// value falls back to a documented default". The fallback is returned rather than
/// only applied, so the caller can report it as the spec's "the fallback is
/// reported" requires.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LevelConfig {
    /// Robust z-score at or above which a sample departs. See
    /// [`DEFAULT_DEPARTURE_Z`].
    pub departure_z: f64,
    /// Consecutive agreeing measured samples required before reporting. See
    /// [`DEFAULT_MIN_CONSECUTIVE_SAMPLES`].
    pub min_consecutive_samples: u32,
    /// Baseline samples required before this detector may fire at all. See
    /// [`DEFAULT_MIN_BASELINE_SAMPLES`].
    pub min_baseline_samples: u32,
    /// Absolute spread floor in the metric's own units. See
    /// [`DEFAULT_SPREAD_FLOOR_ABS`].
    pub spread_floor_abs: f64,
    /// Spread floor as a fraction of `|centre|`. See
    /// [`DEFAULT_SPREAD_FLOOR_REL`].
    pub spread_floor_rel: f64,
    /// Fraction of `departure_z` the z-score must fall under to clear. See
    /// [`DEFAULT_CLEAR_Z_FRACTION`].
    pub clear_z_fraction: f64,
}

impl Default for LevelConfig {
    fn default() -> Self {
        Self {
            departure_z: DEFAULT_DEPARTURE_Z,
            min_consecutive_samples: DEFAULT_MIN_CONSECUTIVE_SAMPLES,
            min_baseline_samples: DEFAULT_MIN_BASELINE_SAMPLES,
            spread_floor_abs: DEFAULT_SPREAD_FLOOR_ABS,
            spread_floor_rel: DEFAULT_SPREAD_FLOOR_REL,
            clear_z_fraction: DEFAULT_CLEAR_Z_FRACTION,
        }
    }
}

/// One configured field that could not be used, and what was applied instead.
///
/// Carried out of [`LevelConfig::sanitised`] so a bad value surfaces to an operator
/// instead of being silently corrected. Silent correction is worse than the bad
/// value: the operator believes a threshold is in force that is not.
///
/// `Serialize` only: this is a report about configuration, produced at load time and
/// never read back, so the field name can stay a borrowed `&'static str` rather than
/// allocating a `String` per fallback. Evidence differs — it must round-trip, which
/// is why [`Detector`] is an enum.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ConfigFallback {
    /// Field name as written in configuration.
    pub field: &'static str,
    /// The default that was applied instead.
    pub applied_default: f64,
}

impl LevelConfig {
    /// Returns a config with every unusable value replaced by its default, plus the
    /// list of replacements made.
    ///
    /// "Unusable" is NaN, infinity, or a value outside the range where the field is
    /// meaningful: a non-positive `departure_z` would fire on every sample, a zero
    /// `min_consecutive_samples` would drop the anti-spike guard entirely, and a
    /// `clear_z_fraction` above 1.0 would clear a departure while it was still
    /// departing. Rejecting those is not paranoia about typos, it is refusing to run
    /// with the safety properties disabled.
    ///
    /// A zero `min_baseline_samples` is rejected for the same reason: it would let
    /// the detector fire against a baseline built from nothing.
    pub fn sanitised(&self) -> (Self, Vec<ConfigFallback>) {
        let d = Self::default();
        let mut out = *self;
        let mut fallbacks = Vec::new();

        if !(self.departure_z.is_finite() && self.departure_z > 0.0) {
            out.departure_z = d.departure_z;
            fallbacks.push(ConfigFallback {
                field: "departure_z",
                applied_default: d.departure_z,
            });
        }
        if self.min_consecutive_samples == 0 {
            out.min_consecutive_samples = d.min_consecutive_samples;
            fallbacks.push(ConfigFallback {
                field: "min_consecutive_samples",
                applied_default: f64::from(d.min_consecutive_samples),
            });
        }
        if self.min_baseline_samples == 0 {
            out.min_baseline_samples = d.min_baseline_samples;
            fallbacks.push(ConfigFallback {
                field: "min_baseline_samples",
                applied_default: f64::from(d.min_baseline_samples),
            });
        }
        if !(self.spread_floor_abs.is_finite() && self.spread_floor_abs >= 0.0) {
            out.spread_floor_abs = d.spread_floor_abs;
            fallbacks.push(ConfigFallback {
                field: "spread_floor_abs",
                applied_default: d.spread_floor_abs,
            });
        }
        if !(self.spread_floor_rel.is_finite() && self.spread_floor_rel >= 0.0) {
            out.spread_floor_rel = d.spread_floor_rel;
            fallbacks.push(ConfigFallback {
                field: "spread_floor_rel",
                applied_default: d.spread_floor_rel,
            });
        }
        if !(self.clear_z_fraction.is_finite()
            && self.clear_z_fraction > 0.0
            && self.clear_z_fraction <= 1.0)
        {
            out.clear_z_fraction = d.clear_z_fraction;
            fallbacks.push(ConfigFallback {
                field: "clear_z_fraction",
                applied_default: d.clear_z_fraction,
            });
        }

        (out, fallbacks)
    }

    /// The spread actually divided by: the baseline spread raised to the larger of
    /// the two floors.
    ///
    /// This is task 4.2's guard applied at the point of division. Without it a flat
    /// metric has `spread == 0.0` and every non-zero deviation is `inf`, so the
    /// quietest metric on the host becomes the loudest source of findings —
    /// `spec.md`'s "a flat metric cannot manufacture findings".
    ///
    /// Returns `None` only when both floors and the baseline spread are all zero,
    /// which happens for a flat metric centred exactly on zero with no absolute
    /// floor configured. There is no honest z-score there, and dividing anyway
    /// would yield `inf` or `NaN`.
    fn effective_spread(&self, baseline: &BaselineSummary) -> Option<f64> {
        let relative = baseline.centre.abs() * self.spread_floor_rel;
        let spread = baseline.spread.max(self.spread_floor_abs).max(relative);
        (spread > 0.0).then_some(spread)
    }
}

/// Which side of its baseline a metric departed on.
///
/// Tracked because a run of samples only counts as agreeing if they agree on the
/// direction too: a value that swings 4 sigma above, then 4 sigma below, then above
/// again is unstable, but it is not a level departure, and reporting it as one would
/// name a value the process never sustained. It is also what an operator needs
/// first — RSS above baseline is a leak, RSS below baseline is a worker that died.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Value sits above the baseline centre.
    Above,
    /// Value sits below the baseline centre. `spec.md` requires both directions.
    Below,
}

/// Why the detector declined to answer.
///
/// Separate from `Normal` on purpose. `spec.md` requires readiness be reported "so
/// an operator can tell 'nothing wrong' from 'not yet learned'", and that is only
/// possible if the two are distinct values rather than one absent finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UndeterminedReason {
    /// The baseline has seen fewer than `min_baseline_samples`. Carries both counts
    /// so a caller can render "18 of 30 samples" without re-reading config.
    BaselineWarming { observed: u32, required: u32 },
    /// No value was measured this cycle. Not a zero, and not evidence of health.
    SampleAbsent,
    /// The measured value itself is NaN or infinite — a division by a zero interval
    /// upstream, typically. Comparing it would silently read as normal, since every
    /// comparison against NaN is false.
    SampleNotFinite,
    /// The baseline's own numbers are unusable, or its spread floors to zero (a flat
    /// metric centred on zero with no absolute floor). No honest z-score exists.
    BaselineUnusable,
}

/// The detector's answer for one sample.
///
/// A local output shape, not `src/findings.rs`'s type: that module is being written
/// separately and owns the finding lifecycle, confidence formula and evidence
/// contract (tasks section 7). This enum is the detector-side half of the seam —
/// [`LevelVerdict::Departed`] is the trigger a findings layer turns into a raised
/// finding, and [`LevelVerdict::Cleared`] the one it turns into a clear.
///
/// Note there is no "still departing" variant that repeats: while a departure holds,
/// the verdict is [`LevelVerdict::Holding`], which is `spec.md`'s "an active finding
/// SHALL NOT be re-raised on every evaluation" enforced by the type rather than by a
/// convention the caller has to remember.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum LevelVerdict {
    /// The detector cannot say. Not a claim of health.
    Undetermined { reason: UndeterminedReason },
    /// Compared, and within normal range for this process.
    Normal { z: f64 },
    /// Below threshold, or above it for too few consecutive samples. The run length
    /// is carried so the anti-spike suppression is observable rather than looking
    /// like silence — this is what makes a single spike explainable after the fact.
    Building {
        z: f64,
        direction: Direction,
        consecutive: u32,
        required: u32,
    },
    /// A departure is confirmed now. Emitted exactly once per episode.
    Departed { evidence: LevelEvidence },
    /// The value returned under the clear threshold, ending an active departure.
    /// Emitted exactly once per episode.
    Cleared { z: f64 },
    /// A departure is active and still holds. Deliberately not a re-raise.
    Holding { z: f64, direction: Direction },
}

/// Everything an operator needs to recompute a departure by hand.
///
/// `spec.md` makes evidence mandatory: "a finding without evidence SHALL NOT be
/// produced", and "a finding can be recomputed from its evidence". Both hold here
/// because `Departed` cannot be constructed without this struct, and because the
/// fields below are the complete input to the comparison — `z` is
/// `|observed − baseline_centre| / effective_spread`, and the verdict is that `z`
/// against `threshold_z` for `consecutive_samples`.
///
/// `sample_index` is the sample reference required by "evidence references the
/// underlying samples": a monotonic counter of measured samples this detector has
/// seen, which a caller can map to its own history. A timestamp is not recorded
/// here on purpose — this module takes no clock, so the caller stamps it and
/// determinism is preserved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LevelEvidence {
    /// Which detector produced this, so a finding says where it came from.
    pub detector: Detector,
    /// The value measured now.
    pub observed: f64,
    /// The baseline the comparison used, as it stood at this sample.
    pub baseline: BaselineSummary,
    /// The spread actually divided by, after flooring. Differs from
    /// `baseline.spread` exactly when a floor bound it, and recording both is what
    /// lets an operator see that a flat metric was floored rather than wonder why
    /// the arithmetic does not reproduce.
    pub effective_spread: f64,
    /// The robust z-score of `observed`.
    pub z: f64,
    /// The threshold applied, after config sanitisation.
    pub threshold_z: f64,
    /// Which side of the baseline.
    pub direction: Direction,
    /// How many consecutive agreeing samples backed this, at the moment it fired.
    pub consecutive_samples: u32,
    /// Index of the first measured sample in the agreeing run.
    pub window_start_sample: u64,
    /// Index of the sample that confirmed it.
    pub sample_index: u64,
}

/// Identifies the detector behind a finding.
///
/// An enum rather than a `&'static str`, for two reasons: a borrowed string cannot
/// round-trip through `Deserialize`, which evidence must do to be replayable, and a
/// findings layer matching on a string would duplicate the spelling in a second
/// place where it can drift. One variant today because this change ships one
/// detector; section 5b's refinements would add variants here if calibration earns
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Detector {
    /// The core level detector in this module.
    LevelDeparture,
}

/// Incremental level detector for one process and one metric.
///
/// State is six small fields and [`LevelDetector::observe`] is O(1) — no retained
/// history is rescanned, which is task 5.2 and `design.md`'s hard constraint that
/// per-cycle cost must not grow with uptime. The consecutive run is a counter, not a
/// buffer of samples.
///
/// One instance per (process, metric) pair. It holds no identity of its own, because
/// the caller already keys it by that pair and a duplicated key is a key that can
/// disagree with itself.
#[derive(Debug, Clone)]
pub struct LevelDetector {
    /// Sanitised config. Stored post-sanitisation so `observe` cannot accidentally
    /// use a raw unusable threshold.
    config: LevelConfig,
    /// Length of the current run of agreeing measured samples.
    consecutive: u32,
    /// Direction that run agrees on, if any.
    run_direction: Option<Direction>,
    /// Index of the first measured sample in the current run, for evidence.
    run_start_sample: u64,
    /// Whether a departure is currently active. This one flag is what makes a
    /// departure raise once and then hold.
    active: Option<Direction>,
    /// Count of measured samples seen. The sample reference in evidence, and
    /// deliberately not incremented for absent samples so an index always refers to
    /// something that was actually measured.
    samples_seen: u64,
}

impl LevelDetector {
    /// Builds a detector, replacing any unusable configured value with its default.
    ///
    /// Returns the fallbacks applied so the caller can report them, per `spec.md`'s
    /// "the fallback is reported". Dropping them here would make the report
    /// impossible later.
    pub fn new(config: LevelConfig) -> (Self, Vec<ConfigFallback>) {
        let (config, fallbacks) = config.sanitised();
        (
            Self {
                config,
                consecutive: 0,
                run_direction: None,
                run_start_sample: 0,
                active: None,
                samples_seen: 0,
            },
            fallbacks,
        )
    }

    /// Builds a detector with default tuning.
    #[cfg(test)]
    pub fn with_defaults() -> Self {
        Self::new(LevelConfig::default()).0
    }

    /// The config in force, after sanitisation.
    #[cfg(test)]
    pub fn config(&self) -> LevelConfig {
        self.config
    }

    /// Whether a departure is active, and in which direction.
    ///
    /// Exposed for the spec's "absence of findings is explainable": a caller can
    /// distinguish "nothing active" from "active and holding" without waiting for the
    /// next verdict.
    #[cfg(test)]
    pub fn active_direction(&self) -> Option<Direction> {
        self.active
    }

    /// Feeds one sample and returns the verdict.
    ///
    /// `value` is `None` when nothing was measured this cycle — a stopped process, an
    /// interval too short to divide by, a pid that has just changed. `None` is not
    /// zero: it breaks the consecutive run, because a run interrupted by a sample
    /// nobody took is not consecutive evidence, but it does not clear an active
    /// departure. "The daemon lost sight of the metric" is not "the metric recovered",
    /// and auto-clearing on blindness would make every restart look like a fix.
    pub fn observe(&mut self, value: Option<f64>, baseline: &BaselineSummary) -> LevelVerdict {
        let Some(value) = value else {
            self.reset_run();
            return LevelVerdict::Undetermined {
                reason: UndeterminedReason::SampleAbsent,
            };
        };

        if !value.is_finite() {
            self.reset_run();
            return LevelVerdict::Undetermined {
                reason: UndeterminedReason::SampleNotFinite,
            };
        }

        // Warm-up gate comes before any arithmetic: `spec.md` says detectors
        // depending on an immature baseline "SHALL NOT produce findings", and the
        // cheapest way to guarantee that is to have no code path from here to a
        // `Departed`. The run is reset too, so samples taken during warm-up cannot
        // later be counted as part of an agreeing run — that would smuggle startup
        // noise into a post-warm-up finding.
        if baseline.sample_count < self.config.min_baseline_samples {
            self.reset_run();
            return LevelVerdict::Undetermined {
                reason: UndeterminedReason::BaselineWarming {
                    observed: baseline.sample_count,
                    required: self.config.min_baseline_samples,
                },
            };
        }

        if !baseline.is_usable() {
            self.reset_run();
            return LevelVerdict::Undetermined {
                reason: UndeterminedReason::BaselineUnusable,
            };
        }

        let Some(effective_spread) = self.config.effective_spread(baseline) else {
            self.reset_run();
            return LevelVerdict::Undetermined {
                reason: UndeterminedReason::BaselineUnusable,
            };
        };

        self.samples_seen += 1;
        let sample_index = self.samples_seen;

        let deviation = value - baseline.centre;
        let z = deviation.abs() / effective_spread;
        let direction = if deviation < 0.0 {
            Direction::Below
        } else {
            Direction::Above
        };

        if z >= self.config.departure_z {
            self.extend_run(direction, sample_index);

            if self.active.is_some() {
                // Already reported. A departure that flips direction while active is
                // still one episode of "this metric is not at its baseline"; the
                // caller sees the current direction in `Holding` and no second raise
                // is emitted.
                return LevelVerdict::Holding { z, direction };
            }

            if self.consecutive >= self.config.min_consecutive_samples {
                self.active = Some(direction);
                return LevelVerdict::Departed {
                    evidence: LevelEvidence {
                        detector: Detector::LevelDeparture,
                        observed: value,
                        baseline: *baseline,
                        effective_spread,
                        z,
                        threshold_z: self.config.departure_z,
                        direction,
                        consecutive_samples: self.consecutive,
                        window_start_sample: self.run_start_sample,
                        sample_index,
                    },
                };
            }

            return LevelVerdict::Building {
                z,
                direction,
                consecutive: self.consecutive,
                required: self.config.min_consecutive_samples,
            };
        }

        // Under the firing threshold. The run ends here — that is the whole
        // anti-spike mechanism: a lone crossing is followed by a normal sample, which
        // zeroes the counter before it ever reaches the required length.
        self.reset_run();

        if self.active.is_some() {
            // Hysteresis: clearing at exactly `departure_z` would let a value parked
            // on the line raise and clear on alternate samples, each cycle a fresh
            // notification for one unchanged condition.
            if z <= self.config.departure_z * self.config.clear_z_fraction {
                self.active = None;
                return LevelVerdict::Cleared { z };
            }
            let direction = if deviation < 0.0 {
                Direction::Below
            } else {
                Direction::Above
            };
            return LevelVerdict::Holding { z, direction };
        }

        LevelVerdict::Normal { z }
    }

    /// Ends the current agreeing run without touching the active departure.
    fn reset_run(&mut self) {
        self.consecutive = 0;
        self.run_direction = None;
    }

    /// Counts this sample into the run, restarting it if the direction changed.
    ///
    /// A direction change restarts rather than extends: samples that disagree on
    /// which side of the baseline they sit are not evidence of one sustained level,
    /// and averaging over them would report a value the process never held.
    fn extend_run(&mut self, direction: Direction, sample_index: u64) {
        if self.run_direction == Some(direction) {
            self.consecutive += 1;
        } else {
            self.run_direction = Some(direction);
            self.consecutive = 1;
            self.run_start_sample = sample_index;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A baseline that is warm and has real spread. Centre 100, spread 2, so the
    /// default z=3.5 threshold sits at 100 +/- 7.
    fn warm() -> BaselineSummary {
        BaselineSummary::new(100.0, 2.0, DEFAULT_MIN_BASELINE_SAMPLES)
    }

    /// Feeds a slice of measured values and returns every verdict, so a test can
    /// assert on the whole episode rather than one sample.
    fn feed(
        detector: &mut LevelDetector,
        baseline: &BaselineSummary,
        values: &[f64],
    ) -> Vec<LevelVerdict> {
        values
            .iter()
            .map(|v| detector.observe(Some(*v), baseline))
            .collect()
    }

    fn departures(verdicts: &[LevelVerdict]) -> Vec<&LevelEvidence> {
        verdicts
            .iter()
            .filter_map(|v| match v {
                LevelVerdict::Departed { evidence } => Some(evidence),
                _ => None,
            })
            .collect()
    }

    // ---- Silence tests. These are the ones that matter most: every false
    // positive this detector can produce shows up here first.

    #[test]
    fn silent_on_normal_value_with_established_baseline() {
        let mut d = LevelDetector::with_defaults();
        // Not an empty input: 12 samples, all measured, all within the baseline's
        // ordinary range (+/- 3 either side of a centre of 100 with spread 2, so
        // z tops out at 1.5 against a threshold of 3.5).
        let values = [
            100.0, 101.0, 99.0, 102.5, 98.0, 100.5, 103.0, 97.0, 101.5, 99.5, 100.0, 102.0,
        ];
        let verdicts = feed(&mut d, &warm(), &values);

        assert_eq!(verdicts.len(), 12, "every sample must yield a verdict");
        assert!(departures(&verdicts).is_empty());
        for v in &verdicts {
            let LevelVerdict::Normal { z } = v else {
                panic!("expected Normal, got {v:?}");
            };
            assert!(*z < DEFAULT_DEPARTURE_Z);
        }
        assert_eq!(d.active_direction(), None);
    }

    #[test]
    fn silent_on_unestablished_baseline_and_says_why() {
        let mut d = LevelDetector::with_defaults();
        // One sample short of ready, and a value 25 sigma out. A detector that
        // fires here is the largest single source of false positives per spec.md.
        let warming = BaselineSummary::new(100.0, 2.0, DEFAULT_MIN_BASELINE_SAMPLES - 1);
        let verdicts = feed(&mut d, &warming, &[150.0; 10]);

        assert!(departures(&verdicts).is_empty());
        for v in &verdicts {
            assert_eq!(
                *v,
                LevelVerdict::Undetermined {
                    reason: UndeterminedReason::BaselineWarming {
                        observed: DEFAULT_MIN_BASELINE_SAMPLES - 1,
                        required: DEFAULT_MIN_BASELINE_SAMPLES,
                    }
                },
                "warm-up must report as not-yet-learned, never as Normal"
            );
        }
    }

    #[test]
    fn absent_sample_is_undetermined_not_normal_and_not_zero() {
        let mut d = LevelDetector::with_defaults();
        let b = warm();

        assert_eq!(
            d.observe(None, &b),
            LevelVerdict::Undetermined {
                reason: UndeterminedReason::SampleAbsent
            }
        );

        // If `None` were read as 0.0 against a centre of 100 and spread 2, that is
        // z = 50: a massive departure. Three of them in a row would fire.
        let verdicts: Vec<_> = (0..5).map(|_| d.observe(None, &b)).collect();
        assert!(departures(&verdicts).is_empty());
        assert_eq!(d.active_direction(), None);
    }

    #[test]
    fn single_spike_does_not_fire() {
        let mut d = LevelDetector::with_defaults();
        // One sample at z = 25, surrounded by normal ones. The classic transient.
        let verdicts = feed(&mut d, &warm(), &[100.0, 99.0, 150.0, 100.5, 101.0]);

        assert!(departures(&verdicts).is_empty());
        assert_eq!(d.active_direction(), None);
        // The suppression is observable rather than looking like silence.
        assert_eq!(
            verdicts[2],
            LevelVerdict::Building {
                z: 25.0,
                direction: Direction::Above,
                consecutive: 1,
                required: DEFAULT_MIN_CONSECUTIVE_SAMPLES,
            }
        );
    }

    #[test]
    fn repeated_isolated_spikes_never_accumulate() {
        let mut d = LevelDetector::with_defaults();
        // Spikes separated by a normal sample each time. Twelve crossings, and the
        // run counter is zeroed between every one, so nothing is ever reported.
        let mut values = Vec::new();
        for _ in 0..12 {
            values.push(150.0);
            values.push(100.0);
        }
        let verdicts = feed(&mut d, &warm(), &values);

        assert_eq!(verdicts.len(), 24);
        assert!(departures(&verdicts).is_empty());
    }

    #[test]
    fn absent_sample_breaks_a_building_run() {
        let mut d = LevelDetector::with_defaults();
        let b = warm();

        // Two departing samples, then a gap, then two more. Four crossings in total
        // but never three consecutive *measured* ones.
        assert!(matches!(
            d.observe(Some(150.0), &b),
            LevelVerdict::Building { consecutive: 1, .. }
        ));
        assert!(matches!(
            d.observe(Some(151.0), &b),
            LevelVerdict::Building { consecutive: 2, .. }
        ));
        assert!(matches!(
            d.observe(None, &b),
            LevelVerdict::Undetermined {
                reason: UndeterminedReason::SampleAbsent
            }
        ));
        assert!(matches!(
            d.observe(Some(152.0), &b),
            LevelVerdict::Building { consecutive: 1, .. }
        ));
        assert!(matches!(
            d.observe(Some(153.0), &b),
            LevelVerdict::Building { consecutive: 2, .. }
        ));
        assert_eq!(d.active_direction(), None);
    }

    #[test]
    fn alternating_directions_do_not_accumulate_a_run() {
        let mut d = LevelDetector::with_defaults();
        // Every sample crosses the threshold, but they disagree on which side. Not
        // one sustained level, so not a level departure.
        let verdicts = feed(&mut d, &warm(), &[150.0, 50.0, 150.0, 50.0, 150.0, 50.0]);

        assert!(departures(&verdicts).is_empty());
        for v in &verdicts {
            assert!(
                matches!(v, LevelVerdict::Building { consecutive: 1, .. }),
                "each flip restarts the run, got {v:?}"
            );
        }
    }

    #[test]
    fn flat_metric_is_silent_on_a_small_change() {
        let mut d = LevelDetector::with_defaults();
        // MAD of a perfectly flat series is 0. Without the floor this is z = inf.
        let flat = BaselineSummary::new(400.0, 0.0, 120);
        // 1% relative floor on a centre of 400 gives an effective spread of 4.0, so
        // a 2-unit change is z = 0.5.
        let verdicts = feed(&mut d, &flat, &[402.0, 402.0, 402.0, 402.0, 402.0]);

        assert!(departures(&verdicts).is_empty());
        for v in &verdicts {
            let LevelVerdict::Normal { z } = v else {
                panic!("expected Normal on a floored flat metric, got {v:?}");
            };
            assert!(z.is_finite(), "floor must prevent an infinite z-score");
            assert!((*z - 0.5).abs() < 1e-9, "z was {z}");
        }
    }

    #[test]
    fn flat_metric_centred_on_zero_with_no_absolute_floor_is_undetermined() {
        let mut d = LevelDetector::with_defaults();
        // Both floors evaluate to 0 here, so there is no honest denominator. The
        // answer must be "cannot say", not an infinite z-score.
        let flat_zero = BaselineSummary::new(0.0, 0.0, 120);
        let verdicts = feed(&mut d, &flat_zero, &[5.0, 5.0, 5.0, 5.0]);

        assert!(departures(&verdicts).is_empty());
        for v in &verdicts {
            assert_eq!(
                *v,
                LevelVerdict::Undetermined {
                    reason: UndeterminedReason::BaselineUnusable
                }
            );
        }
    }

    #[test]
    fn non_finite_sample_is_undetermined() {
        let mut d = LevelDetector::with_defaults();
        let b = warm();
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                d.observe(Some(bad), &b),
                LevelVerdict::Undetermined {
                    reason: UndeterminedReason::SampleNotFinite
                }
            );
        }
    }

    #[test]
    fn unusable_baseline_is_undetermined() {
        let mut d = LevelDetector::with_defaults();
        for bad in [
            BaselineSummary::new(f64::NAN, 2.0, 120),
            BaselineSummary::new(100.0, f64::NAN, 120),
            BaselineSummary::new(100.0, -1.0, 120),
        ] {
            assert_eq!(
                d.observe(Some(150.0), &bad),
                LevelVerdict::Undetermined {
                    reason: UndeterminedReason::BaselineUnusable
                }
            );
        }
    }

    // ---- Firing tests. The detector must not be silent on a real step change,
    // or the silence tests above would pass for the wrong reason.

    #[test]
    fn sustained_step_fires_once_after_the_required_run() {
        let mut d = LevelDetector::with_defaults();
        // A clean step to 150 (z = 25) held for eight samples.
        let verdicts = feed(&mut d, &warm(), &[150.0; 8]);

        let fired = departures(&verdicts);
        assert_eq!(fired.len(), 1, "one step must produce exactly one raise");

        // Fires on the third sample, not the first or second.
        assert!(matches!(
            verdicts[0],
            LevelVerdict::Building { consecutive: 1, .. }
        ));
        assert!(matches!(
            verdicts[1],
            LevelVerdict::Building { consecutive: 2, .. }
        ));
        assert!(matches!(verdicts[2], LevelVerdict::Departed { .. }));

        // Everything after is Holding: active but not re-raised.
        for v in &verdicts[3..] {
            assert!(
                matches!(
                    v,
                    LevelVerdict::Holding {
                        direction: Direction::Above,
                        ..
                    }
                ),
                "expected Holding after the raise, got {v:?}"
            );
        }
        assert_eq!(d.active_direction(), Some(Direction::Above));
    }

    #[test]
    fn departure_below_the_baseline_fires() {
        let mut d = LevelDetector::with_defaults();
        // A worker pool that died: 100 -> 50, i.e. z = 25 on the low side.
        let verdicts = feed(&mut d, &warm(), &[50.0; 5]);

        let fired = departures(&verdicts);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].direction, Direction::Below);
        assert_eq!(fired[0].observed, 50.0);
    }

    #[test]
    fn meaningful_change_on_a_quiet_metric_still_fires() {
        let mut d = LevelDetector::with_defaults();
        // Same flat baseline as the silence test, but a substantial move: centre
        // 400, effective spread 4.0 from the 1% relative floor, value 430 is z =
        // 7.5. The floor must not be so aggressive that it hides a real shift.
        let flat = BaselineSummary::new(400.0, 0.0, 120);
        let verdicts = feed(&mut d, &flat, &[430.0; 4]);

        let fired = departures(&verdicts);
        assert_eq!(fired.len(), 1);
        assert!((fired[0].z - 7.5).abs() < 1e-9, "z was {}", fired[0].z);
        assert!((fired[0].effective_spread - 4.0).abs() < 1e-9);
    }

    #[test]
    fn evidence_reproduces_the_comparison() {
        let mut d = LevelDetector::with_defaults();
        let b = warm();
        let verdicts = feed(&mut d, &b, &[100.0, 150.0, 150.0, 150.0]);
        let e = departures(&verdicts)[0];

        assert_eq!(e.detector, Detector::LevelDeparture);
        assert_eq!(e.observed, 150.0);
        assert_eq!(e.baseline, b);
        assert_eq!(e.threshold_z, DEFAULT_DEPARTURE_Z);
        assert_eq!(e.consecutive_samples, DEFAULT_MIN_CONSECUTIVE_SAMPLES);
        assert_eq!(e.direction, Direction::Above);

        // Recompute z from the recorded numbers alone: this is the spec's "a
        // finding can be recomputed from its evidence".
        let recomputed = (e.observed - e.baseline.centre).abs() / e.effective_spread;
        assert!((recomputed - e.z).abs() < 1e-9);
        assert!(e.z >= e.threshold_z);

        // Sample references: the run started at the second measured sample and was
        // confirmed by the fourth.
        assert_eq!(e.window_start_sample, 2);
        assert_eq!(e.sample_index, 4);
    }

    #[test]
    fn evidence_is_machine_readable() {
        let mut d = LevelDetector::with_defaults();
        let verdicts = feed(&mut d, &warm(), &[150.0; 3]);
        let json = serde_json::to_string(&verdicts[2]).expect("verdict must serialise");

        assert!(json.contains(r#""verdict":"departed""#), "{json}");
        assert!(json.contains(r#""detector":"level_departure""#), "{json}");
        assert!(json.contains(r#""direction":"above""#), "{json}");
        assert!(json.contains(r#""threshold_z":3.5"#), "{json}");
    }

    #[test]
    fn departure_clears_with_hysteresis_then_can_raise_again() {
        let mut d = LevelDetector::with_defaults();
        let b = warm();

        let rise = feed(&mut d, &b, &[150.0; 3]);
        assert_eq!(departures(&rise).len(), 1);

        // z = 3.0: back under the firing threshold of 3.5 but above the clear
        // threshold of 2.625, so it holds rather than clearing. This is the
        // oscillation guard.
        let held = d.observe(Some(106.0), &b);
        assert!(matches!(held, LevelVerdict::Holding { .. }), "got {held:?}");
        assert_eq!(d.active_direction(), Some(Direction::Above));

        // z = 0.5: under the clear threshold.
        let cleared = d.observe(Some(101.0), &b);
        let LevelVerdict::Cleared { z } = cleared else {
            panic!("expected Cleared, got {cleared:?}");
        };
        assert!(z <= DEFAULT_DEPARTURE_Z * DEFAULT_CLEAR_Z_FRACTION);
        assert_eq!(d.active_direction(), None);

        // A recurrence after clearing raises a new finding.
        let again = feed(&mut d, &b, &[150.0; 3]);
        assert_eq!(departures(&again).len(), 1);
    }

    #[test]
    fn absent_samples_do_not_clear_an_active_departure() {
        let mut d = LevelDetector::with_defaults();
        let b = warm();
        assert_eq!(departures(&feed(&mut d, &b, &[150.0; 3])).len(), 1);

        // The process stops reporting. Losing sight of a metric is not recovery.
        for _ in 0..5 {
            assert_eq!(
                d.observe(None, &b),
                LevelVerdict::Undetermined {
                    reason: UndeterminedReason::SampleAbsent
                }
            );
        }
        assert_eq!(d.active_direction(), Some(Direction::Above));
    }

    #[test]
    fn identical_input_sequences_give_identical_output() {
        let b = warm();
        let values = [
            100.0, 103.0, 150.0, 151.0, 149.0, 100.0, 99.0, 50.0, 51.0, 52.0, 100.0,
        ];
        let mut a = LevelDetector::with_defaults();
        let mut c = LevelDetector::with_defaults();

        let first = feed(&mut a, &b, &values);
        let second = feed(&mut c, &b, &values);

        assert_eq!(first, second, "detection must be deterministic");
        assert!(!departures(&first).is_empty(), "test must exercise a raise");
    }

    #[test]
    fn threshold_is_configurable() {
        let b = warm();
        // z of 150 against centre 100, spread 2, is 25. A threshold of 30 must not
        // fire on it; the default 3.5 must.
        let (mut strict, fallbacks) = LevelDetector::new(LevelConfig {
            departure_z: 30.0,
            ..LevelConfig::default()
        });
        assert!(fallbacks.is_empty());
        assert!(departures(&feed(&mut strict, &b, &[150.0; 6])).is_empty());

        let mut lenient = LevelDetector::with_defaults();
        assert_eq!(departures(&feed(&mut lenient, &b, &[150.0; 6])).len(), 1);
    }

    #[test]
    fn persistence_requirement_is_configurable() {
        let b = warm();
        let (mut patient, _) = LevelDetector::new(LevelConfig {
            min_consecutive_samples: 6,
            ..LevelConfig::default()
        });
        // Five departing samples with a six-sample requirement: still silent.
        assert!(departures(&feed(&mut patient, &b, &[150.0; 5])).is_empty());
        // The sixth confirms it.
        let sixth = patient.observe(Some(150.0), &b);
        let LevelVerdict::Departed { evidence } = sixth else {
            panic!("expected Departed on the sixth sample, got {sixth:?}");
        };
        assert_eq!(evidence.consecutive_samples, 6);
    }

    #[test]
    fn unusable_config_falls_back_and_reports_each_fallback() {
        let (d, fallbacks) = LevelDetector::new(LevelConfig {
            departure_z: f64::NAN,
            min_consecutive_samples: 0,
            min_baseline_samples: 0,
            spread_floor_abs: -1.0,
            spread_floor_rel: f64::INFINITY,
            clear_z_fraction: 1.5,
        });

        let applied = d.config();
        assert_eq!(applied, LevelConfig::default());

        let fields: Vec<&str> = fallbacks.iter().map(|f| f.field).collect();
        assert_eq!(
            fields,
            vec![
                "departure_z",
                "min_consecutive_samples",
                "min_baseline_samples",
                "spread_floor_abs",
                "spread_floor_rel",
                "clear_z_fraction",
            ],
            "every unusable value must be reported, not silently corrected"
        );
    }

    #[test]
    fn absolute_spread_floor_overrides_a_smaller_relative_floor() {
        // A caller that knows its measurement granularity sets the absolute floor.
        // Centre 10, so the 1% relative floor is 0.1; an absolute floor of 5.0 must
        // win, making a 12-unit deviation z = 2.4 rather than 120.
        let (mut d, _) = LevelDetector::new(LevelConfig {
            spread_floor_abs: 5.0,
            ..LevelConfig::default()
        });
        let flat = BaselineSummary::new(10.0, 0.0, 120);
        let verdicts = feed(&mut d, &flat, &[22.0; 5]);

        assert!(departures(&verdicts).is_empty());
        let LevelVerdict::Normal { z } = verdicts[0] else {
            panic!("got {:?}", verdicts[0]);
        };
        assert!((z - 2.4).abs() < 1e-9, "z was {z}");
    }

    #[test]
    fn baseline_spread_wins_when_it_exceeds_both_floors() {
        // The floors are a minimum, not a replacement: a genuinely noisy metric must
        // keep its own measured spread.
        let noisy = BaselineSummary::new(100.0, 20.0, 120);
        let mut d = LevelDetector::with_defaults();
        let verdicts = feed(&mut d, &noisy, &[150.0; 5]);

        // 50 / 20 = 2.5, under the 3.5 threshold. The same value fired on the
        // spread-2 baseline; here it is ordinary variation.
        assert!(departures(&verdicts).is_empty());
        let LevelVerdict::Normal { z } = verdicts[0] else {
            panic!("got {:?}", verdicts[0]);
        };
        assert!((z - 2.5).abs() < 1e-9, "z was {z}");
    }

    #[test]
    fn warm_up_discards_a_run_built_before_readiness() {
        let mut d = LevelDetector::with_defaults();
        let warming = BaselineSummary::new(100.0, 2.0, DEFAULT_MIN_BASELINE_SAMPLES - 1);

        // Two departing samples while warming must not count toward the run.
        d.observe(Some(150.0), &warming);
        d.observe(Some(150.0), &warming);

        // Now ready. If the warming samples had counted, this one would fire.
        let first_ready = d.observe(Some(150.0), &warm());
        assert!(
            matches!(first_ready, LevelVerdict::Building { consecutive: 1, .. }),
            "startup samples must not be counted as agreement, got {first_ready:?}"
        );
    }
}
