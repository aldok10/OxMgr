//! Per-cycle analysis: the seam between retained history and findings.
//!
//! This is the module the rest of `process-intelligence` was waiting on. History, baselines,
//! detectors, the findings registry, the rule engine and the protection gate were all built and
//! tested in isolation; nothing joined them to the daemon's 2-second maintenance tick, so no
//! finding had ever actually been produced by a running daemon.
//!
//! # The tick budget is the hard constraint
//!
//! Analysis shares the maintenance tick with restarts, health checks and watch checks.
//! `design.md` states the constraint as hard: per-cycle cost must not grow with uptime or with
//! retained history. Two things enforce it here:
//!
//! - Every detector is incremental. [`crate::detector_level::LevelDetector::observe`] is O(1) over
//!   six fields; nothing rescans the rings. The trend detector is the exception and is therefore
//!   run on its own slower cadence ([`AnalysisConfig::trend_every`]), because a least-squares fit
//!   is O(window).
//! - A per-cycle deadline. [`Engine::analyse`] checks its elapsed time between processes and
//!   defers the remainder rather than overrunning, recording the deferral. Supervision keeping its
//!   cadence matters more than every process being analysed on every tick.
//!
//! # Nothing here acts
//!
//! The engine produces findings and decisions. [`crate::protection`] is the only thing that can
//! turn a decision into an action, and it is not called from here — the caller does that, so the
//! observe-only default is structural rather than a flag this module has to respect.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use crate::baseline::{Metric, ProcessBaselines};
use crate::detector_incremental::{
    ChangePointConfig, ChangePointDetector, CusumConfig, CusumDetector, EwmaConfig, EwmaDetector,
};
use crate::detector_level::{
    BaselineSummary as LevelBaseline, LevelConfig, LevelDetector, LevelVerdict,
};
use crate::detector_trend::{TrendConfig, TrendSample, analyse_trend, forecast_time_to_threshold};
use crate::metrics_history::{MetricHistoryStore, MetricKind};
use oxmgr_core::findings::{
    Agreement, BaselineSummary, Detector, Evidence, EvidenceWindow, FindingKey, ForecastAssumption,
    Metric as FindingMetric, SampleRef, ThresholdForecast, Transition, TrendFit,
};
use oxmgr_core::findings::{Direction as FindingDirection, FindingRegistry};
use oxmgr_core::protection::{Gate, ProtectionConfig};
use oxmgr_core::rules::{Decision, DecisionLog, ProcessFacts, RuleConfig, decide};
use oxmgr_core::tuning::{SuppressionCounters, TuningConfig};

/// How many maintenance ticks between trend analyses.
///
/// 15 ticks is 30s at the 2s maintenance cadence. The level detector runs every tick because it is
/// O(1); the trend fit walks a bounded window, so it runs rarely. A leak developing over minutes
/// does not need a fresh fit every two seconds, and paying for one would spend the tick budget on
/// the least urgent question.
pub const DEFAULT_TREND_EVERY: u32 = 15;

/// The share of one maintenance cycle analysis may consume before deferring.
///
/// 20ms of a 2000ms tick, i.e. 1%. Chosen against measurement rather than taste: the whole point
/// is that supervision keeps its cadence, and the observed cost of analysing one process is tens
/// of microseconds, so this bound is roughly 300 processes before it can even be reached.
pub const DEFAULT_BUDGET_MS: u64 = 20;

/// Engine tuning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnalysisConfig {
    /// Ticks between trend analyses.
    pub trend_every: u32,
    /// Per-cycle wall-clock budget.
    pub budget: Duration,
}

impl Default for AnalysisConfig {
    fn default() -> Self {
        Self {
            trend_every: DEFAULT_TREND_EVERY,
            budget: Duration::from_millis(DEFAULT_BUDGET_MS),
        }
    }
}

/// What one process looked like to the engine on one cycle.
///
/// Passed in rather than read from a manager handle, so the engine takes no lock and can be tested
/// without one. The caller already holds these values — it just sampled them.
#[derive(Debug, Clone, Default)]
pub struct ProcessObservation {
    /// The process name, which is the findings identity.
    pub process: String,
    /// Current metric readings. `None` for a metric not measured this cycle — a stopped process, a
    /// pid that just changed, an interval too short to divide by.
    ///
    /// `None` is not zero, and the distinction survives all the way to the detector: an absent
    /// sample breaks a consecutive run without clearing an active departure, because "the daemon
    /// lost sight of the metric" is not "the metric recovered".
    pub cpu_percent: Option<f64>,
    pub memory_bytes: Option<f64>,
    /// The process's configuration fingerprint.
    ///
    /// Needed because a baseline set is created on first observation, and it must be labelled with
    /// the configuration it is learning under — that label is what lets a later restart keep the
    /// baseline while a reconfiguration discards it.
    pub config_fingerprint: String,
    /// Facts the rule engine may read.
    pub facts: ProcessFacts,
    /// The process's configured memory limit in bytes, when it has one.
    ///
    /// Only used as the forecast target: `forecast_time_to_threshold` needs a level to predict
    /// arrival AT, and a leak with no limit to hit has nothing to forecast against. `None` is
    /// therefore not a degraded case — it means "no limit configured", and the finding is still
    /// raised with its fit, just without a time-to-threshold.
    pub memory_limit_bytes: Option<f64>,
}

/// One cycle's outcome, for reporting and for the caller to act on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CycleReport {
    /// Wall-clock the cycle took.
    pub elapsed: Duration,
    /// Processes analysed.
    pub analysed: usize,
    /// Processes skipped because the budget ran out. Non-zero is the deferral being *recorded*
    /// rather than silent, which is what `spec.md` requires of it.
    pub deferred: usize,
    /// Findings raised, re-raised, held and cleared this cycle.
    pub raised: usize,
    pub re_raised: usize,
    pub held: usize,
    pub cleared: usize,
    /// Findings blocked by tuning, attributed to the scope that blocked them.
    pub suppressed: SuppressionCounters,
    /// Decisions reached. Recorded even when nothing acts, which is what lets an operator see what
    /// protection mode WOULD do before enabling it.
    pub decisions: Vec<Decision>,
    /// Findings whose state changed this cycle, for the caller to publish.
    ///
    /// Returned rather than emitted from here, because this module holds no event sender: an engine
    /// that could publish would need the bus, and then a test would need one too. The caller already
    /// has `emit`, so the split costs nothing and keeps analysis a pure function of its inputs.
    ///
    /// Only raises, re-raises and clears appear. A held finding is deliberately absent — that is the
    /// whole reason a condition does not produce an event every two seconds.
    pub events: Vec<FindingEvent>,
}

/// A finding transition worth publishing.
#[derive(Debug, Clone, PartialEq)]
pub struct FindingEvent {
    pub process: String,
    /// `"{key}#{occurrence}"`, stable for the life of an episode so a consumer can pair a clear
    /// with the raise that opened it.
    pub id: String,
    pub key: String,
    pub detector: String,
    pub metric: String,
    pub confidence: f64,
    pub occurrence: u32,
    pub summary: Option<String>,
    /// Whether this is a raise (or re-raise) rather than a clear.
    pub raised: bool,
}

impl CycleReport {
    /// Whether the cycle completed every process.
    #[cfg(test)]
    pub fn complete(&self) -> bool {
        self.deferred == 0
    }
}

/// Per-process, per-metric detector state.
///
/// Keyed by `(process, metric)`. Held across cycles because every detector here is incremental:
/// the consecutive-run counter and the active-departure flag are the whole reason a departure
/// raises once and then holds rather than re-firing every tick.
type DetectorKey = (String, Metric);

/// One process's full set of detectors: level, EWMA drift, CUSUM shift, change point.
///
/// Created once per `(process, metric)` and held for the process's life; the cost is four small
/// structs of scalar state, which `design.md` accepts explicitly ("the detector is itself
/// observable" — a finding must be able to say which detector produced it).
#[derive(Debug, Clone)]
struct DetectorSet {
    level: LevelDetector,
    ewma: EwmaDetector,
    cusum: CusumDetector,
    change_point: ChangePointDetector,
}

impl DetectorSet {
    fn new(level: LevelConfig) -> Self {
        Self {
            level: LevelDetector::new(level).0,
            ewma: EwmaDetector::new(),
            cusum: CusumDetector::new(),
            change_point: ChangePointDetector::new(ChangePointConfig::default().window_size),
        }
    }
}

/// The analysis engine.
pub struct Engine {
    config: AnalysisConfig,
    /// `BTreeMap`, not `HashMap`: `baseline::Metric` derives `Ord` but not `Hash`, and adding a
    /// `Hash` derive to a public enum in another module to satisfy a container choice here is the
    /// wrong direction. Ordered iteration is also free determinism, which this change asks for
    /// everywhere else.
    detectors: BTreeMap<DetectorKey, DetectorSet>,
    registry: FindingRegistry,
    decisions: DecisionLog,
    /// The action gate between a rule's decision and a real action. Observe-only by default:
    /// `ProtectionConfig::default()` admits nothing, and nothing in this daemon ever calls
    /// [`Gate::confirm`], so a permitted action is still never executed. The gate is wired for
    /// its record, not for its power.
    gate: Gate,
    protection: ProtectionConfig,
    tuning: TuningConfig,
    rules: RuleConfig,
    counters: SuppressionCounters,
    /// The last decision outcome recorded per process, so an unchanged decision is not re-recorded
    /// every tick. See the change check in `analyse_one` for the measured reason this exists.
    last_decision: HashMap<String, DecisionSignature>,
    /// Dependencies implicated in a failure, for the next cycle's process facts.
    implicated_dependencies: HashMap<String, Vec<String>>,
    /// Ticks since the last trend pass.
    tick: u32,
    /// Where the previous cycle stopped, so a deferred remainder is analysed FIRST next cycle
    /// rather than being starved. Without this, a daemon permanently over budget would analyse
    /// only its first N processes for ever and never notice the rest.
    resume_at: usize,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new(AnalysisConfig::default())
    }
}

impl Engine {
    pub fn new(config: AnalysisConfig) -> Self {
        Self {
            config,
            detectors: BTreeMap::new(),
            registry: FindingRegistry::new(),
            decisions: DecisionLog::default(),
            gate: Gate::default(),
            protection: ProtectionConfig::default(),
            tuning: TuningConfig::default(),
            rules: RuleConfig::default(),
            counters: SuppressionCounters::default(),
            last_decision: HashMap::new(),
            implicated_dependencies: HashMap::new(),
            tick: 0,
            resume_at: 0,
        }
    }

    /// The findings registry, for the endpoints and the CLI.
    pub fn registry(&self) -> &FindingRegistry {
        &self.registry
    }

    /// The decision log, for the endpoints and the CLI.
    #[cfg(test)]
    pub fn decisions(&self) -> &DecisionLog {
        &self.decisions
    }

    /// Live tuning, so a runtime change takes effect on the next cycle with no restart.
    #[cfg(test)]
    pub fn tuning_mut(&mut self) -> &mut TuningConfig {
        &mut self.tuning
    }

    /// Cumulative suppression counters, for the engine's self-report.
    #[cfg(test)]
    pub fn counters(&self) -> SuppressionCounters {
        self.counters
    }

    /// Drops everything held for a process. Called on delete, for the same reason metric history
    /// and baselines are dropped: a later process reusing the name must not inherit a stranger's
    /// findings.
    #[cfg(test)]
    pub fn forget(&mut self, process: &str) {
        self.detectors.retain(|(name, _), _| name != process);
        self.registry.clear_process(process);
        self.decisions.forget(process);
        self.gate.forget(process);
        self.tuning.forget(process);
        // Dropped too, or a later process reusing the name would inherit a stranger's signature and
        // have its first real decision silently suppressed as "unchanged".
        self.last_decision.remove(process);
    }
}

impl Engine {
    /// Analyses one maintenance cycle.
    ///
    /// Returns what happened, including any deferral. The caller passes the observations it just
    /// sampled plus the live baselines, so this function takes no lock and reads no clock beyond
    /// `at_unix` and its own elapsed-time measurement.
    ///
    /// # Ordering, and why the resume point exists
    ///
    /// Processes are analysed from [`Self::resume_at`], wrapping, so a cycle that runs out of
    /// budget continues where it stopped rather than restarting at the first process. A fixed
    /// start would mean a permanently over-budget daemon analysing only its first N processes for
    /// ever while silently never looking at the rest — the failure that makes a deferral
    /// dangerous rather than merely imperfect.
    pub fn analyse(
        &mut self,
        observations: &[ProcessObservation],
        baselines: &mut HashMap<String, ProcessBaselines>,
        history: &MetricHistoryStore,
        at_unix: u64,
    ) -> CycleReport {
        let started = Instant::now();
        let mut report = CycleReport::default();
        // Scheduling counter, used only for `is_multiple_of(trend_every)`. Wrapping is
        // deliberate: after 2^32 ticks (~136 years at a 2s cadence) the sequence restarts,
        // which merely repeats the same schedule; no value derived from `tick` ever
        // participates in a verdict.
        self.tick = self.tick.wrapping_add(1);
        let run_trend =
            self.config.trend_every > 0 && self.tick.is_multiple_of(self.config.trend_every);

        if observations.is_empty() {
            report.elapsed = started.elapsed();
            self.resume_at = 0;
            return report;
        }

        let total = observations.len();
        let start = self.resume_at % total;

        for offset in 0..total {
            // The budget is checked BETWEEN processes, never mid-process: abandoning one halfway
            // would leave its detector state advanced and its finding unwritten, which is worse
            // than not analysing it at all.
            if offset > 0 && started.elapsed() >= self.config.budget {
                report.deferred = total - offset;
                self.resume_at = (start + offset) % total;
                break;
            }

            let observation = &observations[(start + offset) % total];
            self.analyse_one(
                observation,
                baselines,
                history,
                at_unix,
                run_trend,
                &mut report,
            );
            report.analysed += 1;
        }

        if report.deferred == 0 {
            self.resume_at = 0;
        }

        report.suppressed = self.counters;
        report.elapsed = started.elapsed();
        report
    }

    /// One process: every metric through its detector, then one decision.
    fn analyse_one(
        &mut self,
        observation: &ProcessObservation,
        baselines: &mut HashMap<String, ProcessBaselines>,
        history: &MetricHistoryStore,
        at_unix: u64,
        run_trend: bool,
        report: &mut CycleReport,
    ) {
        let process = observation.process.as_str();

        for (metric, value) in [
            (Metric::CpuPercent, observation.cpu_percent),
            (Metric::MemoryBytes, observation.memory_bytes),
        ] {
            // Tuning is consulted BEFORE the baseline is fed, so a suppressed detector costs one
            // map lookup rather than an observation plus a discarded verdict.
            let gate = self.tuning.gate(process, &detector_for(metric));
            if let Some(blocked) = gate.blocked() {
                self.counters.record(blocked);
                continue;
            }

            // The baseline is fed even when the detector will not fire: warm-up only ends by
            // observing, so skipping this on a warming process would keep it warming for ever.
            let Some(summary) = self.observe_baseline(
                process,
                &observation.config_fingerprint,
                metric,
                value,
                baselines,
            ) else {
                continue;
            };

            // Resolved BEFORE the entry call: `or_insert_with` holds a mutable borrow of
            // `self.detectors`, so reading configs inside the closure would need a
            // second, immutable borrow of `self`. Hoisting them costs nothing on the
            // path where a detector set already exists for this process and metric.
            let level_config = self.level_config(process);
            let ewma_config = self.ewma_config();
            let cusum_config = self.cusum_config();
            let cp_config = self.change_point_config();

            // All detector observations happen inside this block so the borrow of
            // `self.detectors` (through `set`) ends before the registry writes below,
            // which need `&mut self` again. The incremental verdicts are converted to
            // `Option<IncrementalVerdict>` right here: Warming and Normal verdicts are
            // silently discarded, preserving the anti-spike suppression across all
            // detector kinds.
            let (verdict, incremental) = {
                let set = self
                    .detectors
                    .entry((process.to_string(), metric))
                    .or_insert_with(|| DetectorSet::new(level_config));
                (
                    set.level.observe(value, &summary),
                    // Only a measured value feeds the incremental detectors; an absent
                    // sample (`None`) is neither a zero nor evidence of health.
                    value.map(|v| {
                        (
                            set.ewma.observe(v, &ewma_config).into(),
                            set.cusum.observe(v, summary.centre, &cusum_config).into(),
                            set.change_point.observe(v, &cp_config).into(),
                        )
                    }),
                )
            };

            // Check whether the level verdict leaves room for incremental detectors
            // BEFORE applying it, because `apply_verdict` takes `verdict` by value.
            let level_was_normal = matches!(&verdict, LevelVerdict::Normal { .. });

            self.apply_verdict(
                Measured {
                    process,
                    metric,
                    summary,
                    at_unix,
                },
                verdict,
                history,
                report,
            );

            // Incremental detectors (tasks 5b.1-5b.3): EWMA drift, CUSUM sustained shift,
            // change point. Applied only when the level detector says Normal — the "one
            // real event, one finding" contract (task 5b.5). Any other verdict blocks
            // them: Departed/Holding mean the level detector already owns this episode,
            // Building means a departure is forming (firing now would stack findings on
            // the same event), Cleared means the episode just ended, and Undetermined
            // means the baseline is not trustworthy as a CUSUM target.
            if level_was_normal && let Some((ewma_inc, cusum_inc, cp_inc)) = incremental {
                for (detector, verdict) in [
                    (Detector::Drift, ewma_inc),
                    (Detector::SustainedShift, cusum_inc),
                    (Detector::ChangePoint, cp_inc),
                ] {
                    if let Some(v) = verdict {
                        self.apply_incremental_verdict(
                            process, metric, at_unix, report, detector, v,
                        );
                    }
                }
            }
        }

        // Trend analysis, on its own slower cadence.
        //
        // Placed after the level detectors and BEFORE the decision, so a leak raised on this cycle
        // is visible to `decide` on the same cycle rather than one tick late.
        //
        // Memory only. A CPU percentage is bounded at 100 per core and oscillates by nature, so a
        // rising least-squares fit over it describes load arriving, not a resource being leaked —
        // running the leak detector on it would produce a finding on every process that gets busy.
        // The spec's leak shape is a monotonic climb in a quantity nothing returns, which is what
        // memory is and CPU is not.
        if run_trend {
            self.analyse_trend_for(process, observation, history, at_unix, report);
        }

        // One decision per process per cycle, from whatever is active now. Taken after every
        // metric so a decision sees the whole finding set rather than a partial one.
        let active = self.registry.active_for(process);
        let decision = decide(process, &active, &observation.facts, &self.rules, at_unix);

        // The action gate: with the default observe-only config it withholds every
        // action and records the refusal. Nothing here ever calls `confirm`, so even
        // an action the gate would permit is never executed.
        let _ = self.gate.admit(&decision, &self.protection, at_unix);

        // Recorded only when the OUTCOME changes, not on every tick it holds.
        //
        // Measured on a live daemon: two departing processes filled 52 of the 256 log slots in
        // ninety seconds with just 2 distinct decisions — x28 and x24 of the same entry. At that
        // rate the log holds under nine minutes of history and evicts anything worth reading, which
        // makes "what did protection mode decide this morning" unanswerable.
        //
        // This is the same rule the findings registry already applies with `Held`: an ongoing
        // condition is not news. The timestamp of the first occurrence is kept rather than
        // refreshed, so the record answers "since when" rather than "as of the last tick".
        if decision.rule.is_some() {
            let signature = DecisionSignature::of(&decision);
            let changed = self
                .last_decision
                .get(process)
                .is_none_or(|previous| previous != &signature);
            if changed {
                self.last_decision.insert(process.to_string(), signature);
                self.decisions.record(decision.clone());
                report.decisions.push(decision);
            }
        } else {
            // No rule matched, so the process has nothing to say. Forgetting its last signature is
            // what makes a RE-occurrence record again later: without this, a condition that cleared
            // and came back would be silently treated as unchanged.
            self.last_decision.remove(process);
        }
    }

    /// Feeds one value into the process's baseline and returns the summary to compare against.
    ///
    /// `None` when there is no baseline to compare with yet, which is distinct from a baseline that
    /// exists and is warming — the detector needs the summary in the second case so it can report
    /// `BaselineWarming` rather than silence.
    fn observe_baseline(
        &self,
        process: &str,
        fingerprint: &str,
        metric: Metric,
        value: Option<f64>,
        baselines: &mut HashMap<String, ProcessBaselines>,
    ) -> Option<LevelBaseline> {
        // Created on first observation, NOT merely looked up.
        //
        // This was a real bug, found by watching a live daemon sit at "still building baselines"
        // for 75 seconds when 30 samples at a 2s tick should take 60. The lookup was
        // `baselines.get_mut(process)?`, and baselines were only ever inserted by
        // `restore_baselines` reading the persisted store — so a process that had never been
        // analysed before had no entry, the `?` returned early, and it could never accumulate the
        // samples that would create one. Warm-up was unreachable for every new process, and the
        // symptom was silence rather than an error.
        //
        // Labelled with the current fingerprint, so a later restart keeps the baseline while a
        // reconfiguration discards it — the same rule `ProcessBaselines::restore` applies.
        let set = baselines
            .entry(process.to_string())
            .or_insert_with(|| ProcessBaselines::new(fingerprint));
        if let Some(value) = value {
            set.observe(metric, value);
        }
        let snapshot = set.get(metric)?.snapshot();
        // `BaselineReadiness` carries the count in both states, which is what makes "warming, 12
        // of 30" answerable — so the count is read from the variant rather than from a separate
        // accessor. Saturating to u32 because the detector's threshold is a u32 and a baseline
        // cannot have observed more than 4 billion samples in any real deployment.
        let samples = match snapshot.readiness {
            crate::baseline::BaselineReadiness::Warming { samples, .. }
            | crate::baseline::BaselineReadiness::Ready { samples } => {
                u32::try_from(samples).unwrap_or(u32::MAX)
            }
        };
        // Centre and spread are withheld while warming, deliberately, so an unestablished baseline
        // cannot be mistaken for one centred at zero. The summary is still returned with its real
        // sample count, so the detector reports `BaselineWarming` rather than treating the metric
        // as unmeasured — those are different absences.
        Some(LevelBaseline::new(
            snapshot.centre.unwrap_or(0.0),
            snapshot.spread.unwrap_or(snapshot.spread_floor),
            samples,
        ))
    }

    /// Turns a verdict into a registry transition.
    ///
    /// The four values that identify *what was measured* travel together in [`Measured`] rather
    /// than as loose parameters: they are always passed as a set, and threading them individually
    /// made the signature long enough that the compiler was right to complain.
    fn apply_verdict(
        &mut self,
        measured: Measured<'_>,
        verdict: LevelVerdict,
        history: &MetricHistoryStore,
        report: &mut CycleReport,
    ) {
        let Measured {
            process,
            metric,
            summary,
            at_unix,
        } = measured;
        match verdict {
            LevelVerdict::Departed { evidence } => {
                let key =
                    FindingKey::new(process, Detector::LevelDeparture, finding_metric(metric))
                        .with_variant(direction_wire(evidence.direction));
                let Some(built) =
                    self.evidence_for(process, metric, &evidence, &summary, history, at_unix)
                else {
                    return;
                };
                let key_string = key.as_string();
                match self.registry.observe(key.clone(), at_unix, built) {
                    Ok(Transition::Raised { id }) => {
                        report.raised += 1;
                        self.push_event(report, process, &key, key_string, id, true);
                    }
                    Ok(Transition::ReRaised { id, .. }) => {
                        report.re_raised += 1;
                        self.push_event(report, process, &key, key_string, id, true);
                    }
                    // Held emits nothing. That is the point of the identity model: an ongoing
                    // condition must not produce an event every two seconds, or an operator filters
                    // the channel and loses the real ones with it.
                    Ok(Transition::Held { .. }) => report.held += 1,
                    Ok(Transition::Cleared { id }) => {
                        report.cleared += 1;
                        self.push_event(report, process, &key, key_string, id, false);
                    }
                    // A rejected observation leaves the held finding exactly as it was. Swallowed
                    // rather than propagated: a malformed evidence payload is a bug in this
                    // module, not an operational condition the daemon should fail a tick over —
                    // but a bug must be visible, so the rejection is logged at debug.
                    Err(err) => {
                        tracing::debug!(
                            "level finding observation rejected for {} ({}): {err}",
                            key.process,
                            key_string
                        );
                    }
                }
            }
            LevelVerdict::Holding { .. } => {
                // Deliberately no registry write. `Holding` is the detector saying "already
                // raised, still true", and re-observing would refresh evidence for no benefit —
                // the reason a finding does not reappear with a new id every two seconds.
                report.held += 1;
            }
            LevelVerdict::Cleared { .. } => {
                // Both directions are attempted because the detector reports "the value returned
                // under the clear threshold" without saying which side it had departed on, and
                // clearing the wrong one is a no-op. Only the one that was actually active returns
                // `Some`, so exactly one event is published.
                for direction in ["above", "below"] {
                    let key =
                        FindingKey::new(process, Detector::LevelDeparture, finding_metric(metric))
                            .with_variant(direction);
                    let key_string = key.as_string();
                    if let Some(Transition::Cleared { id }) = self.registry.clear(&key, at_unix) {
                        report.cleared += 1;
                        self.push_event(report, process, &key, key_string, id, false);
                    }
                }
            }
            // Normal, Building and Undetermined all write nothing. Building in particular is the
            // anti-spike suppression working: a single excursion is not a finding.
            LevelVerdict::Normal { .. }
            | LevelVerdict::Building { .. }
            | LevelVerdict::Undetermined { .. } => {}
        }
    }

    /// Builds findings-layer evidence from the detector's own evidence plus real samples.
    ///
    /// Returns `None` when no samples can be named. Evidence without samples is refused by
    /// `Finding` construction, so producing it here would only move the failure later.
    fn evidence_for(
        &self,
        process: &str,
        metric: Metric,
        level: &crate::detector_level::LevelEvidence,
        summary: &LevelBaseline,
        history: &MetricHistoryStore,
        at_unix: u64,
    ) -> Option<Evidence> {
        let kind = history_kind(metric);
        // The samples the finding rests on, taken from the retained history rather than
        // synthesised: "evidence references the underlying samples" is only true if they are the
        // samples that were actually recorded.
        let window_secs = 60;
        let to_ms = at_unix.saturating_mul(1000);
        let from_ms = to_ms.saturating_sub(window_secs * 1000);
        let series = history
            .history(process)
            .and_then(|h| h.query(kind, from_ms, to_ms).ok());

        // `SeriesPoint` is an enum, not a struct: a downsampled window returns summaries rather
        // than samples, and the type refuses to let one be read as the other. A summary contributes
        // its mean at its window end — labelled by when the aggregate ENDS rather than starts, so a
        // reader plotting these against time does not shift an hour of history backwards.
        let mut samples: Vec<SampleRef> = series
            .as_ref()
            .map(|s| {
                s.points
                    .iter()
                    .rev()
                    .take(6)
                    .rev()
                    .map(|point| match point {
                        crate::metrics_history::SeriesPoint::Sample { at_ms, value } => {
                            SampleRef::new(at_ms / 1000, *value)
                        }
                        crate::metrics_history::SeriesPoint::Summary { end_ms, mean, .. } => {
                            SampleRef::new(end_ms / 1000, *mean)
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        if samples.is_empty() {
            // The observed value is itself a sample, and it is the one the detector fired on. A
            // finding backed by that alone is thinner than one backed by six, but it is honest and
            // checkable — whereas withholding the finding would lose a real departure because the
            // history query happened to return nothing.
            samples.push(SampleRef::new(at_unix, level.observed));
        }

        Some(Evidence {
            window: EvidenceWindow {
                start_unix: samples.first().map(|s| s.at_unix).unwrap_or(at_unix),
                end_unix: at_unix,
                // SAFETY: samples come from history ring buffers with fixed, bounded capacity;
                // u32 covers all plausible counts. Saturating cast is safe.
                sample_count: u32::try_from(samples.len()).unwrap_or(u32::MAX),
            },
            observed: level.observed,
            expected: level.baseline.centre,
            statistic: level.z,
            threshold: level.threshold_z,
            direction: Some(match level.direction {
                crate::detector_level::Direction::Above => FindingDirection::Above,
                crate::detector_level::Direction::Below => FindingDirection::Below,
            }),
            agreement: Some(Agreement {
                consecutive_samples: level.consecutive_samples,
                required_samples: level.consecutive_samples.max(1),
            }),
            baseline: Some(BaselineSummary {
                center: summary.centre,
                spread: level.effective_spread,
                sample_count: summary.sample_count,
                min_samples: self.level_config(process).min_baseline_samples,
            }),
            trend: None,
            forecast: None,
            samples,
            extra: Default::default(),
        })
    }

    /// The level config for a process, with any per-process threshold overrides applied.
    fn level_config(&self, process: &str) -> LevelConfig {
        let resolved = self.tuning.resolve_thresholds(process);
        LevelConfig {
            departure_z: resolved.departure_z,
            min_consecutive_samples: resolved.min_consecutive_samples,
            min_baseline_samples: resolved.min_baseline_samples,
            ..LevelConfig::default()
        }
    }

    fn ewma_config(&self) -> EwmaConfig {
        EwmaConfig::default()
    }

    fn cusum_config(&self) -> CusumConfig {
        CusumConfig::default()
    }

    fn change_point_config(&self) -> ChangePointConfig {
        ChangePointConfig::default()
    }

    /// Applies one incremental-detector verdict (EWMA / CUSUM / change point) to the
    /// finding registry, mirroring `apply_verdict`'s identity model: a finding keyed by
    /// process + detector + metric + direction, raised once and held until cleared.
    ///
    /// Warming and Normal verdicts write nothing, exactly like the level detector's
    /// Building/Normal — the anti-spike suppression stays intact across detector kinds.
    fn apply_incremental_verdict(
        &mut self,
        process: &str,
        metric: Metric,
        at_unix: u64,
        report: &mut CycleReport,
        detector: Detector,
        verdict: crate::detector_incremental::IncrementalVerdict,
    ) {
        let (evidence, variant) = match verdict {
            crate::detector_incremental::IncrementalVerdict::EwmaDrift(finding) => {
                let (direction, variant) = if finding.deviation >= 0.0 {
                    (Some(FindingDirection::Above), "above")
                } else {
                    (Some(FindingDirection::Below), "below")
                };
                (
                    Evidence {
                        window: EvidenceWindow {
                            start_unix: at_unix,
                            end_unix: at_unix,
                            sample_count: 1,
                        },
                        observed: finding.sample_value,
                        expected: finding.ewma_mean,
                        statistic: finding.deviation.abs(),
                        threshold: self.ewma_config().threshold,
                        direction,
                        agreement: Some(Agreement {
                            consecutive_samples: finding.consecutive,
                            required_samples: self.ewma_config().min_consecutive,
                        }),
                        baseline: None,
                        trend: None,
                        forecast: None,
                        samples: vec![SampleRef::new(at_unix, finding.sample_value)],
                        extra: Default::default(),
                    },
                    variant,
                )
            }
            crate::detector_incremental::IncrementalVerdict::CusumShift(finding) => {
                let variant = if finding.is_positive {
                    "above"
                } else {
                    "below"
                };
                (
                    Evidence {
                        window: EvidenceWindow {
                            start_unix: at_unix,
                            end_unix: at_unix,
                            sample_count: 1,
                        },
                        observed: finding.sample_value,
                        expected: finding.sample_value - finding.deviation,
                        statistic: finding.accumulator,
                        threshold: finding.threshold,
                        direction: if finding.is_positive {
                            Some(FindingDirection::Above)
                        } else {
                            Some(FindingDirection::Below)
                        },
                        agreement: None,
                        baseline: None,
                        trend: None,
                        forecast: None,
                        samples: vec![SampleRef::new(at_unix, finding.sample_value)],
                        extra: Default::default(),
                    },
                    variant,
                )
            }
            crate::detector_incremental::IncrementalVerdict::ChangePoint(finding) => (
                Evidence {
                    window: EvidenceWindow {
                        start_unix: at_unix,
                        end_unix: at_unix,
                        sample_count: 1,
                    },
                    observed: finding.sample_value,
                    expected: finding.prior_mean,
                    statistic: finding.effect_size,
                    threshold: self.change_point_config().effect_size_threshold,
                    direction: None,
                    agreement: None,
                    baseline: None,
                    trend: None,
                    forecast: None,
                    samples: vec![SampleRef::new(at_unix, finding.sample_value)],
                    extra: Default::default(),
                },
                "regime",
            ),
        };

        let key = FindingKey::new(process, detector, finding_metric(metric)).with_variant(variant);
        let key_string = key.as_string();
        match self.registry.observe(key.clone(), at_unix, evidence) {
            Ok(Transition::Raised { id }) => {
                report.raised += 1;
                self.push_event(report, process, &key, key_string, id, true);
            }
            Ok(Transition::ReRaised { id, .. }) => {
                report.re_raised += 1;
                self.push_event(report, process, &key, key_string, id, true);
            }
            Ok(Transition::Held { .. }) => report.held += 1,
            Ok(Transition::Cleared { id }) => {
                report.cleared += 1;
                self.push_event(report, process, &key, key_string, id, false);
            }
            // Same rule as the level path above: rejection is a module bug, not an
            // operational condition — but it is logged so the bug cannot hide.
            Err(err) => {
                tracing::debug!(
                    "trend finding observation rejected for {} ({}): {err}",
                    key.process,
                    key_string
                );
            }
        }
    }

    /// Dependencies implicated in a correlated failure, for this cycle's process facts.
    ///
    /// Reflects the previous pattern pass: implications are gathered when a dependency
    /// correlation is detected and consumed when the next cycle's observations are
    /// built. One tick behind by construction, and self-clearing — each pass replaces the
    /// map, so a correlation that stops appearing stops withholding.
    pub fn implicated_dependencies_for(&self, process: &str) -> Vec<String> {
        self.implicated_dependencies
            .get(process)
            .cloned()
            .unwrap_or_default()
    }

    /// Runs the failure-pattern detectors over the retained lifecycle events, folding their
    /// findings into the registry, and returns the transitions that changed state.
    ///
    /// The pattern detectors are stateless by design: recomputation is how a finding clears,
    /// so this method re-runs detection every cycle and lets the registry's raise / hold /
    /// clear lifecycle decide what changed. Only transitions that changed state (raised,
    /// re-raised, cleared) are returned, so the caller can emit exactly the events that
    /// happened — `Held` is deliberately silent.
    pub fn analyse_patterns(
        &mut self,
        events: &[crate::failure_patterns::FailureEvent],
        now: u64,
        retain_secs: u64,
        config: &crate::failure_patterns::PatternConfig,
    ) -> Vec<FindingEvent> {
        use crate::failure_patterns::{self, Detector as PatternDetector, Pattern, PatternFinding};

        const MIN_CONFIDENCE: f64 = 0.5;

        let report = failure_patterns::analyse(events, now, retain_secs, config);

        // The key a finding registers under. Real processes use their name; storms are group
        // findings scoped to a namespace with no owning process, so they register under a
        // synthetic scope. `:` cannot collide with a process name — `validate_process_name`
        // allows only alphanumerics, `_` and `-`.
        fn finding_key(finding: &PatternFinding) -> Option<FindingKey> {
            let detector = match finding.evidence.detector {
                PatternDetector::CrashLoop => Detector::CrashLoop,
                PatternDetector::RestartAcceleration => Detector::RestartAcceleration,
                PatternDetector::RepeatedExit => Detector::RepeatedExit,
                PatternDetector::RestartStorm => Detector::RestartStorm,
                PatternDetector::DependencyCorrelation => Detector::DependencyCorrelation,
            };
            let scope = match &finding.pattern {
                Pattern::RestartStorm { namespace, .. } => format!("namespace:{namespace}"),
                _ => finding.process.clone()?,
            };
            Some(FindingKey::new(
                scope,
                detector,
                oxmgr_core::findings::Metric::RestartRate,
            ))
        }

        // Populate implications for the next cycle's facts. Replaced, not accumulated: the
        // recomputation-clears rule applies here too, so a correlation that stops appearing
        // stops withholding a tick later.
        self.implicated_dependencies.clear();
        for finding in &report.findings {
            if let Pattern::DependencyCorrelation { dependency, .. } = &finding.pattern
                && let Some(process) = &finding.process
            {
                let deps = self
                    .implicated_dependencies
                    .entry(process.clone())
                    .or_default();
                if !deps.contains(dependency) {
                    deps.push(dependency.clone());
                }
            }
        }

        // First pass: report the conditions still seen, in detector order.
        let mut holding: BTreeMap<String, std::collections::BTreeSet<FindingKey>> = BTreeMap::new();
        // Upper bound: at most one event per finding in the report.
        let mut events = Vec::with_capacity(report.findings.len());

        for finding in &report.findings {
            let Some(key) = finding_key(finding) else {
                continue;
            };
            holding
                .entry(key.process.clone())
                .or_default()
                .insert(key.clone());

            if finding.confidence < MIN_CONFIDENCE {
                continue;
            }

            let evidence = Evidence {
                window: EvidenceWindow {
                    start_unix: finding.evidence.window.start_secs,
                    end_unix: finding.evidence.window.end_secs,
                    sample_count: u32::try_from(finding.evidence.events.len()).unwrap_or(u32::MAX),
                },
                // The number of observed failures is the metric in metric units; the healthy
                // expectation is none. `statistic` is the detector's own confidence score.
                observed: finding
                    .evidence
                    .observed
                    .iter()
                    .fold(0.0, |acc, (_, v)| acc + v),
                expected: 0.0,
                statistic: finding.confidence,
                threshold: MIN_CONFIDENCE,
                direction: None,
                agreement: None,
                baseline: None,
                trend: None,
                forecast: None,
                samples: finding
                    .evidence
                    .events
                    .iter()
                    .map(|ev| SampleRef::new(ev.at_secs, 1.0))
                    .collect(),
                extra: std::collections::BTreeMap::new(),
            };

            match self.registry.observe(key.clone(), now, evidence) {
                Ok(Transition::Raised { id }) => {
                    events.push(FindingEvent {
                        process: key.process.clone(),
                        id,
                        key: key.as_string(),
                        detector: key.detector.as_wire().to_string(),
                        metric: oxmgr_core::findings::Metric::RestartRate
                            .as_wire()
                            .to_string(),
                        confidence: finding.confidence,
                        occurrence: 1,
                        summary: Some(finding.summary.clone()),
                        raised: true,
                    });
                }
                Ok(Transition::ReRaised { id, occurrence }) => {
                    events.push(FindingEvent {
                        process: key.process.clone(),
                        id,
                        key: key.as_string(),
                        detector: key.detector.as_wire().to_string(),
                        metric: oxmgr_core::findings::Metric::RestartRate
                            .as_wire()
                            .to_string(),
                        confidence: finding.confidence,
                        occurrence,
                        summary: Some(finding.summary.clone()),
                        raised: true,
                    });
                }
                Ok(Transition::Held { .. }) | Ok(Transition::Cleared { .. }) => {
                    // Held is silent by design; Cleared cannot come from an observation.
                }
                Err(err) => {
                    tracing::debug!("pattern finding rejected for {}: {err}", key.process);
                }
            }
        }

        // Second pass: clear pattern findings the detectors no longer see. Scoped to the
        // scopes that held a pattern finding this pass, so a level/drift finding for the
        // same process is never touched.
        for (scope, still_holding) in &holding {
            let stale: Vec<FindingKey> = self
                .registry
                .active_for(scope)
                .iter()
                .filter(|finding| {
                    matches!(
                        finding.key.detector,
                        Detector::CrashLoop
                            | Detector::RestartAcceleration
                            | Detector::RepeatedExit
                            | Detector::RestartStorm
                            | Detector::DependencyCorrelation
                    ) && !still_holding.contains(&finding.key)
                })
                .map(|finding| finding.key.clone())
                .collect();
            for key in stale {
                // Capture the finding's own occurrence and last confidence before clearing.
                let (occurrence, confidence) = self
                    .registry
                    .get(&key)
                    .map(|f| (f.occurrence, f.confidence.score))
                    .unwrap_or((1, 0.0));
                if let Some(transition) = self.registry.clear(&key, now) {
                    events.push(FindingEvent {
                        process: scope.clone(),
                        id: transition.id().to_string(),
                        key: key.as_string(),
                        detector: key.detector.as_wire().to_string(),
                        metric: oxmgr_core::findings::Metric::RestartRate
                            .as_wire()
                            .to_string(),
                        confidence,
                        occurrence,
                        summary: None,
                        raised: false,
                    });
                }
            }
        }

        events
    }
}

impl Engine {
    /// Records a publishable transition, reading the confidence and summary from the stored finding.
    ///
    /// Read back from the registry rather than passed in, so the event cannot disagree with what was
    /// stored — a confidence computed twice is a confidence that can drift.
    /// Trend analysis for one process's memory series.
    ///
    /// This is the wiring that was missing. `detector_trend` was fully implemented and covered by
    /// 37 tests, but nothing called it: `analyse_one` took the cadence flag as `_run_trend` and
    /// discarded it, so `Detector::ResourceLeak` could never be produced and the whole module was
    /// dead code hidden by a dead-code lint suppression in `main.rs`. Tasks 6.1–6.6 were marked
    /// complete on the strength of unit tests alone.
    ///
    /// Memory only, and why: see the call site. A CPU percentage is bounded and oscillates, so a
    /// rising fit over it means load arrived, not that anything leaked.
    fn analyse_trend_for(
        &mut self,
        process: &str,
        observation: &ProcessObservation,
        history: &MetricHistoryStore,
        at_unix: u64,
        report: &mut CycleReport,
    ) {
        let gate = self.tuning.gate(process, &Detector::ResourceLeak);
        if let Some(blocked) = gate.blocked() {
            self.counters.record(blocked);
            return;
        }

        let key = FindingKey::new(process, Detector::ResourceLeak, FindingMetric::MemoryBytes);
        let config = TrendConfig::default();

        // The window asked of retention: the detector's own minimum, with headroom so a series
        // sitting exactly at `min_window_secs` is not refused for being one sample short. Bounded
        // rather than "all history", which is what keeps the O(window) fit from growing with uptime.
        // `min_window_secs` is an integer-valued u64 config (detector_trend::TrendConfig);
        // multiplied here in integer space, so no float cast and no precision loss.
        let span_secs = config.min_window_secs.saturating_mul(3);
        let to_ms = at_unix.saturating_mul(1000);
        let from_ms = to_ms.saturating_sub(span_secs.saturating_mul(1000));

        let Some(series) = history
            .history(process)
            .and_then(|h| h.query(MetricKind::Memory, from_ms, to_ms).ok())
        else {
            return;
        };

        // A downsampled Summary contributes its mean at the window END, matching what
        // `evidence_for` already does — labelling an aggregate by its start would shift the series
        // backwards in time and tilt the slope.
        let samples: Vec<TrendSample> = series
            .points
            .iter()
            .map(|point| match point {
                crate::metrics_history::SeriesPoint::Sample { at_ms, value } => {
                    TrendSample::new(*at_ms, *value)
                }
                crate::metrics_history::SeriesPoint::Summary { end_ms, mean, .. } => {
                    TrendSample::new(*end_ms, *mean)
                }
            })
            .collect();

        let analysis = analyse_trend(&samples, &config);

        let Some(leak) = analysis.leak().copied() else {
            // Every refusal clears an active finding: `clears_active_finding` is true for all of
            // them, because a finding whose supporting history has been reset, gapped or shortened
            // out of admissibility is no longer supported. Task 6.4.
            if analysis.clears_active_finding() {
                let key_string = key.as_string();
                if let Some(Transition::Cleared { id }) = self.registry.clear(&key, at_unix) {
                    report.cleared += 1;
                    self.push_event(report, process, &key, key_string, id, false);
                }
            }
            return;
        };

        // The detector's own `TrendFit` is richer than the findings wire form, so it is translated
        // rather than stored: `findings::TrendFit` is what serialises into `/api/findings`.
        let fit = TrendFit {
            slope_per_second: leak.fit.slope_per_sec,
            fit_quality: leak.fit.r_squared.unwrap_or(0.0),
            monotonicity_ratio: Some(leak.fit.monotonic_ratio),
            slope_std_error: Some(leak.fit.slope_std_error),
        };

        // The forecast needs a level to predict arrival at. With no configured memory limit there
        // is nothing to forecast against, so the finding is raised with its fit and no ETA — an
        // absent forecast, not a zero one.
        let forecast = observation.memory_limit_bytes.and_then(|limit| {
            forecast_time_to_threshold(&samples, limit, &config)
                .forecast()
                .map(|f| ThresholdForecast {
                    threshold_value: f.threshold,
                    eta_seconds: f.seconds_to_threshold,
                    interval_low_seconds: f.earliest_secs,
                    interval_high_seconds: f.latest_secs,
                    assumes: ForecastAssumption::CurrentTrendContinues,
                })
        });

        let cited: Vec<SampleRef> = samples
            .iter()
            .rev()
            .take(6)
            .rev()
            .map(|s| SampleRef::new(s.at_unix_ms / 1000, s.value))
            .collect();
        if cited.is_empty() {
            // `Evidence` construction rejects an empty sample list, and a finding whose samples
            // cannot be named is not checkable. Refusing here keeps that guarantee local.
            return;
        }

        let evidence = Evidence {
            window: EvidenceWindow {
                start_unix: leak.fit.window_start_unix_ms / 1000,
                end_unix: leak.fit.window_end_unix_ms / 1000,
                sample_count: u32::try_from(leak.fit.sample_count).unwrap_or(u32::MAX),
            },
            observed: leak.fit.last_value,
            // What a non-leaking process would have shown: the level the fit started from. The
            // distance between this and `observed` is the growth the finding is about.
            expected: leak.fit.intercept_at_window_start,
            // Saturated, because `slope_t` is `slope / std_error` and a PERFECTLY straight series
            // has zero standard error, giving `inf`. That is not an error condition — a synthetic
            // ramp and a real process that grows by exactly n bytes a tick both produce it — but
            // `Evidence::validate` requires a finite statistic and rejected the whole finding.
            // Measured: the leak was detected correctly and then discarded at the registry, with
            // the `Err` arm swallowing the reason.
            //
            // Capped at twice the gate rather than at some large number: the confidence term
            // already saturates at 2x `min_slope_t`, so anything beyond it carries no extra
            // meaning, and a huge value would make `exceedance_ratio` dominate the score.
            statistic: if leak.fit.slope_t.is_finite() {
                leak.fit.slope_t
            } else {
                config.min_slope_t * 2.0
            },
            threshold: config.min_slope_t,
            direction: Some(FindingDirection::Above),
            agreement: None,
            // No baseline term: a leak test compares a series against its own fitted line, not
            // against a learned centre. `None` means "not baseline-derived", never "immature".
            baseline: None,
            trend: Some(fit),
            forecast,
            samples: cited,
            extra: BTreeMap::from([
                ("growth_bytes_per_hour".to_string(), leak.growth_per_hour),
                ("monotonic_ratio".to_string(), leak.fit.monotonic_ratio),
                ("window_secs".to_string(), leak.fit.window_secs),
                ("confidence_fit".to_string(), leak.confidence.fit_term),
                (
                    "confidence_monotonicity".to_string(),
                    leak.confidence.monotonicity_term,
                ),
            ]),
        };

        let key_string = key.as_string();
        match self.registry.observe(key.clone(), at_unix, evidence) {
            Ok(Transition::Raised { id }) => {
                report.raised += 1;
                self.push_event(report, process, &key, key_string, id, true);
            }
            Ok(Transition::ReRaised { id, .. }) => {
                report.re_raised += 1;
                self.push_event(report, process, &key, key_string, id, true);
            }
            Ok(Transition::Held { .. }) => report.held += 1,
            Ok(Transition::Cleared { id }) => {
                report.cleared += 1;
                self.push_event(report, process, &key, key_string, id, false);
            }
            Err(err) => {
                // Logged, NOT swallowed. The level path above discards this silently, and while
                // writing this method that silence hid a real defect for one debugging round: a
                // perfect fit gives `slope_t == inf`, `Evidence::validate` rejects a non-finite
                // statistic, and the leak finding vanished with no trace whatsoever. A rejected
                // evidence payload is a bug in this module, so it belongs in the log where it can
                // be found rather than in a branch that does nothing.
                tracing::debug!(
                    process,
                    detector = "resource_leak",
                    error = %err,
                    "leak evidence rejected"
                );
            }
        }
    }

    fn push_event(
        &self,
        report: &mut CycleReport,
        process: &str,
        key: &FindingKey,
        key_string: String,
        id: String,
        raised: bool,
    ) {
        let stored = self.registry.get(key);
        report.events.push(FindingEvent {
            process: process.to_string(),
            id,
            key: key_string,
            detector: key.detector.to_string(),
            metric: key.metric.to_string(),
            confidence: stored.map(|f| f.confidence.score).unwrap_or(0.0),
            occurrence: stored.map(|f| f.occurrence).unwrap_or(1),
            summary: stored.and_then(|f| f.summary.clone()),
            raised,
        });
    }
}

/// The identity of a decision's OUTCOME, ignoring its timestamp.
///
/// Two decisions with the same signature say the same thing about the same process, so the second
/// one is not news. Deliberately excludes `at_unix` — including it would make every decision unique
/// and defeat the deduplication entirely.
///
/// The withheld reason is compared as its rendered string rather than structurally, because a
/// confidence refusal carries `f64` scores that drift by rounding between ticks: comparing those
/// numerically would report a change when nothing an operator cares about has changed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DecisionSignature {
    rule: Option<oxmgr_core::rules::RuleId>,
    action: Option<oxmgr_core::rules::Action>,
    withheld: Option<String>,
    /// The findings that satisfied the rule, so a decision backed by a NEW finding records again
    /// even when the rule and action are unchanged.
    findings: Vec<String>,
}

impl DecisionSignature {
    fn of(decision: &Decision) -> Self {
        Self {
            rule: decision.rule,
            action: decision.action,
            withheld: decision.withheld.as_ref().map(|w| w.reason()),
            findings: decision
                .findings
                .iter()
                .map(|key| key.as_string())
                .collect(),
        }
    }
}

/// What was measured, for one process and one metric on one cycle.
///
/// Grouped because these four are always passed together and never independently: the process and
/// metric name the series, the summary is the baseline it was compared against, and `at_unix` is
/// the evaluation timestamp — the only clock any of this may read.
struct Measured<'a> {
    process: &'a str,
    metric: Metric,
    summary: LevelBaseline,
    at_unix: u64,
}

/// The tuning-layer detector identity for a metric's level detector.
fn detector_for(_metric: Metric) -> oxmgr_core::findings::Detector {
    oxmgr_core::findings::Detector::LevelDeparture
}

/// Maps a baseline metric onto the findings-layer metric. Same wire names on both sides, so this
/// cannot silently disagree — but it is a match rather than a string cast so a new variant on
/// either side fails to compile instead of producing `Other("...")`.
fn finding_metric(metric: Metric) -> FindingMetric {
    match metric {
        Metric::CpuPercent => FindingMetric::CpuPercent,
        Metric::MemoryBytes => FindingMetric::MemoryBytes,
        Metric::DiskReadBytes => FindingMetric::DiskReadBytes,
        Metric::DiskWriteBytes => FindingMetric::DiskWriteBytes,
    }
}

/// Maps a baseline metric onto the history metric.
fn history_kind(metric: Metric) -> MetricKind {
    match metric {
        Metric::CpuPercent => MetricKind::Cpu,
        Metric::MemoryBytes => MetricKind::Memory,
        Metric::DiskReadBytes => MetricKind::DiskRead,
        Metric::DiskWriteBytes => MetricKind::DiskWrite,
    }
}

/// The variant discriminator for a direction, so a metric above its baseline and one below are
/// different conditions rather than one condition that moved.
fn direction_wire(direction: crate::detector_level::Direction) -> &'static str {
    match direction {
        crate::detector_level::Direction::Above => "above",
        crate::detector_level::Direction::Below => "below",
    }
}

#[cfg(test)]
/// Test code casts are bounded and exact.
mod tests {
    use super::*;
    use crate::baseline::ProcessBaselines;
    use crate::metrics_history::MetricSample;
    use oxmgr_core::numeric::{u64_to_f64, usize_to_f64};

    /// A process whose CPU baseline is warm and flat at `centre`.
    ///
    /// Warmed by feeding the real `observe` path rather than by constructing a summary, so the
    /// warm-up gate and the spread floor behave exactly as they do in production.
    fn warm(process: &str, centre: f64, samples: usize) -> HashMap<String, ProcessBaselines> {
        let mut set = ProcessBaselines::new("fingerprint");
        for _ in 0..samples {
            set.observe(Metric::CpuPercent, centre);
            set.observe(Metric::MemoryBytes, centre * 1_000_000.0);
        }
        let mut map = HashMap::new();
        map.insert(process.to_string(), set);
        map
    }

    fn observation(process: &str, cpu: Option<f64>) -> ProcessObservation {
        ProcessObservation {
            process: process.to_string(),
            config_fingerprint: "fingerprint".to_string(),
            cpu_percent: cpu,
            memory_bytes: None,
            // No limit by default: the leak forecast needs a level to project toward, and most of
            // these fixtures exercise the level detector, which has no use for one.
            memory_limit_bytes: None,
            facts: ProcessFacts::running(),
        }
    }

    #[test]
    fn a_sustained_departure_raises_exactly_one_finding() {
        // The end-to-end claim this module exists for: history and baselines in, a finding out.
        // Before this engine, every piece of that chain was tested and nothing joined them.
        let mut engine = Engine::default();
        let mut baselines = warm("api", 10.0, 60);
        let history = MetricHistoryStore::new();

        // Three consecutive samples far from a baseline centred at 10 with a floored spread.
        // Three, because `min_consecutive_samples` defaults to 3 — two would be `Building`, which
        // is the anti-spike suppression rather than a finding.
        let mut raised = 0;
        for tick in 0..3 {
            let report = engine.analyse(
                &[observation("api", Some(90.0))],
                &mut baselines,
                &history,
                1_000 + tick,
            );
            raised += report.raised;
        }
        assert_eq!(raised, 1, "one episode, raised once");

        // And it HOLDS rather than re-raising: a finding that reappeared with a new id every tick
        // is the failure the registry's identity model exists to prevent.
        let report = engine.analyse(
            &[observation("api", Some(90.0))],
            &mut baselines,
            &history,
            1_010,
        );
        assert_eq!(report.raised, 0, "a held finding must not re-raise");
        assert_eq!(engine.registry().active_for("api").len(), 1);
    }

    /// Records a rising memory series into history, spaced so it clears the detector's window gate.
    ///
    /// `min_window_secs` defaults to 600, so the samples span 20 minutes at 60s spacing. Anything
    /// tighter is refused as describing a burst rather than a leak, which is the gate working.
    fn leaking_history(
        process: &str,
        samples: usize,
        start_bytes: u64,
        per_sample: u64,
    ) -> MetricHistoryStore {
        let mut history = MetricHistoryStore::new();
        for index in 0..samples {
            let at_ms = (1_000 + u64::try_from(index).unwrap_or(0) * 60) * 1_000;
            history.record(
                process,
                crate::metrics_history::MetricSample::cpu_memory(
                    at_ms,
                    10.0,
                    start_bytes + per_sample * u64::try_from(index).unwrap_or(0),
                ),
            );
        }
        history
    }

    /// Drives `analyse` until the trend cadence fires, and returns the report from that cycle.
    ///
    /// `trend_every` is 15 by default, so the trend detector runs on tick 15 and not before. A test
    /// that called `analyse` once would assert on a cycle where the trend path never executed.
    fn run_until_trend(
        engine: &mut Engine,
        observation: &ProcessObservation,
        baselines: &mut HashMap<String, ProcessBaselines>,
        history: &MetricHistoryStore,
    ) -> CycleReport {
        let mut last = CycleReport::default();
        for tick in 0..DEFAULT_TREND_EVERY {
            last = engine.analyse(
                std::slice::from_ref(observation),
                baselines,
                history,
                2_000 + u64::from(tick),
            );
        }
        last
    }

    #[test]
    fn a_growing_memory_series_raises_a_resource_leak_finding() {
        // THE WIRING TEST. `detector_trend` was fully implemented with 37 passing unit tests and
        // was nevertheless unreachable: `analyse_one` took the cadence flag as `_run_trend` and
        // dropped it, so `Detector::ResourceLeak` was never produced by a running daemon. Every
        // one of those 37 tests called the detector directly, which is exactly why they all passed
        // while the feature did not exist. This asserts the path a daemon actually takes.
        let mut engine = Engine::default();
        let mut baselines = warm("api", 10.0, 60);
        // 30 samples at 60s, +2 MB each: 29 minutes of monotonic growth, well past the 600s
        // window gate and the 8-sample minimum.
        let history = leaking_history("api", 30, 100 * 1024 * 1024, 2 * 1024 * 1024);

        let mut obs = observation("api", Some(10.0));
        obs.memory_bytes = Some(158.0 * 1024.0 * 1024.0);

        let report = run_until_trend(&mut engine, &obs, &mut baselines, &history);
        assert_eq!(
            report.raised, 1,
            "a monotonic climb must raise a leak finding"
        );

        let active = engine.registry().active_for("api");
        let leak = active
            .iter()
            .find(|f| f.key.detector == Detector::ResourceLeak)
            .expect("the finding must be a resource_leak, not a level_departure");

        // The evidence must carry the fit, or the finding is an assertion rather than something an
        // operator can check.
        let trend = leak
            .evidence
            .trend
            .as_ref()
            .expect("a leak finding must carry its fit");
        assert!(
            trend.slope_per_second > 0.0,
            "slope must be positive, got {}",
            trend.slope_per_second
        );
        assert!(
            trend.fit_quality > 0.85,
            "a straight line must fit well, got {}",
            trend.fit_quality
        );
        assert!(
            leak.evidence.extra.contains_key("growth_bytes_per_hour"),
            "growth per hour is the figure an operator reads"
        );
        assert!(
            !leak.evidence.samples.is_empty(),
            "evidence without samples is not checkable"
        );
    }

    #[test]
    fn a_flat_memory_series_raises_no_leak() {
        // The silence requirement. A process holding steady must produce nothing, or the detector
        // reports a leak on every process that simply runs.
        let mut engine = Engine::default();
        let mut baselines = warm("api", 10.0, 60);
        let history = leaking_history("api", 30, 100 * 1024 * 1024, 0);

        let mut obs = observation("api", Some(10.0));
        obs.memory_bytes = Some(100.0 * 1024.0 * 1024.0);

        let report = run_until_trend(&mut engine, &obs, &mut baselines, &history);
        assert_eq!(report.raised, 0, "flat memory is not a leak");
        assert!(
            engine
                .registry()
                .active_for("api")
                .iter()
                .all(|f| f.key.detector != Detector::ResourceLeak),
            "no resource_leak finding may exist for a flat series"
        );
    }

    #[test]
    fn a_configured_memory_limit_produces_a_forecast() {
        // Task 6.5: the forecast rides on the same fit. Withheld when there is no limit to project
        // toward, present when there is — and this asserts the limit actually reaches the detector
        // through `ProcessObservation`, which is the part that was not connected.
        let mut engine = Engine::default();
        let mut baselines = warm("api", 10.0, 60);
        let history = leaking_history("api", 30, 100 * 1024 * 1024, 2 * 1024 * 1024);

        let mut obs = observation("api", Some(10.0));
        obs.memory_bytes = Some(158.0 * 1024.0 * 1024.0);
        // 200 MB, not 512.
        //
        // `max_horizon_window_multiple` is 3.0, so a forecast further out than 3x the fitted
        // window (960s here, giving a 2880s horizon) is REFUSED as extrapolating too far. 512 MB
        // is 11400s away and correctly withheld as `BeyondHorizon` — my first choice of threshold
        // was asserting the detector should do something it is designed not to do. 200 MB is
        // 2040s out, inside the horizon.
        obs.memory_limit_bytes = Some(200.0 * 1024.0 * 1024.0);

        run_until_trend(&mut engine, &obs, &mut baselines, &history);
        let active = engine.registry().active_for("api");
        let leak = active
            .iter()
            .find(|f| f.key.detector == Detector::ResourceLeak)
            .expect("a leak finding");
        let forecast = leak
            .evidence
            .forecast
            .as_ref()
            .expect("a limit was configured, so a forecast must be present");
        assert!(
            forecast.eta_seconds > 0.0,
            "arrival must be in the future, got {}",
            forecast.eta_seconds
        );
        assert!(
            forecast.interval_low_seconds <= forecast.eta_seconds
                && forecast.eta_seconds <= forecast.interval_high_seconds,
            "the point estimate must sit inside its interval"
        );
    }

    #[test]
    fn a_forecast_beyond_the_horizon_is_withheld_but_the_leak_is_still_raised() {
        // Locks in what a debugging round taught: `max_horizon_window_multiple` is 3.0, so a
        // threshold further out than 3x the fitted window is refused rather than extrapolated. My
        // first version of the forecast test asserted a 512 MB target would produce an ETA; it is
        // 11400s away against a 2880s horizon, and the detector was right to withhold it.
        //
        // The important half of this: withholding the FORECAST must not withhold the FINDING. A
        // leak whose limit is far away is still a leak, and reporting nothing because the arrival
        // time is unknowable would lose the only part an operator can act on.
        let mut engine = Engine::default();
        let mut baselines = warm("api", 10.0, 60);
        let history = leaking_history("api", 30, 100 * 1024 * 1024, 2 * 1024 * 1024);

        let mut obs = observation("api", Some(10.0));
        obs.memory_bytes = Some(158.0 * 1024.0 * 1024.0);
        obs.memory_limit_bytes = Some(512.0 * 1024.0 * 1024.0);

        run_until_trend(&mut engine, &obs, &mut baselines, &history);
        let active = engine.registry().active_for("api");
        let leak = active
            .iter()
            .find(|f| f.key.detector == Detector::ResourceLeak)
            .expect("the leak must still be raised");
        assert!(
            leak.evidence.forecast.is_none(),
            "a target beyond the horizon must not carry an ETA"
        );
        assert!(
            leak.evidence.trend.is_some(),
            "the fit is what remains actionable when the ETA is withheld"
        );
    }

    #[test]
    fn a_leak_finding_clears_when_growth_stops() {
        // Task 6.4. Raise on a climbing series, then re-analyse against a flat one and require the
        // finding to clear — a leak that never clears is a leak an operator learns to ignore.
        let mut engine = Engine::default();
        let mut baselines = warm("api", 10.0, 60);
        let climbing = leaking_history("api", 30, 100 * 1024 * 1024, 2 * 1024 * 1024);

        let mut obs = observation("api", Some(10.0));
        obs.memory_bytes = Some(158.0 * 1024.0 * 1024.0);
        run_until_trend(&mut engine, &obs, &mut baselines, &climbing);
        assert!(
            engine
                .registry()
                .active_for("api")
                .iter()
                .any(|f| f.key.detector == Detector::ResourceLeak),
            "precondition: a leak must be active before it can clear"
        );

        let flat = leaking_history("api", 30, 200 * 1024 * 1024, 0);
        let report = run_until_trend(&mut engine, &obs, &mut baselines, &flat);
        assert!(report.cleared >= 1, "levelling off must clear the finding");
        assert!(
            engine
                .registry()
                .active_for("api")
                .iter()
                .all(|f| f.key.detector != Detector::ResourceLeak),
            "the leak must no longer be active"
        );
    }

    #[test]
    fn a_single_spike_produces_no_finding() {
        // One excursion is noise. `Building` is the detector counting, and the whole reason a
        // dashboard full of false positives is avoided.
        let mut engine = Engine::default();
        let mut baselines = warm("api", 10.0, 60);
        let history = MetricHistoryStore::new();

        let report = engine.analyse(
            &[observation("api", Some(400.0))],
            &mut baselines,
            &history,
            1_000,
        );
        assert_eq!(report.raised, 0, "a single spike is not a finding");
        assert!(engine.registry().active_for("api").is_empty());
    }

    #[test]
    fn a_warming_baseline_produces_no_finding() {
        // "No detector fires on an immature baseline." Asserted here as well as in the detector,
        // because this is the path a real daemon takes and a gate that only holds one level down
        // is a gate that can be bypassed by a new caller.
        let mut engine = Engine::default();
        let mut baselines = warm("api", 10.0, 5); // below the 30-sample default
        let history = MetricHistoryStore::new();

        for tick in 0..5 {
            let report = engine.analyse(
                &[observation("api", Some(900.0))],
                &mut baselines,
                &history,
                1_000 + tick,
            );
            assert_eq!(report.raised, 0, "warming must not fire");
        }
    }

    #[test]
    fn an_absent_sample_does_not_clear_an_active_finding() {
        // "The daemon lost sight of the metric" is not "the metric recovered". Auto-clearing on
        // absence would make every restart look like a fix.
        let mut engine = Engine::default();
        let mut baselines = warm("api", 10.0, 60);
        let history = MetricHistoryStore::new();

        for tick in 0..3 {
            engine.analyse(
                &[observation("api", Some(90.0))],
                &mut baselines,
                &history,
                1_000 + tick,
            );
        }
        assert_eq!(engine.registry().active_for("api").len(), 1);

        let report = engine.analyse(&[observation("api", None)], &mut baselines, &history, 1_010);
        assert_eq!(report.cleared, 0, "an absent sample must not clear");
        assert_eq!(
            engine.registry().active_for("api").len(),
            1,
            "the finding is still active"
        );
    }

    #[test]
    fn a_recovered_metric_clears_its_finding() {
        let mut engine = Engine::default();
        let mut baselines = warm("api", 10.0, 60);
        let history = MetricHistoryStore::new();

        for tick in 0..3 {
            engine.analyse(
                &[observation("api", Some(90.0))],
                &mut baselines,
                &history,
                1_000 + tick,
            );
        }
        assert_eq!(engine.registry().active_for("api").len(), 1);

        // Back at the centre. One sample under the clear threshold ends the episode.
        let report = engine.analyse(
            &[observation("api", Some(10.0))],
            &mut baselines,
            &history,
            1_010,
        );
        assert_eq!(report.cleared, 1);
        assert!(engine.registry().active_for("api").is_empty());
        // Cleared, not deleted: "this resolved" is only observable if the clearing is visible.
        assert_eq!(engine.registry().all().len(), 1);
    }

    #[test]
    fn suppression_blocks_the_finding_and_is_counted() {
        let mut engine = Engine::default();
        let mut baselines = warm("api", 10.0, 60);
        let history = MetricHistoryStore::new();
        engine
            .tuning_mut()
            .suppress("api", &Detector::LevelDeparture, Some("noisy".into()));

        for tick in 0..4 {
            let report = engine.analyse(
                &[observation("api", Some(90.0))],
                &mut baselines,
                &history,
                1_000 + tick,
            );
            assert_eq!(report.raised, 0, "a suppressed detector produces nothing");
        }
        // Counted, not silently dropped: an engine that cannot say how much it suppressed cannot
        // be tuned.
        assert!(
            engine.counters().blocked_by_process >= 4,
            "each blocked evaluation is counted, got {}",
            engine.counters().blocked_by_process
        );
    }

    #[test]
    fn the_budget_defers_the_remainder_and_records_it() {
        // A zero budget forces the deferral path deterministically, without depending on how fast
        // the machine is — a timing-based test here would be flaky on CI.
        let mut engine = Engine::new(AnalysisConfig {
            trend_every: DEFAULT_TREND_EVERY,
            budget: Duration::ZERO,
        });
        let mut baselines = HashMap::new();
        for name in ["a", "b", "c", "d"] {
            baselines.extend(warm(name, 10.0, 60));
        }
        let history = MetricHistoryStore::new();
        let observations: Vec<ProcessObservation> = ["a", "b", "c", "d"]
            .iter()
            .map(|n| observation(n, Some(90.0)))
            .collect();

        let report = engine.analyse(&observations, &mut baselines, &history, 1_000);
        assert_eq!(report.analysed, 1, "one process before the budget is spent");
        assert_eq!(report.deferred, 3, "the rest is deferred");
        assert!(!report.complete(), "a deferred cycle is not complete");
    }

    #[test]
    fn a_deferred_cycle_resumes_where_it_stopped() {
        // The property that makes deferral safe rather than merely imperfect. A fixed start would
        // mean a permanently over-budget daemon analysing only its first process for ever while
        // never looking at the rest — and never saying so.
        let mut engine = Engine::new(AnalysisConfig {
            trend_every: DEFAULT_TREND_EVERY,
            budget: Duration::ZERO,
        });
        let mut baselines = HashMap::new();
        for name in ["a", "b", "c"] {
            baselines.extend(warm(name, 10.0, 60));
        }
        let history = MetricHistoryStore::new();
        let observations: Vec<ProcessObservation> = ["a", "b", "c"]
            .iter()
            .map(|n| observation(n, Some(90.0)))
            .collect();

        // Three cycles, one process each. Every process must have been analysed exactly once,
        // which is only true if the resume point advances.
        for _ in 0..3 {
            engine.analyse(&observations, &mut baselines, &history, 1_000);
        }
        // Each process reached gets a CPU detector, so the set of detector keys is the record of
        // which processes were actually analysed.
        let missed: Vec<&str> = ["a", "b", "c"]
            .into_iter()
            .filter(|name| {
                !engine
                    .detectors
                    .contains_key(&((*name).to_string(), Metric::CpuPercent))
            })
            .collect();
        assert!(
            missed.is_empty(),
            "every process must be reached across cycles, not just the first; missed {missed:?}"
        );
    }

    #[test]
    fn an_empty_cycle_costs_nothing_and_resets() {
        let mut engine = Engine::default();
        let mut baselines = HashMap::new();
        let history = MetricHistoryStore::new();
        let report = engine.analyse(&[], &mut baselines, &history, 1_000);
        assert_eq!(report.analysed, 0);
        assert_eq!(report.deferred, 0);
        assert!(report.complete());
    }

    #[test]
    fn forgetting_a_process_releases_everything_held_for_it() {
        let mut engine = Engine::default();
        let mut baselines = warm("api", 10.0, 60);
        baselines.extend(warm("worker", 10.0, 60));
        let history = MetricHistoryStore::new();
        let observations = vec![
            observation("api", Some(90.0)),
            observation("worker", Some(90.0)),
        ];

        for tick in 0..3 {
            engine.analyse(&observations, &mut baselines, &history, 1_000 + tick);
        }
        assert_eq!(engine.registry().active_for("api").len(), 1);
        assert_eq!(engine.registry().active_for("worker").len(), 1);

        engine.forget("api");

        assert!(
            engine.registry().active_for("api").is_empty(),
            "a deleted process keeps no findings"
        );
        assert!(
            !engine
                .detectors
                .contains_key(&("api".to_string(), Metric::CpuPercent)),
            "and no detector state, or a later process reusing the name inherits its run"
        );
        assert_eq!(
            engine.registry().active_for("worker").len(),
            1,
            "deleting one process must not clear another"
        );
    }

    #[test]
    fn a_finding_carries_reproducible_evidence() {
        // "A finding can be recomputed from its evidence" made checkable at runtime rather than
        // only in a comment: the stored confidence must follow from the stored evidence.
        let mut engine = Engine::default();
        let mut baselines = warm("api", 10.0, 60);
        let mut history = MetricHistoryStore::new();
        for tick in 0..5u64 {
            history.record(
                "api",
                MetricSample::cpu_memory(1_000_000 + tick * 2_000, 90.0, 1024),
            );
        }

        for tick in 0..3 {
            engine.analyse(
                &[observation("api", Some(90.0))],
                &mut baselines,
                &history,
                1_000 + tick,
            );
        }

        let active = engine.registry().active_for("api");
        let finding = active.first().expect("a finding was raised");
        assert!(
            finding.confidence_is_reproducible(),
            "the stored score must follow from the stored evidence"
        );
        assert!(
            !finding.evidence.samples.is_empty(),
            "evidence must name the samples it rests on"
        );
        // The statistic must actually exceed the threshold it fired against.
        assert!(finding.evidence.statistic >= finding.evidence.threshold);
    }

    #[test]
    fn a_process_with_no_prior_baseline_creates_one_and_warms() {
        // REGRESSION. The lookup was `baselines.get_mut(process)?`, and baselines were only ever
        // inserted by `restore_baselines` reading the persisted store — so a process that had never
        // been analysed had no entry, the `?` returned early, and it could never accumulate the
        // samples that would create one. Warm-up was unreachable for every new process.
        //
        // Found on a live daemon: it sat at "still building baselines" for 75 seconds when 30
        // samples at a 2s tick should take 60. The symptom was silence, not an error, which is why
        // this test asserts on the baseline being CREATED rather than only on a finding appearing.
        let mut engine = Engine::default();
        // Deliberately empty: this is the state a fresh process is in.
        let mut baselines: HashMap<String, ProcessBaselines> = HashMap::new();
        let history = MetricHistoryStore::new();

        engine.analyse(
            &[observation("api", Some(10.0))],
            &mut baselines,
            &history,
            1_000,
        );

        let set = baselines
            .get("api")
            .expect("a first observation must create the baseline set");
        // Labelled with the fingerprint it is learning under, so a later restart keeps it while a
        // reconfiguration discards it.
        assert_eq!(set.config_fingerprint(), "fingerprint");

        // And it actually warms: 30 samples is the default requirement, so it must be ready after
        // that many cycles and not before.
        for tick in 1..30 {
            engine.analyse(
                &[observation("api", Some(10.0))],
                &mut baselines,
                &history,
                1_000 + tick,
            );
        }
        let ready = baselines
            .get("api")
            .and_then(|set| set.get(Metric::CpuPercent))
            .map(|baseline| baseline.snapshot().readiness.is_ready())
            .expect("baseline exists");
        assert!(
            ready,
            "30 observations must satisfy the default warm-up requirement"
        );
    }

    #[test]
    fn a_created_baseline_reaches_a_finding_without_any_persisted_state() {
        // The end-to-end consequence of the bug above: with no persisted baselines at all — a fresh
        // install — a sustained departure must still be detected. Before the fix this produced
        // nothing, for ever.
        let mut engine = Engine::default();
        let mut baselines: HashMap<String, ProcessBaselines> = HashMap::new();
        let history = MetricHistoryStore::new();

        // Warm at a flat 10%.
        for tick in 0..30 {
            engine.analyse(
                &[observation("api", Some(10.0))],
                &mut baselines,
                &history,
                1_000 + tick,
            );
        }

        // Then depart, for the three consecutive samples the detector requires.
        let mut raised = 0;
        for tick in 30..33 {
            let report = engine.analyse(
                &[observation("api", Some(90.0))],
                &mut baselines,
                &history,
                1_000 + tick,
            );
            raised += report.raised;
        }

        assert_eq!(
            raised, 1,
            "a fresh daemon with no persisted baselines must still detect a departure"
        );
        // And the transition is published, so the event surfaces reach it too.
        assert_eq!(engine.registry().active_for("api").len(), 1);
    }

    #[test]
    fn an_unchanged_decision_is_recorded_once_not_every_tick() {
        // MEASURED DEFECT, not a hypothetical. On a live daemon two departing processes filled 52 of
        // the 256 decision slots in ninety seconds with exactly 2 distinct decisions — x28 and x24
        // of the same entry. At that rate the log holds under nine minutes and evicts anything worth
        // reading, so "what did protection mode decide this morning" becomes unanswerable.
        let mut engine = Engine::default();
        let mut baselines = warm("api", 10.0, 60);
        let history = MetricHistoryStore::new();

        // Drive a departure, then hold it for twenty more ticks.
        let mut recorded = 0;
        for tick in 0..23 {
            let report = engine.analyse(
                &[observation("api", Some(90.0))],
                &mut baselines,
                &history,
                1_000 + tick,
            );
            recorded += report.decisions.len();
        }

        assert_eq!(
            recorded, 1,
            "a decision that has not changed must be recorded once, not once per tick"
        );
        assert_eq!(engine.decisions().len(), 1);
    }

    #[test]
    fn a_changed_decision_is_recorded_again() {
        // The other half: deduplication must not swallow a real change. Same process, same rule, but
        // the outcome flips from acting to withheld once the process is marked stopped — which is a
        // different decision and must appear.
        let mut engine = Engine::default();
        let mut baselines = warm("api", 10.0, 60);
        let history = MetricHistoryStore::new();

        for tick in 0..4 {
            engine.analyse(
                &[observation("api", Some(90.0))],
                &mut baselines,
                &history,
                1_000 + tick,
            );
        }
        let before = engine.decisions().len();
        assert_eq!(before, 1);

        // Now the operator stops it. `ProcessStopped` is declared before the acting rules, so the
        // decision becomes a refusal.
        let mut stopped = observation("api", Some(90.0));
        stopped.facts.desired_stopped = true;
        let report = engine.analyse(&[stopped], &mut baselines, &history, 1_010);

        assert_eq!(
            report.decisions.len(),
            1,
            "a changed outcome must be recorded"
        );
        assert_eq!(engine.decisions().len(), 2);
    }

    #[test]
    fn a_recurrence_after_recovery_records_again() {
        // A condition that cleared and came back is news. Without clearing the remembered signature
        // on "no rule matched", the second episode would be silently treated as unchanged.
        let mut engine = Engine::default();
        let mut baselines = warm("api", 10.0, 60);
        let history = MetricHistoryStore::new();

        for tick in 0..4 {
            engine.analyse(
                &[observation("api", Some(90.0))],
                &mut baselines,
                &history,
                1_000 + tick,
            );
        }
        assert_eq!(engine.decisions().len(), 1);

        // Recover: back at the centre, so nothing matches and the finding clears.
        for tick in 4..8 {
            engine.analyse(
                &[observation("api", Some(10.0))],
                &mut baselines,
                &history,
                1_000 + tick,
            );
        }

        // Depart again.
        for tick in 8..12 {
            engine.analyse(
                &[observation("api", Some(90.0))],
                &mut baselines,
                &history,
                1_000 + tick,
            );
        }

        assert_eq!(
            engine.decisions().len(),
            2,
            "a second episode must record its own decision"
        );
    }

    // ── Performance and safety verification (13.x) ───────────────────────────────────────────────

    /// Builds `count` warmed processes, so a measurement runs against the state a real daemon
    /// reaches rather than against warm-up.
    fn warmed_fleet(count: usize) -> (Vec<ProcessObservation>, HashMap<String, ProcessBaselines>) {
        let mut baselines = HashMap::new();
        let mut observations = Vec::with_capacity(count);
        for index in 0..count {
            let name = format!("svc-{index}");
            baselines.extend(warm(&name, 10.0, 60));
            observations.push(observation(&name, Some(11.0)));
        }
        (observations, baselines)
    }

    #[test]
    fn per_cycle_cost_does_not_grow_with_retained_history() {
        // Task 13.1, asserted STRUCTURALLY rather than by timing, because the structural property is
        // the one that matters and a wall-clock assertion would be flaky on CI.
        //
        // The claim is that analysis is incremental: the detector carries a run counter and a flag,
        // not a buffer, so a cycle's work is independent of how much history exists. The way to show
        // that is to run one cycle against an empty history and another against a full one, and
        // assert the detector state advanced identically — if anything rescanned the rings, the two
        // would diverge.
        let mut lean = Engine::default();
        let mut fat = Engine::default();
        let (obs, mut lean_baselines) = warmed_fleet(1);
        let (_, mut fat_baselines) = warmed_fleet(1);

        let empty = MetricHistoryStore::new();
        let mut full = MetricHistoryStore::new();
        // 3,000 samples: past the 900-slot raw tier, so the minute tier is populated too.
        for tick in 0..3_000u64 {
            full.record("svc-0", MetricSample::cpu_memory(tick * 2_000, 10.0, 1024));
        }
        assert!(full.history("svc-0").is_some(), "history must be populated");

        for tick in 0..5 {
            lean.analyse(&obs, &mut lean_baselines, &empty, 1_000 + tick);
            fat.analyse(&obs, &mut fat_baselines, &full, 1_000 + tick);
        }

        // Identical detector state proves the same work was done in both cases: the amount of
        // retained history did not change what a cycle touched.
        assert_eq!(
            lean.detectors.len(),
            fat.detectors.len(),
            "retained history must not change how many detectors a cycle advances"
        );
        assert_eq!(
            lean.registry().all().len(),
            fat.registry().all().len(),
            "retained history must not change the findings a cycle produces"
        );
    }

    #[test]
    fn per_cycle_cost_grows_no_faster_than_process_count() {
        // Task 13.5. Also structural: one cycle touches each process exactly once, so the work is
        // linear in process count by construction. Asserted by counting detector entries — two
        // metrics per process, no more — because a super-linear implementation would have to compare
        // processes against each other and would show up here as a quadratic count.
        for count in [1usize, 4, 16] {
            let mut engine = Engine::default();
            let (obs, mut baselines) = warmed_fleet(count);
            let history = MetricHistoryStore::new();
            let report = engine.analyse(&obs, &mut baselines, &history, 1_000);

            assert_eq!(report.analysed, count);
            assert_eq!(
                engine.detectors.len(),
                count * 2,
                "exactly two detectors per process (cpu, memory), so cost is linear in count"
            );
        }
    }

    #[test]
    fn memory_per_process_is_bounded_and_flat() {
        // Task 13.4, in the only form that is honest without a heap profiler: assert the STRUCTURES
        // are bounded, since a bounded structure cannot grow with uptime.
        //
        // Per process the engine holds two `LevelDetector`s (six scalar fields each), at most one
        // finding per (detector, metric, direction), one decision signature, and its share of a
        // 256-entry decision log. None of those is a function of uptime.
        let mut engine = Engine::default();
        let (obs, mut baselines) = warmed_fleet(2);
        let history = MetricHistoryStore::new();

        // 500 cycles is well past any warm-up or ring-fill transient.
        for tick in 0..500 {
            engine.analyse(&obs, &mut baselines, &history, 1_000 + tick);
        }

        assert_eq!(
            engine.detectors.len(),
            4,
            "two processes x two metrics, regardless of how many cycles ran"
        );
        // The registry holds one finding per condition, not one per observation.
        assert!(
            engine.registry().all().len() <= 4,
            "findings are per condition, not per cycle: got {}",
            engine.registry().all().len()
        );
        // And the decision log is bounded by its capacity, not by cycles.
        assert!(
            engine.decisions().len() <= engine.decisions().capacity(),
            "the decision log must stay within capacity"
        );
        // The dedup map holds at most one entry per process.
        assert!(engine.last_decision.len() <= 2);
    }

    #[test]
    #[ignore = "timing measurement, not an assertion; run explicitly with --nocapture"]
    fn measure_per_cycle_analysis_cost() {
        // Task 13.1's actual figures. Ignored by default because a wall-clock assertion on a shared
        // CI runner is flaky, and a number nobody can reproduce is worse than no number.
        //
        // Run: cargo test --release measure_per_cycle_analysis_cost -- --ignored --nocapture
        println!("\n  processes   p50 per cycle   per process   budget share");
        for count in [1usize, 8, 32, 128, 512] {
            let mut engine = Engine::default();
            let (obs, mut baselines) = warmed_fleet(count);
            let history = MetricHistoryStore::new();

            // Warm the path so first-touch allocation is not counted.
            engine.analyse(&obs, &mut baselines, &history, 999);

            let mut samples = Vec::new();
            for tick in 0..25 {
                let started = std::time::Instant::now();
                engine.analyse(&obs, &mut baselines, &history, 1_000 + tick);
                samples.push(started.elapsed().as_secs_f64() * 1000.0);
            }
            samples.sort_by(|a, b| a.partial_cmp(b).expect("no NaN timings"));
            let p50 = samples[samples.len() / 2];
            // Display-path measurement prints: counts are ≤512 and budget constants small,
            // provably exact in f64.
            let per_process = p50 * 1000.0 / usize_to_f64(count);
            let share = p50 / u64_to_f64(DEFAULT_BUDGET_MS) * 100.0;
            println!("  {count:>9}   {p50:>10.3}ms   {per_process:>9.1}us   {share:>9.1}%");
        }
    }

    #[test]
    #[ignore = "timing measurement, not an assertion; run explicitly with --nocapture"]
    fn measure_cost_against_retained_history_depth() {
        // The figures behind 13.1's "does not grow with retained history". Two runs, identical
        // except for how much history exists.
        println!("\n  history samples   p50 per cycle");
        for depth in [0usize, 100, 900, 3_000, 10_000] {
            let mut engine = Engine::default();
            let (obs, mut baselines) = warmed_fleet(8);
            let mut history = MetricHistoryStore::new();
            for tick in 0..u64::try_from(depth).unwrap_or(0) {
                for index in 0..8 {
                    history.record(
                        &format!("svc-{index}"),
                        MetricSample::cpu_memory(tick * 2_000, 10.0, 1024),
                    );
                }
            }

            engine.analyse(&obs, &mut baselines, &history, 999);
            let mut samples = Vec::new();
            for tick in 0..25 {
                let started = std::time::Instant::now();
                engine.analyse(&obs, &mut baselines, &history, 1_000 + tick);
                samples.push(started.elapsed().as_secs_f64() * 1000.0);
            }
            samples.sort_by(|a, b| a.partial_cmp(b).expect("no NaN timings"));
            println!("  {depth:>15}   {:>10.3}ms", samples[samples.len() / 2]);
        }
    }
}

/// A read-only copy of analysis output, for the HTTP surface and the CLI.
///
/// Snapshotted rather than borrowed because the handlers must not take the manager lock: a scrape
/// arriving mid-tick would otherwise block supervision. Up to one tick stale by construction, which
/// is the same staleness the process list already carries.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AnalysisSnapshot {
    /// Every finding, active and recently cleared, most recent first.
    pub findings: Vec<oxmgr_core::findings::Finding>,
    /// Recent decisions, newest first.
    pub decisions: Vec<Decision>,
    /// What the action gate withheld, oldest first. Empty while nothing has decided — which is
    /// itself the answer to "does protection mode have anything to say?"
    pub withheld: Vec<oxmgr_core::protection::ActionRecord>,
    /// Cumulative suppression counters.
    pub suppressed: SuppressionCounters,
    /// Per-process baseline readiness, so "no findings" is explainable — a process whose baselines
    /// are still warming is not a process that has been checked and found healthy.
    pub warming: Vec<String>,
}

impl AnalysisSnapshot {
    /// Findings for one process, active first.
    pub fn for_process(&self, process: &str) -> Vec<&oxmgr_core::findings::Finding> {
        self.findings
            .iter()
            .filter(|finding| finding.key.process == process)
            .collect()
    }

    /// Active findings across every process.
    pub fn active(&self) -> impl Iterator<Item = &oxmgr_core::findings::Finding> {
        self.findings.iter().filter(|finding| finding.is_active())
    }
}

impl Engine {
    /// Copies current state for the read-only surfaces.
    pub fn snapshot(&self, warming: Vec<String>) -> AnalysisSnapshot {
        let mut findings: Vec<oxmgr_core::findings::Finding> =
            self.registry.all().into_iter().cloned().collect();
        // Active first, then most recently seen. A consumer rendering a list wants what is wrong
        // now above what was wrong earlier, and sorting here means every surface agrees rather than
        // each imposing its own order.
        findings.sort_by(|a, b| {
            b.is_active()
                .cmp(&a.is_active())
                .then(b.last_seen_unix.cmp(&a.last_seen_unix))
        });
        let mut decisions: Vec<Decision> = self.decisions.all().cloned().collect();
        decisions.reverse();
        let withheld: Vec<oxmgr_core::protection::ActionRecord> =
            self.gate.history().cloned().collect();
        AnalysisSnapshot {
            findings,
            decisions,
            withheld,
            suppressed: self.counters,
            warming,
        }
    }
}
