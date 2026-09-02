//! Bounded retention of recent process events, for pattern analysis over what already happened.
//!
//! Lint-level cleanup: display-path casts in tests.
//!
//! Scaffold for OpenSpec change `process-intelligence`, tasks section(s) 2.
//! The contract is `openspec/changes/process-intelligence/specs/`; read it before adding to this file.
//!
//! Pure logic on purpose: nothing here reaches for the process manager, the HTTP layer or the
//! dashboard, so it can be unit-tested without a running daemon. Wiring it into those is a
//! separate integration step, which is why the module is currently unreferenced.
//!
//! The event bus is a 512-slot `broadcast` channel (`events.rs:13`) whose slots are reclaimed as
//! subscribers consume them, so nothing that happened more than 512 events ago is recoverable, and
//! nothing at all is recoverable when no subscriber is attached. Every detection in this change
//! needs the opposite: history that exists whether or not anyone was listening. That history is new
//! resident memory in a daemon whose whole point is to stay out of the way, so it is capped twice —
//! per process and in total — and both caps are stated in arithmetic below rather than left to be
//! discovered under load.
//!
//! Only lifecycle events are kept. `log:out` and `log:err` are published per line and would fill
//! any ring in seconds with data no failure pattern reads.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::ops::{Bound, RangeBounds};

use oxmgr_core::events::BusEvent;

/// Lifecycle events kept per process before the oldest is dropped.
///
/// One crash-and-restart cycle publishes four events — `process:crashed`,
/// `process:restarting`, `process:started`, `process:online` — so 128 slots hold 32 cycles. The
/// default `max_restarts` is 10 (`ecosystem.rs:286`), which means a process that reaches its
/// crash-loop limit still has every event of that loop retained, with room for the stable period
/// before it.
pub const DEFAULT_PER_PROCESS_CAPACITY: usize = 128;

/// Processes tracked before the least recently active one's history is released.
///
/// A cap on distinct processes rather than on total events is what makes the global bound
/// arithmetic instead of a race: no process can consume another's slots, because slots are not
/// shared. 256 is well past any managed set this daemon is aimed at, so in practice the eviction
/// path below is dead code that exists to keep the bound true if that assumption is wrong.
pub const DEFAULT_PROCESS_CAPACITY: usize = 256;

/// Which lifecycle transition an event records.
///
/// Deliberately narrower than [`BusEvent`]: log lines and `daemon:shutdown` are not process
/// lifecycle and are refused at the door by [`EventRetention::record_bus_event`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleKind {
    Started,
    Online,
    Stopped,
    Exited,
    Crashed,
    Restarting,
    Errored,
    HealthHealthy,
    HealthUnhealthy,
}

impl LifecycleKind {
    /// The bus event name this kind came from, so evidence can cite the wire format an operator
    /// saw in `oxmgr events` rather than an internal spelling.
    #[cfg(test)]
    pub fn event_name(self) -> &'static str {
        match self {
            LifecycleKind::Started => "process:started",
            LifecycleKind::Online => "process:online",
            LifecycleKind::Stopped => "process:stopped",
            LifecycleKind::Exited => "process:exited",
            LifecycleKind::Crashed => "process:crashed",
            LifecycleKind::Restarting => "process:restarting",
            LifecycleKind::Errored => "process:errored",
            LifecycleKind::HealthHealthy => "health:healthy",
            LifecycleKind::HealthUnhealthy => "health:unhealthy",
        }
    }

    /// Whether this kind ended a run of the process, i.e. it carries an exit status.
    pub fn is_exit(self) -> bool {
        matches!(
            self,
            LifecycleKind::Exited | LifecycleKind::Crashed | LifecycleKind::Restarting
        )
    }
}

/// One retained lifecycle event.
///
/// 40 bytes measured on 64-bit (asserted in the tests, since the global bound below is quoted from
/// it). The process name is the map key rather than a field: repeating it per event would more than
/// double the struct for a string that is identical across all 128 of a process's slots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedEvent {
    /// Unix epoch seconds, copied from the bus event's `at` rather than read from the clock here,
    /// so a retained event carries the time it happened and not the time it was recorded.
    pub at: u64,
    pub kind: LifecycleKind,
    /// `None` for a signal death or for a transition that is not an exit.
    pub exit_code: Option<i32>,
    /// POSIX signal name (`"SIGSEGV"`). `Box<str>` rather than `String`: 16 bytes against 24, and
    /// it is never mutated after extraction.
    pub signal: Option<Box<str>>,
    /// The process's restart count as of this event; 0 where the event does not carry one.
    pub restart_count: u32,
}

impl RetainedEvent {
    /// A transition with no exit status, for tests and for callers recording directly.
    pub fn transition(at: u64, kind: LifecycleKind) -> Self {
        Self {
            at,
            kind,
            exit_code: None,
            signal: None,
            restart_count: 0,
        }
    }

    /// An exit, with whichever of code or signal applies.
    pub fn exit(
        at: u64,
        kind: LifecycleKind,
        exit_code: Option<i32>,
        signal: Option<&str>,
        restart_count: u32,
    ) -> Self {
        Self {
            at,
            kind,
            exit_code,
            signal: signal.map(|s| s.into()),
            restart_count,
        }
    }

    /// The exit status as one comparable value, for detecting repeated *identical* exits.
    ///
    /// A code and a signal are different failures even when the numbers coincide, so they are kept
    /// distinguishable rather than folded into one integer.
    pub fn exit_status(&self) -> Option<ExitStatus<'_>> {
        match (self.exit_code, self.signal.as_deref()) {
            (_, Some(sig)) => Some(ExitStatus::Signal(sig)),
            (Some(code), None) => Some(ExitStatus::Code(code)),
            (None, None) => None,
        }
    }
}

/// How a run ended, as an equality-comparable value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus<'a> {
    Code(i32),
    Signal(&'a str),
}

/// One process's ring, oldest at the front.
#[derive(Debug)]
struct ProcessHistory {
    events: VecDeque<RetainedEvent>,
    /// Events dropped from this ring to make room. Counted rather than inferred, because once an
    /// event is evicted there is nothing left to infer from.
    dropped: u64,
}

impl ProcessHistory {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            // Allocated once at capacity so a full ring never reallocates and never grows past the
            // stated bound by a rounding-up of its backing store.
            events: VecDeque::with_capacity(capacity),
            dropped: 0,
        }
    }

    /// Timestamp of the newest retained event, which is what "least recently active" is judged on.
    fn last_at(&self) -> u64 {
        self.events.back().map(|e| e.at).unwrap_or(0)
    }

    fn push(&mut self, event: RetainedEvent, capacity: usize) {
        while self.events.len() >= capacity {
            self.events.pop_front();
            self.dropped += 1;
        }
        self.events.push_back(event);
    }
}

/// The answer to a query, carrying whether it is complete.
///
/// A caller counting exits to decide "this process failed identically five times" must not read a
/// truncated count as a total. `process.rs:327` documents its disk totals as a lower bound for the
/// same reason: the honest move is to say so in the type, not in a comment the caller will not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventQuery<'a> {
    /// Retained events matching the query, oldest first.
    pub events: Vec<&'a RetainedEvent>,
    /// Set when this process has had events evicted, so `events` is a lower bound on what happened
    /// and any count derived from it is a lower bound too.
    pub truncated: bool,
    /// How many of this process's events were evicted in total. Not scoped to the query window —
    /// the timestamps of dropped events are gone, so which of them fell inside the window is not
    /// answerable.
    pub dropped: u64,
}

impl EventQuery<'_> {
    /// Number of events returned. A lower bound on what occurred whenever [`Self::truncated`].
    pub fn count(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// Bounded, per-process retention of recent lifecycle events.
///
/// Two caps, both in slots rather than bytes, because a slot is a fixed-size [`RetainedEvent`]:
///
/// - per process: [`DEFAULT_PER_PROCESS_CAPACITY`] events
/// - globally: [`DEFAULT_PROCESS_CAPACITY`] processes
///
/// so the worst case is 256 × 128 = 32,768 events × 40 bytes = 1.31 MB of events, plus the
/// `VecDeque` and `HashMap` overhead for 256 entries and their name keys — under 1.4 MB total. That
/// is the whole memory cost of retention, and it does not grow with uptime: a daemon up for a month
/// holds the same as one up for a minute.
///
/// The per-process cap is the reason both are needed. A single global ring would let one
/// crash-looping process — four events per cycle, restarting every few seconds — evict every other
/// process's history within a minute, which is precisely when that history is wanted for
/// correlation. Per-process rings make each process's retention independent of its neighbours'
/// behaviour.
///
/// Eviction is oldest-first within a process, as the spec requires: recent events are what
/// detection reads, and an old event's value decays because every window in this change is bounded.
/// Across processes it is the least recently active process that is released, on the same reasoning
/// one level up — a process with no events for hours is the one whose history is least likely to
/// explain anything now. Only a process's own [`Self::forget`] is silent; both evictions are
/// counted.
#[derive(Debug)]
pub struct EventRetention {
    per_process_capacity: usize,
    process_capacity: usize,
    by_process: HashMap<Box<str>, ProcessHistory>,
    /// Processes released wholesale to stay inside `process_capacity`.
    evicted_processes: u64,
}

impl Default for EventRetention {
    fn default() -> Self {
        Self::new(DEFAULT_PER_PROCESS_CAPACITY, DEFAULT_PROCESS_CAPACITY)
    }
}

impl EventRetention {
    /// Zero for either capacity is treated as 1, matching how retention configuration elsewhere in
    /// this change falls back on an unusable value rather than refusing to start. A store that
    /// silently accepted 0 would report "no events" for a running process, which reads as "nothing
    /// happened".
    pub fn new(per_process_capacity: usize, process_capacity: usize) -> Self {
        Self {
            per_process_capacity: per_process_capacity.max(1),
            process_capacity: process_capacity.max(1),
            by_process: HashMap::new(),
            evicted_processes: 0,
        }
    }

    #[cfg(test)]
    pub fn per_process_capacity(&self) -> usize {
        self.per_process_capacity
    }

    #[cfg(test)]
    pub fn process_capacity(&self) -> usize {
        self.process_capacity
    }

    /// Processes currently holding history.
    #[cfg(test)]
    pub fn tracked_processes(&self) -> usize {
        self.by_process.len()
    }

    /// Retained events across all processes. Bounded by
    /// `per_process_capacity * process_capacity`.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.by_process.values().map(|h| h.events.len()).sum()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.by_process.values().all(|h| h.events.is_empty())
    }

    /// Whole process histories released to respect the process cap.
    #[cfg(test)]
    pub fn evicted_processes(&self) -> u64 {
        self.evicted_processes
    }

    /// Events dropped from one process's ring. `None` if that process is not tracked at all — which
    /// is distinct from a tracked process that has dropped nothing.
    #[cfg(test)]
    pub fn dropped_for(&self, process: &str) -> Option<u64> {
        self.by_process.get(process).map(|h| h.dropped)
    }

    /// Records one event against a process.
    ///
    /// Nothing here consults a subscriber list, and there is no path by which it could: retention
    /// is a function of the event alone, so an unobserved daemon retains exactly what an observed
    /// one does.
    pub fn record(&mut self, process: &str, event: RetainedEvent) {
        let per_process_capacity = self.per_process_capacity;
        if !self.by_process.contains_key(process) {
            self.evict_process_if_full();
            self.by_process.insert(
                process.into(),
                ProcessHistory::with_capacity(per_process_capacity),
            );
        }
        if let Some(history) = self.by_process.get_mut(process) {
            history.push(event, per_process_capacity);
        }
    }

    /// Records a bus event if it is process lifecycle, and reports whether it was kept.
    ///
    /// Returns `false` for `log:out`, `log:err` and `daemon:shutdown` — the first two because their
    /// volume is per output line and no failure pattern reads them, the third because it names no
    /// process. Recording is driven from the existing publication points so there is one path an
    /// event can travel; a parallel path is how retention and the stream drift apart.
    pub fn record_bus_event(&mut self, event: &BusEvent) -> bool {
        let (name, retained) = match event {
            BusEvent::ProcessStarted { at, process, .. } => (
                process.name.as_str(),
                RetainedEvent::transition(*at, LifecycleKind::Started),
            ),
            BusEvent::ProcessOnline { at, process, .. } => (
                process.name.as_str(),
                RetainedEvent::transition(*at, LifecycleKind::Online),
            ),
            BusEvent::ProcessStopped { at, process, .. } => (
                process.name.as_str(),
                RetainedEvent::transition(*at, LifecycleKind::Stopped),
            ),
            BusEvent::ProcessErrored { at, process, .. } => (
                process.name.as_str(),
                RetainedEvent::transition(*at, LifecycleKind::Errored),
            ),
            BusEvent::HealthHealthy { at, process } => (
                process.name.as_str(),
                RetainedEvent::transition(*at, LifecycleKind::HealthHealthy),
            ),
            BusEvent::HealthUnhealthy { at, process, .. } => (
                process.name.as_str(),
                RetainedEvent::transition(*at, LifecycleKind::HealthUnhealthy),
            ),
            BusEvent::ProcessExited { at, process, data } => (
                process.name.as_str(),
                RetainedEvent::exit(
                    *at,
                    LifecycleKind::Exited,
                    data.exit_code,
                    data.signal.as_deref(),
                    data.restart_count,
                ),
            ),
            BusEvent::ProcessCrashed { at, process, data } => (
                process.name.as_str(),
                RetainedEvent::exit(
                    *at,
                    LifecycleKind::Crashed,
                    data.exit_code,
                    data.signal.as_deref(),
                    data.restart_count,
                ),
            ),
            BusEvent::ProcessRestarting { at, process, data } => (
                process.name.as_str(),
                RetainedEvent::exit(
                    *at,
                    LifecycleKind::Restarting,
                    data.exit_code,
                    data.signal.as_deref(),
                    data.restart_count,
                ),
            ),
            BusEvent::LogOut { .. } | BusEvent::LogErr { .. } | BusEvent::DaemonShutdown { .. } => {
                return false;
            }
            // Analysis output is not retained here, and the reason is circular dependence: this ring
            // is the INPUT to failure-pattern detection, and the findings are its OUTPUT. Recording
            // an `anomaly:detected` would feed a detector's own conclusion back in as evidence for
            // the next one.
            //
            // Nothing is lost by it. Findings live in the registry with their full evidence, and
            // decisions live in `DecisionLog`, both bounded and both queryable — so these events
            // already have a durable home that is better suited than a lifecycle ring.
            BusEvent::AnomalyDetected { .. }
            | BusEvent::AnomalyCleared { .. }
            | BusEvent::RemediationDecided { .. } => {
                return false;
            }
        };
        self.record(name, retained);
        true
    }

    /// Releases the least recently active process's history when the process cap is already met.
    ///
    /// Linear in tracked processes, and only on the first event of a process that is not yet
    /// tracked — not per event. With `DEFAULT_PROCESS_CAPACITY` at 256 the scan never runs on any
    /// realistic managed set.
    fn evict_process_if_full(&mut self) {
        while self.by_process.len() >= self.process_capacity {
            let Some(victim) = self
                .by_process
                .iter()
                .min_by(|(a_name, a), (b_name, b)| {
                    // Name breaks a timestamp tie so eviction is deterministic; `HashMap` iteration
                    // order is not, and a store whose contents depend on hash seed is untestable.
                    a.last_at().cmp(&b.last_at()).then(a_name.cmp(b_name))
                })
                .map(|(name, _)| name.clone())
            else {
                return;
            };
            self.by_process.remove(&victim);
            self.evicted_processes += 1;
        }
    }

    /// Drops a process's history, for use when the process itself is deleted.
    ///
    /// Not counted as an eviction: nothing was lost that anyone can still ask about.
    pub fn forget(&mut self, process: &str) -> bool {
        self.by_process.remove(process).is_some()
    }

    /// Every retained event for one process, oldest first.
    pub fn events_for(&self, process: &str) -> EventQuery<'_> {
        self.query(process, ..)
    }

    /// Retained events for one process whose `at` falls in `window`, oldest first.
    ///
    /// Any range works, so a caller can ask for a closed window (`start..=end`), everything since a
    /// point (`start..`), or everything (`..`). Timestamps are appended in the order events are
    /// recorded, and the bus stamps `at` at publication, so the ring is already ordered by time and
    /// the filter is a single pass with no sort.
    pub fn query<R: RangeBounds<u64>>(&self, process: &str, window: R) -> EventQuery<'_> {
        let Some(history) = self.by_process.get(process) else {
            return EventQuery {
                events: Vec::new(),
                truncated: false,
                dropped: 0,
            };
        };
        let events = history
            .events
            .iter()
            .filter(|e| in_window(e.at, &window))
            .collect();
        EventQuery {
            events,
            // Truncation is a property of the ring, not of the window: an evicted event may well
            // have fallen inside it, and there is no way left to tell.
            truncated: history.dropped > 0,
            dropped: history.dropped,
        }
    }

    /// Names of every tracked process, sorted so callers iterate deterministically.
    pub fn processes(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.by_process.keys().map(|k| k.as_ref()).collect();
        names.sort_unstable();
        names
    }
}

/// `RangeBounds::contains` needs `u64: PartialOrd<u64>` through a borrow that a generic `R` does not
/// give without an extra bound, so the three cases are spelled out.
fn in_window<R: RangeBounds<u64>>(at: u64, window: &R) -> bool {
    let above_start = match window.start_bound() {
        Bound::Included(start) => at >= *start,
        Bound::Excluded(start) => at > *start,
        Bound::Unbounded => true,
    };
    let below_end = match window.end_bound() {
        Bound::Included(end) => at <= *end,
        Bound::Excluded(end) => at < *end,
        Bound::Unbounded => true,
    };
    above_start && below_end
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use oxmgr_core::events::EventProcessInfo;

    fn pinfo(name: &str) -> EventProcessInfo {
        EventProcessInfo {
            id: 1,
            name: name.to_string(),
            namespace: None,
            pid: None,
            command: String::new(),
            cwd: None,
        }
    }

    /// A bus event with a controlled `at`, since the constructors in `events.rs` stamp it from the
    /// wall clock and window filtering needs known timestamps.
    fn crashed_at(name: &str, at: u64, exit_code: Option<i32>) -> BusEvent {
        match BusEvent::process_crashed(pinfo(name), exit_code, None, 1, 0, vec![]) {
            BusEvent::ProcessCrashed { process, data, .. } => {
                BusEvent::ProcessCrashed { at, process, data }
            }
            other => panic!("expected ProcessCrashed, got {}", other.event_name()),
        }
    }

    // --- Scenario: Lifecycle events are recorded ---

    #[test]
    fn records_every_lifecycle_kind_with_time_and_process() {
        let mut store = EventRetention::default();
        let events = [
            BusEvent::process_started(pinfo("api")),
            BusEvent::process_online(pinfo("api")),
            BusEvent::process_stopped(pinfo("api")),
            BusEvent::process_exited(pinfo("api"), Some(0), None, 5, 0, vec![]),
            BusEvent::process_crashed(pinfo("api"), Some(1), None, 5, 1, vec![]),
            BusEvent::process_restarting(pinfo("api"), Some(1), None, 5, 2, 1),
            BusEvent::process_errored(pinfo("api")),
            BusEvent::health_healthy(pinfo("api")),
            BusEvent::health_unhealthy(pinfo("api"), "timeout".into(), 1),
        ];
        for event in &events {
            assert!(store.record_bus_event(event), "{}", event.event_name());
        }

        let kept = store.events_for("api");
        assert_eq!(kept.count(), events.len());
        let names: Vec<&str> = kept.events.iter().map(|e| e.kind.event_name()).collect();
        assert_eq!(
            names,
            [
                "process:started",
                "process:online",
                "process:stopped",
                "process:exited",
                "process:crashed",
                "process:restarting",
                "process:errored",
                "health:healthy",
                "health:unhealthy",
            ]
        );
        // Every event carries a time, and it is the publication time, not zero.
        assert!(kept.events.iter().all(|e| e.at > 0));
        assert_eq!(store.processes(), ["api"]);
    }

    #[test]
    fn log_lines_and_daemon_events_are_not_retained() {
        let mut store = EventRetention::default();
        assert!(!store.record_bus_event(&BusEvent::log_out(pinfo("api"), "x".into())));
        assert!(!store.record_bus_event(&BusEvent::log_err(pinfo("api"), "y".into())));
        assert!(!store.record_bus_event(&BusEvent::daemon_shutdown()));
        assert!(store.is_empty());
        assert_eq!(store.tracked_processes(), 0);
    }

    #[test]
    fn exit_details_are_preserved() {
        let mut store = EventRetention::default();
        store.record_bus_event(&BusEvent::process_crashed(
            pinfo("api"),
            Some(1),
            Some("SIGSEGV".into()),
            5,
            3,
            vec![],
        ));
        let kept = store.events_for("api");
        let event = kept.events[0];
        assert_eq!(event.exit_code, Some(1));
        assert_eq!(event.signal.as_deref(), Some("SIGSEGV"));
        assert_eq!(event.restart_count, 3);
        // A signal death and an exit code are distinguishable, which is what repeated-identical-exit
        // detection compares on.
        assert_eq!(event.exit_status(), Some(ExitStatus::Signal("SIGSEGV")));
        assert!(event.kind.is_exit());
    }

    // --- Scenario: Event retention is bounded ---

    #[test]
    fn per_process_ring_keeps_newest_and_drops_oldest() {
        let mut store = EventRetention::new(4, 8);
        for at in 1..=10 {
            store.record("api", RetainedEvent::transition(at, LifecycleKind::Started));
        }
        let kept = store.events_for("api");
        assert_eq!(kept.count(), 4, "capacity is 4, not {}", kept.count());
        // The most recent are retained; the oldest are discarded.
        let times: Vec<u64> = kept.events.iter().map(|e| e.at).collect();
        assert_eq!(times, [7, 8, 9, 10]);
        assert_eq!(kept.dropped, 6);
        assert_eq!(store.len(), 4);
    }

    #[test]
    fn global_bound_holds_at_capacity_in_both_dimensions() {
        let (per_process, processes) = (4, 3);
        let mut store = EventRetention::new(per_process, processes);
        for p in 0..10 {
            for at in 1..=10 {
                store.record(
                    &format!("p{p}"),
                    RetainedEvent::transition(at + p * 100, LifecycleKind::Crashed),
                );
            }
        }
        assert_eq!(store.tracked_processes(), processes);
        assert_eq!(store.len(), per_process * processes);
        // Least recently active released first: p0..p6 have older newest-events than p7..p9.
        assert_eq!(store.processes(), ["p7", "p8", "p9"]);
        assert_eq!(store.evicted_processes(), 7);
    }

    #[test]
    fn one_noisy_process_cannot_evict_another_history() {
        let mut store = EventRetention::new(4, 8);
        for at in 1..=4 {
            store.record(
                "quiet",
                RetainedEvent::transition(at, LifecycleKind::Online),
            );
        }
        // A crash loop, four events per cycle, far past its own capacity.
        for at in 100..500 {
            store.record(
                "looper",
                RetainedEvent::transition(at, LifecycleKind::Crashed),
            );
        }
        let quiet = store.events_for("quiet");
        assert_eq!(quiet.count(), 4);
        assert!(
            !quiet.truncated,
            "quiet process lost history to a neighbour"
        );
        assert_eq!(quiet.dropped, 0);
        assert_eq!(store.dropped_for("looper"), Some(396));
    }

    #[test]
    fn zero_capacity_falls_back_to_one_slot() {
        let mut store = EventRetention::new(0, 0);
        assert_eq!(store.per_process_capacity(), 1);
        assert_eq!(store.process_capacity(), 1);
        store.record("api", RetainedEvent::transition(1, LifecycleKind::Started));
        store.record("api", RetainedEvent::transition(2, LifecycleKind::Online));
        let kept = store.events_for("api");
        assert_eq!(kept.count(), 1);
        assert_eq!(kept.events[0].at, 2);
    }

    #[test]
    fn documented_bound_arithmetic_holds() {
        // The module doc quotes 256 x 128 = 32,768 events at 40 bytes = 1.31 MB. If either the
        // struct or a default changes, that sentence is wrong and this fails.
        assert_eq!(std::mem::size_of::<RetainedEvent>(), 40);
        assert_eq!(DEFAULT_PER_PROCESS_CAPACITY, 128);
        assert_eq!(DEFAULT_PROCESS_CAPACITY, 256);
        let slots = DEFAULT_PER_PROCESS_CAPACITY * DEFAULT_PROCESS_CAPACITY;
        assert_eq!(slots, 32_768);
        assert_eq!(slots * std::mem::size_of::<RetainedEvent>(), 1_310_720);
    }

    // --- Scenario: Events can be queried by process and window ---

    #[test]
    fn query_returns_only_that_process_within_the_window() {
        let mut store = EventRetention::default();
        for at in [10_u64, 20, 30, 40] {
            store.record_bus_event(&crashed_at("api", at, Some(1)));
            store.record_bus_event(&crashed_at("worker", at, Some(2)));
        }

        let window = store.query("api", 20..=30);
        assert_eq!(window.count(), 2);
        assert_eq!(
            window.events.iter().map(|e| e.at).collect::<Vec<_>>(),
            [20, 30]
        );
        // Scoped to the named process only.
        assert!(window.events.iter().all(|e| e.exit_code == Some(1)));

        // Exclusive end, and open-ended windows.
        assert_eq!(store.query("api", 20..30).count(), 1);
        assert_eq!(store.query("api", 30..).count(), 2);
        assert_eq!(store.query("api", ..).count(), 4);
    }

    #[test]
    fn query_for_unknown_process_is_empty_not_truncated() {
        let store = EventRetention::default();
        let result = store.query("nope", ..);
        assert!(result.is_empty());
        assert!(!result.truncated);
        assert_eq!(result.dropped, 0);
        // Distinguishable from a tracked process that has dropped nothing.
        assert_eq!(store.dropped_for("nope"), None);
    }

    #[test]
    fn window_outside_retained_range_is_empty() {
        let mut store = EventRetention::default();
        store.record_bus_event(&crashed_at("api", 100, Some(1)));
        assert!(store.query("api", 200..=300).is_empty());
    }

    #[test]
    fn events_are_returned_oldest_first() {
        let mut store = EventRetention::new(8, 8);
        for at in [5_u64, 6, 7, 8, 9] {
            store.record("api", RetainedEvent::transition(at, LifecycleKind::Started));
        }
        let times: Vec<u64> = store
            .events_for("api")
            .events
            .iter()
            .map(|e| e.at)
            .collect();
        assert!(
            times.windows(2).all(|w| w[0] <= w[1]),
            "not ordered: {times:?}"
        );
        assert_eq!(times.first(), Some(&5));
        assert_eq!(times.last(), Some(&9));
    }

    // --- Truncated counts are not presented as complete ---

    #[test]
    fn truncated_query_is_marked_and_count_is_a_lower_bound() {
        let mut store = EventRetention::new(4, 8);
        for at in 1..=10 {
            store.record(
                "api",
                RetainedEvent::exit(
                    at,
                    LifecycleKind::Crashed,
                    Some(1),
                    None,
                    u32::try_from(at).unwrap_or(0),
                ),
            );
        }
        let all = store.events_for("api");
        assert!(
            all.truncated,
            "6 events were evicted but the query claims completeness"
        );
        assert_eq!(all.dropped, 6);
        // 10 identical exits occurred; 4 are visible. A caller must not read 4 as the total.
        assert_eq!(all.count(), 4);
        let total = u64::try_from(all.count()).expect("count fits in u64") + all.dropped;
        assert!(total >= 10);

        // Truncation is a property of the ring, so a window query inherits it: an evicted event
        // may have fallen inside the window and there is no way left to check.
        let window = store.query("api", 8..=10);
        assert!(window.truncated);
        assert_eq!(window.count(), 3);
    }

    #[test]
    fn untruncated_query_is_not_marked() {
        let mut store = EventRetention::new(8, 8);
        for at in 1..=8 {
            store.record("api", RetainedEvent::transition(at, LifecycleKind::Online));
        }
        let all = store.events_for("api");
        assert!(!all.truncated);
        assert_eq!(all.dropped, 0);
        assert_eq!(all.count(), 8);
    }

    // --- Scenario: Retention does not depend on subscribers ---

    #[test]
    fn retention_holds_with_no_subscriber_and_past_bus_capacity() {
        // No broadcast receiver is created anywhere in this test; the store still fills. The bus is
        // 512 slots and drops history as it is consumed, so push well past it.
        let mut store = EventRetention::new(DEFAULT_PER_PROCESS_CAPACITY, 8);
        let published = oxmgr_core::events::BUS_CAPACITY * 3;
        let published_u64 = u64::try_from(published).expect("published count fits in u64");
        for at in 0..published_u64 {
            store.record_bus_event(&crashed_at("api", at, Some(1)));
        }
        let kept = store.events_for("api");
        assert_eq!(kept.count(), DEFAULT_PER_PROCESS_CAPACITY);
        assert_eq!(
            usize::try_from(kept.dropped).unwrap_or(usize::MAX),
            published - DEFAULT_PER_PROCESS_CAPACITY
        );
        assert_eq!(kept.events.last().map(|e| e.at), Some(published_u64 - 1));
    }

    // --- Release on delete ---

    #[test]
    fn forget_releases_history_and_is_not_an_eviction() {
        let mut store = EventRetention::default();
        store.record_bus_event(&crashed_at("api", 1, Some(1)));
        assert!(store.forget("api"));
        assert!(!store.forget("api"));
        assert_eq!(store.tracked_processes(), 0);
        assert_eq!(store.evicted_processes(), 0);
        assert!(store.events_for("api").is_empty());
    }
}
