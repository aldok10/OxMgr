//! Recognising repeated failure shapes from retained events.
//!
//! Scaffold for OpenSpec change `process-intelligence`, tasks section(s) 8.
//! The contract is `openspec/changes/process-intelligence/specs/`; read it before adding to this file.
//!
//! Pure logic on purpose: nothing here reaches for the process manager, the HTTP layer or the
//! dashboard, so it can be unit-tested without a running daemon. Wiring it into those is a
//! separate integration step, which is why the module is currently unreferenced.
//!
//! # What this module is *not*
//!
//! The daemon already stops crash loops: `crash_loop_limit_reached`
//! (`src/process_manager/restart.rs`) counts auto-restarts in a 5 minute window and is
//! authoritative. Nothing here duplicates or overrides that. These detectors only *name the
//! shape* a sequence of failures makes, so an operator reading a dashboard understands why a
//! process is unhappy. Every output is advisory; no function in this module returns an action,
//! and none can, because none of them can see or mutate process state.
//!
//! # Recomputation is how findings clear
//!
//! Every detector is a pure function of `(events, now)` and looks only inside a window ending
//! at `now`. There is no stored finding to expire, so a pattern that has stopped simply stops
//! appearing in the next call's output. A process that crash-looped an hour ago and has been
//! quiet since produces nothing, which is what the "failure findings clear" scenario requires.
//! The cost of that choice is that callers must re-run detection on a tick rather than being
//! told about a transition.
//!
//! # Absent evidence is not a clean bill of health
//!
//! An empty return could mean "nothing is wrong" or "too little happened to tell", and those
//! are different answers to an operator. So [`FailureReport`] carries [`Inconclusive`] entries
//! alongside findings: a process with some failures but fewer than a detector's minimum
//! evidence is reported as inconclusive for that detector rather than silently omitted.
//!
//! # Every threshold below is a guess
//!
//! The windows, rates and minimum counts are chosen for plausibility, not measured against
//! real incidents. They are `pub const` so they can be read, argued with and overridden via
//! [`PatternConfig`] rather than being buried in comparisons. Treat every one of them as
//! uncalibrated until it has been checked against a real failure archive.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use oxmgr_core::numeric::{u64_to_f64, usize_to_f64};

/// How a process exited.
///
/// Mirrors the information `ProcessExitEvent` already carries (`exit_code`, `signal`,
/// `wait_error`) but keeps this module free of a dependency on process types: these detectors
/// are pure and must stay testable from hand-built sequences.
///
/// The variants are compared for equality by the repeated-exit detector, so a signal is held as
/// its name rather than folded into a number. `SIGSEGV` every time and `SIGKILL` every time are
/// both deterministic, and they are not the same fault.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum ExitStatus {
    /// Exited normally with this status code. `0` is a success, anything else is not.
    Code(i32),
    /// Killed by a signal, e.g. `"SIGSEGV"`.
    Signal(String),
    /// The daemon could not determine how the process exited.
    ///
    /// Counted as a failure: a process whose exit could not be read is not evidence of health.
    /// It is deliberately never treated as *identical* to another `WaitError` for the
    /// repeated-exit detector, because "we don't know" twice is not a deterministic fault.
    Unknown,
}

impl ExitStatus {
    /// Whether this exit counts as a failure.
    pub fn is_failure(&self) -> bool {
        match self {
            ExitStatus::Code(code) => *code != 0,
            ExitStatus::Signal(_) => true,
            ExitStatus::Unknown => true,
        }
    }

    /// Whether two failing exits are the same fault for repeated-exit purposes.
    ///
    /// [`ExitStatus::Unknown`] never matches, including against itself: the point of the
    /// repeated-exit detector is to distinguish a deterministic fault from an intermittent one,
    /// and an unreadable exit status carries no information either way.
    fn same_fault(&self, other: &ExitStatus) -> bool {
        match (self, other) {
            (ExitStatus::Unknown, _) | (_, ExitStatus::Unknown) => false,
            (a, b) => a == b,
        }
    }

    /// Operator-facing label, used in finding descriptions.
    pub fn label(&self) -> String {
        match self {
            ExitStatus::Code(code) => format!("exit code {code}"),
            ExitStatus::Signal(name) => format!("signal {name}"),
            ExitStatus::Unknown => "unknown exit".to_string(),
        }
    }
}

/// What happened to a process.
///
/// `Started` is carried because a real event stream contains starts, and a detector that
/// silently counted them as failures would report a healthy process as looping. Keeping the
/// variant means tests can interleave starts and assert they are ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureEventKind {
    /// The process was started or restarted.
    #[cfg(test)]
    Started,
    /// The process exited. Only a failing [`ExitStatus`] feeds the detectors.
    Exited { status: ExitStatus },
}

/// One timestamped lifecycle event for one process.
///
/// Deliberately a plain owned struct rather than a borrow of the event-retention store: the
/// detectors must be callable from a unit test with a literal `Vec`, and the retention layer is
/// free to change shape without touching this module.
///
/// `at_secs` is seconds since the Unix epoch, matching `ManagedProcess::auto_restart_history`.
/// Events are *not* required to be sorted; every detector sorts what it needs, because an
/// unsorted input producing a wrong answer would be a silent failure rather than a loud one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureEvent {
    pub at_secs: u64,
    /// Process name, as the operator knows it.
    pub process: String,
    /// Namespace the process belongs to. Storms are scoped by this.
    pub namespace: String,
    pub kind: FailureEventKind,
}

impl FailureEvent {
    /// The failing exit status, if this event is a failing exit.
    fn failure_status(&self) -> Option<&ExitStatus> {
        match &self.kind {
            FailureEventKind::Exited { status } if status.is_failure() => Some(status),
            _ => None,
        }
    }
}

/// Window in which repeated failing exits are read as a loop, in seconds.
///
/// Matches `CRASH_RESTART_WINDOW_SECS` in `src/process_manager/restart.rs` on purpose: the
/// shape reported here should be the same shape the existing protection is reacting to, or an
/// operator would see a loop named that the daemon did not act on and vice versa. UNCALIBRATED.
pub const CRASH_LOOP_WINDOW_SECS: u64 = 5 * 60;

/// Failing exits inside [`CRASH_LOOP_WINDOW_SECS`] before the shape is called a loop.
///
/// Three, not two. Two failures are a coincidence with a line drawn through them; the interval
/// between them is a single sample and says nothing about a rate. UNCALIBRATED.
pub const CRASH_LOOP_MIN_FAILURES: usize = 3;

/// Longest mean interval between failures that still reads as a loop, in seconds.
///
/// [`CRASH_LOOP_MIN_FAILURES`] inside [`CRASH_LOOP_WINDOW_SECS`] already implies roughly this,
/// but stating it separately means "one restart an hour" cannot become a loop by widening the
/// window later without someone also revisiting this. UNCALIBRATED.
pub const CRASH_LOOP_MAX_MEAN_INTERVAL_SECS: u64 = 120;

/// Total window for restart-rate acceleration, split into two halves for comparison.
pub const ACCELERATION_WINDOW_SECS: u64 = 30 * 60;

/// Failures required in the recent half before acceleration is considered. UNCALIBRATED.
pub const ACCELERATION_MIN_RECENT: usize = 3;

/// Failures required in the earlier half to have an earlier rate at all.
///
/// The spec asks for recent frequency compared *against the process's earlier frequency*. With
/// no earlier failures there is no earlier rate, only a division by zero dressed up as
/// certainty, so that case is reported inconclusive instead.
pub const ACCELERATION_MIN_EARLIER: usize = 1;

/// How much faster the recent half must be than the earlier half. UNCALIBRATED.
pub const ACCELERATION_MIN_RATIO: f64 = 2.0;

/// Window over which identical exits are read as one deterministic fault, in seconds.
pub const REPEATED_EXIT_WINDOW_SECS: u64 = 30 * 60;

/// Identical failing exits required before reporting. Three, for the same reason as
/// [`CRASH_LOOP_MIN_FAILURES`]. UNCALIBRATED.
pub const REPEATED_EXIT_MIN_OCCURRENCES: usize = 3;

/// Window in which failures across several processes are read as one storm, in seconds.
///
/// Short on purpose. A storm is a claim that one cause hit several processes at once, and the
/// wider this gets the more it is really a claim about a busy hour. UNCALIBRATED.
pub const STORM_WINDOW_SECS: u64 = 60;

/// Distinct processes that must fail inside [`STORM_WINDOW_SECS`].
///
/// Three distinct processes. Two is a pair, and pairs happen. Counted by distinct process, so
/// one process failing ten times is never a storm. UNCALIBRATED.
pub const STORM_MIN_PROCESSES: usize = 3;

/// How far before a failure a dependency's failure may sit and still be reported, in seconds.
pub const DEPENDENCY_WINDOW_SECS: u64 = 120;

/// How many declared dependency edges correlation will follow.
///
/// Two: direct dependencies and theirs. Deeper walks turn "probable contributing cause" into a
/// list of everything the process transitively touches, which is not a finding.
pub const DEPENDENCY_MAX_DEPTH: usize = 2;

/// Tunable copy of the constants above, plus per-process suppression.
///
/// Every detector takes one of these rather than reading the constants directly, so a caller
/// can widen a window for one deployment without a rebuild, and so a test can prove a
/// threshold is actually consulted rather than coincidentally satisfied.
#[derive(Debug, Clone, PartialEq)]
pub struct PatternConfig {
    pub crash_loop_window_secs: u64,
    pub crash_loop_min_failures: usize,
    pub crash_loop_max_mean_interval_secs: u64,
    pub acceleration_window_secs: u64,
    pub acceleration_min_recent: usize,
    pub acceleration_min_earlier: usize,
    pub acceleration_min_ratio: f64,
    pub repeated_exit_window_secs: u64,
    pub repeated_exit_min_occurrences: usize,
    pub storm_window_secs: u64,
    pub storm_min_processes: usize,
    pub dependency_window_secs: u64,
    pub dependency_max_depth: usize,
    /// Processes for which no per-process finding is produced.
    ///
    /// Suppression is applied per process and leaves other processes alone. A suppressed
    /// process is also excluded from storm membership: naming it in a group finding would
    /// reintroduce exactly the noise the operator suppressed.
    pub suppressed: BTreeSet<String>,
}

impl Default for PatternConfig {
    fn default() -> Self {
        Self {
            crash_loop_window_secs: CRASH_LOOP_WINDOW_SECS,
            crash_loop_min_failures: CRASH_LOOP_MIN_FAILURES,
            crash_loop_max_mean_interval_secs: CRASH_LOOP_MAX_MEAN_INTERVAL_SECS,
            acceleration_window_secs: ACCELERATION_WINDOW_SECS,
            acceleration_min_recent: ACCELERATION_MIN_RECENT,
            acceleration_min_earlier: ACCELERATION_MIN_EARLIER,
            acceleration_min_ratio: ACCELERATION_MIN_RATIO,
            repeated_exit_window_secs: REPEATED_EXIT_WINDOW_SECS,
            repeated_exit_min_occurrences: REPEATED_EXIT_MIN_OCCURRENCES,
            storm_window_secs: STORM_WINDOW_SECS,
            storm_min_processes: STORM_MIN_PROCESSES,
            dependency_window_secs: DEPENDENCY_WINDOW_SECS,
            dependency_max_depth: DEPENDENCY_MAX_DEPTH,
            suppressed: BTreeSet::new(),
        }
    }
}

/// Which detector produced a finding.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum Detector {
    CrashLoop,
    RestartAcceleration,
    RepeatedExit,
    RestartStorm,
    DependencyCorrelation,
}

impl Detector {}

/// A reference back to one event a finding rests on.
///
/// Findings cite events rather than embedding them so the record stays small, and so a reviewer
/// can go back to the event stream and check the claim. A finding that cannot show its working
/// is not reviewable after the fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRef {
    pub at_secs: u64,
    pub process: String,
    pub detail: String,
}

/// The window a detector applied, recorded so a finding can be recomputed from its own evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowRef {
    pub start_secs: u64,
    pub end_secs: u64,
}

/// Structured evidence behind a finding.
#[derive(Debug, Clone, PartialEq)]
pub struct Evidence {
    pub detector: Detector,
    pub window: WindowRef,
    /// The measured statistic, named. `("failures_in_window", 4.0)` and similar.
    pub observed: Vec<(&'static str, f64)>,
    pub events: Vec<EventRef>,
}

/// What shape was recognised.
///
/// Each variant carries the numbers behind it, because "crash looping" without a rate is not
/// something an operator can check.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Pattern {
    /// Repeated failing exits at a high enough rate inside the crash-loop window.
    ///
    /// Naming only. `crash_loop_limit_reached` remains the thing that stops the process.
    CrashLoop {
        failures: usize,
        mean_interval_secs: f64,
    },
    /// The recent half of the window failed measurably faster than the earlier half.
    RestartAcceleration {
        earlier_per_hour: f64,
        recent_per_hour: f64,
        ratio: f64,
    },
    /// The same failing exit status, repeatedly: a deterministic fault rather than a flaky one.
    RepeatedExit {
        status: ExitStatus,
        occurrences: usize,
    },
    /// Several distinct processes in one namespace failed inside the storm window.
    RestartStorm {
        namespace: String,
        processes: Vec<String>,
    },
    /// A declared dependency failed shortly before this process did.
    ///
    /// A correlation in time along a declared edge. NOT a causal claim: the dependency may be a
    /// victim of the same cause, or unrelated and merely unlucky.
    DependencyCorrelation {
        dependency: String,
        depth: usize,
        lag_secs: u64,
    },
}

/// One recognised shape, with its subject and evidence.
///
/// `confidence` is in [0,1] and computed by a stated formula (see [`ratio_confidence`]). It is
/// reproducible rather than objectively meaningful: its purpose is letting an operator see why
/// one finding ranks above another.
#[derive(Debug, Clone, PartialEq)]
pub struct PatternFinding {
    /// The process, or `None` for a group finding such as a storm.
    pub process: Option<String>,
    pub pattern: Pattern,
    pub confidence: f64,
    pub evidence: Evidence,
    /// One-line operator-facing description.
    pub summary: String,
}

/// Why a detector could not reach a conclusion.
///
/// Distinguishing this from "nothing found" is the point: a process with one crash is not
/// crash-looping, but neither has it been shown healthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InconclusiveReason {
    /// Fewer events in the window than the detector's minimum evidence.
    InsufficientEvidence,
    /// Events exist but not in the comparison slot the detector needs, e.g. an accelerating
    /// process with no earlier failures to compare against.
    NoBaseline,
}

/// A detector that declined to answer for a subject, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Inconclusive {
    pub process: Option<String>,
    pub detector: Detector,
    pub reason: InconclusiveReason,
    /// How many qualifying events were seen, against how many the detector needed.
    pub observed: usize,
    pub required: usize,
}

/// Everything one detection pass concluded.
///
/// Empty `findings` with a populated `inconclusive` means "not enough happened to tell". Both
/// empty means no failure events in the window at all, which is the closest this module comes
/// to reporting health, and is still only a statement about the window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FailureReport {
    pub findings: Vec<PatternFinding>,
    pub inconclusive: Vec<Inconclusive>,
}

impl FailureReport {
    /// Whether any shape was recognised.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.findings.is_empty()
    }
}

/// Confidence from how far past a threshold a ratio sits, plus how much evidence backs it.
///
/// Two named terms, fixed weights: 0.7 for exceedance (saturating at three times the
/// threshold, so a runaway ratio cannot buy certainty) and 0.3 for evidence depth (saturating
/// at three times the minimum). Capped at 0.95, because a finding from a handful of events in a
/// bounded window is never a certainty.
fn ratio_confidence(observed: f64, threshold: f64, count: usize, min_count: usize) -> f64 {
    let exceedance = if threshold <= 0.0 {
        1.0
    } else {
        ((observed / threshold - 1.0) / 2.0).clamp(0.0, 1.0)
    };
    let depth = if min_count == 0 {
        1.0
    } else {
        ((usize_to_f64(count) / usize_to_f64(min_count) - 1.0) / 2.0).clamp(0.0, 1.0)
    };
    (0.5 + 0.7 * exceedance * 0.5 + 0.3 * depth * 0.5).min(0.95)
}

/// What one detector concluded about one subject.
///
/// Three answers, not two. [`DetectorOutcome::NotPresent`] means the detector had enough
/// evidence and the shape does not hold; [`DetectorOutcome::Inconclusive`] means it did not have
/// enough to say. Collapsing them would report an unmonitored process as a healthy one.
#[derive(Debug, Clone, PartialEq)]
pub enum DetectorOutcome {
    Detected(PatternFinding),
    NotPresent,
    Inconclusive(Inconclusive),
}

/// One failing event paired with the status that made it a failure.
///
/// Carried as a pair so a detector never has to re-check `is_failure` and never has to unwrap an
/// `Option` it has already proven is `Some`.
type FailureHit<'a> = (&'a FailureEvent, &'a ExitStatus);

/// Failing exits for one process inside `[now - window, now]`, oldest first.
///
/// Input order is not trusted: an unsorted stream that produced a negative interval would give a
/// wrong rate rather than an error, so this sorts every time. The cost is one sort per detector
/// call over a bounded window, which is cheap next to being quietly wrong.
fn failures_for<'a>(
    events: &'a [FailureEvent],
    process: &str,
    window: WindowRef,
) -> Vec<FailureHit<'a>> {
    let mut hits: Vec<FailureHit<'_>> = events
        .iter()
        .filter(|event| event.process == process)
        .filter(|event| event.at_secs >= window.start_secs && event.at_secs <= window.end_secs)
        .filter_map(|event| event.failure_status().map(|status| (event, status)))
        .collect();
    hits.sort_by_key(|(event, _)| event.at_secs);
    hits
}

/// A window of `span` seconds ending at `now`, clamped at the epoch.
fn window_ending_at(now: u64, span: u64) -> WindowRef {
    WindowRef {
        start_secs: now.saturating_sub(span),
        end_secs: now,
    }
}

fn event_refs(hits: &[FailureHit<'_>]) -> Vec<EventRef> {
    hits.iter()
        .map(|(event, status)| EventRef {
            at_secs: event.at_secs,
            process: event.process.clone(),
            detail: status.label(),
        })
        .collect()
}

fn insufficient(
    process: Option<String>,
    detector: Detector,
    observed: usize,
    required: usize,
) -> DetectorOutcome {
    DetectorOutcome::Inconclusive(Inconclusive {
        process,
        detector,
        reason: InconclusiveReason::InsufficientEvidence,
        observed,
        required,
    })
}

/// Names the crash-loop *shape*: repeated failing exits, close together, right now.
///
/// Two independent conditions, both required. A count alone would call three failures spread
/// over the whole window a loop; a mean interval alone would call two adjacent failures a loop.
/// Neither is a rate.
///
/// This does not stop anything and cannot: `crash_loop_limit_reached` in
/// `src/process_manager/restart.rs` is the control, and it is untouched by this module. The value
/// here is that an operator gets the word "crash loop" with a rate attached instead of a wall of
/// individual crash events.
///
/// A loop that has stopped stops being reported because the window ends at `now`: once the
/// failures age past `crash_loop_window_secs` the count falls below the minimum and this returns
/// [`DetectorOutcome::Inconclusive`], never a stale finding.
pub fn detect_crash_loop(
    events: &[FailureEvent],
    process: &str,
    now: u64,
    config: &PatternConfig,
) -> DetectorOutcome {
    if config.suppressed.contains(process) {
        return DetectorOutcome::NotPresent;
    }
    let window = window_ending_at(now, config.crash_loop_window_secs);
    let hits = failures_for(events, process, window);
    let min = config.crash_loop_min_failures.max(2);
    if hits.len() < min {
        return insufficient(
            Some(process.to_string()),
            Detector::CrashLoop,
            hits.len(),
            min,
        );
    }

    let first = hits[0].0.at_secs;
    let last = hits[hits.len() - 1].0.at_secs;
    let gaps = usize_to_f64(hits.len() - 1);
    let mean_interval = u64_to_f64(last.saturating_sub(first)) / gaps;
    if mean_interval > u64_to_f64(config.crash_loop_max_mean_interval_secs) {
        return DetectorOutcome::NotPresent;
    }

    let confidence = ratio_confidence(
        u64_to_f64(config.crash_loop_max_mean_interval_secs) / mean_interval.max(1.0),
        1.0,
        hits.len(),
        min,
    );
    DetectorOutcome::Detected(PatternFinding {
        process: Some(process.to_string()),
        pattern: Pattern::CrashLoop {
            failures: hits.len(),
            mean_interval_secs: mean_interval,
        },
        confidence,
        summary: format!(
            "{process} failed {} times in the last {}s, about every {mean_interval:.0}s",
            hits.len(),
            config.crash_loop_window_secs
        ),
        evidence: Evidence {
            detector: Detector::CrashLoop,
            window,
            observed: vec![
                ("failures_in_window", usize_to_f64(hits.len())),
                ("mean_interval_secs", mean_interval),
                (
                    "max_mean_interval_secs",
                    u64_to_f64(config.crash_loop_max_mean_interval_secs),
                ),
            ],
            events: event_refs(&hits),
        },
    })
}

/// Compares failure frequency in the recent half of the window against the earlier half.
///
/// The halves are equal in duration, so the ratio of rates is the ratio of counts; the rates are
/// still reported per hour because that is the number an operator can reason about, and the spec
/// asks the finding to report both.
///
/// Reported *before* the crash-loop limit is reached, and independently of it: nothing here reads
/// `auto_restart_history` or the limit, so an accelerating process is named while it is still
/// being restarted normally.
///
/// No earlier failures is [`InconclusiveReason::NoBaseline`], not infinite acceleration. A first
/// burst of failures has nothing to be faster *than*.
pub fn detect_acceleration(
    events: &[FailureEvent],
    process: &str,
    now: u64,
    config: &PatternConfig,
) -> DetectorOutcome {
    if config.suppressed.contains(process) {
        return DetectorOutcome::NotPresent;
    }
    let span = config.acceleration_window_secs;
    let half = span / 2;
    if half == 0 {
        return insufficient(
            Some(process.to_string()),
            Detector::RestartAcceleration,
            0,
            1,
        );
    }
    let window = window_ending_at(now, span);
    let split = now.saturating_sub(half);
    let hits = failures_for(events, process, window);
    let (recent, earlier): (Vec<FailureHit<'_>>, Vec<FailureHit<'_>>) = hits
        .iter()
        .copied()
        .partition(|(event, _)| event.at_secs > split);

    if recent.len() < config.acceleration_min_recent {
        return insufficient(
            Some(process.to_string()),
            Detector::RestartAcceleration,
            recent.len(),
            config.acceleration_min_recent,
        );
    }
    if earlier.len() < config.acceleration_min_earlier.max(1) {
        return DetectorOutcome::Inconclusive(Inconclusive {
            process: Some(process.to_string()),
            detector: Detector::RestartAcceleration,
            reason: InconclusiveReason::NoBaseline,
            observed: earlier.len(),
            required: config.acceleration_min_earlier.max(1),
        });
    }

    let hours = u64_to_f64(half) / 3600.0;
    let recent_per_hour = usize_to_f64(recent.len()) / hours;
    let earlier_per_hour = usize_to_f64(earlier.len()) / hours;
    let ratio = recent_per_hour / earlier_per_hour;
    if ratio < config.acceleration_min_ratio {
        return DetectorOutcome::NotPresent;
    }

    let cited: Vec<FailureHit<'_>> = hits.to_vec();
    DetectorOutcome::Detected(PatternFinding {
        process: Some(process.to_string()),
        pattern: Pattern::RestartAcceleration {
            earlier_per_hour,
            recent_per_hour,
            ratio,
        },
        confidence: ratio_confidence(
            ratio,
            config.acceleration_min_ratio,
            recent.len(),
            config.acceleration_min_recent,
        ),
        summary: format!(
            "{process} is failing faster: {recent_per_hour:.1}/h now against \
             {earlier_per_hour:.1}/h earlier in the window"
        ),
        evidence: Evidence {
            detector: Detector::RestartAcceleration,
            window,
            observed: vec![
                ("recent_per_hour", recent_per_hour),
                ("earlier_per_hour", earlier_per_hour),
                ("ratio", ratio),
                ("min_ratio", config.acceleration_min_ratio),
                ("split_at_secs", u64_to_f64(split)),
            ],
            events: event_refs(&cited),
        },
    })
}

/// Finds the same failing exit status repeated inside the window.
///
/// Distinct from a crash loop by design: a loop is about *rate*, this is about *sameness*. Three
/// `exit code 1`s an hour apart is not a loop but is very likely a deterministic fault that will
/// happen again on the next start, and an operator wants those named differently.
///
/// Only the most frequent qualifying status is reported. A process with three `SIGSEGV`s and
/// three `exit code 2`s is not showing one deterministic fault, and emitting two findings for one
/// process would imply it is.
///
/// Clean exits never qualify, and [`ExitStatus::Unknown`] never matches itself, so an
/// unreadable exit repeated cannot masquerade as a deterministic fault.
pub fn detect_repeated_exit(
    events: &[FailureEvent],
    process: &str,
    now: u64,
    config: &PatternConfig,
) -> DetectorOutcome {
    if config.suppressed.contains(process) {
        return DetectorOutcome::NotPresent;
    }
    let window = window_ending_at(now, config.repeated_exit_window_secs);
    let hits = failures_for(events, process, window);
    let min = config.repeated_exit_min_occurrences.max(2);
    if hits.len() < min {
        return insufficient(
            Some(process.to_string()),
            Detector::RepeatedExit,
            hits.len(),
            min,
        );
    }

    let mut best: Option<(ExitStatus, Vec<FailureHit<'_>>)> = None;
    for (_, status) in &hits {
        if !status.same_fault(status) {
            continue;
        }
        let matching: Vec<FailureHit<'_>> = hits
            .iter()
            .filter(|(_, other)| status.same_fault(other))
            .copied()
            .collect();
        if best
            .as_ref()
            .is_none_or(|(_, found)| matching.len() > found.len())
        {
            best = Some(((*status).clone(), matching));
        }
    }

    let Some((status, matching)) = best.filter(|(_, found)| found.len() >= min) else {
        return DetectorOutcome::NotPresent;
    };

    DetectorOutcome::Detected(PatternFinding {
        process: Some(process.to_string()),
        pattern: Pattern::RepeatedExit {
            status: status.clone(),
            occurrences: matching.len(),
        },
        confidence: ratio_confidence(
            usize_to_f64(matching.len()),
            usize_to_f64(min),
            matching.len(),
            min,
        ),
        summary: format!(
            "{process} exited with the same {} {} times: a deterministic fault, not a flaky one",
            status.label(),
            matching.len()
        ),
        evidence: Evidence {
            detector: Detector::RepeatedExit,
            window,
            observed: vec![
                ("occurrences", usize_to_f64(matching.len())),
                ("min_occurrences", usize_to_f64(min)),
                ("distinct_failures_in_window", usize_to_f64(hits.len())),
            ],
            events: event_refs(&matching),
        },
    })
}

/// Finds namespaces where several distinct processes failed inside one short window.
///
/// One finding per namespace, never one per process: the whole claim of a storm is that these
/// failures share a cause, and splitting it into per-process findings would bury that.
///
/// Counted by **distinct process**, which is what stops a single process's crash loop from being
/// reported as a storm. That process is still picked up by [`detect_crash_loop`] and friends.
///
/// Scoped by namespace because a storm confined to one namespace is a different operational
/// story from one spanning the host, and reporting them as one group would lose that.
///
/// The scan is a sliding window over sorted events rather than a window ending at `now`, because
/// a storm that happened four minutes ago is still the thing an operator is looking at. It is
/// bounded to `retain_secs` before `now` so a storm from yesterday does not resurface for ever.
pub fn detect_storms(
    events: &[FailureEvent],
    now: u64,
    retain_secs: u64,
    config: &PatternConfig,
) -> Vec<PatternFinding> {
    let horizon = now.saturating_sub(retain_secs);
    let mut by_namespace: BTreeMap<&str, Vec<FailureHit<'_>>> = BTreeMap::new();
    for event in events {
        if event.at_secs < horizon || event.at_secs > now {
            continue;
        }
        if config.suppressed.contains(&event.process) {
            continue;
        }
        if let Some(status) = event.failure_status() {
            by_namespace
                .entry(event.namespace.as_str())
                .or_default()
                .push((event, status));
        }
    }

    let min = config.storm_min_processes.max(2);
    let mut findings = Vec::new();
    for (namespace, mut hits) in by_namespace {
        hits.sort_by_key(|(event, _)| event.at_secs);
        // Widest set of distinct processes visible through one sliding window. `best` keeps the
        // strongest burst rather than the first, so a namespace reports its worst moment.
        let mut queue: VecDeque<FailureHit<'_>> = VecDeque::new();
        let mut best: Option<Vec<FailureHit<'_>>> = None;
        for hit in hits {
            queue.push_back(hit);
            let cutoff = hit.0.at_secs.saturating_sub(config.storm_window_secs);
            while queue
                .front()
                .is_some_and(|(event, _)| event.at_secs < cutoff)
            {
                queue.pop_front();
            }
            let distinct: BTreeSet<&str> = queue
                .iter()
                .map(|(event, _)| event.process.as_str())
                .collect();
            if distinct.len() >= min {
                let candidate: Vec<FailureHit<'_>> = queue.iter().copied().collect();
                let better = best.as_ref().is_none_or(|found| {
                    let found_distinct: BTreeSet<&str> = found
                        .iter()
                        .map(|(event, _)| event.process.as_str())
                        .collect();
                    distinct.len() > found_distinct.len()
                });
                if better {
                    best = Some(candidate);
                }
            }
        }

        let Some(burst) = best else { continue };
        let processes: Vec<String> = burst
            .iter()
            .map(|(event, _)| event.process.clone())
            .collect::<BTreeSet<String>>()
            .into_iter()
            .collect();
        let first = burst[0].0.at_secs;
        let last = burst[burst.len() - 1].0.at_secs;
        findings.push(PatternFinding {
            process: None,
            pattern: Pattern::RestartStorm {
                namespace: namespace.to_string(),
                processes: processes.clone(),
            },
            confidence: ratio_confidence(
                usize_to_f64(processes.len()),
                usize_to_f64(min),
                processes.len(),
                min,
            ),
            summary: format!(
                "{} processes in namespace {namespace} failed within {}s: {}",
                processes.len(),
                config.storm_window_secs,
                processes.join(", ")
            ),
            evidence: Evidence {
                detector: Detector::RestartStorm,
                window: WindowRef {
                    start_secs: first,
                    end_secs: last,
                },
                observed: vec![
                    ("distinct_processes", usize_to_f64(processes.len())),
                    ("min_processes", usize_to_f64(min)),
                    ("storm_window_secs", u64_to_f64(config.storm_window_secs)),
                ],
                events: event_refs(&burst),
            },
        });
    }
    findings
}

/// Declared `depends_on` edges: process name to the names it declares.
///
/// A plain map rather than a borrow of `ManagedProcess`, so correlation is testable and so this
/// module does not depend on `depends_on` having landed on the runtime record yet. A name with no
/// entry has no declared dependencies and therefore correlates with nothing.
///
/// Permanent API (workspace-crate-layout phase 5): test-only visibility does not
/// cross a crate boundary; `dependency_graph` returns this type.
pub type DependencyGraph = BTreeMap<String, Vec<String>>;

/// One declared dependency and whether it names a currently managed process.
///
/// Carried as a pair rather than filtering the unresolved ones out, because an operator's typo and
/// a dependency they never declared must not look the same. An unresolved edge is retained,
/// reported, and simply correlates with nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredDependency {
    /// The name as declared.
    pub name: String,
    /// Whether a process by that name is currently managed.
    ///
    /// A point-in-time fact, not a permanent property: a dependency declared before the process it
    /// names exists resolves as soon as that process is added.
    pub resolved: bool,
}

/// Reports a process's declared dependencies, each marked resolved or not.
///
/// Permanent API (workspace-crate-layout phase 5): test-only visibility does not
/// cross a crate boundary and the manager's accessor (`declared_dependencies`)
/// returns this type; the findings endpoints are the production caller.
///
/// `managed` is the set of currently managed process names. Order follows the declaration, so the
/// report reads the way the operator wrote it; duplicates are preserved for the same reason.
pub fn resolve_dependencies(
    declared: &[String],
    managed: &BTreeSet<String>,
) -> Vec<DeclaredDependency> {
    declared
        .iter()
        .map(|name| DeclaredDependency {
            name: name.clone(),
            resolved: managed.contains(name),
        })
        .collect()
}

/// Builds a [`DependencyGraph`] from declared edges.
///
/// Unresolved names are KEPT as edges. Correlation walks them and finds no failures, which is the
/// correct outcome — dropping them here would silently narrow the graph and make the walk's depth
/// bound describe a different shape than the operator declared.
///
/// Permanent API (workspace-crate-layout phase 5): test-only visibility does not
/// cross a crate boundary; the manager's `dependency_graph` accessor returns this
/// type.
pub fn dependency_graph<'a>(
    declarations: impl IntoIterator<Item = (&'a str, &'a [String])>,
) -> DependencyGraph {
    declarations
        .into_iter()
        .filter(|(_, deps)| !deps.is_empty())
        .map(|(name, deps)| (name.to_string(), deps.to_vec()))
        .collect()
}

/// Reports declared dependencies that failed shortly before this process failed.
///
/// Walks **declared edges only**, breadth-first to `dependency_max_depth`. Two processes failing
/// together with no declared edge between them yield nothing: inferring an edge from co-timing is
/// how a correlation engine starts blaming the wrong service.
///
/// Bounded by `dependency_window_secs` before the subject's failure. A dependency that failed an
/// hour earlier and recovered is not a contributing cause of this failure, and the window is
/// recorded in the evidence so the claim can be re-checked.
///
/// **This is correlation, not causality.** The dependency may share a cause with the subject, or
/// be coincidental. The finding says "probable contributing cause" and the evidence gives the lag
/// so a human can judge; nothing here should be read as proof, and per the change's observe-only
/// stance nothing acts on it.
///
/// A cycle in the declared graph terminates via the visited set rather than recursing for ever.
#[cfg(test)]
pub fn correlate_dependencies(
    events: &[FailureEvent],
    process: &str,
    failed_at: u64,
    graph: &DependencyGraph,
    config: &PatternConfig,
) -> Vec<PatternFinding> {
    if config.suppressed.contains(process) {
        return Vec::new();
    }
    let window = WindowRef {
        start_secs: failed_at.saturating_sub(config.dependency_window_secs),
        end_secs: failed_at,
    };

    let mut findings = Vec::new();
    let mut visited: BTreeSet<String> = BTreeSet::from([process.to_string()]);
    let mut frontier: Vec<(String, usize)> = graph
        .get(process)
        .map(|names| names.iter().map(|name| (name.clone(), 1)).collect())
        .unwrap_or_default();

    while let Some((dependency, depth)) = frontier.pop() {
        if depth > config.dependency_max_depth || !visited.insert(dependency.clone()) {
            continue;
        }
        if let Some(next) = graph.get(&dependency) {
            for name in next {
                frontier.push((name.clone(), depth + 1));
            }
        }

        let hits = failures_for(events, &dependency, window);
        let Some((latest, status)) = hits.last() else {
            continue;
        };
        let lag = failed_at.saturating_sub(latest.at_secs);
        findings.push(PatternFinding {
            process: Some(process.to_string()),
            pattern: Pattern::DependencyCorrelation {
                dependency: dependency.clone(),
                depth,
                lag_secs: lag,
            },
            // Depth erodes confidence: a direct dependency's failure is a better explanation
            // than one two edges away, and the number should say so.
            confidence: (0.6 / usize_to_f64(depth)).min(0.95),
            summary: format!(
                "{process} failed {lag}s after its dependency {dependency} ({}) — a correlation \
                 within {}s, not a proven cause",
                status.label(),
                config.dependency_window_secs
            ),
            evidence: Evidence {
                detector: Detector::DependencyCorrelation,
                window,
                observed: vec![
                    ("depth", usize_to_f64(depth)),
                    ("lag_secs", u64_to_f64(lag)),
                    ("window_secs", u64_to_f64(config.dependency_window_secs)),
                    ("dependency_failures_in_window", usize_to_f64(hits.len())),
                ],
                events: event_refs(&hits),
            },
        });
    }

    findings.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    findings
}

/// Runs every per-process detector plus storms over one event slice.
///
/// Subjects are taken from the events themselves, so a process that has produced nothing is not
/// reported at all — there is no basis for saying anything about it, healthy or otherwise.
///
/// Detector order is fixed (crash loop, acceleration, repeated exit, then storms) so two runs
/// over the same input produce byte-identical output. A caller ranking findings gets a stable
/// order rather than whichever detector happened to finish first.
///
/// `retain_secs` bounds how far back storms are looked for; per-process detectors use their own
/// windows ending at `now`.
pub fn analyse(
    events: &[FailureEvent],
    now: u64,
    retain_secs: u64,
    config: &PatternConfig,
) -> FailureReport {
    let mut report = FailureReport::default();
    let subjects: BTreeSet<&str> = events
        .iter()
        .filter(|event| event.at_secs <= now)
        .map(|event| event.process.as_str())
        .collect();

    for subject in subjects {
        if config.suppressed.contains(subject) {
            continue;
        }
        for outcome in [
            detect_crash_loop(events, subject, now, config),
            detect_acceleration(events, subject, now, config),
            detect_repeated_exit(events, subject, now, config),
        ] {
            match outcome {
                DetectorOutcome::Detected(finding) => report.findings.push(finding),
                DetectorOutcome::Inconclusive(entry) => report.inconclusive.push(entry),
                DetectorOutcome::NotPresent => {}
            }
        }
    }
    report
        .findings
        .extend(detect_storms(events, now, retain_secs, config));
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000;

    fn exit(at_secs: u64, process: &str, code: i32) -> FailureEvent {
        FailureEvent {
            at_secs,
            process: process.to_string(),
            namespace: "default".to_string(),
            kind: FailureEventKind::Exited {
                status: ExitStatus::Code(code),
            },
        }
    }

    fn exit_in(at_secs: u64, process: &str, namespace: &str, code: i32) -> FailureEvent {
        FailureEvent {
            namespace: namespace.to_string(),
            ..exit(at_secs, process, code)
        }
    }

    fn started(at_secs: u64, process: &str) -> FailureEvent {
        FailureEvent {
            at_secs,
            process: process.to_string(),
            namespace: "default".to_string(),
            kind: FailureEventKind::Started,
        }
    }

    fn detected(outcome: DetectorOutcome) -> PatternFinding {
        match outcome {
            DetectorOutcome::Detected(finding) => finding,
            other => panic!("expected a finding, got {other:?}"),
        }
    }

    /// Four failures 30s apart: well inside the window and far under the mean-interval cap.
    #[test]
    fn tight_crash_loop_is_named() {
        let events = vec![
            exit(NOW - 90, "api", 1),
            exit(NOW - 60, "api", 1),
            exit(NOW - 30, "api", 1),
            exit(NOW, "api", 1),
        ];
        let finding = detected(detect_crash_loop(
            &events,
            "api",
            NOW,
            &PatternConfig::default(),
        ));
        match finding.pattern {
            Pattern::CrashLoop {
                failures,
                mean_interval_secs,
            } => {
                assert_eq!(failures, 4);
                assert!((mean_interval_secs - 30.0).abs() < f64::EPSILON);
            }
            other => panic!("wrong pattern: {other:?}"),
        }
        assert_eq!(finding.evidence.events.len(), 4);
        assert!(finding.confidence > 0.5 && finding.confidence <= 0.95);
    }

    /// One crash is not a loop, and must not read as a clean bill of health either.
    #[test]
    fn single_crash_is_inconclusive_not_absent() {
        let events = vec![started(NOW - 600, "api"), exit(NOW - 10, "api", 1)];
        match detect_crash_loop(&events, "api", NOW, &PatternConfig::default()) {
            DetectorOutcome::Inconclusive(entry) => {
                assert_eq!(entry.reason, InconclusiveReason::InsufficientEvidence);
                assert_eq!(entry.observed, 1);
                assert_eq!(entry.required, CRASH_LOOP_MIN_FAILURES);
            }
            other => panic!("expected inconclusive, got {other:?}"),
        }
    }

    /// Two failures are a coincidence. This asserts the minimum-evidence rule directly, since
    /// two adjacent failures would otherwise satisfy the mean-interval test easily.
    #[test]
    fn two_failures_are_not_a_pattern() {
        let events = vec![exit(NOW - 20, "api", 1), exit(NOW, "api", 1)];
        assert!(matches!(
            detect_crash_loop(&events, "api", NOW, &PatternConfig::default()),
            DetectorOutcome::Inconclusive(_)
        ));
    }

    /// Three failures inside the window but ~140s apart: enough events, too slow to be a loop.
    /// Distinguishes the rate test from the count test.
    #[test]
    fn widely_spaced_failures_are_not_a_loop() {
        let events = vec![
            exit(NOW - 280, "api", 1),
            exit(NOW - 140, "api", 1),
            exit(NOW, "api", 1),
        ];
        assert_eq!(
            detect_crash_loop(&events, "api", NOW, &PatternConfig::default()),
            DetectorOutcome::NotPresent
        );
    }

    /// A loop that ended an hour ago is not a loop now. This is the whole clearing mechanism.
    #[test]
    fn a_stopped_loop_stops_being_reported() {
        let long_ago = NOW - 3600;
        let events = vec![
            exit(long_ago, "api", 1),
            exit(long_ago + 30, "api", 1),
            exit(long_ago + 60, "api", 1),
            exit(long_ago + 90, "api", 1),
        ];
        // Detected while it was happening...
        assert!(matches!(
            detect_crash_loop(&events, "api", long_ago + 90, &PatternConfig::default()),
            DetectorOutcome::Detected(_)
        ));
        // ...and silent an hour later, from the identical event slice.
        assert!(matches!(
            detect_crash_loop(&events, "api", NOW, &PatternConfig::default()),
            DetectorOutcome::Inconclusive(_)
        ));
    }

    #[test]
    fn empty_sequence_concludes_nothing() {
        let report = analyse(&[], NOW, 86_400, &PatternConfig::default());
        assert!(report.findings.is_empty());
        assert!(report.inconclusive.is_empty());
    }

    /// Starts and clean exits must not be counted as failures.
    #[test]
    fn starts_and_clean_exits_are_ignored() {
        let events = vec![
            started(NOW - 90, "api"),
            exit(NOW - 60, "api", 0),
            started(NOW - 55, "api"),
            exit(NOW - 30, "api", 0),
            started(NOW - 25, "api"),
        ];
        assert!(matches!(
            detect_crash_loop(&events, "api", NOW, &PatternConfig::default()),
            DetectorOutcome::Inconclusive(_)
        ));
        // Zero failures is inconclusive, not a clean bill of health: a process that has only
        // ever exited cleanly in this window has still not been shown to be free of a
        // deterministic fault.
        match detect_repeated_exit(&events, "api", NOW, &PatternConfig::default()) {
            DetectorOutcome::Inconclusive(entry) => assert_eq!(entry.observed, 0),
            other => panic!("expected inconclusive, got {other:?}"),
        }
    }

    /// Unsorted input must give the same answer as sorted input, not a negative interval.
    #[test]
    fn unsorted_input_is_handled() {
        let sorted = vec![
            exit(NOW - 90, "api", 1),
            exit(NOW - 60, "api", 1),
            exit(NOW - 30, "api", 1),
        ];
        let shuffled = vec![
            exit(NOW - 30, "api", 1),
            exit(NOW - 90, "api", 1),
            exit(NOW - 60, "api", 1),
        ];
        let config = PatternConfig::default();
        assert_eq!(
            detect_crash_loop(&sorted, "api", NOW, &config),
            detect_crash_loop(&shuffled, "api", NOW, &config)
        );
    }
    /// Recent half four times the earlier half's rate.
    #[test]
    fn accelerating_restarts_are_reported_with_both_rates() {
        let mut events = vec![exit(NOW - 1500, "api", 1)];
        for offset in [300, 200, 100, 20] {
            events.push(exit(NOW - offset, "api", 1));
        }
        let finding = detected(detect_acceleration(
            &events,
            "api",
            NOW,
            &PatternConfig::default(),
        ));
        match finding.pattern {
            Pattern::RestartAcceleration {
                earlier_per_hour,
                recent_per_hour,
                ratio,
            } => {
                assert!(recent_per_hour > earlier_per_hour);
                assert!((ratio - 4.0).abs() < 1e-9, "ratio was {ratio}");
            }
            other => panic!("wrong pattern: {other:?}"),
        }
    }

    /// Equal counts in each half of the window: a steady rate, ratio 1.0.
    #[test]
    fn steady_restart_rate_is_not_acceleration() {
        let events = vec![
            // Earlier half (before NOW - 900).
            exit(NOW - 1700, "api", 1),
            exit(NOW - 1400, "api", 1),
            exit(NOW - 1000, "api", 1),
            // Recent half.
            exit(NOW - 800, "api", 1),
            exit(NOW - 500, "api", 1),
            exit(NOW - 200, "api", 1),
        ];
        assert_eq!(
            detect_acceleration(&events, "api", NOW, &PatternConfig::default()),
            DetectorOutcome::NotPresent
        );
    }

    /// 3 recent against 2 earlier is a ratio of 1.5: rising, but under the 2.0 threshold.
    /// Proves the threshold is actually consulted rather than every increase being reported.
    #[test]
    fn a_mild_increase_is_under_the_threshold() {
        let events = vec![
            exit(NOW - 1700, "api", 1),
            exit(NOW - 1200, "api", 1),
            exit(NOW - 700, "api", 1),
            exit(NOW - 400, "api", 1),
            exit(NOW - 100, "api", 1),
        ];
        assert_eq!(
            detect_acceleration(&events, "api", NOW, &PatternConfig::default()),
            DetectorOutcome::NotPresent
        );
    }

    /// A burst with nothing earlier has no baseline: not "infinitely accelerating".
    #[test]
    fn a_first_burst_has_no_baseline() {
        let events = vec![
            exit(NOW - 120, "api", 1),
            exit(NOW - 60, "api", 1),
            exit(NOW - 10, "api", 1),
        ];
        match detect_acceleration(&events, "api", NOW, &PatternConfig::default()) {
            DetectorOutcome::Inconclusive(entry) => {
                assert_eq!(entry.reason, InconclusiveReason::NoBaseline);
            }
            other => panic!("expected no baseline, got {other:?}"),
        }
    }

    #[test]
    fn one_restart_after_stability_is_not_acceleration() {
        let events = vec![exit(NOW - 30, "api", 1)];
        assert!(matches!(
            detect_acceleration(&events, "api", NOW, &PatternConfig::default()),
            DetectorOutcome::Inconclusive(_)
        ));
    }

    #[test]
    fn repeated_identical_exit_reports_code_and_count() {
        let events = vec![
            exit(NOW - 900, "api", 3),
            exit(NOW - 600, "api", 3),
            exit(NOW - 60, "api", 3),
        ];
        let finding = detected(detect_repeated_exit(
            &events,
            "api",
            NOW,
            &PatternConfig::default(),
        ));
        match &finding.pattern {
            Pattern::RepeatedExit {
                status,
                occurrences,
            } => {
                assert_eq!(status, &ExitStatus::Code(3));
                assert_eq!(*occurrences, 3);
            }
            other => panic!("wrong pattern: {other:?}"),
        }
        assert_eq!(finding.evidence.events.len(), 3);
        assert!(
            finding
                .evidence
                .events
                .iter()
                .all(|e| e.detail == "exit code 3")
        );
    }

    /// Distinguishes repeated-exit from crash-loop: same rate, differing codes, no finding.
    #[test]
    fn varying_exit_codes_are_not_a_repeated_exit() {
        let events = vec![
            exit(NOW - 900, "api", 1),
            exit(NOW - 600, "api", 2),
            exit(NOW - 60, "api", 3),
        ];
        assert_eq!(
            detect_repeated_exit(&events, "api", NOW, &PatternConfig::default()),
            DetectorOutcome::NotPresent
        );
    }

    /// "We don't know" three times is not a deterministic fault.
    #[test]
    fn repeated_unknown_exits_are_not_a_deterministic_fault() {
        let unknown = |at_secs: u64| FailureEvent {
            at_secs,
            process: "api".to_string(),
            namespace: "default".to_string(),
            kind: FailureEventKind::Exited {
                status: ExitStatus::Unknown,
            },
        };
        let events = vec![unknown(NOW - 600), unknown(NOW - 300), unknown(NOW - 10)];
        assert_eq!(
            detect_repeated_exit(&events, "api", NOW, &PatternConfig::default()),
            DetectorOutcome::NotPresent
        );
    }

    #[test]
    fn signal_exits_compare_by_name() {
        let sig = |at_secs: u64, name: &str| FailureEvent {
            at_secs,
            process: "api".to_string(),
            namespace: "default".to_string(),
            kind: FailureEventKind::Exited {
                status: ExitStatus::Signal(name.to_string()),
            },
        };
        let same = vec![
            sig(NOW - 600, "SIGSEGV"),
            sig(NOW - 300, "SIGSEGV"),
            sig(NOW - 10, "SIGSEGV"),
        ];
        assert!(matches!(
            detect_repeated_exit(&same, "api", NOW, &PatternConfig::default()),
            DetectorOutcome::Detected(_)
        ));
        let mixed = vec![
            sig(NOW - 600, "SIGSEGV"),
            sig(NOW - 300, "SIGKILL"),
            sig(NOW - 10, "SIGTERM"),
        ];
        assert_eq!(
            detect_repeated_exit(&mixed, "api", NOW, &PatternConfig::default()),
            DetectorOutcome::NotPresent
        );
    }

    #[test]
    fn three_processes_failing_together_are_one_storm_finding() {
        let events = vec![
            exit(NOW - 20, "api", 1),
            exit(NOW - 15, "worker", 1),
            exit(NOW - 5, "cache", 1),
        ];
        let storms = detect_storms(&events, NOW, 86_400, &PatternConfig::default());
        assert_eq!(storms.len(), 1);
        match &storms[0].pattern {
            Pattern::RestartStorm {
                namespace,
                processes,
            } => {
                assert_eq!(namespace, "default");
                assert_eq!(processes, &["api", "cache", "worker"]);
            }
            other => panic!("wrong pattern: {other:?}"),
        }
        assert!(storms[0].process.is_none(), "a storm is a group finding");
    }

    /// The defining non-storm: one process, many failures, well inside the window.
    #[test]
    fn one_process_failing_repeatedly_is_never_a_storm() {
        let events = vec![
            exit(NOW - 40, "api", 1),
            exit(NOW - 30, "api", 1),
            exit(NOW - 20, "api", 1),
            exit(NOW - 10, "api", 1),
        ];
        assert!(detect_storms(&events, NOW, 86_400, &PatternConfig::default()).is_empty());
        // ...but the single-process detector still fires for it.
        assert!(matches!(
            detect_crash_loop(&events, "api", NOW, &PatternConfig::default()),
            DetectorOutcome::Detected(_)
        ));
    }

    #[test]
    fn restarts_spread_over_time_are_not_a_storm() {
        let events = vec![
            exit(NOW - 5000, "api", 1),
            exit(NOW - 2500, "worker", 1),
            exit(NOW - 10, "cache", 1),
        ];
        assert!(detect_storms(&events, NOW, 86_400, &PatternConfig::default()).is_empty());
    }

    /// Namespaces are judged separately, so a storm in one is not diluted by quiet in another.
    #[test]
    fn storms_are_scoped_per_namespace() {
        let events = vec![
            exit_in(NOW - 20, "api", "prod", 1),
            exit_in(NOW - 15, "worker", "prod", 1),
            exit_in(NOW - 10, "cache", "prod", 1),
            exit_in(NOW - 12, "sandbox", "dev", 1),
        ];
        let storms = detect_storms(&events, NOW, 86_400, &PatternConfig::default());
        assert_eq!(storms.len(), 1);
        match &storms[0].pattern {
            Pattern::RestartStorm { namespace, .. } => assert_eq!(namespace, "prod"),
            other => panic!("wrong pattern: {other:?}"),
        }
    }

    fn graph(edges: &[(&str, &[&str])]) -> DependencyGraph {
        edges
            .iter()
            .map(|(name, deps)| {
                (
                    name.to_string(),
                    deps.iter().map(|d| d.to_string()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn a_preceding_dependency_failure_is_correlated() {
        let events = vec![exit(NOW - 30, "db", 1), exit(NOW, "api", 1)];
        let graph = graph(&[("api", &["db"])]);
        let found = correlate_dependencies(&events, "api", NOW, &graph, &PatternConfig::default());
        assert_eq!(found.len(), 1);
        match &found[0].pattern {
            Pattern::DependencyCorrelation {
                dependency,
                depth,
                lag_secs,
            } => {
                assert_eq!(dependency, "db");
                assert_eq!(*depth, 1);
                assert_eq!(*lag_secs, 30);
            }
            other => panic!("wrong pattern: {other:?}"),
        }
        assert!(!found[0].evidence.events.is_empty(), "must cite the events");
        assert!(found[0].summary.contains("not a proven cause"));
    }

    #[test]
    fn a_healthy_dependency_is_not_implicated() {
        let events = vec![exit(NOW, "api", 1)];
        let graph = graph(&[("api", &["db"])]);
        assert!(
            correlate_dependencies(&events, "api", NOW, &graph, &PatternConfig::default())
                .is_empty()
        );
    }

    /// Co-timing without a declared edge must infer nothing.
    #[test]
    fn undeclared_relationships_are_not_inferred() {
        let events = vec![exit(NOW - 5, "unrelated", 1), exit(NOW, "api", 1)];
        let graph = graph(&[("api", &[])]);
        assert!(
            correlate_dependencies(&events, "api", NOW, &graph, &PatternConfig::default())
                .is_empty()
        );
    }

    #[test]
    fn correlation_is_bounded_by_its_window() {
        let config = PatternConfig::default();
        let graph = graph(&[("api", &["db"])]);
        let long_before = vec![
            exit(NOW - config.dependency_window_secs - 60, "db", 1),
            exit(NOW, "api", 1),
        ];
        assert!(correlate_dependencies(&long_before, "api", NOW, &graph, &config).is_empty());
    }

    #[test]
    fn transitive_dependencies_stop_at_the_depth_limit() {
        // api -> db -> disk -> san; the depth limit is 2, so `san` is out of reach.
        let graph = graph(&[("api", &["db"]), ("db", &["disk"]), ("disk", &["san"])]);
        let events = vec![
            exit(NOW - 10, "db", 1),
            exit(NOW - 20, "disk", 1),
            exit(NOW - 30, "san", 1),
            exit(NOW, "api", 1),
        ];
        let found = correlate_dependencies(&events, "api", NOW, &graph, &PatternConfig::default());
        let named: BTreeSet<String> = found
            .iter()
            .map(|finding| match &finding.pattern {
                Pattern::DependencyCorrelation { dependency, .. } => dependency.clone(),
                other => panic!("wrong pattern: {other:?}"),
            })
            .collect();
        assert!(named.contains("db"));
        assert!(named.contains("disk"));
        assert!(!named.contains("san"), "depth limit must hold");
        // A direct dependency outranks a transitive one.
        assert!(found[0].confidence >= found[1].confidence);
    }

    /// A declared cycle must terminate rather than recurse.
    #[test]
    fn a_dependency_cycle_terminates() {
        let graph = graph(&[("api", &["db"]), ("db", &["api"])]);
        let events = vec![exit(NOW - 10, "db", 1), exit(NOW, "api", 1)];
        let found = correlate_dependencies(&events, "api", NOW, &graph, &PatternConfig::default());
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn suppression_is_per_process() {
        let events = vec![
            exit(NOW - 90, "api", 1),
            exit(NOW - 60, "api", 1),
            exit(NOW - 30, "api", 1),
            exit(NOW - 80, "worker", 1),
            exit(NOW - 50, "worker", 1),
            exit(NOW - 20, "worker", 1),
        ];
        let config = PatternConfig {
            suppressed: BTreeSet::from(["api".to_string()]),
            ..PatternConfig::default()
        };
        assert_eq!(
            detect_crash_loop(&events, "api", NOW, &config),
            DetectorOutcome::NotPresent
        );
        assert!(matches!(
            detect_crash_loop(&events, "worker", NOW, &config),
            DetectorOutcome::Detected(_)
        ));
        let report = analyse(&events, NOW, 86_400, &config);
        assert!(
            report
                .findings
                .iter()
                .all(|finding| finding.process.as_deref() != Some("api"))
        );
    }

    /// Two runs over the same input must agree exactly, or ranking is unstable.
    #[test]
    fn analyse_is_deterministic() {
        let events = vec![
            exit_in(NOW - 90, "api", "prod", 1),
            exit_in(NOW - 60, "api", "prod", 1),
            exit_in(NOW - 30, "api", "prod", 1),
            exit_in(NOW - 25, "worker", "prod", 1),
            exit_in(NOW - 20, "cache", "prod", 1),
        ];
        let config = PatternConfig::default();
        assert_eq!(
            analyse(&events, NOW, 86_400, &config),
            analyse(&events, NOW, 86_400, &config)
        );
    }

    /// Confidence must stay inside [0,1] and never reach certainty.
    #[test]
    fn confidence_is_bounded() {
        let mut events = Vec::new();
        for step in 0..40u64 {
            events.push(exit(NOW - 200 + step * 5, "api", 1));
        }
        let finding = detected(detect_crash_loop(
            &events,
            "api",
            NOW,
            &PatternConfig::default(),
        ));
        assert!(finding.confidence > 0.0 && finding.confidence <= 0.95);
    }

    /// A single failure must produce inconclusive entries, not silence.
    #[test]
    fn analyse_reports_inconclusive_for_thin_evidence() {
        let events = vec![exit(NOW - 10, "api", 1)];
        let report = analyse(&events, NOW, 86_400, &PatternConfig::default());
        assert!(report.is_empty());
        assert_eq!(report.inconclusive.len(), 3, "one per per-process detector");
    }
}
