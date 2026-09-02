//! Typical resource usage, derived from a retained sample history.
//!
//! Severity derivation lives on the client side (`web/dashboard.js`): the client
//! receives raw figures and the thresholds below via `/api/config`, and renders
//! the graduated band itself. This module therefore provides the server-side
//! half that the client cannot: a typical value derived from retained
//! measurements, for the figures the daemon serves.
//!
//! # Typical values
//!
//! The median, not the mean, because one startup spike moves a mean and then
//! masks the next real excursion. A typical value is withheld (`Typical::Unavailable`)
//! rather than computed from too few samples, and a non-finite sample is a broken
//! measurement, not a number to average.
//!
//! The window is carried with the value, not assumed: "typical" over ten minutes
//! and over a day are different claims.

use serde::{Deserialize, Serialize};

/// The percentage of its denominator at which a reading becomes critical.
///
/// Documented default served to the client via `/api/config`; enforcement in this
/// codebase acts when a reading *exceeds* its configured limit, so 100% is the
/// point of intervention and critical sits below it with room for one sampling
/// interval.
pub const DEFAULT_CRITICAL_PERCENT: f32 = 90.0;

/// The percentage at which a reading becomes a warning.
///
/// A heuristic, flagged as such: no code path acts at 75%, so unlike the critical
/// boundary it cannot be derived from anything. It exists to give the graduated
/// scale a visible middle. Being wrong about it is safe: severity is display-only
/// and the boundary is configurable.
pub const DEFAULT_WARNING_PERCENT: f32 = 75.0;

/// The margin, in percentage points, a ratio must clear before a band change.
///
/// Metrics refresh on the daemon's 2s maintenance tick, so a figure sitting on a
/// boundary can cross it in both directions several times a minute, and each
/// crossing would repaint. 2 points absorbs the jitter of a genuinely flat figure
/// while a real move of a few points still changes band on the next frame.
pub const DEFAULT_BAND_HYSTERESIS_PERCENT: f32 = 2.0;

/// The smallest number of retained samples from which a typical value is presented.
///
/// The derivation is a median, and a median is only resistant to a single extreme
/// once one sample cannot be the middle one. At n = 5 the median is the 3rd of 5,
/// so one startup spike shifts it by a single rank; at n = 2 it *is* the spike,
/// averaged with one other reading. The floor therefore has to be at least 3, and
/// 5 gives one rank of slack on either side. Being wrong is cheap in one direction
/// only — too low shows a figure that swings, so the error is taken on the side of
/// withholding. This is a display-time floor; nothing acts on it.
pub const MINIMUM_TYPICAL_SAMPLES: usize = 5;

/// A typical value and the window it summarises.
///
/// The window is carried, not assumed: "typical" over ten minutes and over a day
/// are different claims, and an operator comparing a spike against an unlabelled
/// figure cannot tell which they are being shown.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TypicalValue {
    /// Median of the retained samples. The median, not the mean, because one startup spike
    /// moves a mean and then masks the next real excursion.
    pub median: f64,
    /// The period the samples span, stated so the claim is interpretable.
    pub window_secs: u64,
    /// How many samples it was derived from, so the operator can weigh it.
    pub sample_count: usize,
}

/// Why no typical value could be derived. `NotEnoughSamples` is not zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypicalUnavailable {
    /// No history retained at all.
    NoHistory,
    /// Some history, but too little to call anything typical.
    NotEnoughSamples { retained: usize, required: usize },
    /// A sample was not a finite number, so a median over them would not be a measurement.
    NonFiniteSample,
}

/// A typical value, or the reason there is none. Never zero standing in for absent.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Typical {
    Available(TypicalValue),
    Unavailable { reason: TypicalUnavailable },
}

impl Typical {
    pub fn value(self) -> Option<TypicalValue> {
        match self {
            Self::Available(value) => Some(value),
            Self::Unavailable { .. } => None,
        }
    }

    #[cfg(test)]
    pub fn is_unavailable(self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }
}

/// Median of the retained samples over a stated window, or the reason it is withheld.
///
/// Applies unchanged to process and host figures. A reading with no honest
/// ceiling still has a typical value, which is where such a figure gets its
/// meaning.
pub fn typical_from_samples(samples: &[f64], window_secs: u64) -> Typical {
    if samples.is_empty() {
        return Typical::Unavailable {
            reason: TypicalUnavailable::NoHistory,
        };
    }

    if samples.len() < MINIMUM_TYPICAL_SAMPLES {
        return Typical::Unavailable {
            reason: TypicalUnavailable::NotEnoughSamples {
                retained: samples.len(),
                required: MINIMUM_TYPICAL_SAMPLES,
            },
        };
    }

    if samples.iter().any(|sample| !sample.is_finite()) {
        return Typical::Unavailable {
            reason: TypicalUnavailable::NonFiniteSample,
        };
    }

    // Copy to sort: the caller's retained window is theirs, and reordering a history buffer
    // as a side effect of displaying it would be a surprising thing for a display function
    // to do.
    let mut ordered = samples.to_vec();
    // `total_cmp` is a total order over all f64, so no `Option` to handle here — a
    // `partial_cmp` fallback would need a branch that non-finite values (rejected
    // above) make unreachable anyway. For finite inputs the computed median is
    // identical: -0.0 and +0.0 sort adjacently and arithmetically coincide.
    ordered.sort_by(|a, b| a.total_cmp(b));

    let middle = ordered.len() / 2;
    let median = if ordered.len().is_multiple_of(2) {
        (ordered[middle - 1] + ordered[middle]) / 2.0
    } else {
        ordered[middle]
    };

    Typical::Available(TypicalValue {
        median,
        window_secs,
        sample_count: ordered.len(),
    })
}

#[cfg(test)]
/// Test code casts are bounded and exact.
mod tests {
    use super::*;

    #[test]
    fn a_single_spike_does_not_move_the_typical_value() {
        let steady = [100.0, 102.0, 98.0, 101.0, 99.0, 100.0, 101.0];
        let mut spiked = steady.to_vec();
        spiked.push(9000.0);

        let before = typical_from_samples(&steady, 600)
            .value()
            .expect("available");
        let after = typical_from_samples(&spiked, 600)
            .value()
            .expect("available");
        assert!(
            (after.median - before.median).abs() <= 1.0,
            "median moved from {} to {}",
            before.median,
            after.median
        );

        // A mean over the same samples moves by an order of magnitude: the reason for
        // the median is that this comparison holds.
        let mean_before: f64 =
            steady.iter().sum::<f64>() / crate::numeric::usize_to_f64(steady.len());
        let mean_after: f64 =
            spiked.iter().sum::<f64>() / crate::numeric::usize_to_f64(spiked.len());
        assert!(mean_after - mean_before > 1000.0);
    }

    #[test]
    fn a_sustained_change_moves_the_typical_value() {
        let low = [100.0; 9];
        let settled = [
            500.0, 505.0, 495.0, 500.0, 502.0, 498.0, 501.0, 499.0, 500.0,
        ];
        let before = typical_from_samples(&low, 600).value().expect("available");
        let after = typical_from_samples(&settled, 600)
            .value()
            .expect("available");
        assert!(after.median > before.median * 4.0);
    }

    #[test]
    fn too_few_samples_are_withheld_rather_than_computed() {
        let result = typical_from_samples(&[100.0, 4000.0, 101.0], 600);
        assert_eq!(
            result,
            Typical::Unavailable {
                reason: TypicalUnavailable::NotEnoughSamples {
                    retained: 3,
                    required: MINIMUM_TYPICAL_SAMPLES
                }
            }
        );
        assert!(result.value().is_none());
    }

    #[test]
    fn absent_history_is_unavailable_rather_than_zero() {
        let result = typical_from_samples(&[], 600);
        assert_eq!(
            result,
            Typical::Unavailable {
                reason: TypicalUnavailable::NoHistory
            }
        );
        // The failure this guards against: reporting 0.0 as the typical value.
        assert!(result.value().is_none());
        assert!(result.is_unavailable());
    }

    #[test]
    fn the_window_and_sample_count_accompany_a_typical_value() {
        let typical = typical_from_samples(&[1.0, 2.0, 3.0, 4.0, 5.0], 900)
            .value()
            .expect("available");
        assert_eq!(typical.window_secs, 900);
        assert_eq!(typical.sample_count, 5);
        assert_eq!(typical.median, 3.0);
    }

    #[test]
    fn an_even_sample_count_averages_the_middle_pair() {
        let typical = typical_from_samples(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 60)
            .value()
            .expect("available");
        assert_eq!(typical.median, 3.5);
    }

    #[test]
    fn a_non_finite_sample_yields_unavailable_not_nan() {
        let result = typical_from_samples(&[1.0, 2.0, f64::NAN, 4.0, 5.0], 600);
        assert_eq!(
            result,
            Typical::Unavailable {
                reason: TypicalUnavailable::NonFiniteSample
            }
        );
    }

    #[test]
    fn deriving_a_typical_value_does_not_reorder_the_callers_samples() {
        let samples = vec![5.0, 1.0, 4.0, 2.0, 3.0];
        let snapshot = samples.clone();
        let _ = typical_from_samples(&samples, 600);
        assert_eq!(samples, snapshot);
    }
}
