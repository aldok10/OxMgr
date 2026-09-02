//! Tuning and false-positive control: what detection is allowed to run, and for whom.
//!
//! False positives are the failure mode that kills this feature. An operator who learns to ignore
//! findings has gained noise and lost nothing else, so every layer here exists to let them turn
//! something off *narrowly* rather than abandoning detection altogether.
//!
//! # Four scopes, narrowest wins
//!
//! 1. Global disable — no findings at all.
//! 2. Per-detector disable — that detector produces nothing, for anybody.
//! 3. Per-process, per-detector suppression — that pair produces nothing; every other pair is
//!    unaffected.
//! 4. Per-process threshold overrides — detection still runs, tuned differently.
//!
//! [`TuningConfig::gate`] answers all four in one call and returns *why*, because a caller that only
//! learns "no" cannot report the reason and the spec requires suppression to be visible.
//!
//! # Suppression is visible, never silent
//!
//! A suppressed detector still counts what it suppressed ([`SuppressionCounters`]) and the reason is
//! reportable per process ([`TuningConfig::suppressions_for`]). Silent suppression is worse than no
//! suppression: it makes "this process is healthy" and "we stopped looking at this process"
//! indistinguishable, and the second one is the state in which an incident is missed.
//!
//! # History is retained even when detection is off
//!
//! Disabling detection stops findings, not sampling. That is deliberate and is asserted: an operator
//! who disables detection during an incident still wants the metric history afterwards, and a
//! disable that also discarded history would destroy the evidence needed to tune the thresholds that
//! caused the disable.

use std::collections::{BTreeMap, BTreeSet};

use crate::findings::Detector;

/// The documented default departure z-score (robust z-score threshold).
///
/// Source of truth lives in `oxmgr-core` tuning so analytics (detector_level)
/// can depend on it without creating a reverse layer arrow.
pub const DEFAULT_DEPARTURE_Z: f64 = 3.5;

/// The documented default for consecutive agreeing samples.
pub const DEFAULT_MIN_CONSECUTIVE_SAMPLES: u32 = 3;

/// The documented default for baseline samples required before a detector may fire.
pub const DEFAULT_MIN_BASELINE_SAMPLES: u32 = 30;

/// Why detection did not run.
///
/// Carried rather than collapsed to a boolean so the reason reaches the operator surface. "No
/// findings because everything is fine" and "no findings because you switched this off in March" are
/// the two states an operator most needs to tell apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked {
    /// Detection is disabled daemon-wide.
    DetectionDisabled,
    /// This detector is disabled for every process.
    DetectorDisabled { detector: Detector },
    /// This detector is suppressed for this process only.
    SuppressedForProcess {
        detector: Detector,
        process: String,
        /// The operator's note, when they left one.
        note: Option<String>,
    },
}

impl Blocked {
    /// A short operator-facing reason.
    #[cfg(test)]
    pub fn reason(&self) -> String {
        match self {
            Self::DetectionDisabled => "detection is disabled".to_string(),
            Self::DetectorDisabled { detector } => format!("detector {detector} is disabled"),
            Self::SuppressedForProcess {
                detector,
                process,
                note,
            } => match note {
                Some(note) => {
                    format!("detector {detector} is suppressed for {process}: {note}")
                }
                None => format!("detector {detector} is suppressed for {process}"),
            },
        }
    }

    /// The narrowest scope this block applies to, for ordering a report.
    #[cfg(test)]
    pub fn scope(&self) -> &'static str {
        match self {
            Self::DetectionDisabled => "global",
            Self::DetectorDisabled { .. } => "detector",
            Self::SuppressedForProcess { .. } => "process",
        }
    }
}

/// Whether a detector may run, and why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// Detection may run.
    Allowed,
    /// Detection must not run.
    Blocked(Blocked),
}

impl Gate {
    #[cfg(test)]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }

    /// The reason, when blocked.
    pub fn blocked(&self) -> Option<&Blocked> {
        match self {
            Self::Allowed => None,
            Self::Blocked(reason) => Some(reason),
        }
    }
}

/// One suppression, as declared.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Suppression {
    /// The operator's note. Optional, but surfaced when present — six months later, "why is this
    /// suppressed" is the question, and the answer is rarely reconstructible.
    pub note: Option<String>,
}

/// A configured value that could not be used, and what was applied instead.
///
/// Reported rather than logged and forgotten. A daemon quietly running on defaults because a
/// threshold was mistyped looks identical to one that was tuned correctly, and the operator has no
/// way to discover the difference.
#[derive(Debug, Clone, PartialEq)]
pub struct Fallback {
    /// Which setting.
    pub setting: String,
    /// The process it was configured for, or `None` for a daemon-wide value.
    pub process: Option<String>,
    /// How the value was unusable.
    pub problem: String,
    /// The documented default that was applied, rendered.
    pub applied: String,
}

impl Fallback {
    /// A short operator-facing sentence.
    #[cfg(test)]
    pub fn reason(&self) -> String {
        match &self.process {
            Some(process) => format!(
                "{}: {} for {process} is unusable ({}); applied the default {}",
                self.setting, self.setting, self.problem, self.applied
            ),
            None => format!(
                "{} is unusable ({}); applied the default {}",
                self.setting, self.problem, self.applied
            ),
        }
    }
}

/// Per-process detector thresholds.
///
/// Every field is optional: absent means "use the default", which is different from a configured
/// value that happens to equal the default — only the latter appears in a report of what was
/// overridden.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcessThresholds {
    pub departure_z: Option<f64>,
    pub min_consecutive_samples: Option<u32>,
    pub min_baseline_samples: Option<u32>,
}

/// The resolved thresholds for one process, plus any fallbacks that were applied.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedThresholds {
    pub departure_z: f64,
    pub min_consecutive_samples: u32,
    pub min_baseline_samples: u32,
    /// Values that could not be used. Empty in the ordinary case.
    pub fallbacks: Vec<Fallback>,
}

impl ResolvedThresholds {
    /// Whether every configured value was usable.
    #[cfg(test)]
    pub fn is_clean(&self) -> bool {
        self.fallbacks.is_empty()
    }
}

/// Counters for what detection did and did not do.
///
/// Part of *the detection engine reports its own operation*: an engine that cannot say how much it
/// suppressed cannot be tuned, because the operator has no measure of whether a suppression is doing
/// nothing or hiding a flood.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SuppressionCounters {
    /// Findings that would have been produced but were blocked globally.
    pub blocked_globally: u64,
    /// Blocked by a per-detector disable.
    pub blocked_by_detector: u64,
    /// Blocked by a per-process suppression.
    pub blocked_by_process: u64,
}

impl SuppressionCounters {
    /// Total blocked across every scope.
    #[cfg(test)]
    pub fn total(&self) -> u64 {
        self.blocked_globally
            .saturating_add(self.blocked_by_detector)
            .saturating_add(self.blocked_by_process)
    }

    /// Counts one block against the scope that caused it.
    pub fn record(&mut self, blocked: &Blocked) {
        match blocked {
            Blocked::DetectionDisabled => {
                self.blocked_globally = self.blocked_globally.saturating_add(1);
            }
            Blocked::DetectorDisabled { .. } => {
                self.blocked_by_detector = self.blocked_by_detector.saturating_add(1);
            }
            Blocked::SuppressedForProcess { .. } => {
                self.blocked_by_process = self.blocked_by_process.saturating_add(1);
            }
        }
    }
}

/// Daemon-wide tuning configuration.
///
/// Defaults to everything enabled and nothing suppressed: detection is useful out of the box, and
/// the tuning surface exists to narrow it after the operator has seen what it reports.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TuningConfig {
    /// Master switch. `false` produces no findings at all — but does not stop sampling.
    pub detection_disabled: bool,
    /// Detectors disabled for every process.
    pub disabled_detectors: BTreeSet<String>,
    /// Per-process, per-detector suppressions, keyed by process then detector wire name.
    ///
    /// `BTreeMap` for deterministic iteration: the suppression report is served over HTTP and shown
    /// in a TUI, and a set that reordered itself would make one identical to a diff.
    pub suppressions: BTreeMap<String, BTreeMap<String, Suppression>>,
    /// Per-process threshold overrides.
    pub thresholds: BTreeMap<String, ProcessThresholds>,
}

impl TuningConfig {
    /// Whether `detector` may produce findings for `process`, and why not.
    ///
    /// Checked widest-scope first, so the reported reason is the one an operator would act on: a
    /// globally disabled daemon reports `DetectionDisabled` rather than naming a per-process
    /// suppression that is also, incidentally, in effect.
    pub fn gate(&self, process: &str, detector: &Detector) -> Gate {
        if self.detection_disabled {
            return Gate::Blocked(Blocked::DetectionDisabled);
        }

        let wire = detector.as_wire();
        if self.disabled_detectors.contains(wire) {
            return Gate::Blocked(Blocked::DetectorDisabled {
                detector: detector.clone(),
            });
        }

        if let Some(suppression) = self
            .suppressions
            .get(process)
            .and_then(|per_detector| per_detector.get(wire))
        {
            return Gate::Blocked(Blocked::SuppressedForProcess {
                detector: detector.clone(),
                process: process.to_string(),
                note: suppression.note.clone(),
            });
        }

        Gate::Allowed
    }

    /// Every suppression in effect for one process, widest scope first.
    ///
    /// This is what makes *absence of findings is explainable*: a process with no active findings can
    /// be asked why, and answers with the blocks that apply to it rather than an empty list that
    /// reads as "healthy".
    pub fn suppressions_for(&self, process: &str, detectors: &[Detector]) -> Vec<Blocked> {
        if self.detection_disabled {
            // One entry, not one per detector: the daemon-wide disable is a single fact, and
            // repeating it per detector would bury the per-process reasons under it.
            return vec![Blocked::DetectionDisabled];
        }

        detectors
            .iter()
            .filter_map(|detector| match self.gate(process, detector) {
                Gate::Allowed => None,
                Gate::Blocked(reason) => Some(reason),
            })
            .collect()
    }

    /// Whether anything is suppressed for a process.
    #[cfg(test)]
    pub fn is_suppressed(&self, process: &str, detector: &Detector) -> bool {
        !self.gate(process, detector).is_allowed()
    }

    /// Suppresses one detector for one process.
    pub fn suppress(&mut self, process: &str, detector: &Detector, note: Option<String>) {
        self.suppressions
            .entry(process.to_string())
            .or_default()
            .insert(detector.as_wire().to_string(), Suppression { note });
    }

    /// Lifts a suppression. Reports whether one was in effect.
    #[cfg(test)]
    pub fn unsuppress(&mut self, process: &str, detector: &Detector) -> bool {
        let Some(per_detector) = self.suppressions.get_mut(process) else {
            return false;
        };
        let removed = per_detector.remove(detector.as_wire()).is_some();
        if per_detector.is_empty() {
            // Pruned so a process with no suppressions does not linger as an empty map and read as
            // "configured" in a report.
            self.suppressions.remove(process);
        }
        removed
    }

    /// Drops a process's tuning, for a delete.
    pub fn forget(&mut self, process: &str) {
        self.suppressions.remove(process);
        self.thresholds.remove(process);
    }

    /// Resolves thresholds for one process, applying documented defaults where a configured value is
    /// absent or unusable.
    ///
    /// Unusable values FALL BACK and are reported; they are never clamped. Clamping a mistyped
    /// `departure_z` of 350 to some ceiling would silently mean "never fires", and an operator who
    /// typed a percentage deserves the default plus a report rather than a permanently quiet
    /// detector.
    pub fn resolve_thresholds(&self, process: &str) -> ResolvedThresholds {
        let mut resolved = ResolvedThresholds {
            departure_z: DEFAULT_DEPARTURE_Z,
            min_consecutive_samples: DEFAULT_MIN_CONSECUTIVE_SAMPLES,
            min_baseline_samples: DEFAULT_MIN_BASELINE_SAMPLES,
            fallbacks: Vec::new(),
        };

        let Some(overrides) = self.thresholds.get(process) else {
            return resolved;
        };

        if let Some(value) = overrides.departure_z {
            // Finite and positive. A zero or negative z-score would make every sample a departure,
            // which is not a tuning choice but a misunderstanding, and acting on it would flood the
            // operator with findings on a healthy process.
            if value.is_finite() && value > 0.0 {
                resolved.departure_z = value;
            } else {
                resolved.fallbacks.push(Fallback {
                    setting: "departure_z".to_string(),
                    process: Some(process.to_string()),
                    problem: if value.is_finite() {
                        format!("{value} is not positive")
                    } else {
                        "not a finite number".to_string()
                    },
                    applied: format!("{DEFAULT_DEPARTURE_Z}"),
                });
            }
        }

        if let Some(value) = overrides.min_consecutive_samples {
            // Zero would mean "report on a single sample", removing the agreement requirement that
            // exists specifically to absorb one-off spikes.
            if value > 0 {
                resolved.min_consecutive_samples = value;
            } else {
                resolved.fallbacks.push(Fallback {
                    setting: "min_consecutive_samples".to_string(),
                    process: Some(process.to_string()),
                    problem: "0 would report on a single sample".to_string(),
                    applied: format!("{DEFAULT_MIN_CONSECUTIVE_SAMPLES}"),
                });
            }
        }

        if let Some(value) = overrides.min_baseline_samples {
            // Zero would let a detector fire against a baseline of nothing, which is the warm-up
            // gate that removes the largest class of false positives.
            if value > 0 {
                resolved.min_baseline_samples = value;
            } else {
                resolved.fallbacks.push(Fallback {
                    setting: "min_baseline_samples".to_string(),
                    process: Some(process.to_string()),
                    problem: "0 would fire against an unestablished baseline".to_string(),
                    applied: format!("{DEFAULT_MIN_BASELINE_SAMPLES}"),
                });
            }
        }

        resolved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detectors() -> Vec<Detector> {
        vec![
            Detector::LevelDeparture,
            Detector::ResourceLeak,
            Detector::Drift,
        ]
    }

    // ---- global disable (11.1) ----

    #[test]
    fn the_default_configuration_allows_every_detector() {
        // Detection is useful out of the box; the tuning surface exists to narrow it afterwards.
        let config = TuningConfig::default();
        for detector in detectors() {
            assert!(
                config.gate("api", &detector).is_allowed(),
                "{detector} should be allowed by default"
            );
        }
        assert!(config.suppressions_for("api", &detectors()).is_empty());
    }

    #[test]
    fn a_global_disable_blocks_every_detector_for_every_process() {
        let config = TuningConfig {
            detection_disabled: true,
            ..TuningConfig::default()
        };

        for process in ["api", "worker"] {
            for detector in detectors() {
                assert_eq!(
                    config.gate(process, &detector),
                    Gate::Blocked(Blocked::DetectionDisabled),
                    "{detector} on {process} must be blocked"
                );
            }
        }
    }

    #[test]
    fn a_global_disable_is_reported_once_rather_than_per_detector() {
        // A daemon-wide disable is a single fact. Repeating it per detector would bury the
        // per-process reasons an operator is actually looking for under N copies of one.
        let config = TuningConfig {
            detection_disabled: true,
            ..TuningConfig::default()
        };

        let reported = config.suppressions_for("api", &detectors());
        assert_eq!(reported, vec![Blocked::DetectionDisabled]);
    }

    #[test]
    fn a_global_disable_does_not_touch_retention() {
        // Asserted structurally, since it is a claim about what this module CANNOT do: `TuningConfig`
        // has no handle to a history store, no method that removes samples, and `gate` returns a
        // verdict rather than performing anything. Disabling detection therefore cannot discard
        // history — which matters because an operator who disables during an incident still needs the
        // samples afterwards to tune the thresholds that caused the disable.
        let config = TuningConfig {
            detection_disabled: true,
            ..TuningConfig::default()
        };

        // The only thing a disable changes is the verdict.
        assert!(!config.gate("api", &Detector::LevelDeparture).is_allowed());
        // Thresholds still resolve, so the sampling path that consults them is unaffected.
        assert!(config.resolve_thresholds("api").is_clean());
    }

    // ---- per-detector disable (11.2) ----

    #[test]
    fn disabling_one_detector_leaves_the_others_producing() {
        let mut config = TuningConfig::default();
        config
            .disabled_detectors
            .insert(Detector::ResourceLeak.as_wire().to_string());

        assert_eq!(
            config.gate("api", &Detector::ResourceLeak),
            Gate::Blocked(Blocked::DetectorDisabled {
                detector: Detector::ResourceLeak
            })
        );
        assert!(
            config.gate("api", &Detector::LevelDeparture).is_allowed(),
            "another detector must be unaffected"
        );
    }

    #[test]
    fn a_disabled_detector_is_disabled_for_every_process() {
        let mut config = TuningConfig::default();
        config
            .disabled_detectors
            .insert(Detector::Drift.as_wire().to_string());

        for process in ["api", "worker", "batch"] {
            assert!(!config.gate(process, &Detector::Drift).is_allowed());
        }
    }

    // ---- per-process suppression (11.3) ----

    #[test]
    fn suppression_is_scoped_to_one_process_and_one_detector() {
        // The narrowest scope, and the one that matters most: an operator suppressing a noisy
        // detector on one process must not lose it everywhere.
        let mut config = TuningConfig::default();
        config.suppress("api", &Detector::LevelDeparture, None);

        assert!(config.is_suppressed("api", &Detector::LevelDeparture));
        assert!(
            config.gate("api", &Detector::ResourceLeak).is_allowed(),
            "another detector on the same process must still run"
        );
        assert!(
            config
                .gate("worker", &Detector::LevelDeparture)
                .is_allowed(),
            "the same detector on another process must still run"
        );
    }

    #[test]
    fn a_suppression_is_visible_when_findings_are_queried() {
        // Silent suppression makes "this process is healthy" and "we stopped looking at this process"
        // indistinguishable, and the second is the state in which an incident is missed.
        let mut config = TuningConfig::default();
        config.suppress(
            "api",
            &Detector::LevelDeparture,
            Some("known noisy during nightly batch".to_string()),
        );

        let reported = config.suppressions_for("api", &detectors());
        assert_eq!(reported.len(), 1, "the suppression is reported");
        match &reported[0] {
            Blocked::SuppressedForProcess {
                detector,
                process,
                note,
            } => {
                assert_eq!(detector, &Detector::LevelDeparture);
                assert_eq!(process, "api");
                // The operator's note survives, because six months later "why is this suppressed" is
                // the question and the answer is rarely reconstructible.
                assert_eq!(note.as_deref(), Some("known noisy during nightly batch"));
                assert!(reported[0].reason().contains("nightly batch"));
            }
            other => panic!("expected a process suppression, got {other:?}"),
        }
        assert!(config.suppressions_for("worker", &detectors()).is_empty());
    }

    #[test]
    fn lifting_a_suppression_reports_whether_one_was_in_effect() {
        let mut config = TuningConfig::default();
        config.suppress("api", &Detector::LevelDeparture, None);

        assert!(config.unsuppress("api", &Detector::LevelDeparture));
        assert!(config.gate("api", &Detector::LevelDeparture).is_allowed());
        // Idempotent, and honest about it: a second lift reports that nothing was in effect.
        assert!(!config.unsuppress("api", &Detector::LevelDeparture));
        // Pruned rather than left as an empty map, so a process with no suppressions does not read as
        // "configured" in a report.
        assert!(!config.suppressions.contains_key("api"));
    }

    #[test]
    fn the_widest_applicable_scope_is_the_one_reported() {
        // Both a global disable and a per-process suppression are in effect. The report must name the
        // global one, because that is the one an operator would act on — lifting the suppression
        // would change nothing.
        let mut config = TuningConfig {
            detection_disabled: true,
            ..TuningConfig::default()
        };
        config.suppress("api", &Detector::LevelDeparture, None);

        assert_eq!(
            config.gate("api", &Detector::LevelDeparture),
            Gate::Blocked(Blocked::DetectionDisabled)
        );
    }

    #[test]
    fn forgetting_a_process_drops_its_tuning() {
        let mut config = TuningConfig::default();
        config.suppress("api", &Detector::LevelDeparture, None);
        config.suppress("worker", &Detector::LevelDeparture, None);
        config.thresholds.insert(
            "api".to_string(),
            ProcessThresholds {
                departure_z: Some(5.0),
                ..ProcessThresholds::default()
            },
        );

        config.forget("api");

        assert!(config.gate("api", &Detector::LevelDeparture).is_allowed());
        assert_eq!(
            config.resolve_thresholds("api").departure_z,
            DEFAULT_DEPARTURE_Z
        );
        assert!(
            config.is_suppressed("worker", &Detector::LevelDeparture),
            "forgetting one process must not clear another"
        );
    }

    // ---- threshold overrides (11.4) ----

    #[test]
    fn an_override_applies_to_its_process_and_others_use_the_default() {
        let mut config = TuningConfig::default();
        config.thresholds.insert(
            "api".to_string(),
            ProcessThresholds {
                departure_z: Some(6.0),
                min_consecutive_samples: Some(5),
                min_baseline_samples: None,
            },
        );

        let api = config.resolve_thresholds("api");
        assert_eq!(api.departure_z, 6.0);
        assert_eq!(api.min_consecutive_samples, 5);
        // Absent means default, per field rather than per process: a partial override does not
        // discard the settings it did not mention.
        assert_eq!(api.min_baseline_samples, DEFAULT_MIN_BASELINE_SAMPLES);
        assert!(api.is_clean(), "usable values produce no fallbacks");

        let worker = config.resolve_thresholds("worker");
        assert_eq!(worker.departure_z, DEFAULT_DEPARTURE_Z);
        assert_eq!(
            worker.min_consecutive_samples,
            DEFAULT_MIN_CONSECUTIVE_SAMPLES
        );
    }

    // ---- unusable values fall back and report (11.5) ----

    #[test]
    fn an_unusable_departure_z_falls_back_and_reports() {
        // Falls back, never clamps. Clamping a mistyped 350 to a ceiling would silently mean "never
        // fires", and an operator who typed a percentage deserves the default plus a report rather
        // than a permanently quiet detector.
        for (bad, expected_problem) in [
            (0.0_f64, "not positive"),
            (-3.5, "not positive"),
            (f64::NAN, "finite"),
            (f64::INFINITY, "finite"),
        ] {
            let mut config = TuningConfig::default();
            config.thresholds.insert(
                "api".to_string(),
                ProcessThresholds {
                    departure_z: Some(bad),
                    ..ProcessThresholds::default()
                },
            );

            let resolved = config.resolve_thresholds("api");
            assert_eq!(
                resolved.departure_z, DEFAULT_DEPARTURE_Z,
                "{bad} must fall back to the documented default"
            );
            assert!(!resolved.is_clean(), "{bad} must be reported");
            let fallback = &resolved.fallbacks[0];
            assert_eq!(fallback.setting, "departure_z");
            assert_eq!(fallback.process.as_deref(), Some("api"));
            assert!(
                fallback.problem.contains(expected_problem),
                "problem {:?} should mention {expected_problem}",
                fallback.problem
            );
            // The report names the value that WAS applied, so the operator can tell what the daemon
            // is actually running on.
            assert!(fallback.applied.contains("3.5"));
            assert!(fallback.reason().contains("departure_z"));
        }
    }

    #[test]
    fn a_zero_sample_requirement_falls_back_and_reports() {
        // 0 for either counter would defeat the two mechanisms that remove the largest classes of
        // false positive: agreement absorbs one-off spikes, and the warm-up gate stops a detector
        // firing against a baseline of nothing.
        let mut config = TuningConfig::default();
        config.thresholds.insert(
            "api".to_string(),
            ProcessThresholds {
                departure_z: None,
                min_consecutive_samples: Some(0),
                min_baseline_samples: Some(0),
            },
        );

        let resolved = config.resolve_thresholds("api");
        assert_eq!(
            resolved.min_consecutive_samples,
            DEFAULT_MIN_CONSECUTIVE_SAMPLES
        );
        assert_eq!(resolved.min_baseline_samples, DEFAULT_MIN_BASELINE_SAMPLES);
        assert_eq!(
            resolved.fallbacks.len(),
            2,
            "both unusable values are reported, not just the first"
        );
        let settings: Vec<&str> = resolved
            .fallbacks
            .iter()
            .map(|fallback| fallback.setting.as_str())
            .collect();
        assert!(settings.contains(&"min_consecutive_samples"));
        assert!(settings.contains(&"min_baseline_samples"));
    }

    #[test]
    fn one_unusable_value_does_not_discard_the_usable_ones() {
        // A single bad field must not cost the operator their whole override block.
        let mut config = TuningConfig::default();
        config.thresholds.insert(
            "api".to_string(),
            ProcessThresholds {
                departure_z: Some(-1.0),
                min_consecutive_samples: Some(7),
                min_baseline_samples: Some(60),
            },
        );

        let resolved = config.resolve_thresholds("api");
        assert_eq!(
            resolved.departure_z, DEFAULT_DEPARTURE_Z,
            "the bad one falls back"
        );
        assert_eq!(
            resolved.min_consecutive_samples, 7,
            "the good ones are kept"
        );
        assert_eq!(resolved.min_baseline_samples, 60);
        assert_eq!(resolved.fallbacks.len(), 1);
    }

    // ---- engine self-reporting (11.3, and 10.x readiness) ----

    #[test]
    fn suppression_counters_attribute_each_block_to_its_scope() {
        // An engine that cannot say how much it suppressed cannot be tuned: the operator has no
        // measure of whether a suppression is doing nothing or hiding a flood.
        let mut counters = SuppressionCounters::default();
        counters.record(&Blocked::DetectionDisabled);
        counters.record(&Blocked::DetectorDisabled {
            detector: Detector::Drift,
        });
        counters.record(&Blocked::SuppressedForProcess {
            detector: Detector::LevelDeparture,
            process: "api".to_string(),
            note: None,
        });
        counters.record(&Blocked::SuppressedForProcess {
            detector: Detector::ResourceLeak,
            process: "api".to_string(),
            note: None,
        });

        assert_eq!(counters.blocked_globally, 1);
        assert_eq!(counters.blocked_by_detector, 1);
        assert_eq!(counters.blocked_by_process, 2);
        assert_eq!(counters.total(), 4);
    }

    #[test]
    fn the_absence_of_findings_is_explainable() {
        // The scenario this whole reporting surface exists for: a process with no active findings, and
        // the question of whether that means healthy or unwatched.
        let mut config = TuningConfig::default();
        config.suppress("api", &Detector::ResourceLeak, Some("expected".to_string()));
        config
            .disabled_detectors
            .insert(Detector::Drift.as_wire().to_string());

        let reported = config.suppressions_for("api", &detectors());
        assert_eq!(
            reported.len(),
            2,
            "both the per-detector disable and the per-process suppression are reported"
        );
        let scopes: Vec<&str> = reported.iter().map(Blocked::scope).collect();
        assert!(scopes.contains(&"detector"));
        assert!(scopes.contains(&"process"));

        // And the detector that is neither disabled nor suppressed is absent from the report, so an
        // empty entry cannot be confused with a block.
        assert!(config.gate("api", &Detector::LevelDeparture).is_allowed());
    }
}
