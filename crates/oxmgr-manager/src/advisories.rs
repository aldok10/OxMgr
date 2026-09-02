//! Configuration risk advisories: internally inconsistent settings, and settings implausible against host capacity.
//!
//! Scaffold for OpenSpec change `resource-awareness`, tasks section(s) 1 and 2.
//!
//! Lint-level cleanup: display-path casts for GB formatting strings.
//! The contract is `openspec/changes/resource-awareness/specs/`; read it before adding to this file.
//!
//! Pure logic on purpose: nothing here reaches for the process manager, the HTTP layer or the
//! dashboard, so it can be unit-tested without a running daemon. Wiring it into those is a
//! separate integration step, which is why the module is currently unreferenced.
//!
//! Two classes of rule, kept apart because they fail differently:
//!
//! 1. **Internal consistency** — decidable from the configuration alone, on any machine, before
//!    the process has ever run. These are always evaluated.
//! 2. **Capacity-relative** — needs the host's measured capacity. When capacity is unknown the
//!    answer is "cannot say", never "fine": the class is withheld and the withholding is reported
//!    in [`AdvisoryReport::capacity`], rather than compared against an assumed or zero total.
//!
//! Every capacity figure here is the **host's**, per `docs/HOST-METRICS.md`. Inside a container
//! `total_memory` is typically the host's, not the cgroup limit, so a capacity advisory states the
//! figure it compared against instead of presenting it as the limit that will apply.
//!
//! Advisories are informational. Nothing in this module changes configuration, blocks a start, or
//! acts on a finding — acting is an explicit non-goal of this change.

use oxmgr_core::numeric::u64_to_f64;
use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::ecosystem::EcosystemProcessSpec;
use oxmgr_metrics::process::{HealthCheck, ManagedProcess, RestartPolicy, StartProcessSpec};

/// Restart budget at or below which a memory limit is treated as ending in an outage.
///
/// `run_resource_limit_checks` (`src/process_manager.rs:2000`) stops the process and marks it
/// `Errored` once `restart_count >= max_restarts`, so *any* finite budget ends that way
/// eventually. Advising on every configured budget would fire on the common case and train the
/// operator to ignore the rule, so the advisory is limited to budgets small enough that a single
/// sustained breach exhausts them. The oxfile default is 10, which deliberately does not fire.
const LOW_RESTART_BUDGET: u32 = 3;

/// How serious an advisory is. Ordered so that `Critical > Warning > Info`, which is what
/// [`AdvisoryReport`] sorts on.
///
/// Deliberately separate from the resource-reading severity in `crate::severity`: that one grades
/// a measured figure against a denominator, this one grades a configuration finding. Sharing a
/// type would tie two unrelated scales together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdvisorySeverity {
    /// Worth knowing; no safety mechanism is affected.
    Info,
    /// A mechanism behaves differently than its settings suggest.
    Warning,
    /// A safety mechanism is disabled, or the configuration leads to an outage.
    Critical,
}

impl AdvisorySeverity {
    /// Stable lower-case label for display and for the wire.
    ///
    /// Lives here rather than being formatted at each call site so the CLI, the API and the
    /// dashboard cannot end up calling the same severity by three different names.
    pub fn label(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

/// The mechanism an advisory is about, so a consumer can group findings without parsing prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mechanism {
    /// The 5-minute crash-restart circuit breaker in `src/process_manager/restart.rs`.
    CrashLoopProtection,
    /// Restart delay and backoff scheduling.
    RestartScheduling,
    /// Periodic health-check execution.
    HealthChecking,
    /// Resource-limit enforcement in `run_resource_limit_checks`, or by the kernel via cgroups.
    ResourceLimitEnforcement,
    /// Restart-on-file-change.
    WatchRestart,
}

/// Identity of a rule. Stable across runs so a dismissal (section 3) can name one, and so the
/// dashboard and CLI cannot drift apart over wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdvisoryRule {
    /// `crash_restart_limit == 0` leaves the circuit breaker permanently untripped.
    CrashLoopProtectionDisabled,
    /// `RestartPolicy::Always` with `restart_delay_secs == 0`.
    ImmediateRestartLoop,
    /// A health check whose interval is not greater than its timeout.
    HealthCheckIntervalNotAboveTimeout,
    /// `max_memory_mb` without `cgroup_enforce`.
    MemoryLimitWithoutKernelEnforcement,
    /// `max_memory_mb` with a restart budget small enough that sustained breach stops the process.
    MemoryLimitEndsInOutage,
    /// `max_memory_mb` at or above the host's total memory.
    MemoryLimitAboveHostMemory,
    /// Instance count times `max_memory_mb` above the host's total memory.
    InstancesOversubscribeHostMemory,
    /// `watch` over an unbounded path set with nothing ignored.
    WatchScopeUnbounded,
}

impl AdvisoryRule {
    /// Stable machine-readable identifier.
    pub fn id(self) -> &'static str {
        match self {
            Self::CrashLoopProtectionDisabled => "crash_loop_protection_disabled",
            Self::ImmediateRestartLoop => "immediate_restart_loop",
            Self::HealthCheckIntervalNotAboveTimeout => "health_check_interval_not_above_timeout",
            Self::MemoryLimitWithoutKernelEnforcement => "memory_limit_without_kernel_enforcement",
            Self::MemoryLimitEndsInOutage => "memory_limit_ends_in_outage",
            Self::MemoryLimitAboveHostMemory => "memory_limit_above_host_memory",
            Self::InstancesOversubscribeHostMemory => "instances_oversubscribe_host_memory",
            Self::WatchScopeUnbounded => "watch_scope_unbounded",
        }
    }

    /// Parses a wire identifier back into a rule.
    ///
    /// `None` for anything unrecognised, which is what lets a dismissal request name a rule that
    /// does not exist and be REFUSED rather than silently stored. A stored dismissal for a
    /// misspelled rule would look accepted and suppress nothing, and the operator would only find
    /// out the next time the real advisory fired.
    pub fn from_id(id: &str) -> Option<Self> {
        // Matched against `id()` rather than a second literal table, so a renamed rule cannot parse
        // under its old name while rendering under the new one.
        Self::ALL.iter().copied().find(|rule| rule.id() == id)
    }

    /// Every rule. The order is the evaluation order.
    pub const ALL: [Self; 8] = [
        Self::CrashLoopProtectionDisabled,
        Self::ImmediateRestartLoop,
        Self::HealthCheckIntervalNotAboveTimeout,
        Self::MemoryLimitWithoutKernelEnforcement,
        Self::MemoryLimitEndsInOutage,
        Self::MemoryLimitAboveHostMemory,
        Self::InstancesOversubscribeHostMemory,
        Self::WatchScopeUnbounded,
    ];

    /// Whether the rule needs host capacity to be decided.
    ///
    /// Formerly `#[cfg(test)]`; promoted to permanent API because
    /// `commands::validate`'s tests assert against it across the crate boundary
    /// (test-only visibility does not cross a crate — same rule as phases 2-4).
    pub fn is_capacity_relative(self) -> bool {
        matches!(
            self,
            Self::MemoryLimitAboveHostMemory | Self::InstancesOversubscribeHostMemory
        )
    }
}

/// One setting and its value, as structured evidence.
///
/// Prose alone would force a consumer to parse a sentence to find out which setting was involved;
/// the same discipline `host_metrics` applies to its figures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingValue {
    /// The configuration field, spelled as the operator wrote it.
    ///
    /// `Cow` so a rule can name it with a `&'static str` at no cost while the type still
    /// round-trips through `serde`, which cannot deserialise a borrowed `&'static str`.
    pub setting: Cow<'static, str>,
    /// Its value, rendered.
    pub value: String,
}

impl SettingValue {
    fn new(setting: &'static str, value: impl std::fmt::Display) -> Self {
        Self {
            setting: Cow::Borrowed(setting),
            value: value.to_string(),
        }
    }
}

/// A configured figure against the capacity it was compared with.
///
/// Carries the label so a host total is never presented as the limit that applies to the process:
/// in a container the two differ, and `docs/HOST-METRICS.md` says so.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityComparison {
    /// What was configured, in bytes.
    pub configured_bytes: u64,
    /// What it was measured against, in bytes.
    pub capacity_bytes: u64,
    /// What that capacity is, e.g. `host total memory`.
    pub capacity_label: Cow<'static, str>,
}

/// A single finding: what will happen, which settings caused it, and what it was compared against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Advisory {
    /// Which rule produced this.
    pub rule: AdvisoryRule,
    /// Stable identifier, duplicated into the payload so a client need not map the enum.
    pub id: Cow<'static, str>,
    /// How serious it is.
    pub severity: AdvisorySeverity,
    /// The mechanism affected.
    pub mechanism: Mechanism,
    /// What will happen as a result of the configuration. Never only the name of a setting.
    pub consequence: String,
    /// Every setting involved, with its value.
    pub evidence: Vec<SettingValue>,
    /// The capacity comparison, for capacity-relative rules only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<CapacityComparison>,
}

/// Why capacity-relative evaluation did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityWithheldReason {
    /// No capacity was supplied — host metrics unavailable, or not collected yet.
    CapacityUnavailable,
    /// A total of zero was supplied. Zero memory is not a measurement, so it is treated as
    /// unavailable rather than compared against, which would declare every limit oversubscribed.
    CapacityReportedZero,
}

/// Whether the capacity-relative class of rules ran.
///
/// Reported alongside the advisories so silence in that class is distinguishable from a verdict of
/// "fine" — the distinction `host_metrics` draws between unavailable and zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CapacityEvaluation {
    /// Capacity was known and the class was evaluated.
    Evaluated {
        /// The total the rules compared against, in bytes.
        total_memory_bytes: u64,
    },
    /// The class was withheld; no conclusion either way.
    Withheld {
        /// Why it was withheld.
        reason: CapacityWithheldReason,
    },
}

impl CapacityEvaluation {
    /// Whether capacity-relative rules were actually evaluated.
    pub fn was_evaluated(self) -> bool {
        matches!(self, Self::Evaluated { .. })
    }
}

/// Host capacity figures the capacity-relative rules need.
///
/// A struct rather than a bare `u64` so adding CPU or disk later does not change every call site.
/// Taken as an `Option` by [`evaluate`]: absent means "cannot say".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCapacity {
    /// The host's total physical memory in bytes — the machine's, not a cgroup's.
    pub total_memory_bytes: u64,
}

impl HostCapacity {
    /// Builds capacity from a measured total, or `None` when that total is zero.
    ///
    /// Zero is rejected here rather than at each rule so no rule can accidentally divide by it or
    /// declare every limit above capacity.
    pub fn from_total_memory_bytes(total_memory_bytes: u64) -> Option<Self> {
        (total_memory_bytes > 0).then_some(Self { total_memory_bytes })
    }
}

/// The subset of a process's configuration these rules read.
///
/// A borrowed view rather than a `ManagedProcess` so a configuration can be judged before any
/// process exists — which is what makes the rules usable from `validate` as well as the dashboard.
#[derive(Debug, Clone, Copy)]
pub struct ProcessConfig<'a> {
    /// Restart policy in force. Borrowed because `RestartPolicy` is not `Copy`, and this view
    /// stays copyable so a rule can take it without cloning a configuration.
    pub restart_policy: &'a RestartPolicy,
    /// Ceiling on restarts before `run_resource_limit_checks` stops the process.
    pub max_restarts: u32,
    /// Crash-loop circuit breaker threshold. Zero disables it.
    pub crash_restart_limit: u32,
    /// Base delay before an automatic restart.
    pub restart_delay_secs: u64,
    /// Health check, when configured.
    pub health_check: Option<&'a HealthCheck>,
    /// Configured memory ceiling in MB, when set.
    pub max_memory_mb: Option<u64>,
    /// Whether the kernel enforces the limits via cgroup v2.
    pub cgroup_enforce: bool,
    /// Whether restart-on-change is enabled.
    pub watch: bool,
    /// Number of watch roots configured. Zero with `watch` enabled means the whole cwd.
    pub watch_path_count: usize,
    /// Number of ignore patterns configured.
    pub ignore_watch_count: usize,
    /// Instances this process runs as, when clustered.
    pub cluster_instances: Option<u32>,
}

impl<'a> From<&'a StartProcessSpec> for ProcessConfig<'a> {
    fn from(spec: &'a StartProcessSpec) -> Self {
        Self {
            restart_policy: &spec.restart_policy,
            max_restarts: spec.max_restarts,
            crash_restart_limit: spec.crash_restart_limit,
            restart_delay_secs: spec.restart_delay_secs,
            health_check: spec.health_check.as_ref(),
            max_memory_mb: spec
                .resource_limits
                .as_ref()
                .and_then(|limits| limits.max_memory_mb),
            cgroup_enforce: spec
                .resource_limits
                .as_ref()
                .is_some_and(|limits| limits.cgroup_enforce),
            watch: spec.watch,
            watch_path_count: spec.watch_paths.len(),
            ignore_watch_count: spec.ignore_watch.len(),
            cluster_instances: spec.cluster_instances,
        }
    }
}

/// A config-file spec, so `validate` advises on the same rules the daemon and the dashboard use.
///
/// This is the third view onto the same rule set, and the point of having it: without it `validate`
/// would need its own copy of the rules and the two would eventually disagree about whether a
/// configuration is risky. That drift is what task 3.5 exists to prevent.
///
/// `instances` rather than `cluster_instances`: the ecosystem format carries both, and `instances`
/// is the count actually expanded at import. Using the cluster field would understate the combined
/// memory of an expanded set and silently miss the oversubscription rule.
impl<'a> From<&'a EcosystemProcessSpec> for ProcessConfig<'a> {
    fn from(spec: &'a EcosystemProcessSpec) -> Self {
        Self {
            restart_policy: &spec.restart_policy,
            max_restarts: spec.max_restarts,
            crash_restart_limit: spec.crash_restart_limit,
            restart_delay_secs: spec.restart_delay_secs,
            health_check: spec.health_check.as_ref(),
            max_memory_mb: spec
                .resource_limits
                .as_ref()
                .and_then(|limits| limits.max_memory_mb),
            cgroup_enforce: spec
                .resource_limits
                .as_ref()
                .is_some_and(|limits| limits.cgroup_enforce),
            watch: spec.watch,
            watch_path_count: spec.watch_paths.len(),
            ignore_watch_count: spec.ignore_watch.len(),
            // `instances` is the expansion count; fall back to the cluster field when it is the one
            // set, so neither format is missed.
            cluster_instances: Some(spec.instances.max(1)).or(spec.cluster_instances),
        }
    }
}

impl<'a> From<&'a ManagedProcess> for ProcessConfig<'a> {
    fn from(process: &'a ManagedProcess) -> Self {
        Self {
            restart_policy: &process.restart_policy,
            max_restarts: process.max_restarts,
            crash_restart_limit: process.crash_restart_limit,
            restart_delay_secs: process.restart_delay_secs,
            health_check: process.health_check.as_ref(),
            max_memory_mb: process
                .resource_limits
                .as_ref()
                .and_then(|limits| limits.max_memory_mb),
            cgroup_enforce: process
                .resource_limits
                .as_ref()
                .is_some_and(|limits| limits.cgroup_enforce),
            watch: process.watch,
            watch_path_count: process.watch_paths.len(),
            ignore_watch_count: process.ignore_watch.len(),
            cluster_instances: process.cluster_instances,
        }
    }
}

/// Effective instance count: clustering below 1 still runs one process, as `bundle.rs` normalises.
fn effective_instances(config: &ProcessConfig<'_>) -> u32 {
    config.cluster_instances.unwrap_or(1).max(1)
}

const BYTES_PER_MB: u64 = 1024 * 1024;

/// Every advisory for one configuration, plus whether the capacity class ran.
///
/// The withholding travels with the findings deliberately: a consumer that only read `advisories`
/// could not tell an empty capacity class from a clean bill of health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdvisoryReport {
    /// Findings, most severe first.
    pub advisories: Vec<Advisory>,
    /// Whether capacity-relative rules were evaluated or withheld.
    pub capacity: CapacityEvaluation,
}

#[cfg(test)]
impl AdvisoryReport {
    /// Whether any advisory was produced.
    pub fn is_empty(&self) -> bool {
        self.advisories.is_empty()
    }

    /// The most severe advisory, or `None` when there are none. First by construction.
    pub fn highest_severity(&self) -> Option<AdvisorySeverity> {
        self.advisories.first().map(|advisory| advisory.severity)
    }

    /// Whether a given rule fired.
    pub fn contains(&self, rule: AdvisoryRule) -> bool {
        self.advisories.iter().any(|advisory| advisory.rule == rule)
    }
}

/// Evaluates a configuration, with host capacity when it is known.
///
/// Internal-consistency rules always run. Capacity-relative rules run only when `capacity` is
/// `Some` with a non-zero total; otherwise they are withheld and the reason is reported. Pass
/// `None` when host metrics have not been collected or the collection failed.
pub fn evaluate(config: &ProcessConfig<'_>, capacity: Option<HostCapacity>) -> AdvisoryReport {
    let mut advisories = internal_consistency_advisories(config);

    let capacity_state = match capacity {
        Some(capacity) if capacity.total_memory_bytes > 0 => {
            advisories.extend(capacity_advisories(config, capacity));
            CapacityEvaluation::Evaluated {
                total_memory_bytes: capacity.total_memory_bytes,
            }
        }
        // A zero total is not a measurement. Comparing against it would put every configured limit
        // at or above capacity and produce a page of alarming, worthless advisories.
        Some(_) => CapacityEvaluation::Withheld {
            reason: CapacityWithheldReason::CapacityReportedZero,
        },
        None => CapacityEvaluation::Withheld {
            reason: CapacityWithheldReason::CapacityUnavailable,
        },
    };

    // Most severe first, so a disabled circuit breaker is never buried under an observation about
    // watch scope. Stable sort, and the secondary key is the rule's declaration order, so the same
    // configuration always yields the same sequence.
    advisories.sort_by(|left, right| {
        right
            .severity
            .cmp(&left.severity)
            .then_with(|| left.rule.cmp(&right.rule))
    });

    AdvisoryReport {
        advisories,
        capacity: capacity_state,
    }
}

/// The rules decidable from configuration alone, on any host, before the process runs.
fn internal_consistency_advisories(config: &ProcessConfig<'_>) -> Vec<Advisory> {
    let mut advisories = Vec::new();

    if let Some(advisory) = crash_loop_protection_disabled(config) {
        advisories.push(advisory);
    }
    if let Some(advisory) = immediate_restart_loop(config) {
        advisories.push(advisory);
    }
    if let Some(advisory) = health_check_interval_not_above_timeout(config) {
        advisories.push(advisory);
    }
    if let Some(advisory) = memory_limit_without_kernel_enforcement(config) {
        advisories.push(advisory);
    }
    if let Some(advisory) = memory_limit_ends_in_outage(config) {
        advisories.push(advisory);
    }
    if let Some(advisory) = watch_scope_unbounded(config) {
        advisories.push(advisory);
    }

    advisories
}

/// `crash_restart_limit == 0` reads as "no limit" and means "no protection".
///
/// `crash_loop_limit_reached` (`src/process_manager/restart.rs:136`) requires `> 0` to ever return
/// `true`, and `record_auto_restart` (`:141`) returns before recording anything on the same
/// condition — so the breaker has neither a threshold nor a history to trip on.
///
/// Not reported under `RestartPolicy::Never`: `can_auto_restart` refuses every automatic restart
/// there, so there is no loop for a breaker to interrupt and the advisory would be false.
fn crash_loop_protection_disabled(config: &ProcessConfig<'_>) -> Option<Advisory> {
    if config.crash_restart_limit != 0 || *config.restart_policy == RestartPolicy::Never {
        return None;
    }

    Some(Advisory {
        rule: AdvisoryRule::CrashLoopProtectionDisabled,
        id: Cow::Borrowed(AdvisoryRule::CrashLoopProtectionDisabled.id()),
        severity: AdvisorySeverity::Critical,
        mechanism: Mechanism::CrashLoopProtection,
        consequence: format!(
            "Crash-loop protection is disabled: a crash_restart_limit of 0 is read as no \
             protection rather than no limit, so nothing records automatic restarts and the \
             circuit breaker can never trip. With restart_policy {}, a process that keeps \
             crashing will keep being restarted indefinitely and no crash-loop state will be \
             reported for it.",
            config.restart_policy
        ),
        evidence: vec![
            SettingValue::new("crash_restart_limit", config.crash_restart_limit),
            SettingValue::new("restart_policy", config.restart_policy),
        ],
        capacity: None,
    })
}

/// `RestartPolicy::Always` with no delay restarts a failing process as fast as it can exit.
///
/// `compute_restart_delay_secs` (`src/process_manager/restart.rs:91`) returns 0 immediately when
/// the base delay is 0, so neither the exponential backoff nor its jitter applies — the delay
/// stays 0 no matter how many attempts have been made.
fn immediate_restart_loop(config: &ProcessConfig<'_>) -> Option<Advisory> {
    if *config.restart_policy != RestartPolicy::Always || config.restart_delay_secs != 0 {
        return None;
    }

    let mut evidence = vec![
        SettingValue::new("restart_policy", config.restart_policy),
        SettingValue::new("restart_delay_secs", config.restart_delay_secs),
    ];

    // Whether anything stops the loop depends on the breaker, so the consequence says which.
    let bounded = config.crash_restart_limit > 0;
    let tail = if bounded {
        format!(
            "Crash-loop protection is still active at crash_restart_limit {}, so the loop is \
             expected to be interrupted rather than run forever.",
            config.crash_restart_limit
        )
    } else {
        "Crash-loop protection is also disabled, so nothing is expected to interrupt the loop."
            .to_string()
    };
    evidence.push(SettingValue::new(
        "crash_restart_limit",
        config.crash_restart_limit,
    ));

    Some(Advisory {
        rule: AdvisoryRule::ImmediateRestartLoop,
        id: Cow::Borrowed(AdvisoryRule::ImmediateRestartLoop.id()),
        severity: if bounded {
            AdvisorySeverity::Warning
        } else {
            AdvisorySeverity::Critical
        },
        mechanism: Mechanism::RestartScheduling,
        consequence: format!(
            "Restarts are scheduled with no delay and no backoff: a zero restart_delay_secs \
             short-circuits the exponential backoff, so if this process starts failing on \
             startup it will be restarted as fast as it exits, spending CPU on the spawn loop \
             itself. {tail}"
        ),
        evidence,
        capacity: None,
    })
}

/// A health check whose interval is not greater than its timeout schedules the next probe before
/// the previous one can report a failure.
///
/// The scheduler stamps `next_health_check = now + interval_secs.max(1)` when a probe *completes*
/// (`src/process_manager.rs:2214`), and a probe is allowed up to `timeout_secs.max(1)` to run
/// (`src/process_manager/health.rs:26`). With interval <= timeout, a probe that runs to its timeout
/// leaves the next one already due, so checks run back to back with no idle gap.
fn health_check_interval_not_above_timeout(config: &ProcessConfig<'_>) -> Option<Advisory> {
    let check = config.health_check?;
    // The scheduler and the runner both floor these at 1, so compare the effective values rather
    // than the configured zeros — otherwise 0/0 would be judged against numbers never used.
    let interval = check.interval_secs.max(1);
    let timeout = check.timeout_secs.max(1);
    if interval > timeout {
        return None;
    }

    Some(Advisory {
        rule: AdvisoryRule::HealthCheckIntervalNotAboveTimeout,
        id: Cow::Borrowed(AdvisoryRule::HealthCheckIntervalNotAboveTimeout.id()),
        severity: AdvisorySeverity::Warning,
        mechanism: Mechanism::HealthChecking,
        consequence: format!(
            "The next health probe is scheduled before the previous one can time out: the \
             interval of {interval}s is not greater than the timeout of {timeout}s, and the next \
             probe is scheduled from the moment the previous one finishes. A check that runs to \
             its full timeout leaves the following probe already due, so probes run back to back \
             with no idle gap, and after {} consecutive failures the process is restarted.",
            check.max_failures.max(1)
        ),
        evidence: vec![
            SettingValue::new("health_check.interval_secs", check.interval_secs),
            SettingValue::new("health_check.timeout_secs", check.timeout_secs),
            SettingValue::new("health_check.max_failures", check.max_failures),
        ],
        capacity: None,
    })
}

/// `max_memory_mb` without `cgroup_enforce` is not a ceiling the kernel applies.
///
/// `cgroup::apply_limits` returns `Ok(None)` without writing anything when `cgroup_enforce` is
/// false (`src/cgroup.rs:30`). The limit is then applied by `run_resource_limit_checks`
/// (`src/process_manager.rs:2000`), which compares the *last sample* and restarts the process — so
/// the process is allowed above the limit until the next metrics refresh notices.
fn memory_limit_without_kernel_enforcement(config: &ProcessConfig<'_>) -> Option<Advisory> {
    let max_memory_mb = config.max_memory_mb?;
    if config.cgroup_enforce {
        return None;
    }

    Some(Advisory {
        rule: AdvisoryRule::MemoryLimitWithoutKernelEnforcement,
        id: Cow::Borrowed(AdvisoryRule::MemoryLimitWithoutKernelEnforcement.id()),
        severity: AdvisorySeverity::Warning,
        mechanism: Mechanism::ResourceLimitEnforcement,
        consequence: format!(
            "The {max_memory_mb} MB memory limit is enforced by restarting the process, not by \
             the kernel: without cgroup_enforce no cgroup is created, and the daemon instead \
             compares the most recent memory sample against the limit and restarts the process \
             when it is over. The process is therefore allowed above {max_memory_mb} MB until the \
             next sample is taken, and a breach costs a restart rather than a failed allocation."
        ),
        evidence: vec![
            SettingValue::new("resource_limits.max_memory_mb", max_memory_mb),
            SettingValue::new("resource_limits.cgroup_enforce", config.cgroup_enforce),
        ],
        capacity: None,
    })
}

/// A memory limit plus a small restart budget stops the process instead of containing it.
///
/// `run_resource_limit_checks` stops the process, clears its pid, sets `desired_state` to
/// `Stopped` and its status to `Errored` once `restart_count >= max_restarts`
/// (`src/process_manager.rs:2035`). Each limit breach consumes one unit of that budget, so a
/// sustained breach exhausts a small budget and ends in an outage.
///
/// Only reported for budgets at or below [`LOW_RESTART_BUDGET`]: every finite budget ends this way
/// eventually, and firing on the default of 10 would make the rule noise.
fn memory_limit_ends_in_outage(config: &ProcessConfig<'_>) -> Option<Advisory> {
    let max_memory_mb = config.max_memory_mb?;
    if config.max_restarts > LOW_RESTART_BUDGET {
        return None;
    }

    let budget = config.max_restarts;
    let onset = if budget == 0 {
        "The budget is already exhausted at zero, so the first breach detected stops the process \
         rather than restarting it."
            .to_string()
    } else {
        format!(
            "After {budget} restart{} the next breach stops it instead.",
            if budget == 1 { "" } else { "s" }
        )
    };

    Some(Advisory {
        rule: AdvisoryRule::MemoryLimitEndsInOutage,
        id: Cow::Borrowed(AdvisoryRule::MemoryLimitEndsInOutage.id()),
        severity: AdvisorySeverity::Critical,
        mechanism: Mechanism::ResourceLimitEnforcement,
        consequence: format!(
            "A sustained breach of the {max_memory_mb} MB memory limit is expected to stop this \
             process rather than contain it: each breach spends one of the {budget} restarts in \
             max_restarts, and once that budget is used up the daemon stops the process, marks it \
             Errored and leaves it stopped. {onset} If this workload can exceed \
             {max_memory_mb} MB under load, the outcome is an outage rather than a guardrail."
        ),
        evidence: vec![
            SettingValue::new("resource_limits.max_memory_mb", max_memory_mb),
            SettingValue::new("max_restarts", budget),
        ],
        capacity: None,
    })
}

/// `watch` with no explicit roots and nothing ignored fingerprints the entire working directory.
///
/// `watch_fingerprint_for_process` (`src/process_manager/watch.rs:21`) falls back to the process's
/// cwd when `watch_paths` is empty, and `fingerprint_watch_path` walks it recursively, hashing
/// every entry's size and mtime. Build output, logs and dependency directories under the cwd are
/// therefore part of the fingerprint, and a change to any of them triggers a restart.
///
/// `Info`: the configuration works, and for a small project directory it is a reasonable default.
fn watch_scope_unbounded(config: &ProcessConfig<'_>) -> Option<Advisory> {
    if !config.watch || config.watch_path_count > 0 || config.ignore_watch_count > 0 {
        return None;
    }

    Some(Advisory {
        rule: AdvisoryRule::WatchScopeUnbounded,
        id: Cow::Borrowed(AdvisoryRule::WatchScopeUnbounded.id()),
        severity: AdvisorySeverity::Info,
        mechanism: Mechanism::WatchRestart,
        consequence: "Restart-on-change covers the entire working directory: with no watch_paths \
                      and no ignore_watch, the whole cwd is walked recursively and every file's \
                      size and modification time forms the fingerprint. Anything the process or \
                      its toolchain writes underneath it — build output, logs, dependency \
                      directories — counts as a change, so if this workload writes inside its own \
                      cwd it may be restarted repeatedly by its own output."
            .to_string(),
        evidence: vec![
            SettingValue::new("watch", config.watch),
            SettingValue::new("watch_paths", "[] (defaults to cwd)"),
            SettingValue::new("ignore_watch", "[]"),
        ],
        capacity: None,
    })
}

/// The rules that need the host's measured capacity. Only called with a non-zero total.
fn capacity_advisories(config: &ProcessConfig<'_>, capacity: HostCapacity) -> Vec<Advisory> {
    let mut advisories = Vec::new();

    if let Some(advisory) = memory_limit_above_host_memory(config, capacity) {
        advisories.push(advisory);
    }
    if let Some(advisory) = instances_oversubscribe_host_memory(config, capacity) {
        advisories.push(advisory);
    }

    advisories
}

/// A memory limit at or above the host's total memory can never be reached.
///
/// The process would have to hold more memory than the machine has before
/// `run_resource_limit_checks` saw a breach, so the limit provides no containment at all — the
/// kernel's OOM killer acts first.
fn memory_limit_above_host_memory(
    config: &ProcessConfig<'_>,
    capacity: HostCapacity,
) -> Option<Advisory> {
    let max_memory_mb = config.max_memory_mb?;
    let configured_bytes = max_memory_mb.saturating_mul(BYTES_PER_MB);
    if configured_bytes < capacity.total_memory_bytes {
        return None;
    }

    Some(Advisory {
        rule: AdvisoryRule::MemoryLimitAboveHostMemory,
        id: Cow::Borrowed(AdvisoryRule::MemoryLimitAboveHostMemory.id()),
        severity: AdvisorySeverity::Warning,
        mechanism: Mechanism::ResourceLimitEnforcement,
        consequence: format!(
            "The {max_memory_mb} MB memory limit is not expected to ever be reached: it is at or \
             above the {} of {}, so the process would have to hold more memory than the machine \
             has before the limit applied. The kernel's out-of-memory handling is expected to act \
             first, and this limit provides no containment. Note the comparison is against the \
             host's total; a cgroup limit applying to this daemon may be lower.",
            HOST_MEMORY_LABEL,
            format_bytes(capacity.total_memory_bytes)
        ),
        evidence: vec![
            SettingValue::new("resource_limits.max_memory_mb", max_memory_mb),
            SettingValue::new(
                HOST_MEMORY_SETTING,
                format_bytes(capacity.total_memory_bytes),
            ),
        ],
        capacity: Some(CapacityComparison {
            configured_bytes,
            capacity_bytes: capacity.total_memory_bytes,
            capacity_label: Cow::Borrowed(HOST_MEMORY_LABEL),
        }),
    })
}

/// Instance count times the per-instance memory limit above host memory is oversubscribed.
///
/// Each clustered instance carries the same `max_memory_mb`, and nothing compares the product
/// against the machine. Conditional wording on purpose: a workload that never approaches its limit
/// will never realise the demand, which is exactly why this is an advisory and not a refusal.
fn instances_oversubscribe_host_memory(
    config: &ProcessConfig<'_>,
    capacity: HostCapacity,
) -> Option<Advisory> {
    let max_memory_mb = config.max_memory_mb?;
    let instances = effective_instances(config);
    if instances < 2 {
        return None;
    }

    let combined_bytes = max_memory_mb
        .saturating_mul(BYTES_PER_MB)
        .saturating_mul(u64::from(instances));
    if combined_bytes <= capacity.total_memory_bytes {
        return None;
    }

    Some(Advisory {
        rule: AdvisoryRule::InstancesOversubscribeHostMemory,
        id: Cow::Borrowed(AdvisoryRule::InstancesOversubscribeHostMemory.id()),
        severity: AdvisorySeverity::Warning,
        mechanism: Mechanism::ResourceLimitEnforcement,
        consequence: format!(
            "The instances of this process are oversubscribed against host memory: {instances} \
             instances at {max_memory_mb} MB each allow {} in total, against a {} of {}. If every \
             instance approaches its limit at the same time the host runs out of memory before any \
             instance reaches its own limit, so the outcome is expected to be kernel \
             out-of-memory handling rather than a per-instance restart. An instance that stays \
             well below its limit never realises this demand. The comparison is against the \
             host's total, which a cgroup limit applying to this daemon may be lower than.",
            format_bytes(combined_bytes),
            HOST_MEMORY_LABEL,
            format_bytes(capacity.total_memory_bytes)
        ),
        evidence: vec![
            SettingValue::new("resource_limits.max_memory_mb", max_memory_mb),
            SettingValue::new("cluster_instances", instances),
            SettingValue::new("combined_memory_demand", format_bytes(combined_bytes)),
            SettingValue::new(
                HOST_MEMORY_SETTING,
                format_bytes(capacity.total_memory_bytes),
            ),
        ],
        capacity: Some(CapacityComparison {
            configured_bytes: combined_bytes,
            capacity_bytes: capacity.total_memory_bytes,
            capacity_label: Cow::Borrowed(HOST_MEMORY_LABEL),
        }),
    })
}

/// How a host memory total is described. Says "host" every time, because in a container it is the
/// host's figure and not the limit that applies — the constraint `docs/HOST-METRICS.md` sets.
const HOST_MEMORY_LABEL: &str = "host total memory";

/// Evidence key for the same figure.
const HOST_MEMORY_SETTING: &str = "measured host total memory";

/// Renders a byte count for an operator, at the largest unit that keeps it readable.
fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;

    if bytes >= GIB {
        format!("{:.1} GB", u64_to_f64(bytes) / u64_to_f64(GIB))
    } else if bytes >= MIB {
        format!("{:.1} MB", u64_to_f64(bytes) / u64_to_f64(MIB))
    } else if bytes >= KIB {
        format!("{:.1} kB", u64_to_f64(bytes) / u64_to_f64(KIB))
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxmgr_metrics::process::{HealthCheck, ResourceLimits};

    /// A configuration nothing should complain about: bounded restarts, a working circuit breaker,
    /// a delay, no memory ceiling, no watching.
    ///
    /// Every test below starts here and changes ONE thing, so a firing advisory is attributable to
    /// that change rather than to the fixture.
    fn sound() -> ProcessConfig<'static> {
        // Leaked deliberately: `ProcessConfig` borrows its policy, and a test fixture that lives
        // for the process is simpler than threading a lifetime through every case.
        static ON_FAILURE: RestartPolicy = RestartPolicy::OnFailure;
        ProcessConfig {
            restart_policy: &ON_FAILURE,
            max_restarts: 10,
            crash_restart_limit: 3,
            restart_delay_secs: 1,
            health_check: None,
            max_memory_mb: None,
            cgroup_enforce: false,
            watch: false,
            watch_path_count: 0,
            ignore_watch_count: 0,
            cluster_instances: None,
        }
    }

    fn always() -> &'static RestartPolicy {
        static ALWAYS: RestartPolicy = RestartPolicy::Always;
        &ALWAYS
    }

    fn never() -> &'static RestartPolicy {
        static NEVER: RestartPolicy = RestartPolicy::Never;
        &NEVER
    }

    fn health(interval_secs: u64, timeout_secs: u64) -> &'static HealthCheck {
        // One per interval/timeout pair used below; a `Box::leak` keeps the borrow simple.
        Box::leak(Box::new(HealthCheck {
            command: "curl -fsS localhost/health".to_string(),
            interval_secs,
            timeout_secs,
            max_failures: 3,
        }))
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn a_sound_configuration_produces_no_advisories() {
        let report = evaluate(&sound(), HostCapacity::from_total_memory_bytes(16 * GIB));
        assert!(
            report.is_empty(),
            "a sound configuration must be quiet: {:?}",
            report.advisories
        );
        assert_eq!(report.highest_severity(), None);
    }

    // -- Section 1: internal consistency, decidable without host capacity ------------------------

    #[test]
    fn a_zero_crash_restart_limit_is_reported_as_no_protection() {
        let mut config = sound();
        config.crash_restart_limit = 0;
        let report = evaluate(&config, None);

        assert!(report.contains(AdvisoryRule::CrashLoopProtectionDisabled));
        let advisory = report
            .advisories
            .iter()
            .find(|a| a.rule == AdvisoryRule::CrashLoopProtectionDisabled)
            .expect("present");
        assert_eq!(advisory.severity, AdvisorySeverity::Critical);
        // The consequence must state what happens, not that the value is unusual.
        assert!(
            advisory.consequence.contains("never trip"),
            "consequence does not state the outcome: {}",
            advisory.consequence
        );
        // Structured evidence, so a consumer need not parse the sentence.
        assert!(
            advisory
                .evidence
                .iter()
                .any(|e| e.setting == "crash_restart_limit")
        );
    }

    #[test]
    fn a_zero_limit_is_not_reported_when_nothing_restarts_automatically() {
        // `can_auto_restart` refuses every automatic restart under `Never`, so there is no loop
        // for a breaker to interrupt. Advising here would be a false positive.
        let mut config = sound();
        config.crash_restart_limit = 0;
        config.restart_policy = never();

        let report = evaluate(&config, None);
        assert!(!report.contains(AdvisoryRule::CrashLoopProtectionDisabled));
    }

    #[test]
    fn always_restart_with_no_delay_is_reported() {
        let mut config = sound();
        config.restart_policy = always();
        config.restart_delay_secs = 0;

        let report = evaluate(&config, None);
        assert!(report.contains(AdvisoryRule::ImmediateRestartLoop));

        // A delay switches it off: `compute_restart_delay_secs` only short-circuits at 0.
        config.restart_delay_secs = 1;
        assert!(!evaluate(&config, None).contains(AdvisoryRule::ImmediateRestartLoop));
    }

    #[test]
    fn an_immediate_loop_is_more_severe_when_the_breaker_is_also_off() {
        let mut bounded = sound();
        bounded.restart_policy = always();
        bounded.restart_delay_secs = 0;
        let bounded_report = evaluate(&bounded, None);
        let bounded_severity = bounded_report
            .advisories
            .iter()
            .find(|a| a.rule == AdvisoryRule::ImmediateRestartLoop)
            .expect("present")
            .severity;

        let mut unbounded = bounded;
        unbounded.crash_restart_limit = 0;
        let unbounded_report = evaluate(&unbounded, None);
        let unbounded_severity = unbounded_report
            .advisories
            .iter()
            .find(|a| a.rule == AdvisoryRule::ImmediateRestartLoop)
            .expect("present")
            .severity;

        assert!(
            unbounded_severity > bounded_severity,
            "a loop nothing stops must outrank one the breaker bounds: {unbounded_severity:?} vs {bounded_severity:?}"
        );
    }

    #[test]
    fn a_health_interval_not_above_its_timeout_is_reported() {
        let mut config = sound();
        // Equal is enough: a probe running to its timeout leaves the next already due.
        config.health_check = Some(health(5, 5));
        assert!(evaluate(&config, None).contains(AdvisoryRule::HealthCheckIntervalNotAboveTimeout));

        config.health_check = Some(health(2, 10));
        assert!(evaluate(&config, None).contains(AdvisoryRule::HealthCheckIntervalNotAboveTimeout));

        // Interval strictly greater: there is idle time between probes, so no advisory.
        config.health_check = Some(health(30, 5));
        assert!(
            !evaluate(&config, None).contains(AdvisoryRule::HealthCheckIntervalNotAboveTimeout)
        );
    }

    #[test]
    fn zero_health_values_are_judged_against_the_floors_actually_used() {
        // Both the scheduler and the runner floor these at 1, so 0/0 behaves as 1/1 — not above,
        // so it is reported. Judging the configured zeros would compare numbers never used.
        let mut config = sound();
        config.health_check = Some(health(0, 0));
        assert!(evaluate(&config, None).contains(AdvisoryRule::HealthCheckIntervalNotAboveTimeout));
    }

    #[test]
    fn a_memory_limit_without_cgroup_enforcement_is_reported() {
        let mut config = sound();
        config.max_memory_mb = Some(512);
        assert!(
            evaluate(&config, None).contains(AdvisoryRule::MemoryLimitWithoutKernelEnforcement)
        );

        config.cgroup_enforce = true;
        assert!(
            !evaluate(&config, None).contains(AdvisoryRule::MemoryLimitWithoutKernelEnforcement)
        );
    }

    #[test]
    fn a_memory_limit_with_a_small_restart_budget_is_reported_as_ending_in_outage() {
        let mut config = sound();
        config.max_memory_mb = Some(512);
        config.max_restarts = LOW_RESTART_BUDGET;
        assert!(evaluate(&config, None).contains(AdvisoryRule::MemoryLimitEndsInOutage));

        // A larger budget is not advised on: every finite budget ends the same way, so reporting
        // all of them would make the advisory noise rather than a signal.
        config.max_restarts = LOW_RESTART_BUDGET + 1;
        assert!(!evaluate(&config, None).contains(AdvisoryRule::MemoryLimitEndsInOutage));
    }

    #[test]
    fn unbounded_watch_scope_is_reported_only_when_nothing_narrows_it() {
        let mut config = sound();
        config.watch = true;
        assert!(evaluate(&config, None).contains(AdvisoryRule::WatchScopeUnbounded));

        // Either an explicit root or an ignore pattern is enough to say the scope was considered.
        config.watch_path_count = 1;
        assert!(!evaluate(&config, None).contains(AdvisoryRule::WatchScopeUnbounded));

        config.watch_path_count = 0;
        config.ignore_watch_count = 1;
        assert!(!evaluate(&config, None).contains(AdvisoryRule::WatchScopeUnbounded));
    }

    // -- Section 2: capacity-relative, and what happens when capacity is unknown -----------------

    #[test]
    fn capacity_rules_are_withheld_rather_than_answered_when_capacity_is_unknown() {
        let mut config = sound();
        // A limit far above any plausible host, so the rule would certainly fire if it could run.
        config.max_memory_mb = Some(1024 * 1024);

        let report = evaluate(&config, None);
        assert_eq!(
            report.capacity,
            CapacityEvaluation::Withheld {
                reason: CapacityWithheldReason::CapacityUnavailable
            }
        );
        assert!(!report.capacity.was_evaluated());
        assert!(!report.contains(AdvisoryRule::MemoryLimitAboveHostMemory));

        // Internal rules still ran: not knowing the host says nothing about the configuration's
        // own consistency.
        assert!(
            report.contains(AdvisoryRule::MemoryLimitWithoutKernelEnforcement),
            "an unknown host must not suppress the internal class"
        );
    }

    #[test]
    fn a_zero_capacity_is_withheld_rather_than_treated_as_no_memory() {
        let mut config = sound();
        config.max_memory_mb = Some(512);

        // Constructing from zero refuses outright — the same discipline as
        // `host_metrics::utilisation_percent`.
        assert!(HostCapacity::from_total_memory_bytes(0).is_none());

        // And passing one through anyway is withheld, not compared. Comparing against zero would
        // put every configured limit above capacity and produce a page of worthless advisories.
        let report = evaluate(
            &config,
            Some(HostCapacity {
                total_memory_bytes: 0,
            }),
        );
        assert_eq!(
            report.capacity,
            CapacityEvaluation::Withheld {
                reason: CapacityWithheldReason::CapacityReportedZero
            }
        );
        assert!(!report.contains(AdvisoryRule::MemoryLimitAboveHostMemory));
    }

    #[test]
    fn a_limit_at_or_above_host_memory_is_reported() {
        let mut config = sound();
        config.max_memory_mb = Some(16 * 1024); // exactly the host's total
        let report = evaluate(&config, HostCapacity::from_total_memory_bytes(16 * GIB));

        assert!(report.contains(AdvisoryRule::MemoryLimitAboveHostMemory));
        assert_eq!(
            report.capacity,
            CapacityEvaluation::Evaluated {
                total_memory_bytes: 16 * GIB
            }
        );

        let advisory = report
            .advisories
            .iter()
            .find(|a| a.rule == AdvisoryRule::MemoryLimitAboveHostMemory)
            .expect("present");
        // The figure must be qualified as the host's, since inside a container the applicable
        // limit can be far lower. `docs/HOST-METRICS.md` requires this.
        assert!(
            advisory.consequence.contains("host"),
            "a capacity advisory must say whose capacity: {}",
            advisory.consequence
        );
        assert!(
            advisory.capacity.is_some(),
            "the comparison travels with it"
        );
    }

    #[test]
    fn a_limit_below_host_memory_is_not_reported() {
        let mut config = sound();
        config.max_memory_mb = Some(512);
        config.cgroup_enforce = true; // silence the unrelated internal rule
        let report = evaluate(&config, HostCapacity::from_total_memory_bytes(16 * GIB));
        assert!(!report.contains(AdvisoryRule::MemoryLimitAboveHostMemory));
        assert!(report.is_empty(), "unexpected: {:?}", report.advisories);
    }

    #[test]
    fn instances_that_together_exceed_host_memory_are_reported() {
        let mut config = sound();
        config.max_memory_mb = Some(6 * 1024); // 6 GiB each
        config.cluster_instances = Some(4); // 24 GiB combined
        let report = evaluate(&config, HostCapacity::from_total_memory_bytes(16 * GIB));

        assert!(report.contains(AdvisoryRule::InstancesOversubscribeHostMemory));
        // Each instance alone fits, so the single-limit rule must stay quiet — the advisories
        // describe different problems and must not double-report one.
        assert!(!report.contains(AdvisoryRule::MemoryLimitAboveHostMemory));
    }

    #[test]
    fn a_single_instance_is_never_oversubscription() {
        let mut config = sound();
        config.max_memory_mb = Some(6 * 1024);
        config.cluster_instances = Some(1);
        let report = evaluate(&config, HostCapacity::from_total_memory_bytes(16 * GIB));
        assert!(!report.contains(AdvisoryRule::InstancesOversubscribeHostMemory));
    }

    // -- Report shape ----------------------------------------------------------------------------

    #[test]
    fn advisories_are_ordered_most_severe_first_and_deterministically() {
        let mut config = sound();
        config.crash_restart_limit = 0; // Critical
        config.restart_policy = always();
        config.restart_delay_secs = 0; // Critical (breaker also off)
        config.health_check = Some(health(5, 5)); // Warning
        config.watch = true; // Info

        let report = evaluate(&config, None);
        assert!(report.advisories.len() >= 4);

        let severities: Vec<_> = report.advisories.iter().map(|a| a.severity).collect();
        let mut sorted = severities.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(severities, sorted, "not ordered most severe first");
        assert_eq!(report.highest_severity(), Some(AdvisorySeverity::Critical));

        // Same input, same sequence: the secondary key is rule order, so nothing depends on
        // iteration chance.
        let again = evaluate(&config, None);
        assert_eq!(report.advisories, again.advisories);
    }

    #[test]
    fn every_advisory_carries_an_id_evidence_and_a_consequence() {
        let mut config = sound();
        config.crash_restart_limit = 0;
        config.restart_policy = always();
        config.restart_delay_secs = 0;
        config.health_check = Some(health(5, 5));
        config.max_memory_mb = Some(32 * 1024);
        config.max_restarts = 2;
        config.watch = true;

        let report = evaluate(&config, HostCapacity::from_total_memory_bytes(16 * GIB));
        assert!(!report.advisories.is_empty());

        for advisory in &report.advisories {
            assert_eq!(advisory.id, advisory.rule.id(), "id must match the rule");
            assert!(
                !advisory.evidence.is_empty(),
                "{} has no evidence",
                advisory.id
            );
            assert!(
                advisory.consequence.len() > 40,
                "{} states no consequence: {}",
                advisory.id,
                advisory.consequence
            );
            // Informational only: nothing here may imply the daemon will act.
            assert!(
                !advisory.consequence.contains("will be enforced"),
                "{} implies enforcement, which is a non-goal",
                advisory.id
            );
            if advisory.rule.is_capacity_relative() {
                assert!(
                    advisory.capacity.is_some(),
                    "{} lacks its comparison",
                    advisory.id
                );
            }
        }
    }

    #[test]
    fn a_report_round_trips_through_json() {
        let mut config = sound();
        config.crash_restart_limit = 0;
        config.max_memory_mb = Some(32 * 1024);
        let report = evaluate(&config, HostCapacity::from_total_memory_bytes(16 * GIB));

        let json = serde_json::to_string(&report).expect("serialise");
        let back: AdvisoryReport = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(back, report);
    }

    #[test]
    fn a_process_config_can_be_read_from_a_spec_and_a_managed_process() {
        // The `From` impls are the seam to the rest of the daemon, so they are exercised rather
        // than assumed. A field read from the wrong place would silence a rule.
        // `StartProcessSpec` has no `Default`, so every field is spelled out. Tedious, but it
        // means adding a field to the spec forces a decision here rather than silently defaulting
        // a value an advisory rule reads.
        let spec = StartProcessSpec {
            command: "node server.js".to_string(),
            name: Some("api".to_string()),
            pre_reload_cmd: None,
            restart_policy: RestartPolicy::Always,
            max_restarts: 10,
            crash_restart_limit: 0,
            cwd: None,
            env: std::collections::HashMap::new(),
            health_check: None,
            stop_signal: None,
            stop_timeout_secs: 5,
            restart_delay_secs: 0,
            start_delay_secs: 0,
            watch: false,
            watch_paths: Vec::new(),
            ignore_watch: Vec::new(),
            watch_delay_secs: 0,
            cluster_mode: false,
            cluster_instances: None,
            namespace: None,
            resource_limits: Some(ResourceLimits {
                max_memory_mb: Some(512),
                ..ResourceLimits::default()
            }),
            git_repo: None,
            git_ref: None,
            pull_secret_hash: None,
            reuse_port: false,
            wait_ready: false,
            ready_timeout_secs: oxmgr_metrics::process::default_ready_timeout_secs(),
            log_date_format: None,
            unified_logs: false,
            cron_restart: None,
            stdout_log_override: None,
            stderr_log_override: None,
            depends_on: Vec::new(),
        };

        let config = ProcessConfig::from(&spec);
        assert_eq!(config.crash_restart_limit, 0);
        assert_eq!(config.max_memory_mb, Some(512));
        assert!(!config.cgroup_enforce);

        let report = evaluate(&config, None);
        assert!(report.contains(AdvisoryRule::CrashLoopProtectionDisabled));
        assert!(report.contains(AdvisoryRule::ImmediateRestartLoop));
    }

    // ── 8.1 the deliberately dangerous set: every rule, with its consequence text ────────────────

    #[test]
    fn every_rule_fires_on_a_deliberately_dangerous_configuration() {
        // ONE test covering all eight rules, because the property being checked is about the SET: a
        // per-rule test can pass while a rule has no way to fire, or while two rules describe the
        // same mechanism in different words. Building the dangerous configuration for each and
        // asserting the whole set is what catches those.
        //
        // Each case changes exactly one thing from `sound()`, so a firing advisory is attributable to
        // that change rather than to the fixture.
        static ALWAYS: RestartPolicy = RestartPolicy::Always;
        // 16 GiB, so the capacity-relative rules have a real machine to compare against.
        let capacity = HostCapacity::from_total_memory_bytes(16 * 1024 * 1024 * 1024)
            .expect("a non-zero total is usable");

        // (rule, the configuration that provokes it, a word its consequence must contain)
        let cases: Vec<(AdvisoryRule, ProcessConfig<'static>, &str)> = vec![
            (
                AdvisoryRule::CrashLoopProtectionDisabled,
                ProcessConfig {
                    crash_restart_limit: 0,
                    ..sound()
                },
                "protection",
            ),
            (
                AdvisoryRule::ImmediateRestartLoop,
                ProcessConfig {
                    restart_policy: &ALWAYS,
                    restart_delay_secs: 0,
                    ..sound()
                },
                "delay",
            ),
            (
                AdvisoryRule::HealthCheckIntervalNotAboveTimeout,
                ProcessConfig {
                    health_check: Some({
                        static CHECK: std::sync::OnceLock<HealthCheck> = std::sync::OnceLock::new();
                        CHECK.get_or_init(|| HealthCheck {
                            command: "true".to_string(),
                            // Interval equal to the timeout: the next check is due the moment the
                            // previous one may still be running.
                            interval_secs: 5,
                            timeout_secs: 5,
                            max_failures: 3,
                        })
                    }),
                    ..sound()
                },
                "timeout",
            ),
            (
                AdvisoryRule::MemoryLimitWithoutKernelEnforcement,
                ProcessConfig {
                    max_memory_mb: Some(512),
                    cgroup_enforce: false,
                    ..sound()
                },
                "enforc",
            ),
            (
                AdvisoryRule::MemoryLimitEndsInOutage,
                // A ceiling with a restart budget small enough that sustained breach stops it.
                ProcessConfig {
                    max_memory_mb: Some(512),
                    max_restarts: 1,
                    ..sound()
                },
                "stop",
            ),
            (
                AdvisoryRule::MemoryLimitAboveHostMemory,
                // 32 GiB ceiling on a 16 GiB host: the limit can never be reached, so it enforces
                // nothing.
                ProcessConfig {
                    max_memory_mb: Some(32 * 1024),
                    cgroup_enforce: true,
                    ..sound()
                },
                "host",
            ),
            (
                AdvisoryRule::InstancesOversubscribeHostMemory,
                // 8 instances x 4 GiB = 32 GiB against a 16 GiB host.
                ProcessConfig {
                    max_memory_mb: Some(4 * 1024),
                    cluster_instances: Some(8),
                    cgroup_enforce: true,
                    ..sound()
                },
                "host",
            ),
            (
                AdvisoryRule::WatchScopeUnbounded,
                ProcessConfig {
                    watch: true,
                    watch_path_count: 0,
                    ignore_watch_count: 0,
                    ..sound()
                },
                "watch",
            ),
        ];

        // Every declared rule must appear, or a rule exists that nothing can provoke — which is a
        // rule an operator will never see and cannot act on.
        assert_eq!(
            cases.len(),
            AdvisoryRule::ALL.len(),
            "every declared rule needs a case in this set"
        );

        let mut seen_consequences: Vec<(AdvisoryRule, String)> = Vec::new();

        for (rule, config, must_mention) in &cases {
            let report = evaluate(config, Some(capacity));
            let found = report
                .advisories
                .iter()
                .find(|advisory| advisory.rule == *rule)
                .unwrap_or_else(|| {
                    panic!(
                        "{} did not fire on its own dangerous configuration; got {:?}",
                        rule.id(),
                        report
                            .advisories
                            .iter()
                            .map(|a| a.id.as_ref())
                            .collect::<Vec<_>>()
                    )
                });

            // The consequence must state a CONSEQUENCE, not name a setting. That is the whole point
            // of the advisory: "crash_restart_limit is 0" tells an operator nothing they did not
            // already type, while "the circuit breaker never trips" tells them what will happen.
            assert!(
                found.consequence.len() > 40,
                "{}'s consequence is too short to state an outcome: {:?}",
                rule.id(),
                found.consequence
            );
            assert!(
                found.consequence.to_lowercase().contains(must_mention),
                "{}'s consequence should mention {must_mention:?}: {:?}",
                rule.id(),
                found.consequence
            );
            // Evidence names the offending settings, so a consumer can group by mechanism without
            // parsing prose.
            assert!(
                !found.evidence.is_empty(),
                "{} produced no evidence",
                rule.id()
            );
            // The id round-trips, so a dismissal can name it.
            assert_eq!(
                AdvisoryRule::from_id(found.id.as_ref()),
                Some(*rule),
                "{}'s wire id must parse back to the rule",
                rule.id()
            );

            seen_consequences.push((*rule, found.consequence.clone()));
        }

        // No two rules may describe the same thing in the same words. Two advisories with identical
        // consequence text would be one finding reported twice, which is the duplication task 8.4
        // exists to prevent.
        for (index, (rule, consequence)) in seen_consequences.iter().enumerate() {
            for (other_rule, other) in &seen_consequences[index + 1..] {
                assert_ne!(
                    consequence,
                    other,
                    "{} and {} state the same consequence",
                    rule.id(),
                    other_rule.id()
                );
            }
        }
    }

    #[test]
    fn a_capacity_relative_rule_is_withheld_rather_than_guessed() {
        // Without host capacity the two capacity-relative rules CANNOT be decided, and withholding
        // them is the honest outcome: comparing a limit against an unknown total would either invent
        // a machine or silently report a clean bill of health.
        let config = ProcessConfig {
            max_memory_mb: Some(32 * 1024),
            cgroup_enforce: true,
            ..sound()
        };

        let withheld = evaluate(&config, None);
        assert!(
            withheld
                .advisories
                .iter()
                .all(|advisory| !advisory.rule.is_capacity_relative()),
            "a capacity-relative rule must not fire without capacity"
        );

        // And with capacity it does fire, so the absence above is withholding rather than a rule that
        // never works.
        let capacity =
            HostCapacity::from_total_memory_bytes(16 * 1024 * 1024 * 1024).expect("usable");
        let decided = evaluate(&config, Some(capacity));
        assert!(
            decided
                .advisories
                .iter()
                .any(|advisory| advisory.rule == AdvisoryRule::MemoryLimitAboveHostMemory),
            "with capacity the rule must fire: {:?}",
            decided
                .advisories
                .iter()
                .map(|a| a.id.as_ref())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_sound_configuration_produces_nothing() {
        // The fixture itself must be silent, or every test above is asserting against noise.
        let capacity =
            HostCapacity::from_total_memory_bytes(16 * 1024 * 1024 * 1024).expect("usable");
        let report = evaluate(&sound(), Some(capacity));
        assert!(
            report.advisories.is_empty(),
            "the sound fixture must produce nothing, got {:?}",
            report
                .advisories
                .iter()
                .map(|a| a.id.as_ref())
                .collect::<Vec<_>>()
        );
        assert_eq!(report.highest_severity(), None);
    }

    // ── 8.4 no advisory duplicates a process-intelligence finding ───────────────────────────────

    #[test]
    fn no_advisory_shares_an_identifier_with_a_finding_detector() {
        // The mechanical half: the two namespaces must be disjoint, or a consumer keying on an id
        // cannot tell an advisory from a finding.
        let advisory_ids: Vec<&str> = AdvisoryRule::ALL.iter().map(|rule| rule.id()).collect();
        // Bound to a local before mapping: `as_wire` borrows from the detector, so mapping the array
        // by value would return references into temporaries that die at the end of the expression.
        let detectors = [
            oxmgr_core::findings::Detector::LevelDeparture,
            oxmgr_core::findings::Detector::Drift,
            oxmgr_core::findings::Detector::SustainedShift,
            oxmgr_core::findings::Detector::ChangePoint,
            oxmgr_core::findings::Detector::ResourceLeak,
        ];
        let detector_ids: Vec<&str> = detectors
            .iter()
            .map(|detector| detector.as_wire())
            .collect();

        for advisory in &advisory_ids {
            assert!(
                !detector_ids.contains(advisory),
                "{advisory} is used as both an advisory rule and a finding detector"
            );
        }
    }

    /// The semantic half of 8.4, and the one that actually matters.
    ///
    /// The near-miss is real and worth naming: the advisory `crash_loop_protection_disabled` and the
    /// failure pattern `CrashLoop` both concern crash loops. They are NOT duplicates, and the
    /// distinction is the axis each sits on:
    ///
    /// - The advisory is about CONFIGURATION and fires before anything has happened. Its consequence
    ///   is a prediction — "the circuit breaker can never trip" — true of a process that has never
    ///   crashed once, and answerable by editing a setting.
    /// - The pattern is about OBSERVED BEHAVIOUR and fires only after the fact. Its summary is a
    ///   measurement — "failed 5 times in the last 300s" — true regardless of configuration, and not
    ///   answerable by editing anything.
    ///
    /// An operator needs both: the first says "this will not be caught if it happens", the second says
    /// "it is happening".
    ///
    /// # Why this is asserted STRUCTURALLY rather than by inspecting the prose
    ///
    /// Two earlier versions of this test scanned consequence text for observation vocabulary, and both
    /// failed on CORRECT text: `" failed "` matched "a breach costs a restart rather than a failed
    /// allocation", and `"restarted "` matched "a process that keeps crashing will keep being
    /// restarted indefinitely". Both are predictions using a past participle. The distinction between
    /// predicting and observing is carried by grammatical tense, which a keyword list cannot read — and
    /// a test that flags correct text is worse than no test, because the obvious fix is to reword the
    /// rule rather than the assertion.
    ///
    /// So the property is asserted where it is actually enforced: the TYPES. An advisory cannot observe
    /// runtime behaviour because it is never given any — `evaluate` takes `&ProcessConfig` plus an
    /// optional memory total, and `ProcessConfig` holds only configuration fields. There is no history,
    /// no sample, no event slice reachable from it. That is a stronger guarantee than any wording
    /// check: a rule could not start reporting observations without a signature change.
    #[test]
    fn an_advisory_cannot_observe_runtime_behaviour() {
        // Evidence types are the visible consequence of that separation, and they are disjoint:
        // advisory evidence names SETTINGS an operator can change, finding evidence names SAMPLES that
        // were measured. Neither type can hold the other's content.
        let capacity =
            HostCapacity::from_total_memory_bytes(16 * 1024 * 1024 * 1024).expect("usable");
        static ALWAYS: RestartPolicy = RestartPolicy::Always;

        let configs = [
            ProcessConfig {
                crash_restart_limit: 0,
                ..sound()
            },
            ProcessConfig {
                restart_policy: &ALWAYS,
                restart_delay_secs: 0,
                ..sound()
            },
            ProcessConfig {
                max_memory_mb: Some(512),
                cgroup_enforce: false,
                ..sound()
            },
            ProcessConfig {
                max_memory_mb: Some(32 * 1024),
                cgroup_enforce: true,
                ..sound()
            },
            ProcessConfig {
                watch: true,
                watch_path_count: 0,
                ignore_watch_count: 0,
                ..sound()
            },
        ];

        let mut checked = 0;
        for config in &configs {
            for advisory in evaluate(config, Some(capacity)).advisories {
                checked += 1;
                // Every piece of evidence names a configuration setting. A `SettingValue` cannot carry
                // a timestamp or a measured value, so an advisory structurally cannot cite an
                // observation — which is the property, rather than a hope about its wording.
                for entry in &advisory.evidence {
                    assert!(
                        !entry.setting.is_empty(),
                        "{} produced evidence with no setting name",
                        advisory.id
                    );
                }
                // And it carries no sample references at all: the type has no field for them.
                assert!(
                    !advisory.evidence.is_empty(),
                    "{} must name the settings involved",
                    advisory.id
                );
            }
        }
        assert!(
            checked >= 5,
            "expected several advisories to check, got {checked}"
        );
    }

    #[test]
    fn the_crash_loop_advisory_and_pattern_make_different_claims() {
        // The specific near-miss, asserted rather than argued. Both concern crash loops; the advisory
        // must speak about the configuration and the CIRCUIT BREAKER, not about a count of failures.
        let report = evaluate(
            &ProcessConfig {
                crash_restart_limit: 0,
                ..sound()
            },
            None,
        );
        let advisory = report
            .advisories
            .iter()
            .find(|advisory| advisory.rule == AdvisoryRule::CrashLoopProtectionDisabled)
            .expect("the rule fires on crash_restart_limit 0");

        // It predicts a mechanism failure...
        assert!(
            advisory.consequence.contains("never trip"),
            "the advisory should state that the breaker cannot trip: {:?}",
            advisory.consequence
        );
        // ...and names the setting to change, which a behavioural finding cannot.
        assert!(
            advisory
                .evidence
                .iter()
                .any(|entry| entry.setting == "crash_restart_limit"),
            "the advisory must name the setting an operator can change"
        );
        // It does NOT report a failure count, which is what the `CrashLoop` pattern reports.
        assert!(
            !advisory.consequence.contains("failed"),
            "the advisory must not report observed failures: {:?}",
            advisory.consequence
        );
    }
}
