//! Protection mode: the guarded gate between a decision and a real action.
//!
//! [`crate::rules`] decides what *should* happen. Nothing here decides anything — this module only
//! ever **withholds**. That direction is the safety property: a bug here can fail to act, but it
//! cannot invent an action that no rule proposed.
//!
//! Lint-level cleanup: display-path cast in count-to-u32 cap (bounded by `.min(usize::from(u32::MAX))`).
//!
//! # Observing is the default and needs no opt-in
//!
//! [`ProtectionConfig::default`] permits nothing for anybody. Acting requires, per process, that
//! action mode be enabled *and* the specific action be listed. Two separate switches rather than
//! one, because "I want protection mode on" and "I am comfortable with it stopping this process"
//! are different statements, and collapsing them means enabling observation silently grants restart.
//!
//! # Every limit is checked, and the first one to withhold is reported
//!
//! [`Gate::admit`] evaluates the limits in a fixed order and returns on the first refusal, so a
//! record names one reason rather than a set. The order runs from the most fundamental to the most
//! situational — a globally disabled daemon should not report "cooldown", because cooldown is not
//! why it declined.
//!
//! # What this module cannot do
//!
//! It cannot block an operator. There is no path from here into the IPC command handling, and
//! nothing in the daemon consults protection mode before running a user's `restart` or `stop`. That
//! is a structural property, asserted in the tests as an absence.

use std::collections::{HashMap, VecDeque};

use crate::rules::{Action, Decision};

/// Default cooldown after an action, in seconds.
///
/// 300s, matching `restart_backoff_cap_secs`. A leak that took an hour to develop will not be
/// disproven in thirty seconds, and a cooldown shorter than the time it takes to see whether the
/// action helped turns "act once and watch" into "act repeatedly and hope".
pub const DEFAULT_COOLDOWN_SECS: u64 = 300;

/// Default window over which actions are counted for the rate caps.
pub const DEFAULT_RATE_WINDOW_SECS: u64 = 3_600;

/// Default per-process cap within [`DEFAULT_RATE_WINDOW_SECS`].
///
/// 3 in an hour. Beyond that the action is not working, and the honest response is to stop and let a
/// human look rather than to keep restarting something that keeps coming back wrong.
pub const DEFAULT_PER_PROCESS_CAP: u32 = 3;

/// Default daemon-wide cap within [`DEFAULT_RATE_WINDOW_SECS`].
///
/// 10 in an hour. A daemon taking more automated actions than that is describing an incident, not
/// handling one, and a correlated failure across many processes should not become many restarts.
pub const DEFAULT_DAEMON_CAP: u32 = 10;

/// How many action records are retained. Bounds memory independently of the caps.
pub const DEFAULT_HISTORY_CAPACITY: usize = 256;

/// Why an action was not taken.
///
/// Every variant carries what it needs to be re-checked later: a record saying "rate capped" without
/// the count and window cannot be audited against a threshold that has since changed.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// Automated action is disabled daemon-wide.
    Disabled,
    /// Action mode was never enabled for this process.
    NotEnabledForProcess,
    /// The action is not on this process's permitted list.
    ///
    /// `permitted` is included so the record shows what *was* allowed, which is usually the thing an
    /// operator needs in order to fix the configuration.
    ActionNotPermitted {
        action: Action,
        permitted: Vec<Action>,
    },
    /// The process is within its cooldown.
    InCooldown {
        remaining_secs: u64,
        cooldown_secs: u64,
    },
    /// This process has reached its cap within the window.
    PerProcessCapReached {
        taken: u32,
        cap: u32,
        window_secs: u64,
    },
    /// The daemon has reached its cap within the window.
    DaemonCapReached {
        taken: u32,
        cap: u32,
        window_secs: u64,
    },
    /// The decision itself proposed no action.
    ///
    /// Not a safety limit, and kept distinct from one: "no rule wanted to act" and "a rule wanted to
    /// act and was stopped" must never look the same in the record.
    NoActionProposed,
}

#[cfg(test)]
impl Refusal {
    /// A short operator-facing reason.
    pub fn reason(&self) -> String {
        match self {
            Self::Disabled => "automated action is disabled".to_string(),
            Self::NotEnabledForProcess => {
                "automated action is not enabled for this process".to_string()
            }
            Self::ActionNotPermitted { action, permitted } => {
                if permitted.is_empty() {
                    format!("{action} is not permitted: this process permits no actions")
                } else {
                    let names: Vec<&str> = permitted.iter().map(|a| a.as_wire()).collect();
                    format!(
                        "{action} is not permitted (permitted: {})",
                        names.join(", ")
                    )
                }
            }
            Self::InCooldown {
                remaining_secs,
                cooldown_secs,
            } => format!("in cooldown: {remaining_secs}s remaining of {cooldown_secs}s"),
            Self::PerProcessCapReached {
                taken,
                cap,
                window_secs,
            } => format!("per-process cap reached: {taken} of {cap} in {window_secs}s"),
            Self::DaemonCapReached {
                taken,
                cap,
                window_secs,
            } => format!("daemon-wide cap reached: {taken} of {cap} in {window_secs}s"),
            Self::NoActionProposed => "no action was proposed".to_string(),
        }
    }

    /// Whether this refusal came from a safety limit rather than from there being nothing to do.
    pub fn is_safety_limit(&self) -> bool {
        !matches!(self, Self::NoActionProposed)
    }
}

/// What protection mode may do for one process.
///
/// Absent from [`ProtectionConfig::processes`] means "observe only", so a process nobody has
/// configured is safe by construction rather than by remembering to add it to a deny list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessPolicy {
    /// Whether protection mode may act for this process at all.
    pub act: bool,
    /// The actions it may take. An empty list means none, even when `act` is true.
    pub permitted: Vec<Action>,
    /// Per-process cooldown override, in seconds. `None` uses the daemon default.
    pub cooldown_secs: Option<u64>,
    /// Per-process cap override within the window. `None` uses the daemon default.
    pub max_actions: Option<u32>,
}

#[cfg(test)]
impl ProcessPolicy {
    /// A policy permitting the given actions.
    pub fn acting(permitted: impl IntoIterator<Item = Action>) -> Self {
        Self {
            act: true,
            permitted: permitted.into_iter().collect(),
            ..Self::default()
        }
    }
}

/// Daemon-wide protection configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct ProtectionConfig {
    /// The master switch. `false` means no action is taken for anybody.
    ///
    /// Checked at every [`Gate::admit`] rather than cached, so flipping it takes effect on the next
    /// decision with no daemon restart — which is what 12.7 requires.
    pub enabled: bool,
    /// Default cooldown, in seconds.
    pub cooldown_secs: u64,
    /// The window over which actions are counted.
    pub rate_window_secs: u64,
    /// Default per-process cap within the window.
    pub per_process_cap: u32,
    /// Daemon-wide cap within the window.
    pub daemon_cap: u32,
    /// Per-process policies. Absent means observe only.
    pub processes: HashMap<String, ProcessPolicy>,
}

impl Default for ProtectionConfig {
    /// Observing, for everybody.
    ///
    /// `enabled: false` AND an empty policy map: two independent reasons nothing can act, so a
    /// partial configuration cannot accidentally produce an acting daemon.
    fn default() -> Self {
        Self {
            enabled: false,
            cooldown_secs: DEFAULT_COOLDOWN_SECS,
            rate_window_secs: DEFAULT_RATE_WINDOW_SECS,
            per_process_cap: DEFAULT_PER_PROCESS_CAP,
            daemon_cap: DEFAULT_DAEMON_CAP,
            processes: HashMap::new(),
        }
    }
}

impl ProtectionConfig {
    /// The effective cooldown for a process.
    ///
    /// A configured 0 is honoured as "no cooldown" rather than replaced by the default: unlike a
    /// capacity, 0 is a meaningful cooldown, and an operator who sets it has said something
    /// specific. It is the *unusable* values that fall back, and `u64` has none.
    pub fn cooldown_for(&self, process: &str) -> u64 {
        self.processes
            .get(process)
            .and_then(|policy| policy.cooldown_secs)
            .unwrap_or(self.cooldown_secs)
    }

    /// The effective per-process cap.
    pub fn cap_for(&self, process: &str) -> u32 {
        self.processes
            .get(process)
            .and_then(|policy| policy.max_actions)
            .unwrap_or(self.per_process_cap)
    }

    /// Whether a process is configured to act at all.
    pub fn acts_for(&self, process: &str) -> bool {
        self.processes
            .get(process)
            .map(|policy| policy.act)
            .unwrap_or(false)
    }

    /// The permitted actions for a process.
    pub fn permitted_for(&self, process: &str) -> Vec<Action> {
        self.processes
            .get(process)
            .map(|policy| policy.permitted.clone())
            .unwrap_or_default()
    }
}

/// One action, taken or withheld.
///
/// Retained for both outcomes. A record of only what happened cannot answer "why did nothing
/// happen", which is the question an operator evaluating protection mode actually has.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionRecord {
    pub process: String,
    pub at_unix: u64,
    /// The action, or `None` when the decision proposed nothing.
    pub action: Option<Action>,
    /// The rule that produced the decision, rendered for the record.
    pub rule: Option<String>,
    /// Whether the action was actually taken.
    pub taken: bool,
    /// Why not, when it was not.
    pub refusal: Option<Refusal>,
}

#[cfg(test)]
impl ActionRecord {
    /// A short operator-facing sentence.
    pub fn summary(&self) -> String {
        match (&self.action, self.taken, &self.refusal) {
            (Some(action), true, _) => format!("{}: took {action}", self.process),
            (Some(action), false, Some(refusal)) => {
                format!("{}: withheld {action} — {}", self.process, refusal.reason())
            }
            (Some(action), false, None) => format!("{}: withheld {action}", self.process),
            (None, _, _) => format!("{}: no action proposed", self.process),
        }
    }
}

/// The guarded gate between a decision and a real action.
///
/// Holds the mutable state the limits need — when each process last acted, and the timestamps of
/// recent actions — plus a bounded record. The configuration is passed in on each call rather than
/// held, so a runtime change to `enabled` or to a policy takes effect on the next decision without a
/// restart and without this type needing to be told.
#[derive(Debug)]
pub struct Gate {
    /// When each process last had an action taken. Only successful actions enter here: a withheld
    /// action is not an action, and starting a cooldown from one would let a refusal silence the
    /// next real decision.
    last_action_at: HashMap<String, u64>,
    /// Timestamps of actions taken, oldest first, for the rate caps.
    ///
    /// One deque for the daemon, filtered per process, for the same reason `DecisionLog` uses one
    /// ring: a per-process deque bounds each process and therefore bounds nothing.
    taken_at: VecDeque<(String, u64)>,
    history: VecDeque<ActionRecord>,
    history_capacity: usize,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new(DEFAULT_HISTORY_CAPACITY)
    }
}

impl Gate {
    pub fn new(history_capacity: usize) -> Self {
        Self {
            last_action_at: HashMap::new(),
            taken_at: VecDeque::new(),
            history: VecDeque::new(),
            history_capacity: history_capacity.max(1),
        }
    }

    /// Decides whether a decision's action may be taken, and records the outcome.
    ///
    /// Returns the action to take, or `None` with the refusal recorded. **Admission is not
    /// execution**: the caller performs the action and calls [`Self::confirm`] once it has. Splitting
    /// those two is what makes the in-flight case safe — an action abandoned between admit and
    /// confirm consumes no cap and starts no cooldown, so the state describes what actually happened
    /// rather than what was intended.
    ///
    /// The limits are checked most-fundamental first, so the reported reason is the real one: a
    /// disabled daemon reports `Disabled`, not `InCooldown`.
    pub fn admit(
        &mut self,
        decision: &Decision,
        config: &ProtectionConfig,
        at_unix: u64,
    ) -> Option<Action> {
        let process = decision.process.as_str();
        let rule = decision.rule.map(|rule| rule.to_string());

        let Some(action) = decision.action else {
            self.record(ActionRecord {
                process: process.to_string(),
                at_unix,
                action: None,
                rule,
                taken: false,
                refusal: Some(Refusal::NoActionProposed),
            });
            return None;
        };

        if let Some(refusal) = self.refusal_for(process, action, config, at_unix) {
            self.record(ActionRecord {
                process: process.to_string(),
                at_unix,
                action: Some(action),
                rule,
                taken: false,
                refusal: Some(refusal),
            });
            return None;
        }

        self.record(ActionRecord {
            process: process.to_string(),
            at_unix,
            action: Some(action),
            rule,
            taken: true,
            refusal: None,
        });
        Some(action)
    }

    /// The first limit that withholds this action, or `None` when every limit permits it.
    ///
    /// Order is deliberate and is the reason a record names one reason rather than a set.
    fn refusal_for(
        &self,
        process: &str,
        action: Action,
        config: &ProtectionConfig,
        at_unix: u64,
    ) -> Option<Refusal> {
        // 1. The master switch, read fresh on every call so a runtime disable is immediate.
        if !config.enabled {
            return Some(Refusal::Disabled);
        }

        // 2. Per-process opt-in. Absent policy means observe only.
        if !config.acts_for(process) {
            return Some(Refusal::NotEnabledForProcess);
        }

        // 3. The permitted list. An empty list means nothing, even with `act: true`.
        let permitted = config.permitted_for(process);
        if !permitted.contains(&action) {
            return Some(Refusal::ActionNotPermitted { action, permitted });
        }

        // 4. Cooldown, per process.
        let cooldown = config.cooldown_for(process);
        if cooldown > 0
            && let Some(last) = self.last_action_at.get(process)
        {
            let elapsed = at_unix.saturating_sub(*last);
            if elapsed < cooldown {
                return Some(Refusal::InCooldown {
                    remaining_secs: cooldown - elapsed,
                    cooldown_secs: cooldown,
                });
            }
        }

        // 5. Per-process cap before daemon-wide: the narrower limit is the more useful thing to
        //    report, since it names the process the operator is already looking at.
        let window = config.rate_window_secs;
        let cap = config.cap_for(process);
        let taken = self.count_in_window(Some(process), window, at_unix);
        if taken >= cap {
            return Some(Refusal::PerProcessCapReached {
                taken,
                cap,
                window_secs: window,
            });
        }

        // 6. Daemon-wide cap.
        let daemon_taken = self.count_in_window(None, window, at_unix);
        if daemon_taken >= config.daemon_cap {
            return Some(Refusal::DaemonCapReached {
                taken: daemon_taken,
                cap: config.daemon_cap,
                window_secs: window,
            });
        }

        // Reaching a cap NEVER substitutes a different action. There is no branch here that could:
        // the function's whole vocabulary is "permit this action" or "refuse with a reason".
        None
    }

    /// Records that an admitted action was actually performed.
    ///
    /// This is what starts the cooldown and consumes cap. Called after the action, so an action that
    /// was abandoned — the daemon shut down, protection mode was disabled mid-flight, the restart
    /// failed — leaves no trace claiming it happened.
    ///
    /// Deliberately never called by the daemon's wiring: this crate's gate is observe-only, and
    /// execution is the second switch this change deliberately does not flip.
    #[cfg(test)]
    pub fn confirm(&mut self, process: &str, at_unix: u64) {
        self.last_action_at.insert(process.to_string(), at_unix);
        self.taken_at.push_back((process.to_string(), at_unix));
        // Bounded by the same capacity as the record, so a long-lived daemon does not accumulate
        // timestamps for ever. The window filter makes older entries irrelevant anyway.
        while self.taken_at.len() > self.history_capacity {
            self.taken_at.pop_front();
        }
    }

    /// Actions taken within the window, for one process or the whole daemon.
    ///
    /// Counted from timestamps rather than from a running total, which is what makes capacity return
    /// as the window passes without any expiry bookkeeping.
    fn count_in_window(&self, process: Option<&str>, window_secs: u64, at_unix: u64) -> u32 {
        let start = at_unix.saturating_sub(window_secs);
        self.taken_at
            .iter()
            .filter(|(name, at)| {
                *at >= start && process.map(|wanted| name == wanted).unwrap_or(true)
            })
            .count()
            .try_into()
            .unwrap_or(u32::MAX)
    }

    /// Seconds remaining of a process's cooldown. 0 when it is free to act.
    #[cfg(test)]
    pub fn cooldown_remaining(
        &self,
        process: &str,
        config: &ProtectionConfig,
        at_unix: u64,
    ) -> u64 {
        let cooldown = config.cooldown_for(process);
        self.last_action_at
            .get(process)
            .map(|last| cooldown.saturating_sub(at_unix.saturating_sub(*last)))
            .unwrap_or(0)
    }

    /// Actions taken for a process within the window.
    #[cfg(test)]
    pub fn actions_taken(&self, process: &str, config: &ProtectionConfig, at_unix: u64) -> u32 {
        self.count_in_window(Some(process), config.rate_window_secs, at_unix)
    }

    fn record(&mut self, record: ActionRecord) {
        while self.history.len() >= self.history_capacity {
            self.history.pop_front();
        }
        self.history.push_back(record);
    }

    /// Every retained record, oldest first.
    pub fn history(&self) -> impl Iterator<Item = &ActionRecord> {
        self.history.iter()
    }

    /// Retained records for one process, oldest first.
    #[cfg(test)]
    pub fn history_for(&self, process: &str) -> Vec<&ActionRecord> {
        self.history
            .iter()
            .filter(|record| record.process == process)
            .collect()
    }

    /// Drops one process's state and records, for a delete.
    pub fn forget(&mut self, process: &str) {
        self.last_action_at.remove(process);
        self.taken_at.retain(|(name, _)| name != process);
        self.history.retain(|record| record.process != process);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{Action, Decision, RuleId};

    /// A decision that proposes a restart, as `rules::decide` would produce for a leak.
    fn restart_decision(process: &str, at_unix: u64) -> Decision {
        Decision {
            process: process.to_string(),
            at_unix,
            rule: Some(RuleId::ResourceLeak),
            action: Some(Action::Restart),
            withheld: None,
            findings: Vec::new(),
        }
    }

    /// A config that permits restarting `process`, with everything else at its default.
    fn acting_config(process: &str) -> ProtectionConfig {
        let mut config = ProtectionConfig {
            enabled: true,
            ..ProtectionConfig::default()
        };
        config.processes.insert(
            process.to_string(),
            ProcessPolicy::acting([Action::Restart]),
        );
        config
    }

    /// The refusal recorded for the most recent admission.
    fn last_refusal(gate: &Gate) -> Option<Refusal> {
        gate.history()
            .last()
            .and_then(|record| record.refusal.clone())
    }

    // ---- observing is the default (12.1) ----

    #[test]
    fn the_default_configuration_acts_for_nobody() {
        // Two independent reasons nothing can act, so a half-written configuration cannot produce an
        // acting daemon: the master switch is off AND no process has a policy.
        let config = ProtectionConfig::default();
        assert!(!config.enabled);
        assert!(config.processes.is_empty());

        let mut gate = Gate::default();
        let admitted = gate.admit(&restart_decision("api", 1_000), &config, 1_000);

        assert_eq!(admitted, None, "the default must not act");
        assert_eq!(last_refusal(&gate), Some(Refusal::Disabled));
    }

    #[test]
    fn enabling_the_daemon_alone_does_not_act_for_a_process() {
        // The second switch. "Protection mode on" and "you may stop this process" are different
        // statements, so enabling observation must not silently grant restart.
        let config = ProtectionConfig {
            enabled: true,
            ..ProtectionConfig::default()
        };
        let mut gate = Gate::default();

        assert_eq!(
            gate.admit(&restart_decision("api", 1_000), &config, 1_000),
            None
        );
        assert_eq!(
            last_refusal(&gate),
            Some(Refusal::NotEnabledForProcess),
            "an unconfigured process must be observe-only"
        );
    }

    #[test]
    fn enabling_actions_is_per_process() {
        let mut config = acting_config("api");
        config
            .processes
            .insert("worker".to_string(), ProcessPolicy::default());
        let mut gate = Gate::default();

        assert_eq!(
            gate.admit(&restart_decision("api", 1_000), &config, 1_000),
            Some(Action::Restart)
        );
        assert_eq!(
            gate.admit(&restart_decision("worker", 1_000), &config, 1_000),
            None,
            "a process without act enabled must not be acted upon"
        );
    }

    #[test]
    fn a_decision_that_proposes_nothing_is_distinguished_from_a_refusal() {
        // "No rule wanted to act" and "a rule wanted to act and was stopped" must never look the
        // same in the record, or the history cannot answer why nothing happened.
        let mut gate = Gate::default();
        let config = acting_config("api");
        let decision = Decision::no_match("api", 1_000);

        assert_eq!(gate.admit(&decision, &config, 1_000), None);
        let refusal = last_refusal(&gate).expect("recorded");
        assert_eq!(refusal, Refusal::NoActionProposed);
        assert!(
            !refusal.is_safety_limit(),
            "nothing to do is not a safety limit"
        );
    }

    // ---- permitted-action list (12.2) ----

    #[test]
    fn an_unpermitted_action_is_refused_and_records_what_was_allowed() {
        // Restart is decided, but this process only permits notify.
        let mut config = ProtectionConfig {
            enabled: true,
            ..ProtectionConfig::default()
        };
        config
            .processes
            .insert("api".to_string(), ProcessPolicy::acting([Action::Notify]));
        let mut gate = Gate::default();

        assert_eq!(
            gate.admit(&restart_decision("api", 1_000), &config, 1_000),
            None
        );
        match last_refusal(&gate) {
            Some(Refusal::ActionNotPermitted { action, permitted }) => {
                assert_eq!(action, Action::Restart);
                // What WAS permitted is recorded, since that is usually what an operator needs in
                // order to fix the configuration.
                assert_eq!(permitted, vec![Action::Notify]);
            }
            other => panic!("expected a permission refusal, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_permission_list_means_no_action() {
        let mut config = ProtectionConfig {
            enabled: true,
            ..ProtectionConfig::default()
        };
        // `act: true` with nothing permitted: the two switches are independent, so this must refuse.
        config.processes.insert(
            "api".to_string(),
            ProcessPolicy {
                act: true,
                permitted: Vec::new(),
                ..ProcessPolicy::default()
            },
        );
        let mut gate = Gate::default();

        assert_eq!(
            gate.admit(&restart_decision("api", 1_000), &config, 1_000),
            None
        );
        assert!(matches!(
            last_refusal(&gate),
            Some(Refusal::ActionNotPermitted { .. })
        ));
    }

    // ---- cooldown (12.3) ----

    #[test]
    fn a_second_action_is_withheld_during_cooldown_and_resumes_after() {
        // The scenario the cooldown exists for: a finding that survives the action would otherwise
        // trigger it again on the very next tick.
        let config = acting_config("api");
        let mut gate = Gate::default();

        assert_eq!(
            gate.admit(&restart_decision("api", 1_000), &config, 1_000),
            Some(Action::Restart)
        );
        gate.confirm("api", 1_000);

        // Two seconds later, same finding, same decision.
        assert_eq!(
            gate.admit(&restart_decision("api", 1_002), &config, 1_002),
            None
        );
        match last_refusal(&gate) {
            Some(Refusal::InCooldown {
                remaining_secs,
                cooldown_secs,
            }) => {
                assert_eq!(cooldown_secs, DEFAULT_COOLDOWN_SECS);
                assert_eq!(remaining_secs, DEFAULT_COOLDOWN_SECS - 2);
            }
            other => panic!("expected a cooldown refusal, got {other:?}"),
        }

        // And once it has elapsed, the same decision proceeds.
        let after = 1_000 + DEFAULT_COOLDOWN_SECS;
        assert_eq!(
            gate.admit(&restart_decision("api", after), &config, after),
            Some(Action::Restart),
            "action must resume once the cooldown has elapsed"
        );
    }

    #[test]
    fn cooldown_is_per_process() {
        let mut config = acting_config("api");
        config.processes.insert(
            "worker".to_string(),
            ProcessPolicy::acting([Action::Restart]),
        );
        let mut gate = Gate::default();

        gate.admit(&restart_decision("api", 1_000), &config, 1_000);
        gate.confirm("api", 1_000);

        // `api` is in cooldown; `worker` must be unaffected.
        assert_eq!(
            gate.admit(&restart_decision("api", 1_010), &config, 1_010),
            None
        );
        assert_eq!(
            gate.admit(&restart_decision("worker", 1_010), &config, 1_010),
            Some(Action::Restart),
            "one process in cooldown must not block another"
        );
    }

    #[test]
    fn a_configured_cooldown_is_applied() {
        let mut config = acting_config("api");
        config.processes.insert(
            "api".to_string(),
            ProcessPolicy {
                act: true,
                permitted: vec![Action::Restart],
                cooldown_secs: Some(30),
                ..ProcessPolicy::default()
            },
        );
        let mut gate = Gate::default();

        gate.admit(&restart_decision("api", 1_000), &config, 1_000);
        gate.confirm("api", 1_000);

        assert_eq!(gate.cooldown_remaining("api", &config, 1_010), 20);
        // Free at 30s, which the daemon default of 300 would still be blocking — so the override is
        // proven to be consulted rather than assumed.
        assert_eq!(
            gate.admit(&restart_decision("api", 1_030), &config, 1_030),
            Some(Action::Restart)
        );
    }

    #[test]
    fn a_withheld_action_starts_no_cooldown() {
        // `confirm` is what starts a cooldown, not `admit`. A refusal is not an action, and letting
        // one start a cooldown would let the first refusal silence the next real decision.
        let config = ProtectionConfig::default(); // disabled, so every admit refuses
        let mut gate = Gate::default();

        gate.admit(&restart_decision("api", 1_000), &config, 1_000);
        assert_eq!(
            gate.cooldown_remaining("api", &config, 1_001),
            0,
            "a refusal must not start a cooldown"
        );
    }

    // ---- rate caps (12.4) ----

    #[test]
    fn actions_stop_at_the_per_process_cap_without_escalating() {
        let mut config = acting_config("api");
        // No cooldown, so this test is about the cap rather than about the cooldown.
        config.processes.insert(
            "api".to_string(),
            ProcessPolicy {
                act: true,
                permitted: vec![Action::Restart, Action::Notify],
                cooldown_secs: Some(0),
                max_actions: Some(2),
            },
        );
        let mut gate = Gate::default();

        for tick in 0..2u64 {
            let at = 1_000 + tick;
            assert_eq!(
                gate.admit(&restart_decision("api", at), &config, at),
                Some(Action::Restart),
                "the first two actions are within the cap"
            );
            gate.confirm("api", at);
        }

        assert_eq!(
            gate.admit(&restart_decision("api", 1_002), &config, 1_002),
            None
        );
        match last_refusal(&gate) {
            Some(Refusal::PerProcessCapReached { taken, cap, .. }) => {
                assert_eq!(taken, 2);
                assert_eq!(cap, 2);
            }
            other => panic!("expected a per-process cap refusal, got {other:?}"),
        }

        // No escalation: the record shows the SAME action withheld, never a stronger one
        // substituted. `Notify` is permitted here precisely so a substitution would be possible if
        // the code allowed it.
        let last = gate.history().last().expect("a record exists");
        assert_eq!(last.action, Some(Action::Restart));
        assert!(!last.taken);
        assert!(
            gate.history()
                .all(|record| record.action != Some(Action::Notify)),
            "reaching a cap must not substitute a different action"
        );
    }

    #[test]
    fn actions_stop_at_the_daemon_cap() {
        let mut config = ProtectionConfig {
            enabled: true,
            daemon_cap: 2,
            ..ProtectionConfig::default()
        };
        // Three processes, each well within its own cap, so only the daemon-wide limit can stop the
        // third.
        for name in ["api", "worker", "batch"] {
            config.processes.insert(
                name.to_string(),
                ProcessPolicy {
                    act: true,
                    permitted: vec![Action::Restart],
                    cooldown_secs: Some(0),
                    max_actions: Some(10),
                },
            );
        }
        let mut gate = Gate::default();

        for name in ["api", "worker"] {
            assert_eq!(
                gate.admit(&restart_decision(name, 1_000), &config, 1_000),
                Some(Action::Restart)
            );
            gate.confirm(name, 1_000);
        }

        assert_eq!(
            gate.admit(&restart_decision("batch", 1_000), &config, 1_000),
            None,
            "the daemon-wide cap must stop a process that is within its own"
        );
        assert!(matches!(
            last_refusal(&gate),
            Some(Refusal::DaemonCapReached {
                taken: 2,
                cap: 2,
                ..
            })
        ));
    }

    #[test]
    fn capacity_returns_as_the_window_passes() {
        // Counted from timestamps rather than a running total, which is what makes capacity return
        // without any expiry bookkeeping.
        let mut config = acting_config("api");
        config.processes.insert(
            "api".to_string(),
            ProcessPolicy {
                act: true,
                permitted: vec![Action::Restart],
                cooldown_secs: Some(0),
                max_actions: Some(1),
            },
        );
        let mut gate = Gate::default();

        gate.admit(&restart_decision("api", 1_000), &config, 1_000);
        gate.confirm("api", 1_000);
        assert_eq!(
            gate.admit(&restart_decision("api", 1_100), &config, 1_100),
            None
        );

        let after_window = 1_000 + config.rate_window_secs + 1;
        assert_eq!(
            gate.actions_taken("api", &config, after_window),
            0,
            "the earlier action has fallen out of the window"
        );
        assert_eq!(
            gate.admit(
                &restart_decision("api", after_window),
                &config,
                after_window
            ),
            Some(Action::Restart)
        );
    }

    // ---- dependency suppression and crash-loop refusal (12.5, 12.6) ----

    #[test]
    fn a_suppressed_decision_never_reaches_the_gate_as_an_action() {
        // 12.5 and the crash-loop half of 12.6 are enforced in `rules`, which is the right place:
        // they are reasons not to DECIDE an action, so the gate is never asked. Asserted here at the
        // join, because "the gate would have permitted it" is exactly the gap this pairing closes.
        use crate::findings::{
            Agreement, BaselineSummary, Detector, Direction, Evidence, EvidenceWindow, Finding,
            FindingKey, Metric, SampleRef,
        };
        use crate::rules::{ProcessFacts, RuleConfig, decide};
        use std::collections::BTreeMap;

        let evidence = Evidence {
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
            samples: vec![SampleRef::new(1_060, 400.0)],
            extra: BTreeMap::new(),
        };
        let leak = Finding::builder(
            FindingKey::new("api", Detector::ResourceLeak, Metric::MemoryBytes),
            1_060,
            evidence,
        )
        .build()
        .expect("valid fixture");

        // A config that WOULD permit a restart, so the refusal can only come from the decision.
        let config = acting_config("api");

        for facts in [
            ProcessFacts {
                implicated_dependencies: vec!["db".to_string()],
                ..ProcessFacts::running()
            },
            ProcessFacts {
                at_crash_loop_limit: true,
                ..ProcessFacts::running()
            },
            ProcessFacts {
                desired_stopped: true,
                ..ProcessFacts::running()
            },
        ] {
            let decision = decide("api", &[&leak], &facts, &RuleConfig::default(), 2_000);
            assert_eq!(
                decision.action, None,
                "the decision itself must withhold for {facts:?}"
            );

            let mut gate = Gate::default();
            assert_eq!(
                gate.admit(&decision, &config, 2_000),
                None,
                "nothing reaches the gate as an action for {facts:?}"
            );
            // And the record says "no action proposed" rather than naming a safety limit, because
            // the limit that applied was the rule's, and it is recorded there.
            assert_eq!(last_refusal(&gate), Some(Refusal::NoActionProposed));
        }
    }

    #[test]
    fn protection_mode_has_no_path_to_block_an_operator_command() {
        // Asserted as an ABSENCE, which is the only honest way to test "operator commands are
        // unaffected" from inside this module: there is no function here that takes a command, a
        // process handle, or anything an operator's request travels through. Every public entry point
        // takes a `Decision` — something only the detector pipeline produces — so an operator's
        // `restart` cannot reach this code to be refused by it.
        //
        // The complementary evidence is in `process_manager`: its command paths do not import
        // `protection`, so there is no call site to forget.
        let config = ProtectionConfig::default();
        let mut gate = Gate::default();

        // The gate can only ever answer questions about a decision. It withholds; it never returns
        // an action nobody decided.
        let admitted = gate.admit(&Decision::no_match("api", 1_000), &config, 1_000);
        assert_eq!(
            admitted, None,
            "the gate cannot produce an action from a decision that proposed none"
        );
    }

    // ---- runtime disable and in-flight safety (12.7, 12.8) ----

    #[test]
    fn disabling_takes_effect_immediately_without_a_restart() {
        // `enabled` is read fresh inside `refusal_for` on every call rather than cached at
        // construction, so flipping it applies to the next decision. A `Gate` that had captured the
        // config would need rebuilding, which is what "requires a daemon restart" looks like.
        let mut config = acting_config("api");
        let mut gate = Gate::default();

        assert_eq!(
            gate.admit(&restart_decision("api", 1_000), &config, 1_000),
            Some(Action::Restart)
        );

        config.enabled = false;

        assert_eq!(
            gate.admit(&restart_decision("api", 1_001), &config, 1_001),
            None,
            "a runtime disable must apply to the very next decision"
        );
        assert_eq!(last_refusal(&gate), Some(Refusal::Disabled));
    }

    #[test]
    fn disabling_stops_action_but_not_recording() {
        // Detection and the decision record continue; only the acting stops. Asserted by the history
        // still growing while disabled — an operator evaluating protection mode reads exactly this.
        let config = ProtectionConfig::default();
        let mut gate = Gate::default();

        let before = gate.history().count();
        gate.admit(&restart_decision("api", 1_000), &config, 1_000);
        gate.admit(&restart_decision("api", 1_002), &config, 1_002);

        assert_eq!(
            gate.history().count(),
            before + 2,
            "decisions must still be recorded while action is disabled"
        );
        assert!(gate.history().all(|record| !record.taken));
    }

    #[test]
    fn an_abandoned_action_leaves_consistent_state() {
        // Admission is not execution. If an action is admitted and then abandoned — the daemon shut
        // down, the restart failed, protection mode was disabled mid-flight — `confirm` is never
        // called, so no cooldown starts and no cap is consumed. The state describes what actually
        // happened rather than what was intended, and the next decision is free to act.
        let config = acting_config("api");
        let mut gate = Gate::default();

        assert_eq!(
            gate.admit(&restart_decision("api", 1_000), &config, 1_000),
            Some(Action::Restart)
        );
        // No `confirm`: the action never completed.

        assert_eq!(gate.cooldown_remaining("api", &config, 1_001), 0);
        assert_eq!(gate.actions_taken("api", &config, 1_001), 0);
        assert_eq!(
            gate.admit(&restart_decision("api", 1_001), &config, 1_001),
            Some(Action::Restart),
            "an abandoned action must not block the next decision"
        );
    }

    #[test]
    fn every_refusal_records_a_reason() {
        // The blanket requirement behind 12.8: a withheld action must always say which limit
        // withheld it. Asserted across every refusal the gate can produce, so a future variant that
        // forgets its reason fails here.
        let mut gate = Gate::default();

        // Disabled.
        gate.admit(&restart_decision("api", 1), &ProtectionConfig::default(), 1);
        // Not enabled for the process.
        let daemon_only = ProtectionConfig {
            enabled: true,
            ..ProtectionConfig::default()
        };
        gate.admit(&restart_decision("api", 2), &daemon_only, 2);
        // Not permitted.
        let mut notify_only = daemon_only.clone();
        notify_only
            .processes
            .insert("api".to_string(), ProcessPolicy::acting([Action::Notify]));
        gate.admit(&restart_decision("api", 3), &notify_only, 3);
        // Cooldown.
        let acting = acting_config("api");
        gate.admit(&restart_decision("api", 4), &acting, 4);
        gate.confirm("api", 4);
        gate.admit(&restart_decision("api", 5), &acting, 5);

        let withheld: Vec<&ActionRecord> = gate.history().filter(|record| !record.taken).collect();
        assert_eq!(withheld.len(), 4, "four refusals expected");
        for record in withheld {
            let refusal = record
                .refusal
                .as_ref()
                .expect("every withheld action records a refusal");
            assert!(
                refusal.is_safety_limit(),
                "{refusal:?} should be a safety limit"
            );
            assert!(!refusal.reason().is_empty());
            assert!(record.summary().contains("withheld"));
        }
    }

    #[test]
    fn the_record_is_bounded_and_scoped_per_process() {
        let mut gate = Gate::new(3);
        let config = ProtectionConfig::default();
        for tick in 0..10u64 {
            gate.admit(&restart_decision("api", tick), &config, tick);
        }
        assert_eq!(gate.history().count(), 3, "capacity must not be exceeded");
        let retained: Vec<u64> = gate.history().map(|record| record.at_unix).collect();
        assert_eq!(retained, vec![7, 8, 9], "oldest discarded first");

        let mut gate = Gate::new(16);
        gate.admit(&restart_decision("api", 1), &config, 1);
        gate.admit(&restart_decision("worker", 2), &config, 2);
        assert_eq!(gate.history_for("api").len(), 1);

        gate.forget("api");
        assert!(gate.history_for("api").is_empty());
        assert_eq!(
            gate.history_for("worker").len(),
            1,
            "forgetting one process must not clear another"
        );
    }
}
