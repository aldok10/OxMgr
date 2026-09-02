//! Deterministic mapping from findings to intended actions.
//!
//! Detectors never act; they emit findings. This module is the only thing that decides, and its
//! determinism is structural rather than a property to be tested for:
//!
//! - Rules live in one `const` array, [`RULES`], evaluated in that order. First match wins.
//! - Every rule is a pure function of the finding set plus [`ProcessFacts`]. There is no `&mut`,
//!   no interior mutability, and no argument through which a clock or an RNG could arrive — the
//!   evaluation timestamp is passed in, which is the only time reference the spec permits.
//! - Finding order cannot matter, because [`decide`] does not iterate the caller's slice to pick a
//!   subject. It asks each rule to select its own supporting findings and sorts them by
//!   [`FindingKey`], so two permutations of one set produce byte-identical output.
//!
//! # Observe only
//!
//! Nothing here takes an action or touches a process. [`decide`] returns a [`Decision`] describing
//! what *would* happen; the safety limits that gate real execution (permission lists, cooldowns,
//! rate caps) are section 12's, and they can only ever withhold an action this module already
//! decided. That ordering is deliberate: an operator can read the decision record and see exactly
//! what protection mode would have done before enabling it.
//!
//! # Why rule order carries the conflict resolution
//!
//! Two findings can imply opposite actions. A memory leak argues for a restart; a failing declared
//! dependency argues for leaving the process alone, because restarting a symptom while its cause
//! persists just adds an outage to an incident. Encoding that as "whichever detector ran last" is
//! how a remediation engine becomes unpredictable, so the refusals are declared *first* and win.

use std::collections::VecDeque;

use crate::findings::{Detector, Finding, FindingKey};

/// How many decisions are retained per daemon. See [`DecisionLog`].
pub const DEFAULT_DECISION_CAPACITY: usize = 256;

/// The minimum confidence a supporting finding must carry before a rule may propose an action.
///
/// Documented default, applied when configuration is absent or unusable. 0.6 rather than a round
/// 0.5: at 0.5 the score carries no information — half the weighted terms agreed and half did not —
/// and acting on a coin flip is worse than not acting.
pub const DEFAULT_MIN_CONFIDENCE: f64 = 0.6;

/// What a decision would do to a process.
///
/// Deliberately small. Every variant is something the daemon already knows how to do through its
/// ordinary supervision path, so protection mode cannot invent a capability that operator commands
/// do not also have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// Restart the process through the normal supervision path.
    Restart,
    /// Tell the operator; change nothing.
    ///
    /// Distinct from "no action": a decision that deliberately escalates to a human is a decision,
    /// and collapsing it into `None` would make "we looked and chose to report" indistinguishable
    /// from "no rule matched".
    Notify,
}

impl Action {
    /// The canonical wire name.
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Restart => "restart",
            Self::Notify => "notify",
        }
    }
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_wire())
    }
}

/// Identity of a declared rule.
///
/// An enum rather than a string so the declared set is closed and the compiler checks that [`RULES`]
/// covers each one. The wire name is stable: it appears in decision records that operators read and
/// that may be persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuleId {
    /// A declared dependency is implicated, so the dependent process is left alone.
    DependencyImplicated,
    /// Desired state is stopped: an operator asked for this, and it is not ours to undo.
    ProcessStopped,
    /// At the crash-restart limit, where existing supervision has already given up.
    CrashLoopLimitReached,
    /// Sustained, well-fitted growth that will not resolve itself.
    ResourceLeak,
    /// A metric parked far from its baseline.
    LevelDeparture,
}

impl RuleId {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::DependencyImplicated => "dependency_implicated",
            Self::ProcessStopped => "process_stopped",
            Self::CrashLoopLimitReached => "crash_loop_limit_reached",
            Self::ResourceLeak => "resource_leak",
            Self::LevelDeparture => "level_departure",
        }
    }
    /// Why this rule exists, for the decision record.
    #[cfg(test)]
    pub fn rationale(self) -> &'static str {
        match self {
            Self::DependencyImplicated => {
                "a declared dependency is implicated; acting on the symptom would not address the cause"
            }
            Self::ProcessStopped => "the process is stopped by operator intent",
            Self::CrashLoopLimitReached => {
                "the process is at its crash-restart limit, where existing supervision is authoritative"
            }
            Self::ResourceLeak => "sustained resource growth will not resolve without intervention",
            Self::LevelDeparture => {
                "the metric is far from its baseline, which an operator should see"
            }
        }
    }
}

impl std::fmt::Display for RuleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_wire())
    }
}

/// Process state a rule may read.
///
/// Everything a rule is allowed to know that is not a finding, and nothing else. In particular
/// there is no handle to the process manager, no channel, and no clock: a rule that could reach
/// those could stop being a pure function without the signature changing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessFacts {
    /// The operator asked for this process to be stopped.
    pub desired_stopped: bool,
    /// Existing supervision has reached its crash-restart limit for this process.
    pub at_crash_loop_limit: bool,
    /// Declared dependencies currently implicated in a failure correlated with this process.
    ///
    /// Names, not findings, because the correlation is produced by `failure_patterns` and this
    /// module only needs to know that one exists. Empty means nothing is implicated, which is the
    /// case in which a decision proceeds normally.
    pub implicated_dependencies: Vec<String>,
}

impl ProcessFacts {
    /// Facts for an ordinary running process with nothing implicated.
    pub fn running() -> Self {
        Self::default()
    }
}

/// Why an action was not proposed, when a rule matched but declined to act.
///
/// `PartialEq` but not `Eq`: [`Withheld::BelowMinConfidence`] carries the observed and required
/// scores, and `f64` has no total equality. Keeping the numbers is worth more than the marker trait
/// — a record that says only "confidence too low" cannot be re-checked against a later threshold.
#[derive(Debug, Clone, PartialEq)]
pub enum Withheld {
    /// A dependency is implicated; the named ones are reported so the operator can look there.
    DependencyImplicated { dependencies: Vec<String> },
    /// Operator intent is that this process be stopped.
    ProcessStopped,
    /// Existing crash-loop protection is authoritative here.
    CrashLoopLimitReached,
    /// The supporting findings did not reach the minimum confidence.
    ///
    /// Carries both numbers so the record can be re-checked against a later threshold change
    /// instead of only saying "too low".
    BelowMinConfidence { observed: f64, required: f64 },
}

impl Withheld {
    /// A short operator-facing reason.
    pub fn reason(&self) -> String {
        match self {
            Self::DependencyImplicated { dependencies } => format!(
                "withheld: dependency implicated ({})",
                dependencies.join(", ")
            ),
            Self::ProcessStopped => "withheld: process is stopped by operator intent".to_string(),
            Self::CrashLoopLimitReached => {
                "withheld: process is at its crash-restart limit".to_string()
            }
            Self::BelowMinConfidence { observed, required } => {
                format!("withheld: confidence {observed:.2} is below the required {required:.2}")
            }
        }
    }
}

/// One evaluation's outcome.
///
/// `None` for [`Self::rule`] means no rule matched, which is a real and common answer: a healthy
/// process produces no findings and therefore no decision to act.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// The process this decision concerns.
    pub process: String,
    /// When the decision was evaluated. The only time reference a rule may use.
    pub at_unix: u64,
    /// The rule that produced it, or `None` when nothing matched.
    pub rule: Option<RuleId>,
    /// The action that would be taken. `None` when no rule matched, or when a matching rule
    /// deliberately declines to act.
    pub action: Option<Action>,
    /// Why no action was proposed, when a rule matched and declined.
    pub withheld: Option<Withheld>,
    /// Keys of the findings that satisfied the rule, sorted.
    ///
    /// Sorted so the record is identical for two permutations of one finding set. Keys rather than
    /// whole findings: a decision record is retained and must not pin the evidence of every finding
    /// it ever considered in memory.
    pub findings: Vec<FindingKey>,
}

impl Decision {
    /// No rule matched.
    pub fn no_match(process: impl Into<String>, at_unix: u64) -> Self {
        Self {
            process: process.into(),
            at_unix,
            rule: None,
            action: None,
            withheld: None,
            findings: Vec::new(),
        }
    }

    /// Whether this decision proposes doing something.
    #[cfg(test)]
    pub fn proposes_action(&self) -> bool {
        self.action.is_some()
    }

    /// A short operator-facing sentence.
    pub fn summary(&self) -> String {
        match (&self.rule, &self.action, &self.withheld) {
            (None, _, _) => format!("{}: no rule matched", self.process),
            (Some(rule), Some(action), _) => {
                format!("{}: {rule} would {action}", self.process)
            }
            (Some(rule), None, Some(withheld)) => {
                format!("{}: {rule} — {}", self.process, withheld.reason())
            }
            (Some(rule), None, None) => {
                format!("{}: {rule} proposed no action", self.process)
            }
        }
    }
}

/// Configuration for one evaluation.
///
/// Separate from the rules themselves so a per-process override is a value rather than a rebuild,
/// which is what section 11's per-process thresholds need.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RuleConfig {
    /// Minimum confidence a supporting finding must carry before a rule may propose an action.
    pub min_confidence: f64,
}

impl Default for RuleConfig {
    fn default() -> Self {
        Self {
            min_confidence: DEFAULT_MIN_CONFIDENCE,
        }
    }
}

impl RuleConfig {
    /// Applies a configured minimum confidence, falling back to the documented default when the
    /// value is unusable.
    ///
    /// NaN and out-of-range both fall back rather than being clamped. Clamping a configured 5.0 to
    /// 1.0 would silently mean "never act", and an operator who typed a percentage by mistake
    /// deserves the default and a report, not a permanently silent daemon.
    #[cfg(test)]
    pub fn with_min_confidence(value: f64) -> (Self, bool) {
        if value.is_finite() && (0.0..=1.0).contains(&value) {
            (
                Self {
                    min_confidence: value,
                },
                true,
            )
        } else {
            (Self::default(), false)
        }
    }
}

/// A declared rule: a predicate over the finding set plus process state, and the action it proposes.
///
/// `select` returns the findings that satisfy the rule. An empty return means "did not match", so a
/// rule cannot match while naming no evidence — every decision that acts can point at why.
struct Rule {
    id: RuleId,
    /// What this rule would do. `None` for a refusal rule, which matches in order to *prevent* the
    /// rules below it from acting.
    action: Option<Action>,
    /// Why it declined, for a refusal rule.
    withheld: fn(&ProcessFacts) -> Option<Withheld>,
    /// The findings supporting a match, or empty for no match.
    select: fn(&[&Finding], &ProcessFacts) -> Vec<FindingKey>,
}

/// Whether a finding is one this build knows how to reason about at all.
///
/// `Detector::Other` is deliberately excluded from every rule: a detector this build does not
/// recognise has unknown semantics, and inferring an action from a name is how a future detector
/// silently acquires the power to restart processes.
fn is_known(finding: &Finding) -> bool {
    !matches!(finding.key.detector, Detector::Other(_))
}

/// Active findings only, since a cleared finding describes a condition that has ended.
fn active_known<'a>(findings: &'a [&'a Finding]) -> impl Iterator<Item = &'a &'a Finding> {
    findings
        .iter()
        .filter(|finding| finding.is_active() && is_known(finding))
}

/// Keys of every active finding, sorted. Used by the refusal rules, which are about process state
/// rather than about a particular detector, but must still name what they suppressed.
fn all_active_keys(findings: &[&Finding], _facts: &ProcessFacts) -> Vec<FindingKey> {
    let mut keys: Vec<FindingKey> = active_known(findings)
        .map(|finding| finding.key.clone())
        .collect();
    keys.sort();
    keys
}

/// The declared rule set, in evaluation order. First match wins.
///
/// **The order is the specification.** The three refusals come first, so a process that must not be
/// touched is never reached by a rule that would touch it — the alternative, deciding an action and
/// then filtering it downstream, means the safety property lives in the caller and can be forgotten
/// at a new call site.
///
/// Within the refusals: dependency correlation precedes stopped-state and crash-loop, because it is
/// the only one that names an external cause an operator should look at first. Between the two
/// acting rules, `ResourceLeak` precedes `LevelDeparture` because a leak is the stronger claim about
/// the same series — well-fitted monotonic growth over a window, versus a level that is currently
/// far from centre — and a leaking process usually satisfies both.
const RULES: &[Rule] = &[
    Rule {
        id: RuleId::DependencyImplicated,
        action: None,
        withheld: |facts| {
            (!facts.implicated_dependencies.is_empty()).then(|| Withheld::DependencyImplicated {
                dependencies: facts.implicated_dependencies.clone(),
            })
        },
        select: |findings, facts| {
            if facts.implicated_dependencies.is_empty() {
                return Vec::new();
            }
            all_active_keys(findings, facts)
        },
    },
    Rule {
        id: RuleId::ProcessStopped,
        action: None,
        withheld: |facts| facts.desired_stopped.then_some(Withheld::ProcessStopped),
        select: |findings, facts| {
            if !facts.desired_stopped {
                return Vec::new();
            }
            all_active_keys(findings, facts)
        },
    },
    Rule {
        id: RuleId::CrashLoopLimitReached,
        action: None,
        withheld: |facts| {
            facts
                .at_crash_loop_limit
                .then_some(Withheld::CrashLoopLimitReached)
        },
        select: |findings, facts| {
            if !facts.at_crash_loop_limit {
                return Vec::new();
            }
            all_active_keys(findings, facts)
        },
    },
    Rule {
        id: RuleId::ResourceLeak,
        action: Some(Action::Restart),
        withheld: |_| None,
        select: |findings, _| {
            let mut keys: Vec<FindingKey> = active_known(findings)
                .filter(|finding| finding.key.detector == Detector::ResourceLeak)
                .map(|finding| finding.key.clone())
                .collect();
            keys.sort();
            keys
        },
    },
    Rule {
        id: RuleId::LevelDeparture,
        action: Some(Action::Notify),
        withheld: |_| None,
        select: |findings, _| {
            let mut keys: Vec<FindingKey> = active_known(findings)
                .filter(|finding| finding.key.detector == Detector::LevelDeparture)
                .map(|finding| finding.key.clone())
                .collect();
            keys.sort();
            keys
        },
    },
];

/// The declared rules, in evaluation order. For documentation and for the operator surface.
#[cfg(test)]
pub fn declared_rules() -> Vec<RuleId> {
    RULES.iter().map(|rule| rule.id).collect()
}

/// Evaluates the rule set for one process.
///
/// Pure and total: same findings and facts give the same decision, and `at_unix` is the only clock.
/// `findings` may be in any order — no rule reads position, and every selection is sorted — so the
/// production order of detectors cannot change the outcome.
///
/// The confidence gate is applied *after* a rule matches rather than by pre-filtering the input.
/// That ordering is what makes a low-confidence finding "still visible but not acted upon": the
/// decision records the rule it matched and reports the gate as the reason it withheld, instead of
/// silently reporting no match and losing the fact that something was seen.
pub fn decide(
    process: &str,
    findings: &[&Finding],
    facts: &ProcessFacts,
    config: &RuleConfig,
    at_unix: u64,
) -> Decision {
    for rule in RULES {
        let keys = (rule.select)(findings, facts);
        if keys.is_empty() {
            continue;
        }

        // A refusal rule: matched in order to stop the rules below it from acting.
        if let Some(withheld) = (rule.withheld)(facts) {
            return Decision {
                process: process.to_string(),
                at_unix,
                rule: Some(rule.id),
                action: None,
                withheld: Some(withheld),
                findings: keys,
            };
        }

        let Some(action) = rule.action else {
            return Decision {
                process: process.to_string(),
                at_unix,
                rule: Some(rule.id),
                action: None,
                withheld: None,
                findings: keys,
            };
        };

        // The gate reads the best supporting finding, not an average: a decision is justified by
        // the strongest evidence for it, and averaging would let a weak second finding veto a
        // strong first one.
        let best = active_known(findings)
            .filter(|finding| keys.contains(&finding.key))
            .map(|finding| finding.confidence.score)
            .fold(f64::NEG_INFINITY, f64::max);

        if best < config.min_confidence {
            return Decision {
                process: process.to_string(),
                at_unix,
                rule: Some(rule.id),
                action: None,
                withheld: Some(Withheld::BelowMinConfidence {
                    observed: if best.is_finite() { best } else { 0.0 },
                    required: config.min_confidence,
                }),
                findings: keys,
            };
        }

        return Decision {
            process: process.to_string(),
            at_unix,
            rule: Some(rule.id),
            action: Some(action),
            withheld: None,
            findings: keys,
        };
    }

    Decision::no_match(process, at_unix)
}

/// Bounded record of recent decisions, oldest evicted first.
///
/// One ring for the daemon rather than one per process, with per-process reads filtered out of it.
/// A per-process ring would bound each process independently and so not bound the daemon at all —
/// 256 processes would hold 256 rings. Capacity is in decisions, each of which holds a process name
/// and a small vector of keys.
#[derive(Debug)]
pub struct DecisionLog {
    capacity: usize,
    entries: VecDeque<Decision>,
}

impl Default for DecisionLog {
    fn default() -> Self {
        Self::new(DEFAULT_DECISION_CAPACITY)
    }
}

impl DecisionLog {
    /// Zero capacity is treated as 1, matching the retention convention elsewhere in this change: an
    /// unusable configured value falls back rather than producing a log that silently records
    /// nothing.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: VecDeque::new(),
        }
    }

    /// Records a decision, evicting the oldest if full.
    pub fn record(&mut self, decision: Decision) {
        while self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(decision);
    }

    /// Every retained decision, oldest first.
    pub fn all(&self) -> impl Iterator<Item = &Decision> {
        self.entries.iter()
    }

    /// Drops a process's decisions, for a delete.
    pub fn forget(&mut self, process: &str) -> usize {
        let before = self.entries.len();
        self.entries.retain(|decision| decision.process != process);
        before - self.entries.len()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Retained decisions for one process, oldest first.
    pub fn for_process(&self, process: &str) -> Vec<&Decision> {
        self.entries
            .iter()
            .filter(|decision| decision.process == process)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::findings::{
        Agreement, BaselineSummary, Direction, Evidence, EvidenceWindow, Metric, SampleRef,
    };
    use std::collections::BTreeMap;

    /// Evidence whose confidence lands comfortably above the default gate.
    ///
    /// `statistic / threshold` drives the exceedance term, and a ready baseline plus full agreement
    /// supply the other two, so this scores high enough that a test about rule ORDER is not
    /// accidentally a test about the confidence gate.
    fn strong_evidence() -> Evidence {
        Evidence {
            window: EvidenceWindow {
                start_unix: 1_000,
                end_unix: 1_060,
                sample_count: 30,
            },
            observed: 400.0,
            expected: 100.0,
            statistic: 9.0,
            threshold: 3.0,
            direction: Some(Direction::Above),
            agreement: Some(Agreement {
                consecutive_samples: 6,
                required_samples: 3,
            }),
            baseline: Some(BaselineSummary {
                center: 100.0,
                spread: 10.0,
                sample_count: 120,
                min_samples: 30,
            }),
            trend: None,
            forecast: None,
            samples: vec![
                SampleRef::new(1_000, 380.0),
                SampleRef::new(1_030, 390.0),
                SampleRef::new(1_060, 400.0),
            ],
            extra: BTreeMap::new(),
        }
    }

    /// Evidence that barely exceeds its threshold, so confidence lands below the default gate.
    fn weak_evidence() -> Evidence {
        let mut evidence = strong_evidence();
        evidence.statistic = 3.05;
        evidence.agreement = Some(Agreement {
            consecutive_samples: 1,
            required_samples: 3,
        });
        evidence.baseline = Some(BaselineSummary {
            center: 100.0,
            spread: 10.0,
            sample_count: 30,
            min_samples: 30,
        });
        evidence
    }

    fn finding(process: &str, detector: Detector, metric: Metric, evidence: Evidence) -> Finding {
        Finding::builder(FindingKey::new(process, detector, metric), 1_060, evidence)
            .build()
            .expect("fixture evidence must be valid")
    }

    fn leak(process: &str) -> Finding {
        finding(
            process,
            Detector::ResourceLeak,
            Metric::MemoryBytes,
            strong_evidence(),
        )
    }

    fn departure(process: &str) -> Finding {
        finding(
            process,
            Detector::LevelDeparture,
            Metric::CpuPercent,
            strong_evidence(),
        )
    }

    // ---- determinism (9.1, 9.2) ----

    #[test]
    fn the_same_findings_give_the_same_decision() {
        let leak = leak("api");
        let departure = departure("api");
        let findings = [&leak, &departure];
        let facts = ProcessFacts::running();
        let config = RuleConfig::default();

        let first = decide("api", &findings, &facts, &config, 2_000);
        let second = decide("api", &findings, &facts, &config, 2_000);

        assert_eq!(first, second, "two evaluations of one input must agree");
    }

    #[test]
    fn production_order_does_not_affect_the_outcome() {
        // The load-bearing determinism property. `decide` never reads a position from the caller's
        // slice, and every selection is sorted, so a detector that happened to run first cannot
        // influence the decision.
        let leak = leak("api");
        let departure = departure("api");
        let facts = ProcessFacts::running();
        let config = RuleConfig::default();

        let forward = decide("api", &[&leak, &departure], &facts, &config, 2_000);
        let reversed = decide("api", &[&departure, &leak], &facts, &config, 2_000);

        assert_eq!(
            forward, reversed,
            "reordering the findings changed the decision"
        );
    }

    #[test]
    fn rule_order_resolves_conflicting_findings() {
        // A leak argues for a restart; a level departure only warrants telling someone. Both are
        // active on the same process, and `ResourceLeak` is declared first, so restart wins — every
        // time, rather than depending on which detector ran last.
        let leak = leak("api");
        let departure = departure("api");
        let facts = ProcessFacts::running();

        let decision = decide(
            "api",
            &[&departure, &leak],
            &facts,
            &RuleConfig::default(),
            2_000,
        );

        assert_eq!(decision.rule, Some(RuleId::ResourceLeak));
        assert_eq!(decision.action, Some(Action::Restart));
        // And it names only its own supporting finding, not every active one: the record has to
        // show what actually satisfied the rule.
        assert_eq!(decision.findings, vec![leak.key.clone()]);
    }

    #[test]
    fn no_matching_rule_means_no_action() {
        let facts = ProcessFacts::running();
        let decision = decide("api", &[], &facts, &RuleConfig::default(), 2_000);

        assert_eq!(decision.rule, None);
        assert_eq!(decision.action, None);
        assert!(decision.withheld.is_none());
        assert!(decision.findings.is_empty());
        assert!(!decision.proposes_action());
    }

    #[test]
    fn an_unknown_detector_never_satisfies_a_rule() {
        // A detector this build does not recognise has unknown semantics. Inferring an action from
        // its name is how a future detector silently acquires the power to restart a process.
        let unknown = finding(
            "api",
            Detector::from_wire("some_future_detector"),
            Metric::MemoryBytes,
            strong_evidence(),
        );
        let decision = decide(
            "api",
            &[&unknown],
            &ProcessFacts::running(),
            &RuleConfig::default(),
            2_000,
        );

        assert_eq!(
            decision.rule, None,
            "an unrecognised detector must not match a rule"
        );
    }

    #[test]
    fn a_cleared_finding_does_not_drive_a_decision() {
        // A cleared finding describes a condition that has ended, so acting on it would act on the
        // past.
        let mut leak = leak("api");
        leak.status = crate::findings::FindingStatus::Cleared;
        leak.cleared_at_unix = Some(1_100);

        let decision = decide(
            "api",
            &[&leak],
            &ProcessFacts::running(),
            &RuleConfig::default(),
            2_000,
        );

        assert_eq!(decision.rule, None);
    }

    // ---- refusal rules come first (9.1) ----

    #[test]
    fn an_implicated_dependency_withholds_action_and_names_it() {
        // The strongest refusal, and declared first: restarting a symptom while its cause persists
        // adds an outage to an incident. The dependency is named so the operator looks in the right
        // place rather than at the process that merely reported it.
        let leak = leak("api");
        let facts = ProcessFacts {
            implicated_dependencies: vec!["db".to_string()],
            ..ProcessFacts::running()
        };

        let decision = decide("api", &[&leak], &facts, &RuleConfig::default(), 2_000);

        assert_eq!(decision.rule, Some(RuleId::DependencyImplicated));
        assert_eq!(
            decision.action, None,
            "a dependent process must not be acted upon"
        );
        assert_eq!(
            decision.withheld,
            Some(Withheld::DependencyImplicated {
                dependencies: vec!["db".to_string()]
            })
        );
        // The suppressed finding is still named, so the record shows what was seen and not acted on.
        assert_eq!(decision.findings, vec![leak.key.clone()]);
    }

    #[test]
    fn action_proceeds_once_the_dependency_recovers() {
        // Same finding, same process; only the implication is gone. The decision must proceed
        // normally rather than staying suppressed by anything sticky.
        let leak = leak("api");
        let config = RuleConfig::default();

        let suppressed = decide(
            "api",
            &[&leak],
            &ProcessFacts {
                implicated_dependencies: vec!["db".to_string()],
                ..ProcessFacts::running()
            },
            &config,
            2_000,
        );
        assert_eq!(suppressed.action, None);

        let recovered = decide("api", &[&leak], &ProcessFacts::running(), &config, 2_060);
        assert_eq!(
            recovered.action,
            Some(Action::Restart),
            "the decision must resume once nothing is implicated"
        );
    }

    #[test]
    fn a_stopped_process_is_not_acted_upon() {
        // Desired state stopped is operator intent. Restarting it would be protection mode
        // overriding a human, which is the one thing it must never do.
        let leak = leak("api");
        let facts = ProcessFacts {
            desired_stopped: true,
            ..ProcessFacts::running()
        };

        let decision = decide("api", &[&leak], &facts, &RuleConfig::default(), 2_000);

        assert_eq!(decision.rule, Some(RuleId::ProcessStopped));
        assert_eq!(decision.action, None);
        assert_eq!(decision.withheld, Some(Withheld::ProcessStopped));
    }

    #[test]
    fn a_process_at_its_crash_loop_limit_is_not_restarted() {
        // Existing supervision has already decided this process should stop being restarted.
        // Protection mode is not entitled to overrule it.
        let leak = leak("api");
        let facts = ProcessFacts {
            at_crash_loop_limit: true,
            ..ProcessFacts::running()
        };

        let decision = decide("api", &[&leak], &facts, &RuleConfig::default(), 2_000);

        assert_eq!(decision.rule, Some(RuleId::CrashLoopLimitReached));
        assert_eq!(decision.action, None);
        assert_eq!(decision.withheld, Some(Withheld::CrashLoopLimitReached));
    }

    #[test]
    fn refusals_are_declared_before_the_acting_rules() {
        // Asserted on the declared order itself, not only through behaviour. The order IS the
        // specification, so a future edit that moves an acting rule above a refusal should fail
        // here even if some individual scenario still happens to pass.
        let declared = declared_rules();
        let refusals = [
            RuleId::DependencyImplicated,
            RuleId::ProcessStopped,
            RuleId::CrashLoopLimitReached,
        ];
        let acting = [RuleId::ResourceLeak, RuleId::LevelDeparture];

        let last_refusal = refusals
            .iter()
            .map(|id| {
                declared
                    .iter()
                    .position(|d| d == id)
                    .expect("every refusal must be declared")
            })
            .max()
            .expect("refusals is not empty");
        let first_acting = acting
            .iter()
            .map(|id| {
                declared
                    .iter()
                    .position(|d| d == id)
                    .expect("every acting rule must be declared")
            })
            .min()
            .expect("acting is not empty");

        assert!(
            last_refusal < first_acting,
            "every refusal must precede every acting rule: refusals end at {last_refusal}, acting starts at {first_acting}"
        );
    }

    // ---- confidence gate (9.4) ----

    #[test]
    fn a_low_confidence_finding_does_not_act_but_is_still_reported() {
        // Measured, not assumed: this fixture scores 0.0037 against the 0.6 default gate, while the
        // strong fixture scores 1.0.
        let weak = finding(
            "api",
            Detector::ResourceLeak,
            Metric::MemoryBytes,
            weak_evidence(),
        );

        let decision = decide(
            "api",
            &[&weak],
            &ProcessFacts::running(),
            &RuleConfig::default(),
            2_000,
        );

        assert_eq!(decision.action, None, "a weak finding must not act");
        // Still attributed to the rule it matched, and the finding still named. Pre-filtering weak
        // findings would have produced "no match" and lost the fact that anything was seen.
        assert_eq!(decision.rule, Some(RuleId::ResourceLeak));
        assert_eq!(decision.findings, vec![weak.key.clone()]);
        match decision.withheld {
            Some(Withheld::BelowMinConfidence { observed, required }) => {
                assert!(observed < required, "{observed} should be below {required}");
                assert_eq!(required, DEFAULT_MIN_CONFIDENCE);
            }
            other => panic!("expected a confidence refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_high_confidence_finding_may_act() {
        let decision = decide(
            "api",
            &[&leak("api")],
            &ProcessFacts::running(),
            &RuleConfig::default(),
            2_000,
        );

        assert_eq!(decision.action, Some(Action::Restart));
        assert!(decision.withheld.is_none());
    }

    #[test]
    fn the_gate_reads_the_strongest_supporting_finding() {
        // A decision is justified by the best evidence for it. Averaging would let a weak second
        // finding veto a strong first one, so two leak findings — one strong, one weak — must still
        // act.
        let strong = leak("api");
        let weak = finding(
            "api",
            Detector::ResourceLeak,
            Metric::DiskWriteBytes,
            weak_evidence(),
        );

        let decision = decide(
            "api",
            &[&weak, &strong],
            &ProcessFacts::running(),
            &RuleConfig::default(),
            2_000,
        );

        assert_eq!(
            decision.action,
            Some(Action::Restart),
            "a weak finding must not veto a strong one"
        );
        assert_eq!(decision.findings.len(), 2, "both are named as supporting");
    }

    #[test]
    fn an_unusable_min_confidence_falls_back_and_reports() {
        // Out of range and NaN both fall back rather than clamp. Clamping a mistyped 95 to 1.0 would
        // silently mean "never act" — a permanently quiet daemon with no indication why.
        for bad in [95.0_f64, -1.0, f64::NAN, f64::INFINITY] {
            let (config, accepted) = RuleConfig::with_min_confidence(bad);
            assert!(!accepted, "{bad} should be rejected");
            assert_eq!(config.min_confidence, DEFAULT_MIN_CONFIDENCE);
        }

        let (config, accepted) = RuleConfig::with_min_confidence(0.9);
        assert!(accepted, "an in-range value is applied");
        assert_eq!(config.min_confidence, 0.9);
    }

    #[test]
    fn a_configured_minimum_is_applied() {
        // The strong fixture scores 1.0, so only a threshold above that can withhold it. This proves
        // the configured value is consulted rather than the default being hardcoded.
        let (config, accepted) = RuleConfig::with_min_confidence(1.0);
        assert!(accepted);
        let decision = decide(
            "api",
            &[&leak("api")],
            &ProcessFacts::running(),
            &config,
            2_000,
        );
        assert_eq!(
            decision.action,
            Some(Action::Restart),
            "a score meeting the threshold exactly must act"
        );
    }

    // ---- decision record (9.3, 9.5) ----

    #[test]
    fn a_decision_reports_its_rule_and_supporting_findings() {
        let leak = leak("api");
        let decision = decide(
            "api",
            &[&leak],
            &ProcessFacts::running(),
            &RuleConfig::default(),
            2_000,
        );

        assert_eq!(decision.process, "api");
        assert_eq!(
            decision.at_unix, 2_000,
            "the evaluation timestamp is carried"
        );
        assert_eq!(decision.rule, Some(RuleId::ResourceLeak));
        assert_eq!(decision.findings, vec![leak.key.clone()]);
        // The action it WOULD take is recorded even though nothing acts, which is what lets an
        // operator see protection mode's intent before enabling it.
        assert_eq!(decision.action, Some(Action::Restart));
        assert!(decision.summary().contains("restart"));
        // Every rule can explain itself, so a record is readable without the source to hand.
        assert!(!RuleId::ResourceLeak.rationale().is_empty());
    }

    #[test]
    fn the_decision_record_is_bounded_and_evicts_oldest_first() {
        let mut log = DecisionLog::new(3);
        for tick in 0..10u64 {
            log.record(Decision::no_match("api", tick));
        }

        assert_eq!(log.len(), 3, "capacity must not be exceeded");
        let retained: Vec<u64> = log.all().map(|decision| decision.at_unix).collect();
        assert_eq!(
            retained,
            vec![7, 8, 9],
            "the most recent are retained and the oldest discarded"
        );
    }

    #[test]
    fn decisions_are_retrievable_per_process() {
        let mut log = DecisionLog::new(16);
        log.record(Decision::no_match("api", 1));
        log.record(Decision::no_match("worker", 2));
        log.record(Decision::no_match("api", 3));

        let api: Vec<u64> = log
            .for_process("api")
            .iter()
            .map(|decision| decision.at_unix)
            .collect();
        assert_eq!(api, vec![1, 3], "oldest first, and only this process");
        assert!(log.for_process("ghost").is_empty());
    }

    #[test]
    fn forgetting_a_process_leaves_the_others() {
        // Deleting a process ends its identity, the same rule metric history and baselines follow.
        let mut log = DecisionLog::new(16);
        log.record(Decision::no_match("api", 1));
        log.record(Decision::no_match("worker", 2));

        assert_eq!(log.forget("api"), 1);
        assert!(log.for_process("api").is_empty());
        assert_eq!(log.for_process("worker").len(), 1);
    }

    #[test]
    fn a_zero_capacity_log_still_records() {
        // Retention convention for this change: an unusable configured value falls back rather than
        // producing a store that silently records nothing while looking healthy.
        let mut log = DecisionLog::new(0);
        assert_eq!(log.capacity(), 1);
        log.record(Decision::no_match("api", 1));
        assert_eq!(log.len(), 1);
    }
}
