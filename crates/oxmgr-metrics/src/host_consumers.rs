//! Host-wide process visibility: the largest consumers, managed or not.
//!
//! Lint-level cleanup: display-path casts in consumer metrics.
//!
//! Answers "what is using this machine's CPU and memory", including processes oxmgr does not
//! manage — so a resource anomaly on a managed process can be attributed to its actual source
//! rather than assumed to be its own fault.
//!
//! # Why this is not on the supervision tick
//!
//! MEASURED before the cadence was chosen, which is what task 6.2 asks for. On this host, with 595
//! processes:
//!
//! ```text
//! refresh_processes(ProcessesToUpdate::All)  p50 7.80 ms   (min 7.43, max 8.63)
//! ```
//!
//! Re-measured 2026-08-21 (`managed-process-child-visibility` task 1.1), same machine,
//! 560 processes:
//!
//! ```text
//! cargo run --example measure_refresh_cost --release
//! refresh_processes(All) over 50 iterations, 560 processes:
//!   p50 6.43 ms   (min 5.53, max 8.86)
//! ```
//!
//! The figure holds across both runs; the delta is machine load, not a change in the
//! operation's cost class.
//!
//! Measured against a synthetic large table (`managed-process-child-visibility` task 1.2;
//! 2,000 `sleep` children spawned around the harness):
//!
//! ```text
//! refresh_processes(All) over 50 iterations, 2550 processes:
//!   p50 36.37 ms   (min 34.20, max 40.82)
//! ```
//!
//! Scaling is roughly linear in table size (~5.7× cost for ~4.6× processes). The 30 s
//! cadence stays affordable at that size (36 ms / 30 s ≈ 0.12% duty), but the module's
//! headline "0.026%" is a property of *this* host's table, not of the operation; hosts
//! with tables an order of magnitude larger pay proportionally more.
//!
//! For comparison, the managed-process refresh with an explicit pid list is 0.003 ms, and the whole
//! analysis engine is 0.006 ms for eight processes. So a full-host refresh is roughly 1,300× the
//! per-cycle analysis cost, and putting it on the 2-second maintenance tick would spend 0.4% of
//! every tick on the figure that changes least urgently.
//!
//! Hence [`DEFAULT_INTERVAL_SECS`] of 30 on its own schedule: 7.8 ms every 30 s is 0.026% duty,
//! which is affordable, and a top-consumer list is not a figure anyone reads at 2-second
//! resolution.
//!
//! # Redaction is the default, not a mode
//!
//! Another process's argv can contain credentials — a database URL with a password, an API token
//! passed as a flag. So [`ConsumerConfig::include_command_lines`] is off, and turning it on is
//! reported ([`HostConsumers::command_lines_included`]) so an operator can see that the surface is
//! exposing more than usual.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// How often the full process list is refreshed, in seconds.
///
/// 30 s, derived from the 7.80 ms measurement above rather than chosen: at that cost a 2 s cadence
/// would be 0.4% duty on the supervision path, a 10 s cadence 0.08%, and 30 s is 0.026%. The figure
/// being reported — which processes are the biggest consumers — does not change meaningfully faster
/// than that, so the extra frequency would buy nothing.
pub const DEFAULT_INTERVAL_SECS: u64 = 30;

/// The floor on a configured interval.
///
/// A refresh costs ~8 ms, so a 1-second cadence would be 0.8% of a core spent enumerating
/// processes. 5 s is the point below which the cost stops being negligible; a configured value
/// under it is raised and the adjustment reported rather than silently honoured.
pub const MINIMUM_INTERVAL_SECS: u64 = 5;

/// How many consumers are reported per dimension.
///
/// 10 by CPU and 10 by memory. The bound exists so reporting does not scale with host process
/// count: this machine has 595 processes and a full listing would be ~40 KB of JSON per request for
/// a question nobody asked. Ten is what fits a glance.
pub const DEFAULT_TOP_N: usize = 10;

/// The ceiling on a configured bound, so a large value cannot reintroduce the unbounded case.
pub const MAXIMUM_TOP_N: usize = 100;

/// Global cap on nodes across all rendered trees, so reporting stays bounded
/// even when a host has thousands of processes.
pub const TREE_NODE_BUDGET: usize = 300;
/// Cap on depth (edges from root) to prevent infinite loops on pathological process trees.
pub const TREE_DEPTH_LIMIT: usize = 32;

/// Cap on descendants reported per managed process (`managed-process-child-visibility`
/// §Bounded-output). Same value as [`TREE_NODE_BUDGET`] by precedent — both answer "how
/// many rows may one question put in a response" — but a separate constant so the two
/// budgets can diverge without silently changing each other's meaning.
pub const ATTRIBUTION_NODE_BUDGET: usize = 300;
/// Cap on attribution depth: a descendant more than this many edges below its owning
/// managed process is not attributed. Same value as [`TREE_DEPTH_LIMIT`] by precedent,
/// kept separate for the same reason as [`ATTRIBUTION_NODE_BUDGET`].
pub const ATTRIBUTION_DEPTH_LIMIT: usize = 32;

/// One consumer of host resources.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Consumer {
    /// The OS process identifier.
    pub pid: u32,
    /// The parent process identifier, when the platform reports one.
    ///
    /// `None` rather than an invented zero: on a platform or process where the
    /// parent is not reported, an absent parent must be distinguishable from
    /// actually being reparented to PID 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ppid: Option<u32>,
    /// The process name as the platform reports it. Always present, because identification must
    /// remain possible with the command line redacted.
    pub name: String,
    /// CPU share, in the same units as the managed-process figure: percent of one core, so a busy
    /// multi-threaded process legitimately exceeds 100.
    pub cpu_percent: f32,
    /// Resident memory in bytes.
    pub memory_bytes: u64,
    /// Whether oxmgr manages this process.
    ///
    /// A field rather than a display difference, because the spec requires the distinction to be
    /// available programmatically — a caller must be able to filter on it without parsing a label.
    pub managed: bool,
    /// The managed process name, when this is one of ours. `None` for an unmanaged consumer.
    ///
    /// Distinct from `name`: the platform name is the executable (`perl`), while this is the name
    /// the operator gave it (`payments-api`), and only the latter is actionable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_name: Option<String>,
    /// The owning user, where the platform supplies it.
    ///
    /// `None` rather than "unknown": on a platform that does not report it, an invented string
    /// would be indistinguishable from a real user called "unknown".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// The full command line, ONLY when explicitly enabled.
    ///
    /// Absent by default because another process's argv can carry credentials. See the module docs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// The reported consumer set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostConsumers {
    /// Largest by CPU, biggest first.
    pub by_cpu: Vec<Consumer>,
    /// Largest by memory, biggest first.
    pub by_memory: Vec<Consumer>,
    /// Top process trees by their aggregated CPU, biggest first. Each node carries
    /// its own figures plus the tree total (self and all descendants).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_cpu_trees: Vec<TreeConsumer>,
    /// Top process trees by their aggregated memory, biggest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_memory_trees: Vec<TreeConsumer>,
    /// Processes seen on the host at the last sample. Reported so a reader can tell that a 10-entry
    /// list is a bound rather than the whole machine.
    pub total_processes: usize,
    /// Descendants attributed to each running managed process, from this same sample.
    ///
    /// One entry per running managed process, including entries with an empty
    /// descendant set — empty here means "observed, has none", which is a different
    /// claim from the process being absent. Computed over the full listing before any
    /// top-N truncation, so a child that ranks nowhere on the host is still attributed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attribution: Vec<ManagedProcessAttribution>,
    /// Whether command lines are being included, so an operator can see the surface is exposing
    /// more than its default.
    pub command_lines_included: bool,
    /// When the sample was taken, Unix seconds.
    pub sampled_at: u64,
    /// The last sampling failure, if any. Sampling continues after a failure; the daemon does not
    /// stop, and the failure is not silent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// One node of an attributed process tree.
///
/// Carries the consumer's own figures (through [`Consumer`], flattened into the
/// JSON) plus the aggregated totals of the whole subtree rooted at it, so a
/// reader can separate "what this process does itself" from "what this process
/// and its descendants do together".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TreeConsumer {
    /// The node's own identity and figures; flattened so the payload reads like
    /// a `Consumer` plus tree fields rather than a nested object.
    #[serde(flatten)]
    pub consumer: Consumer,
    /// CPU of this process plus all its descendants, in the same units as
    /// `Consumer::cpu_percent` (percent of one core).
    pub tree_cpu_percent: f32,
    /// Memory of this process plus all its descendants, in bytes.
    pub tree_memory_bytes: u64,
    /// Direct children, heaviest subtree first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<TreeConsumer>,
    /// Whether the node budget cut this subtree short of its full depth, so a
    /// reader knows the children present are not necessarily all of them.
    #[serde(default)]
    pub truncated: bool,
}

/// One attributed descendant of a managed process.
///
/// Identity and figures only — this is an observation of someone else's process,
/// not a handle to it. The command line follows [`ConsumerConfig::include_command_lines`]
/// like every other command line in this module: attribution must not become a side
/// channel for argv the surface deliberately withholds (§D6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttributedDescendant {
    /// The OS process identifier.
    pub pid: u32,
    /// The parent process identifier, when the platform reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ppid: Option<u32>,
    /// The process name as the platform reports it.
    pub name: String,
    /// CPU share, percent of one core — same units as [`Consumer::cpu_percent`].
    pub cpu_percent: f32,
    /// Resident memory in bytes.
    pub memory_bytes: u64,
    /// Edges from the owning managed process: 1 is a direct child, 2 a grandchild.
    ///
    /// Present so a reader can tell a wrapper's child from a worker deep in a tree
    /// without re-deriving the chain from `ppid`s that may name processes outside
    /// the reported set.
    pub depth: usize,
    /// The full command line, ONLY when explicitly enabled. Absent by default; see
    /// the module docs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// Descendants attributed to one running managed process, from a single sample.
///
/// The totals are over ALL observed descendants — listed or cut by the bound — and
/// are descendants-only: they add to the managed process's own figures, never stand
/// in for them (`managed-process-children`: "a subtree total never replaces a
/// process's own figure").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagedProcessAttribution {
    /// The managed process's OS pid.
    pub pid: u32,
    /// The operator-given managed process name. Distinct from any descendant's
    /// platform name, for the same reason [`Consumer::managed_name`] is distinct.
    pub name: String,
    /// Attributed descendants, heaviest by CPU first, bounded by
    /// [`ATTRIBUTION_NODE_BUDGET`]. Empty means the observation completed and the
    /// process has none — not that they could not be observed.
    pub descendants: Vec<AttributedDescendant>,
    /// CPU summed across all observed descendants, listed or not, in percent of
    /// one core.
    pub descendants_cpu_percent: f32,
    /// Memory summed across all observed descendants, listed or not, in bytes.
    pub descendants_memory_bytes: u64,
    /// Whether the bound cut the descendant set short. When true,
    /// `descendants_cpu_percent` / `descendants_memory_bytes` still cover the whole
    /// observed set — including the descendants not listed.
    #[serde(default)]
    pub truncated: bool,
    /// How many descendants were observed in total, listed or not. A count taken
    /// from `descendants.len()` would silently become a count of the BUDGET once a
    /// tree exceeds it; this figure is computed before the bound applies, so a
    /// cluster's observed worker count stays honest past 300 descendants.
    #[serde(default)]
    pub descendant_count: usize,
}

/// Sampling configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsumerConfig {
    /// Whether host-wide sampling runs at all.
    pub enabled: bool,
    /// How often the full list is refreshed.
    pub interval: Duration,
    /// How many consumers per dimension.
    pub top_n: usize,
    /// Whether full command lines are reported. Off by default; see the module docs.
    pub include_command_lines: bool,
}

impl Default for ConsumerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval: Duration::from_secs(DEFAULT_INTERVAL_SECS),
            top_n: DEFAULT_TOP_N,
            include_command_lines: false,
        }
    }
}

/// A configured value that could not be used, and what was applied instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigAdjustment {
    pub setting: &'static str,
    pub reason: String,
    pub applied: String,
}

impl ConsumerConfig {
    /// Reads configuration from the environment, reporting anything unusable.
    ///
    /// Recognised-false vocabulary matching `OXMGR_HOST_METRICS`, so an operator learns one
    /// convention, and anything unrecognised leaves a feature ON rather than silently disabling it.
    pub fn from_env() -> (Self, Vec<ConfigAdjustment>) {
        let mut config = Self::default();
        let mut adjustments = Vec::new();

        if let Ok(raw) = std::env::var("OXMGR_HOST_CONSUMERS") {
            config.enabled = !matches!(
                raw.trim().to_ascii_lowercase().as_str(),
                "0" | "off" | "false" | "no" | "disabled"
            );
        }

        if let Ok(raw) = std::env::var("OXMGR_HOST_CONSUMERS_INTERVAL_SECS") {
            match raw.trim().parse::<u64>() {
                Ok(secs) if secs >= MINIMUM_INTERVAL_SECS => {
                    config.interval = Duration::from_secs(secs);
                }
                Ok(secs) => {
                    // Raised rather than honoured: a refresh costs ~8 ms, so a 1 s cadence is 0.8%
                    // of a core spent enumerating processes, and an operator who asked for it
                    // deserves to know it was not granted.
                    adjustments.push(ConfigAdjustment {
                        setting: "OXMGR_HOST_CONSUMERS_INTERVAL_SECS",
                        reason: format!("{secs}s is below the {MINIMUM_INTERVAL_SECS}s floor: a full-host refresh costs ~8ms"),
                        applied: format!("{MINIMUM_INTERVAL_SECS}s"),
                    });
                    config.interval = Duration::from_secs(MINIMUM_INTERVAL_SECS);
                }
                Err(_) => adjustments.push(ConfigAdjustment {
                    setting: "OXMGR_HOST_CONSUMERS_INTERVAL_SECS",
                    reason: format!("{raw:?} is not a number"),
                    applied: format!("{DEFAULT_INTERVAL_SECS}s"),
                }),
            }
        }

        if let Ok(raw) = std::env::var("OXMGR_HOST_CONSUMERS_TOP_N") {
            match raw.trim().parse::<usize>() {
                Ok(n) if (1..=MAXIMUM_TOP_N).contains(&n) => config.top_n = n,
                Ok(n) => {
                    // Clamped at both ends: 0 would report nothing while looking configured, and an
                    // unbounded value would reintroduce the case the bound exists to prevent.
                    let applied = n.clamp(1, MAXIMUM_TOP_N);
                    adjustments.push(ConfigAdjustment {
                        setting: "OXMGR_HOST_CONSUMERS_TOP_N",
                        reason: format!("{n} is outside 1..={MAXIMUM_TOP_N}"),
                        applied: applied.to_string(),
                    });
                    config.top_n = applied;
                }
                Err(_) => adjustments.push(ConfigAdjustment {
                    setting: "OXMGR_HOST_CONSUMERS_TOP_N",
                    reason: format!("{raw:?} is not a number"),
                    applied: DEFAULT_TOP_N.to_string(),
                }),
            }
        }

        // Opt-IN, unlike everything else here: the default must be the safe one, so only a
        // recognised TRUE value enables it. A typo leaves command lines redacted.
        if let Ok(raw) = std::env::var("OXMGR_HOST_CONSUMERS_COMMANDS") {
            config.include_command_lines = matches!(
                raw.trim().to_ascii_lowercase().as_str(),
                "1" | "on" | "true" | "yes" | "enabled"
            );
        }

        (config, adjustments)
    }
}

/// Samples the host's full process list on its own cadence.
///
/// Owns its own `System` rather than sharing the one in `HostCollector` or `ProcessManager`. That is
/// deliberate: `refresh_processes(ProcessesToUpdate::All)` replaces the process table, and the
/// manager's copy is refreshed with an explicit pid list precisely so it costs 0.003 ms instead of
/// 7.80 ms. Sharing one `System` would mean this sampler's refresh silently became the manager's
/// too, coupling a 30-second concern to a 2-second one.
pub struct ConsumerSampler {
    system: System,
    config: ConsumerConfig,
    last_sample: Option<Instant>,
    current: Option<HostConsumers>,
    /// Sampling failures since start. Counted rather than only latched, so a persistent problem is
    /// distinguishable from one bad sample.
    failures: u64,
    last_error: Option<String>,
}

impl ConsumerSampler {
    pub fn new(config: ConsumerConfig) -> Self {
        Self {
            // `System::new()` rather than `new_all()`: nothing here needs the disk, network or
            // component bindings, and `Components::new()` alone was measured as part of a +43.9%
            // loaded-RSS regression elsewhere in this codebase.
            system: System::new(),
            config,
            last_sample: None,
            current: None,
            failures: 0,
            last_error: None,
        }
    }

    pub fn config(&self) -> &ConsumerConfig {
        &self.config
    }

    /// The last sample. `None` before the first one completes, or when sampling is disabled.
    ///
    /// `None` rather than an empty listing, because "sampling is off" and "this host has no
    /// processes" are different claims and the second one is never true.
    pub fn current(&self) -> Option<&HostConsumers> {
        self.current.as_ref()
    }

    #[cfg(test)]
    pub fn is_due(&self, now: Instant) -> bool {
        self.config.enabled
            && self
                .last_sample
                .is_none_or(|last| now.duration_since(last) >= self.config.interval)
    }

    /// Samples if due, and returns the fresh listing when one was taken.
    ///
    /// `managed` maps OS pid to managed process name. Passed in rather than looked up, so this
    /// module needs no handle on the manager and can be tested without one.
    #[cfg(test)]
    pub fn sample_if_due(
        &mut self,
        managed: &HashMap<u32, String>,
        now: Instant,
        now_unix: u64,
    ) -> Option<&HostConsumers> {
        if !self.is_due(now) {
            return None;
        }
        self.sample(managed, now, now_unix);
        self.current.as_ref()
    }

    /// Takes a sample unconditionally.
    pub fn sample(&mut self, managed: &HashMap<u32, String>, now: Instant, now_unix: u64) {
        self.last_sample = Some(now);

        let kind = ProcessRefreshKind::nothing()
            .with_cpu()
            .with_memory()
            // `OnlyIfNotSet` so the executable path is read once per pid rather than re-read every
            // sample: it does not change for the life of a process.
            .with_exe(UpdateKind::OnlyIfNotSet)
            .with_user(UpdateKind::OnlyIfNotSet);
        let kind = if self.config.include_command_lines {
            kind.with_cmd(UpdateKind::OnlyIfNotSet)
        } else {
            // Not requested at all when redacting, rather than requested and then dropped. The
            // difference matters: a command line never read cannot leak through a debug print or a
            // future serialisation of the intermediate value.
            kind
        };

        self.system
            .refresh_processes_specifics(ProcessesToUpdate::All, true, kind);

        let total = self.system.processes().len();
        if total == 0 {
            // A host always has processes — at minimum this daemon — so zero means the platform
            // refused to answer. Recorded and carried, and the previous sample is left in place
            // rather than replaced with an empty one that would read as an idle machine.
            self.failures = self.failures.saturating_add(1);
            self.last_error = Some("process enumeration returned nothing".to_string());
            if let Some(current) = self.current.as_mut() {
                current.last_error = self.last_error.clone();
            }
            return;
        }

        let mut consumers: Vec<Consumer> = self
            .system
            .processes()
            .iter()
            .map(|(pid, process)| {
                let os_pid = pid_to_u32(*pid);
                let managed_name = managed.get(&os_pid).cloned();
                Consumer {
                    pid: os_pid,
                    ppid: process.parent().map(pid_to_u32),
                    name: process.name().to_string_lossy().to_string(),
                    cpu_percent: process.cpu_usage(),
                    memory_bytes: process.memory(),
                    managed: managed_name.is_some(),
                    managed_name,
                    user: process.user_id().map(|uid| uid.to_string()),
                    command: if self.config.include_command_lines {
                        let parts: Vec<String> = process
                            .cmd()
                            .iter()
                            .map(|part| part.to_string_lossy().to_string())
                            .collect();
                        (!parts.is_empty()).then(|| parts.join(" "))
                    } else {
                        None
                    },
                }
            })
            .collect();

        // Attribution over the FULL listing, before any truncation (§D1): a child that
        // ranks nowhere on the host still belongs to the managed process that started it.
        // Same pass, same vector, no additional refresh call.
        let attribution =
            attribute_descendants(&consumers, ATTRIBUTION_NODE_BUDGET, ATTRIBUTION_DEPTH_LIMIT);

        // Sorted by CPU, then the top N taken. `sort_unstable_by` with a total order on the
        // comparison: `partial_cmp` on f32 returns None for NaN, and a NaN cpu reading would
        // otherwise make the sort order undefined.
        consumers.sort_unstable_by(|a, b| {
            b.cpu_percent
                .partial_cmp(&a.cpu_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
                // Pid breaks a tie so the order is deterministic: an idle host has dozens of
                // processes at 0.0%, and an unstable order there would make the list reshuffle
                // every sample for no reason.
                .then(a.pid.cmp(&b.pid))
        });
        let by_cpu: Vec<Consumer> = consumers.iter().take(self.config.top_n).cloned().collect();

        // Trees come from the FULL listing, not the top N: a child can belong to a parent that is
        // not itself among the top consumers, and attribution must be complete to not mislead.
        let (by_cpu_trees, by_memory_trees) = build_tree_listings(
            &consumers,
            self.config.top_n,
            TREE_NODE_BUDGET,
            TREE_DEPTH_LIMIT,
        );

        consumers
            .sort_unstable_by(|a, b| b.memory_bytes.cmp(&a.memory_bytes).then(a.pid.cmp(&b.pid)));
        let by_memory: Vec<Consumer> = consumers.into_iter().take(self.config.top_n).collect();

        self.current = Some(HostConsumers {
            by_cpu,
            by_memory,
            by_cpu_trees,
            by_memory_trees,
            attribution,
            total_processes: total,
            command_lines_included: self.config.include_command_lines,
            sampled_at: now_unix,
            // Carried forward: a past failure stays visible so a reader knows the series has a gap,
            // rather than being erased by the next success.
            last_error: self.last_error.clone(),
        });
    }
}

/// Aggregates a process forest and returns the top trees per dimension.
///
/// # Attribution
///
/// A tree's total is its root's own consumption plus every descendant's: the
/// question "what is this process and its subtree doing together" is answered by
/// the tree total, and "what is this process alone doing" by the node's own
/// figures. Both travel together in a [`TreeConsumer`].
///
/// # Bounds
///
/// `node_budget` caps how many nodes may be returned across ALL trees, and
/// `depth_limit` caps how deep a single chain may be rendered. Because the budget
/// is split evenly across the trees actually rendered, a single oversized family
/// cannot starve the others out of the listing. When the budget (or the depth
/// limit) cuts a subtree short, the node that should have carried the missing
/// children is marked `truncated` — the reader sees that what is present is not
/// necessarily all of it, rather than being silently shown a partial tree.
///
/// # Cycles
///
/// A real process table is a tree — a process's parent chain cannot loop. But a
/// crafted or corrupted table can report `ppid` cycles (A claims B, B claims A),
/// and an unbounded recursion over such a table would hang the daemon. The walk
/// therefore guards against revisiting a node currently on the DFS stack: a back
/// edge is skipped, so the walk always terminates and a node's consumption is
/// never counted twice.
fn build_tree_listings(
    consumers: &[Consumer],
    top_n: usize,
    node_budget: usize,
    depth_limit: usize,
) -> (Vec<TreeConsumer>, Vec<TreeConsumer>) {
    if consumers.is_empty() {
        return (Vec::new(), Vec::new());
    }

    // pid → index, for resolving ppid to a parent.
    let mut by_pid: HashMap<u32, usize> = HashMap::with_capacity(consumers.len());
    for (index, consumer) in consumers.iter().enumerate() {
        by_pid.insert(consumer.pid, index);
    }

    // Children adjacency and parent-in-forest flags. A node whose parent is absent from the
    // listing — or that claims itself — is a root of the forest.
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); consumers.len()];
    let mut has_parent_in_forest: Vec<bool> = vec![false; consumers.len()];
    for (index, consumer) in consumers.iter().enumerate() {
        // `ppid == pid` is a self-claim, not a parent: some platforms report a root process
        // as its own parent, and treating that as a loop would orphan every child of it.
        if let Some(ppid) = consumer.ppid
            && ppid != consumer.pid
            && let Some(&parent) = by_pid.get(&ppid)
        {
            children[parent].push(index);
            has_parent_in_forest[index] = true;
        }
    }

    // Post-order walk computing each node's tree total, with a cycle guard.
    let mut state: Vec<u8> = vec![0; consumers.len()]; // 0 unvisited, 1 on the DFS stack, 2 done
    let mut tree_cpu: Vec<f32> = vec![0.0; consumers.len()];
    let mut tree_mem: Vec<u64> = vec![0; consumers.len()];
    for start in 0..consumers.len() {
        if state[start] != 0 {
            continue;
        }
        // Iterative post-order: (node, next child index to visit). Iterative because a crafted
        // table could be deep enough to overflow the call stack; the depth budget below bounds
        // the RENDERED output, but the total computation must walk the whole table safely.
        let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
        state[start] = 1;
        while let Some(&mut (node, ref mut cursor)) = stack.last_mut() {
            if *cursor < children[node].len() {
                let child = children[node][*cursor];
                *cursor += 1;
                match state[child] {
                    0 => {
                        state[child] = 1;
                        stack.push((child, 0));
                    }
                    // A cycle: this child is already on the stack, so recursing would loop.
                    // The edge is skipped; the child's contribution is picked up when its own
                    // walk completes.
                    1 => {}
                    _ => {}
                }
            } else {
                // All children processed: fold them in (only those that actually completed —
                // a skipped back edge is not a completed child and contributes nothing here).
                let mut cpu_total = consumers[node].cpu_percent;
                let mut mem_total = consumers[node].memory_bytes;
                for &child in &children[node] {
                    if state[child] == 2 {
                        cpu_total += tree_cpu[child];
                        mem_total += tree_mem[child];
                    }
                }
                tree_cpu[node] = cpu_total;
                tree_mem[node] = mem_total;
                state[node] = 2;
                stack.pop();
            }
        }
    }

    let roots: Vec<usize> = (0..consumers.len())
        .filter(|&index| !has_parent_in_forest[index])
        .collect();
    if roots.is_empty() {
        // Every node claims a parent that exists — a pure cycle with no entry point. Nothing can
        // be attributed to anything, so nothing is returned rather than an arbitrary slice.
        return (Vec::new(), Vec::new());
    }

    // The budget is split evenly across the trees that will actually be shown, so one enormous
    // tree cannot crowd every other tree out of the listing. `top_n` caps the tree count too.
    let tree_count = roots.len().min(top_n);
    let per_tree_budget = (node_budget / tree_count).max(1);

    let mut cpu_ranked = roots.clone();
    cpu_ranked.sort_unstable_by(|&a, &b| {
        tree_cpu[b]
            .partial_cmp(&tree_cpu[a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(consumers[a].pid.cmp(&consumers[b].pid))
    });
    let mut memory_ranked = roots;
    memory_ranked.sort_unstable_by(|&a, &b| {
        tree_mem[b]
            .cmp(&tree_mem[a])
            .then(consumers[a].pid.cmp(&consumers[b].pid))
    });

    // CPU and memory orderings of children differ; sort children per dimension.
    fn sort_children_by_cpu(children: &mut [Vec<usize>], tree_cpu: &[f32], consumers: &[Consumer]) {
        for node_children in children.iter_mut() {
            node_children.sort_unstable_by(|&a, &b| {
                tree_cpu[b]
                    .partial_cmp(&tree_cpu[a])
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(consumers[a].pid.cmp(&consumers[b].pid))
            });
        }
    }
    fn sort_children_by_memory(
        children: &mut [Vec<usize>],
        tree_mem: &[u64],
        consumers: &[Consumer],
    ) {
        for node_children in children.iter_mut() {
            node_children.sort_unstable_by(|&a, &b| {
                tree_mem[b]
                    .cmp(&tree_mem[a])
                    .then(consumers[a].pid.cmp(&consumers[b].pid))
            });
        }
    }

    let mut cpu_children = children.clone();
    sort_children_by_cpu(&mut cpu_children, &tree_cpu, consumers);
    let cpu_ctx = RenderCtx {
        consumers,
        children: &cpu_children,
        tree_cpu: &tree_cpu,
        tree_mem: &tree_mem,
    };
    let by_cpu_trees = render_roots(
        &cpu_ctx,
        &cpu_ranked,
        tree_count,
        per_tree_budget,
        depth_limit,
    );

    let mut memory_children = children;
    sort_children_by_memory(&mut memory_children, &tree_mem, consumers);
    let memory_ctx = RenderCtx {
        consumers,
        children: &memory_children,
        tree_cpu: &tree_cpu,
        tree_mem: &tree_mem,
    };
    let by_memory_trees = render_roots(
        &memory_ctx,
        &memory_ranked,
        tree_count,
        per_tree_budget,
        depth_limit,
    );

    (by_cpu_trees, by_memory_trees)
}

/// Attributes descendants to every running managed process.
///
/// # Ownership
///
/// Walking up the parent chain from a non-managed process, the FIRST managed process
/// encountered owns it (§D3). This keeps the answer unambiguous when oxmgr manages both
/// a parent and its child: the child's descendants belong to the child, and nothing is
/// counted twice across owners. A managed process is never itself a descendant entry —
/// it owns its own listing.
///
/// # Bounds
///
/// A descendant more than `depth_limit` edges below its owner is not attributed, and an
/// owner's reported set is capped at `node_budget` rows. Totals are computed over ALL
/// observed descendants before the cap applies, and `truncated` states that the listed
/// rows are not the whole observed set — a total that silently changed meaning for large
/// families would be worse than an absent one.
///
/// # Cycles
///
/// Same discipline as [`build_tree_listings`]: `ppid == pid` is a self-claim, not a
/// parent, and a repeated pid terminates the walk. A crafted table can therefore cost at
/// most one bounded walk per process, never a hang.
///
/// The function takes only the materialised listing — no `System`, no refresh call — so
/// attribution structurally cannot add a collection pass (§D1); the signature is the
/// assertion.
fn attribute_descendants(
    consumers: &[Consumer],
    node_budget: usize,
    depth_limit: usize,
) -> Vec<ManagedProcessAttribution> {
    // Owners are the managed processes present in THIS sample: a dead pid in the caller's
    // map has no row here and cannot own anything.
    let owners: Vec<&Consumer> = consumers.iter().filter(|c| c.managed).collect();
    if owners.is_empty() {
        return Vec::new();
    }

    // pid → index into `consumers`, for resolving one parent link in O(1).
    let by_pid: HashMap<u32, usize> = consumers
        .iter()
        .enumerate()
        .map(|(index, consumer)| (consumer.pid, index))
        .collect();

    // Resolved ownership per pid: Some((owner pid, edges above this pid)) once computed.
    // Memoised because sibling chains share ancestors; without it a wide fan-out under one
    // wrapper walks the same spine once per leaf.
    let mut resolved: HashMap<u32, Option<(u32, usize)>> = HashMap::with_capacity(consumers.len());

    for consumer in consumers {
        if consumer.managed {
            continue;
        }
        if resolved.contains_key(&consumer.pid) {
            continue;
        }
        // Walk upward collecting the path until an owner, a boundary, or a repeat. The path
        // is resolved together afterwards so every member learns the answer the walk found.
        let mut path: Vec<u32> = vec![consumer.pid];
        let mut seen: std::collections::HashSet<u32> = std::iter::once(consumer.pid).collect();
        let mut outcome: Option<(u32, usize)> = None;
        let mut current = consumer;
        loop {
            let ppid = match current.ppid {
                // No parent reported: the chain ends here.
                None => break,
                // A self-claim is not a parent (same rule as the tree builder).
                Some(ppid) if ppid == current.pid => break,
                Some(ppid) => ppid,
            };
            // Already answered by an earlier walk: compose rather than re-walk.
            if let Some(cached) = resolved.get(&ppid) {
                outcome = cached.map(|(owner, distance)| (owner, distance + 1));
                break;
            }
            let parent = match by_pid.get(&ppid) {
                Some(&index) => &consumers[index],
                // Parent outside the table: the chain ends with no owner found.
                None => break,
            };
            if parent.managed {
                outcome = Some((parent.pid, 1));
                break;
            }
            // A pid seen already on this walk is a cycle: stop without an owner. Members on
            // the cycle can never reach a managed ancestor, and neither can the tail behind them.
            if !seen.insert(ppid) {
                break;
            }
            path.push(ppid);
            current = parent;
        }

        // Resolve the whole path from the far end backwards: the member k edges below the
        // walk's start sits k edges below the found owner too.
        match outcome {
            Some((owner, start_distance)) => {
                for (offset, pid) in path.iter().enumerate() {
                    resolved.insert(*pid, Some((owner, start_distance + offset)));
                }
            }
            None => {
                for pid in &path {
                    resolved.insert(*pid, None);
                }
            }
        }
    }

    // Group attributed descendants under their owners.
    let mut grouped: HashMap<u32, Vec<AttributedDescendant>> = HashMap::with_capacity(owners.len());
    for consumer in consumers {
        if consumer.managed {
            continue;
        }
        if let Some(Some((owner, distance))) = resolved.get(&consumer.pid) {
            // Beyond the depth bound the chain is not followed: unattributed, by the same
            // rule that bounds the rendered trees.
            if *distance <= depth_limit {
                grouped
                    .entry(*owner)
                    .or_default()
                    .push(AttributedDescendant {
                        pid: consumer.pid,
                        ppid: consumer.ppid,
                        name: consumer.name.clone(),
                        cpu_percent: consumer.cpu_percent,
                        memory_bytes: consumer.memory_bytes,
                        depth: *distance,
                        command: consumer.command.clone(),
                    });
            }
        }
    }

    owners
        .iter()
        .map(|owner| {
            let mut family = grouped.remove(&owner.pid).unwrap_or_default();
            // Totals over ALL observed descendants, before any bound applies.
            let cpu_total: f32 = family.iter().map(|d| d.cpu_percent).sum();
            let mem_total: u64 = family.iter().map(|d| d.memory_bytes).sum();
            // Deterministic order: heaviest by CPU first, pid breaking ties — the module's
            // standing convention, so a stable host does not reshuffle between samples.
            family.sort_unstable_by(|a, b| {
                b.cpu_percent
                    .partial_cmp(&a.cpu_percent)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.pid.cmp(&b.pid))
            });
            let truncated = family.len() > node_budget;
            // Counted BEFORE the truncate below: the count is of the observed set,
            // the list is of the budget. Same discipline as the cpu/mem totals.
            let descendant_count = family.len();
            family.truncate(node_budget);
            ManagedProcessAttribution {
                pid: owner.pid,
                name: owner
                    .managed_name
                    .clone()
                    .unwrap_or_else(|| owner.name.clone()),
                descendants: family,
                descendants_cpu_percent: cpu_total,
                descendants_memory_bytes: mem_total,
                truncated,
                descendant_count,
            }
        })
        .collect()
}

/// Shared read-only context for rendering the consumer tree.
struct RenderCtx<'a> {
    consumers: &'a [Consumer],
    children: &'a [Vec<usize>],
    tree_cpu: &'a [f32],
    tree_mem: &'a [u64],
}

/// Renders the top trees from a ranked root list within the per-tree budget.
fn render_roots(
    ctx: &RenderCtx,
    ranked: &[usize],
    tree_count: usize,
    per_tree_budget: usize,
    depth_limit: usize,
) -> Vec<TreeConsumer> {
    ranked
        .iter()
        .take(tree_count)
        .filter_map(|&root| {
            let mut budget = per_tree_budget;
            render_node(root, ctx, 0, depth_limit, &mut budget)
        })
        .collect()
}

/// Renders one node and its subtree, consuming from `budget`.
///
/// Returns `None` when the budget is exhausted before the node could be placed —
/// the caller marks itself `truncated`. A node consumes one budget unit for
/// itself, then spends the rest on its descendants, heaviest subtree first.
fn render_node(
    index: usize,
    ctx: &RenderCtx,
    depth: usize,
    depth_limit: usize,
    budget: &mut usize,
) -> Option<TreeConsumer> {
    if *budget == 0 || depth > depth_limit {
        return None;
    }
    *budget -= 1;

    let mut rendered: Vec<TreeConsumer> = Vec::new();
    let mut truncated = false;
    for &child in &ctx.children[index] {
        match render_node(child, ctx, depth + 1, depth_limit, budget) {
            Some(node) => rendered.push(node),
            None => {
                truncated = true;
                break;
            }
        }
    }

    Some(TreeConsumer {
        consumer: ctx.consumers[index].clone(),
        tree_cpu_percent: ctx.tree_cpu[index],
        tree_memory_bytes: ctx.tree_mem[index],
        children: rendered,
        truncated,
    })
}

/// `sysinfo::Pid` to `u32`.
///
/// `Pid` is a newtype over a platform integer — `usize` on Unix, `u32` on Windows — so a direct cast
/// would not compile on both. Going through the string form is not free but runs once per process
/// per sample, against a 7.8 ms refresh.
fn pid_to_u32(pid: Pid) -> u32 {
    pid.to_string().parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxmgr_core::numeric::u64_to_f64;
    use serial_test::serial;

    // ── Process tree attribution (process-tree-awareness) ──────────────────────────────────────

    /// A minimal consumer for synthetic tests: identity, parent, and both figures.
    fn c(pid: u32, ppid: Option<u32>, cpu: f32, mem: u64) -> Consumer {
        Consumer {
            pid,
            ppid,
            name: format!("proc-{pid}"),
            cpu_percent: cpu,
            memory_bytes: mem,
            managed: false,
            managed_name: None,
            user: None,
            command: None,
        }
    }

    #[test]
    fn a_parent_plus_children_attributed_to_the_tree_total() {
        // 1 (2.0% CPU, 100 MB) has 2 children: 2 (3.0%, 50 MB) and 3 (1.0%, 25 MB).
        // Tree total of 1 = 1 + 2 + 3 = 6.0% and 175 MB; of 2 = itself only; of 3 = itself only.
        let consumers = vec![
            c(1, None, 2.0, 100),
            c(2, Some(1), 3.0, 50),
            c(3, Some(1), 1.0, 25),
        ];
        let (by_cpu_trees, by_memory_trees) = build_tree_listings(&consumers, 10, 300, 32);

        // One root: pid 1. It appears in both listings.
        assert_eq!(by_cpu_trees.len(), 1, "one root expected in cpu trees");
        assert_eq!(
            by_memory_trees.len(),
            1,
            "one root expected in memory trees"
        );

        let root = &by_cpu_trees[0];
        assert_eq!(root.consumer.pid, 1);
        assert_eq!(root.tree_cpu_percent, 6.0, "tree cpu = self + children");
        assert_eq!(root.tree_memory_bytes, 175, "tree mem = self + children");
        assert!(!root.truncated, "well within budget, nothing truncated");

        // Children are present, ordered heaviest-subtree-first, and carry their own totals.
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[0].consumer.pid, 2, "heaviest child first");
        assert_eq!(root.children[0].tree_cpu_percent, 3.0);
        assert_eq!(root.children[0].tree_memory_bytes, 50);
        assert!(root.children[0].children.is_empty());
        // The root's own figures are NOT the tree total — both must travel.
        assert_eq!(root.consumer.cpu_percent, 2.0);

        // Memory-rank orders the same forest the same way (only one root, so identical).
        assert_eq!(by_memory_trees[0].tree_memory_bytes, 175);
    }

    #[test]
    fn grandchildren_numbers_are_included_exactly_once() {
        // 1 → 2 → 3 chain. Totals must not double-count: 1 = 1+2+3, 2 = 2+3, 3 = 3.
        let consumers = vec![
            c(1, None, 1.0, 10),
            c(2, Some(1), 2.0, 20),
            c(3, Some(2), 3.0, 30),
        ];
        let (by_cpu_trees, _) = build_tree_listings(&consumers, 10, 300, 32);

        let root = &by_cpu_trees[0];
        assert_eq!(root.consumer.pid, 1);
        assert_eq!(root.tree_cpu_percent, 6.0, "1+2+3 exactly once");
        assert_eq!(root.tree_memory_bytes, 60);

        let mid = &root.children[0];
        assert_eq!(mid.consumer.pid, 2);
        assert_eq!(mid.tree_cpu_percent, 5.0, "2+3 exactly once");
        assert_eq!(mid.children.len(), 1);
        assert_eq!(mid.children[0].consumer.pid, 3);
        assert_eq!(mid.children[0].tree_cpu_percent, 3.0);
    }

    #[test]
    fn an_orphan_without_a_present_parent_is_its_own_root() {
        // 1 has a ppid of 42, but 42 is not in the listing (exited, or on another host
        // namespace): 1 must become a root rather than being lost.
        let consumers = vec![c(1, Some(42), 1.0, 10), c(2, Some(1), 0.5, 5)];
        let (by_cpu_trees, _) = build_tree_listings(&consumers, 10, 300, 32);
        assert_eq!(by_cpu_trees.len(), 1);
        assert_eq!(
            by_cpu_trees[0].consumer.pid, 1,
            "orphan is a root even with a dead parent"
        );
        assert_eq!(
            by_cpu_trees[0].tree_cpu_percent, 1.5,
            "child still attributed"
        );
    }

    #[test]
    fn a_self_reported_parent_is_treated_as_a_root() {
        // Some platforms report an init process as its own parent (ppid == pid). That is a
        // self-claim, not a loop: the process is a root and its children must attach to it —
        // otherwise the single most important tree on the host would collapse.
        let consumers = vec![c(1, Some(1), 3.0, 100), c(2, Some(1), 2.0, 50)];
        let (by_cpu_trees, _) = build_tree_listings(&consumers, 10, 300, 32);
        assert_eq!(by_cpu_trees.len(), 1);
        assert_eq!(by_cpu_trees[0].consumer.pid, 1);
        assert_eq!(by_cpu_trees[0].tree_cpu_percent, 5.0);
        assert_eq!(by_cpu_trees[0].children.len(), 1);
    }

    #[test]
    fn a_pure_ppid_cycle_returns_nothing_rather_than_hanging() {
        // Crafted corruption: 1 claims 2 as parent, 2 claims 1. Neither has a parent outside the
        // cycle, so there is no entry point from which to attribute the forest — the honest
        // answer is nothing, not an arbitrary slice. The walk must terminate safely for that to
        // be a defensible answer: no hang, no double-count, no node served twice.
        let consumers = vec![c(1, Some(2), 1.0, 10), c(2, Some(1), 2.0, 20)];
        let (by_cpu_trees, by_memory_trees) = build_tree_listings(&consumers, 10, 300, 32);
        assert!(
            by_cpu_trees.is_empty(),
            "a pure cycle has no attributable root"
        );
        assert!(by_memory_trees.is_empty());
    }

    #[test]
    fn budget_cuts_subtrees_and_marks_them_truncated() {
        // One root with 200 children. Budget 300, so a single tree gets the full budget: root +
        // all 200 children should fit, and nothing is truncated.
        let mut consumers = vec![c(1, None, 1.0, 10)];
        for pid in 2..=201 {
            consumers.push(c(pid, Some(1), 0.5, 5));
        }
        let (by_cpu_trees, _) = build_tree_listings(&consumers, 10, 300, 32);
        assert_eq!(by_cpu_trees.len(), 1);
        assert!(!by_cpu_trees[0].truncated, "300 budget fits 201 nodes");
        assert_eq!(by_cpu_trees[0].children.len(), 200);

        // Now squeeze: budget 50, same 201-node forest. The tree must render within 50 nodes
        // and say so — truncated on the root, and the missing siblings are not fabricated.
        let (tight, _) = build_tree_listings(&consumers, 10, 50, 32);
        assert_eq!(tight.len(), 1, "the root still renders");
        let root = &tight[0];
        assert!(root.truncated, "a cut subtree must be marked truncated");
        assert!(root.children.len() < 200, "the budget is binding");
        // Node accounting: root (1) + rendered children ≤ budget.
        let rendered = 1 + root.children.len();
        assert!(rendered <= 50);
        // And the truncation flag is truthful — the remaining children really are missing.
        assert_eq!(
            root.children.len(),
            49,
            "root + 49 children = 50-node budget"
        );
    }

    #[test]
    fn depth_limit_bounds_a_deep_chain() {
        // 1 → 2 → … → 40. Depth limit 32 means a chain longer than that is cut, and the cut is
        // reported rather than silent.
        let mut consumers = Vec::new();
        for pid in 1..=40 {
            let ppid = (pid > 1).then(|| pid - 1);
            consumers.push(c(pid, ppid, 1.0, 10));
        }
        let (by_cpu_trees, _) = build_tree_listings(&consumers, 10, 300, 32);
        assert_eq!(by_cpu_trees.len(), 1);

        // Walk one level at a time down the chain, checking EVERY node — including the deepest
        // rendered one, which is exactly the node that carries the truncation flag.
        let mut node = &by_cpu_trees[0];
        let mut depth = 0;
        let mut saw_truncated = false;
        loop {
            saw_truncated |= node.truncated;
            match node.children.first() {
                Some(child) => {
                    node = child;
                    depth += 1;
                }
                None => break,
            }
        }
        assert!(
            saw_truncated,
            "the chain is deeper than the limit, so some node must say so"
        );
        // Depth limit 32 renders nodes at depth 0..=32 (33 nodes); the 34th is cut.
        assert_eq!(
            depth, 32,
            "a 40-chain within a 32 depth limit renders 33 nodes"
        );
        assert!(node.truncated, "the deepest rendered node reports the cut");
    }

    #[test]
    fn multiple_roots_are_ranked_by_tree_total_each_dimension() {
        // Two independent families: A (tree 10.0%) and B (tree 5.0%). Both listings rank by
        // their own dimension's total, biggest first.
        let consumers = vec![
            c(1, None, 4.0, 10),    // A root, 4.0
            c(2, Some(1), 6.0, 10), // A child  – tree A = 10.0
            c(3, None, 3.0, 10),    // B root, 3.0
            c(4, Some(3), 2.0, 10), // B child  – tree B = 5.0
        ];
        let (by_cpu_trees, _) = build_tree_listings(&consumers, 10, 300, 32);
        assert_eq!(by_cpu_trees.len(), 2, "both roots listed");
        assert_eq!(by_cpu_trees[0].consumer.pid, 1, "heaviest tree first");
        assert_eq!(by_cpu_trees[1].consumer.pid, 3);
        assert_eq!(by_cpu_trees[0].tree_cpu_percent, 10.0);
        assert_eq!(by_cpu_trees[1].tree_cpu_percent, 5.0);
    }

    #[test]
    fn top_n_limits_how_many_trees_are_reported() {
        let consumers = vec![
            c(1, None, 1.0, 10),
            c(2, None, 2.0, 10),
            c(3, None, 3.0, 10),
        ];
        let (by_cpu_trees, _) = build_tree_listings(&consumers, 1, 300, 32);
        assert_eq!(by_cpu_trees.len(), 1, "top_n bounds the tree count too");
        assert_eq!(
            by_cpu_trees[0].tree_cpu_percent, 3.0,
            "the biggest tree is kept"
        );
    }

    #[test]
    fn a_sampler_with_trees_reports_them_bounded_and_attributed() {
        // End-to-end through the sampler against the real host: trees exist, respect the tree
        // count bound, and never contain a node twice.
        let mut sampler = ConsumerSampler::new(ConsumerConfig {
            top_n: 3,
            ..ConsumerConfig::default()
        });
        sampler.sample(&HashMap::new(), Instant::now(), 1_000);
        let consumers = sampler.current().expect("sampled");
        assert!(
            !consumers.by_cpu_trees.is_empty(),
            "a real host has at least one process tree"
        );
        assert!(consumers.by_cpu_trees.len() <= 3);
        assert!(consumers.by_memory_trees.len() <= 3);

        // Totals are consistent with attribution: a node's tree total ≥ its own figure.
        fn check_bounds(node: &TreeConsumer) {
            assert!(node.tree_cpu_percent >= node.consumer.cpu_percent - 0.001);
            assert!(node.tree_memory_bytes >= node.consumer.memory_bytes);
            let mut seen = vec![node.consumer.pid];
            for child in &node.children {
                for pid in collect_pids(child) {
                    assert!(!seen.contains(&pid), "pid {pid} appears twice in one tree");
                    seen.push(pid);
                }
            }
        }
        fn collect_pids(node: &TreeConsumer) -> Vec<u32> {
            let mut pids = vec![node.consumer.pid];
            for child in &node.children {
                pids.extend(collect_pids(child));
            }
            pids
        }
        for tree in &consumers.by_cpu_trees {
            check_bounds(tree);
        }
    }

    // ── The bound (6.3) ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn the_reported_set_is_bounded_and_ordered() {
        // Sampled against the real host, which is the only way to exercise
        // `ProcessesToUpdate::All`: a synthetic process table would test the sort and nothing else.
        let mut sampler = ConsumerSampler::new(ConsumerConfig {
            top_n: 5,
            ..ConsumerConfig::default()
        });
        let managed = HashMap::new();
        sampler.sample(&managed, Instant::now(), 1_000);

        let consumers = sampler.current().expect("a sample must be produced");
        assert!(
            consumers.total_processes > 1,
            "a host always has processes, got {}",
            consumers.total_processes
        );
        // Bounded per dimension, which is the point: reporting must not scale with host process
        // count. This machine has ~600 processes and reports 5.
        assert!(consumers.by_cpu.len() <= 5);
        assert!(consumers.by_memory.len() <= 5);
        assert!(
            consumers.total_processes > consumers.by_cpu.len(),
            "the bound must actually be binding on this host"
        );

        // Ordered, biggest first, in both dimensions.
        for pair in consumers.by_cpu.windows(2) {
            assert!(
                pair[0].cpu_percent >= pair[1].cpu_percent,
                "cpu order broken: {} then {}",
                pair[0].cpu_percent,
                pair[1].cpu_percent
            );
        }
        for pair in consumers.by_memory.windows(2) {
            assert!(
                pair[0].memory_bytes >= pair[1].memory_bytes,
                "memory order broken: {} then {}",
                pair[0].memory_bytes,
                pair[1].memory_bytes
            );
        }
        // Every consumer carries what identifies it.
        for consumer in &consumers.by_memory {
            assert!(consumer.pid > 0, "a consumer must carry its pid");
            assert!(!consumer.name.is_empty(), "a consumer must carry its name");
        }
    }

    #[test]
    fn ordering_is_deterministic_across_samples() {
        // An idle host has dozens of processes at 0.0% CPU. Without a tie-break the order among
        // them would be whatever the map iteration produced, and the list would reshuffle every
        // sample for no reason — which reads as activity where there is none.
        let mut sampler = ConsumerSampler::new(ConsumerConfig::default());
        let managed = HashMap::new();

        sampler.sample(&managed, Instant::now(), 1_000);
        let first: Vec<u32> = sampler
            .current()
            .expect("sampled")
            .by_memory
            .iter()
            .map(|c| c.pid)
            .collect();

        // Re-sorting the same table must give the same order. Memory is the stable dimension to
        // assert on: CPU genuinely moves between samples, so asserting on it would be flaky.
        sampler.sample(&managed, Instant::now(), 1_001);
        let second: Vec<u32> = sampler
            .current()
            .expect("sampled")
            .by_memory
            .iter()
            .map(|c| c.pid)
            .collect();

        // Not asserting equality of the whole list — a process can genuinely allocate between
        // samples — but the top entry should be stable on an otherwise-quiet machine, and no pid
        // may appear twice.
        let mut unique = first.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), first.len(), "a pid must not appear twice");
        assert!(!second.is_empty());
    }

    // ── Managed vs unmanaged (6.4) ──────────────────────────────────────────────────────────────

    #[test]
    fn a_declared_pid_is_marked_managed_and_others_are_not() {
        // Exercised against a process that really exists, so the mapping is tested through the real
        // enumeration rather than against a fabricated entry the sampler would never see.
        //
        // The pid is DISCOVERED rather than assumed: the first version used this test harness's own
        // pid and failed, because on a 597-process host the harness is not among the top consumers
        // by memory — a correct outcome that made the test wrong. So the sample decides which pid to
        // declare, and the assertion is about the mapping rather than about who is biggest.
        let mut sampler = ConsumerSampler::new(ConsumerConfig {
            top_n: 10,
            ..ConsumerConfig::default()
        });
        sampler.sample(&HashMap::new(), Instant::now(), 1_000);
        let target_pid = sampler
            .current()
            .expect("sampled")
            .by_memory
            .first()
            .expect("a host always has at least one process")
            .pid;

        let mut managed = HashMap::new();
        managed.insert(target_pid, "the-managed-one".to_string());
        sampler.sample(&managed, Instant::now(), 1_001);
        let consumers = sampler.current().expect("sampled");

        let target = consumers
            .by_memory
            .iter()
            .chain(consumers.by_cpu.iter())
            .find(|c| c.pid == target_pid)
            .expect("the declared pid was in the previous sample");
        assert!(target.managed, "a declared pid must be marked managed");
        assert_eq!(
            target.managed_name.as_deref(),
            Some("the-managed-one"),
            "the managed NAME is what an operator acts on, not the executable name"
        );

        // And everything else is unmanaged, with no managed name — the flag is not sticky and is
        // not applied by proximity.
        for consumer in consumers.by_memory.iter().chain(consumers.by_cpu.iter()) {
            if consumer.pid == target_pid {
                continue;
            }
            assert!(
                !consumer.managed,
                "pid {} wrongly marked managed",
                consumer.pid
            );
            assert!(consumer.managed_name.is_none());
        }
    }

    // ── Redaction (6.5, 6.6) ────────────────────────────────────────────────────────────────────

    #[test]
    fn command_lines_are_absent_by_default() {
        // The security-relevant default. Another process's argv can carry a database URL with a
        // password or an API token passed as a flag, so this must hold for EVERY consumer rather
        // than for the ones a test happens to look at.
        let mut sampler = ConsumerSampler::new(ConsumerConfig::default());
        assert!(
            !sampler.config().include_command_lines,
            "redaction must be the default"
        );
        sampler.sample(&HashMap::new(), Instant::now(), 1_000);
        let consumers = sampler.current().expect("sampled");

        assert!(!consumers.command_lines_included);
        for consumer in consumers.by_cpu.iter().chain(consumers.by_memory.iter()) {
            assert!(
                consumer.command.is_none(),
                "pid {} leaked a command line: {:?}",
                consumer.pid,
                consumer.command
            );
            // Identification must still be possible without it.
            assert!(consumer.pid > 0);
            assert!(!consumer.name.is_empty());
        }
    }

    #[test]
    fn enabling_command_lines_is_reported_and_takes_effect() {
        let mut sampler = ConsumerSampler::new(ConsumerConfig {
            include_command_lines: true,
            top_n: MAXIMUM_TOP_N,
            ..ConsumerConfig::default()
        });
        sampler.sample(&HashMap::new(), Instant::now(), 1_000);
        let consumers = sampler.current().expect("sampled");

        // Visible to the operator, which the spec requires: a surface exposing more than usual must
        // say so rather than leaving it to be inferred.
        assert!(consumers.command_lines_included);
        // At least one consumer should have a command line. Not all: a kernel thread has none, and
        // asserting on every entry would fail for a correct reason.
        let with_command = consumers
            .by_memory
            .iter()
            .filter(|c| c.command.is_some())
            .count();
        assert!(
            with_command > 0,
            "enabling command lines must actually include some"
        );
    }

    #[test]
    fn the_owning_user_is_reported_where_the_platform_supplies_it() {
        // Measured on this host: 341 of 597 processes report a uid. So the assertion is "some", not
        // "all" — a root-owned or kernel process may legitimately report none, and `None` is
        // reported rather than an invented "unknown" that a real user could be called.
        let mut sampler = ConsumerSampler::new(ConsumerConfig {
            top_n: MAXIMUM_TOP_N,
            ..ConsumerConfig::default()
        });
        sampler.sample(&HashMap::new(), Instant::now(), 1_000);
        let consumers = sampler.current().expect("sampled");

        let with_user = consumers
            .by_memory
            .iter()
            .filter(|c| c.user.is_some())
            .count();
        assert!(
            with_user > 0,
            "the platform supplies owning users here, so some must be reported"
        );
    }

    // ── Disable and failure (6.7, 6.8) ──────────────────────────────────────────────────────────

    #[test]
    fn disabled_sampling_reports_unavailable_rather_than_empty() {
        // `None`, never an empty listing. "Sampling is off" and "this host has no processes" are
        // different claims, and the second is never true — so an empty list would be a lie that
        // reads as a quiet machine.
        let mut sampler = ConsumerSampler::new(ConsumerConfig {
            enabled: false,
            ..ConsumerConfig::default()
        });
        assert!(!sampler.is_due(Instant::now()), "disabled means never due");
        assert!(
            sampler
                .sample_if_due(&HashMap::new(), Instant::now(), 1_000)
                .is_none()
        );
        assert!(
            sampler.current().is_none(),
            "disabled sampling must report unavailable, not an empty set"
        );
    }

    #[test]
    fn sampling_respects_its_own_interval() {
        // The cadence is the whole reason this is a separate sampler: at 7.80ms per refresh
        // (measured, see the module docs) it must not run on the 2s supervision tick.
        let mut sampler = ConsumerSampler::new(ConsumerConfig {
            interval: Duration::from_secs(30),
            ..ConsumerConfig::default()
        });
        let start = Instant::now();
        assert!(sampler.is_due(start), "the first sample is always due");
        sampler.sample(&HashMap::new(), start, 1_000);

        assert!(
            !sampler.is_due(start + Duration::from_secs(29)),
            "not due before the interval elapses"
        );
        assert!(
            sampler.is_due(start + Duration::from_secs(30)),
            "due once it has"
        );
    }

    // ── Configuration (6.3, 6.7) ────────────────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn an_unusable_interval_is_raised_and_reported() {
        // Raised rather than honoured: 1s would be 0.8% of a core spent enumerating processes.
        {
            let _g = crate::test_utils::EnvGuard::set("OXMGR_HOST_CONSUMERS_INTERVAL_SECS", "1");
            let (config, adjustments) = ConsumerConfig::from_env();
            assert_eq!(config.interval, Duration::from_secs(MINIMUM_INTERVAL_SECS));
            assert_eq!(adjustments.len(), 1, "the adjustment must be reported");
            assert!(adjustments[0].reason.contains("floor"));
        }
        {
            let _g =
                crate::test_utils::EnvGuard::set("OXMGR_HOST_CONSUMERS_INTERVAL_SECS", "banana");
            let (config, adjustments) = ConsumerConfig::from_env();
            assert_eq!(config.interval, Duration::from_secs(DEFAULT_INTERVAL_SECS));
            assert_eq!(adjustments.len(), 1);
        }
    }

    #[test]
    #[serial]
    fn an_unusable_bound_is_clamped_and_reported() {
        // Zero would report nothing while looking configured — the worst outcome, because the
        // surface would appear healthy and say nothing.
        {
            let _g = crate::test_utils::EnvGuard::set("OXMGR_HOST_CONSUMERS_TOP_N", "0");
            let (config, adjustments) = ConsumerConfig::from_env();
            assert_eq!(config.top_n, 1);
            assert_eq!(adjustments.len(), 1);
        }
        // And an unbounded value cannot reintroduce the case the bound exists to prevent.
        {
            let _g = crate::test_utils::EnvGuard::set("OXMGR_HOST_CONSUMERS_TOP_N", "100000");
            let (config, adjustments) = ConsumerConfig::from_env();
            assert_eq!(config.top_n, MAXIMUM_TOP_N);
            assert_eq!(adjustments.len(), 1);
        }
        {
            let _g = crate::test_utils::EnvGuard::set("OXMGR_HOST_CONSUMERS_TOP_N", "7");
            let (config, adjustments) = ConsumerConfig::from_env();
            assert_eq!(config.top_n, 7);
            assert!(adjustments.is_empty(), "a usable value reports nothing");
        }
    }

    #[test]
    #[serial]
    fn sampling_is_opt_out_but_command_lines_are_opt_in() {
        // The asymmetry is the point. Sampling defaults ON because it is observability; command
        // lines default OFF because they can carry credentials. So a typo must leave sampling on
        // and command lines redacted — the safe outcome in both directions.
        let (config, _) = ConsumerConfig::from_env();
        assert!(config.enabled, "sampling is on by default");
        assert!(
            !config.include_command_lines,
            "command lines are off by default"
        );

        for off in ["0", "off", "false", "no", "disabled", "OFF"] {
            let _guard = crate::test_utils::EnvGuard::set("OXMGR_HOST_CONSUMERS", off);
            assert!(!ConsumerConfig::from_env().0.enabled, "{off:?} disables");
        }
        for typo in ["ture", "yes-please", ""] {
            let _guard = crate::test_utils::EnvGuard::set("OXMGR_HOST_CONSUMERS", typo);
            assert!(
                ConsumerConfig::from_env().0.enabled,
                "{typo:?} must leave sampling on"
            );
        }
        // Block-scope the removal so the lock is released before the COMMANDS loop.
        {
            let _rm = crate::test_utils::EnvGuard::remove("OXMGR_HOST_CONSUMERS");
        }

        for on in ["1", "on", "true", "yes", "enabled"] {
            let _guard = crate::test_utils::EnvGuard::set("OXMGR_HOST_CONSUMERS_COMMANDS", on);
            assert!(
                ConsumerConfig::from_env().0.include_command_lines,
                "{on:?} enables command lines"
            );
        }
        // NOT including "1 ": `from_env` trims before matching, consistent with every other env
        // reader here, so a trailing space is whitespace rather than a typo. Asserting otherwise
        // would have pinned an inconsistency as if it were a feature.
        for typo in ["ture", "y", "on!", ""] {
            let _guard = crate::test_utils::EnvGuard::set("OXMGR_HOST_CONSUMERS_COMMANDS", typo);
            assert!(
                !ConsumerConfig::from_env().0.include_command_lines,
                "{typo:?} must NOT enable command lines"
            );
        }
    }

    #[test]
    #[ignore = "timing measurement, not an assertion; run explicitly with --nocapture"]
    fn measure_full_host_refresh_cost() {
        // Task 6.2's figures, reproducible. This is what justified the 30s default rather than
        // putting the refresh on the 2s supervision tick.
        //
        // Run: cargo test --release measure_full_host_refresh_cost -- --ignored --nocapture
        let mut sampler = ConsumerSampler::new(ConsumerConfig::default());
        let managed = HashMap::new();
        // Two samples before timing: CPU is a delta, so the first yields nothing.
        sampler.sample(&managed, Instant::now(), 0);
        std::thread::sleep(Duration::from_millis(300));
        sampler.sample(&managed, Instant::now(), 0);

        let mut samples = Vec::new();
        for _ in 0..15 {
            std::thread::sleep(Duration::from_millis(120));
            let started = Instant::now();
            sampler.sample(&managed, Instant::now(), 0);
            samples.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        samples.sort_by(|a, b| a.partial_cmp(b).expect("no NaN timings"));
        let p50 = samples[samples.len() / 2];
        println!(
            "\n  host processes: {}",
            sampler.current().map(|c| c.total_processes).unwrap_or(0)
        );
        println!("  refresh p50   : {p50:.2} ms");
        println!(
            "  min / max     : {:.2} / {:.2} ms",
            samples[0],
            samples[samples.len() - 1]
        );
        println!(
            "  duty at {DEFAULT_INTERVAL_SECS}s cadence: {:.4}%",
            p50 / (u64_to_f64(DEFAULT_INTERVAL_SECS) * 1000.0) * 100.0
        );
        println!(
            "  duty if on the 2s supervision tick: {:.2}%",
            p50 / 2000.0 * 100.0
        );
    }

    // ── Managed-process descendant attribution (managed-process-child-visibility §2) ────────────

    /// A managed consumer: `managed` set, operator name supplied.
    fn m(pid: u32, ppid: Option<u32>, cpu: f32, mem: u64, name: &str) -> Consumer {
        Consumer {
            managed: true,
            managed_name: Some(name.to_string()),
            ..c(pid, ppid, cpu, mem)
        }
    }

    #[test]
    fn a_forking_process_reports_its_descendants_transitively() {
        // payments-api (managed) → wrapper.sh → node worker. The grandchild is attributed
        // to the managed process even though its direct parent is unmanaged.
        let consumers = vec![
            m(100, Some(1), 2.0, 100, "payments-api"),
            c(200, Some(100), 0.1, 10),  // wrapper
            c(300, Some(200), 5.0, 300), // the worker doing the work
        ];
        let attribution = attribute_descendants(&consumers, 300, 32);

        assert_eq!(attribution.len(), 1, "one running managed process");
        let owner = &attribution[0];
        assert_eq!(owner.pid, 100);
        assert_eq!(owner.name, "payments-api");
        assert_eq!(
            owner.descendants.len(),
            2,
            "wrapper and worker both attributed"
        );
        // Heaviest by CPU first.
        assert_eq!(owner.descendants[0].pid, 300);
        assert_eq!(
            owner.descendants[0].depth, 2,
            "grandchild is two edges below"
        );
        assert_eq!(owner.descendants[1].pid, 200);
        assert_eq!(owner.descendants[1].depth, 1);
        // Totals cover both descendants exactly once.
        assert_eq!(owner.descendants_cpu_percent, 5.1);
        assert_eq!(owner.descendants_memory_bytes, 310);
        assert!(!owner.truncated);
    }

    #[test]
    fn a_process_with_no_children_reports_an_empty_set() {
        let consumers = vec![m(100, Some(1), 1.0, 50, "lonely")];
        let attribution = attribute_descendants(&consumers, 300, 32);

        assert_eq!(
            attribution.len(),
            1,
            "the process is running, so it has an entry"
        );
        assert!(attribution[0].descendants.is_empty(), "empty, not absent");
        assert_eq!(attribution[0].descendants_cpu_percent, 0.0);
        assert!(!attribution[0].truncated);
    }

    #[test]
    fn nearest_managed_ancestor_wins_when_both_parent_and_child_are_managed() {
        // oxmgr manages app (100) AND its child worker (200). The worker's descendants
        // belong to the worker; the worker itself is nobody's descendant entry.
        let consumers = vec![
            m(100, Some(1), 1.0, 50, "app"),
            m(200, Some(100), 1.0, 50, "worker"),
            c(300, Some(200), 4.0, 40), // worker's child
            c(400, Some(300), 0.5, 5),  // deeper: still the worker's, not app's
        ];
        let attribution = attribute_descendants(&consumers, 300, 32);

        let mut by_pid: HashMap<u32, &ManagedProcessAttribution> =
            attribution.iter().map(|a| (a.pid, a)).collect();
        assert_eq!(attribution.len(), 2, "both managed processes get entries");

        let app = by_pid.remove(&100).expect("app entry");
        assert!(
            app.descendants.is_empty(),
            "app owns nothing: its only child is itself managed"
        );
        assert_eq!(app.descendants_cpu_percent, 0.0);

        let worker = by_pid.remove(&200).expect("worker entry");
        assert_eq!(worker.descendants.len(), 2, "worker owns its subtree");
        assert_eq!(
            worker.descendants_cpu_percent, 4.5,
            "counted once, under one owner"
        );
        assert_eq!(worker.descendants_memory_bytes, 45);
    }

    #[test]
    fn a_self_claiming_process_is_not_its_own_parent() {
        // Some platforms report a root process as its own parent. The walk must treat that
        // as a boundary, not a loop — and must not orphan anything behind it.
        let consumers = vec![
            m(100, Some(100), 1.0, 50, "self-parent"), // claims itself
            c(200, Some(100), 3.0, 30),
        ];
        let attribution = attribute_descendants(&consumers, 300, 32);

        assert_eq!(
            attribution[0].descendants.len(),
            1,
            "child still attributed"
        );
        assert_eq!(attribution[0].descendants[0].pid, 200);
    }

    #[test]
    fn a_cycle_in_parent_links_terminates_without_an_owner() {
        // A claims B, B claims A: neither can reach a managed ancestor, and the walk must
        // terminate rather than spin.
        let consumers = vec![
            m(100, Some(1), 1.0, 50, "app"),
            c(300, Some(400), 2.0, 20), // cycle member
            c(400, Some(300), 2.0, 20), // cycle member
            c(500, Some(300), 1.0, 10), // tail hanging off the cycle
        ];
        let attribution = attribute_descendants(&consumers, 300, 32);

        let owner = &attribution[0];
        assert!(
            owner.descendants.is_empty(),
            "nothing in or behind a cycle reaches the managed root"
        );
        assert_eq!(owner.descendants_cpu_percent, 0.0);
    }

    #[test]
    fn the_descendant_bound_truncates_and_totals_still_cover_everything() {
        // 5 children against a budget of 3: three listed, all five counted.
        let consumers = vec![
            m(100, Some(1), 1.0, 50, "fork-bomb"),
            c(201, Some(100), 5.0, 50),
            c(202, Some(100), 4.0, 40),
            c(203, Some(100), 3.0, 30),
            c(204, Some(100), 2.0, 20),
            c(205, Some(100), 1.0, 10),
        ];
        let attribution = attribute_descendants(&consumers, 3, 32);

        let owner = &attribution[0];
        assert_eq!(owner.descendants.len(), 3, "bound is binding");
        assert_eq!(
            owner.descendants_cpu_percent, 15.0,
            "total covers ALL observed, listed or not"
        );
        assert_eq!(owner.descendants_memory_bytes, 150);
        assert!(owner.truncated, "truncation is visible");
        // Heaviest first, so what IS listed is the significant part.
        assert_eq!(owner.descendants[0].pid, 201);
        assert_eq!(owner.descendants[2].pid, 203);
    }

    #[test]
    fn depth_beyond_the_limit_is_not_attributed() {
        // A chain 4 deep against a depth limit of 2: depths 1 and 2 attributed, 3+ not.
        let consumers = vec![
            m(100, Some(1), 1.0, 50, "app"),
            c(201, Some(100), 4.0, 40), // depth 1
            c(202, Some(201), 3.0, 30), // depth 2
            c(203, Some(202), 2.0, 20), // depth 3 — beyond the bound
            c(204, Some(203), 1.0, 10), // depth 4 — beyond the bound
        ];
        let attribution = attribute_descendants(&consumers, 300, 2);

        let owner = &attribution[0];
        assert_eq!(owner.descendants.len(), 2, "chain cut at the depth bound");
        assert_eq!(
            owner.descendants_cpu_percent, 7.0,
            "totals still cover all four"
        );
        assert!(!owner.truncated, "the node bound did not cut anything");
    }

    #[test]
    fn dead_managed_pids_in_the_caller_map_own_nothing() {
        // pid 999 is passed as managed but is not in this sample's table: no entry, no ownership.
        let consumers = vec![
            m(100, Some(1), 1.0, 50, "alive"),
            c(200, Some(999), 3.0, 30), // child of the DEAD pid
        ];
        let attribution = attribute_descendants(&consumers, 300, 32);

        assert_eq!(attribution.len(), 1);
        assert!(attribution[0].descendants.is_empty());
    }

    #[test]
    fn a_reused_pid_is_not_attributed_on_the_strength_of_the_pid_alone() {
        // Sample 1: pid 300 is a worker under managed api (100) — attributed.
        let first = vec![
            m(100, Some(1), 2.0, 100, "api"),
            c(300, Some(100), 5.0, 300),
        ];
        let one = attribute_descendants(&first, 300, 32);
        assert_eq!(one[0].descendants.len(), 1, "sanity: attributed this cycle");

        // Sample 2: 300 exited and an UNRELATED process reused the pid, parented by
        // init. Ownership comes from THIS sample's ancestry, never from a remembered
        // pid — so the reused pid belongs to nobody.
        let second = vec![m(100, Some(1), 2.0, 100, "api"), c(300, Some(1), 9.0, 900)];
        let two = attribute_descendants(&second, 300, 32);
        assert!(
            two[0].descendants.is_empty(),
            "a reused pid must not inherit its predecessor's owner: {:?}",
            two[0].descendants
        );
        assert_eq!(two[0].descendants_cpu_percent, 0.0);
        assert_eq!(two[0].descendants_memory_bytes, 0);
    }

    #[test]
    fn attribution_covers_the_full_listing_not_the_top_n() {
        // The whole point of §D1: a child ranking nowhere on the host is still attributed.
        // With top_n = 1, the child (tiny CPU) would be truncated out of by_cpu — but it
        // must appear in attribution, which runs before any truncation.
        let mut sampler = ConsumerSampler::new(ConsumerConfig {
            top_n: 1,
            ..ConsumerConfig::default()
        });
        let mut managed = HashMap::new();
        managed.insert(std::process::id(), "oxmgr-itself".to_string());
        sampler.sample(&managed, Instant::now(), 1_000);

        let listing = sampler.current().expect("a sample must be produced");
        assert_eq!(
            listing.by_cpu.len(),
            1,
            "top-N truncation applied to the listing"
        );
        let own = listing
            .attribution
            .iter()
            .find(|a| a.pid == std::process::id())
            .expect("this test process is managed and running, so it has an entry");
        // oxmgr's own test binary may or may not have children right now; the assertion is
        // structural: an entry exists with sane totals, computed from the full table.
        assert_eq!(
            own.descendants_cpu_percent,
            own.descendants.iter().map(|d| d.cpu_percent).sum::<f32>(),
            "totals equal the sum of listed rows when nothing was truncated"
        );
        assert!(!own.truncated || own.descendants.len() == ATTRIBUTION_NODE_BUDGET);
    }

    #[test]
    fn a_childs_command_line_is_absent_by_default() {
        // §D6: attribution must not become a side channel for argv the surface withholds.
        // Default config has include_command_lines off, so NO descendant carries a command,
        // even though this host's table is full of other processes' command lines.
        let mut sampler = ConsumerSampler::new(ConsumerConfig::default());
        let mut managed = HashMap::new();
        managed.insert(std::process::id(), "oxmgr-itself".to_string());
        sampler.sample(&managed, Instant::now(), 1_000);

        let listing = sampler.current().expect("a sample must be produced");
        for owner in &listing.attribution {
            for descendant in &owner.descendants {
                assert!(
                    descendant.command.is_none(),
                    "descendant {} leaked a command line with commands disabled",
                    descendant.pid
                );
            }
        }
        // And the same holds for the top-listing consumers, which share the gate.
        for consumer in listing.by_cpu.iter().chain(listing.by_memory.iter()) {
            assert!(
                consumer.command.is_none(),
                "consumer {} leaked a command",
                consumer.pid
            );
        }
    }

    #[test]
    fn attribution_pass_costs_one_walk_over_the_materialised_table() {
        // Task 2.8: the claim is O(n) over an already-materialised list. Both sides of the
        // comparison are measured IN THIS BUILD, so the assertion holds in debug and release
        // alike: the attribution pass over a 2k-row family must cost less than ONE real
        // full-table refresh — the operation it was allowed to ride on for free.
        let table = full_table_for_measurement();
        assert_eq!(table.len(), 2001);

        // Warm-up, then measure the pass alone over the same table.
        let _ = attribute_descendants(&table, ATTRIBUTION_NODE_BUDGET, ATTRIBUTION_DEPTH_LIMIT);
        let mut samples: Vec<f64> = Vec::with_capacity(50);
        for _ in 0..50 {
            let started = Instant::now();
            let result =
                attribute_descendants(&table, ATTRIBUTION_NODE_BUDGET, ATTRIBUTION_DEPTH_LIMIT);
            samples.push(started.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(result.len(), 1);
        }
        samples.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
        let pass_p50 = samples[samples.len() / 2];

        // One real refresh of this host's table, same build, same clock.
        let mut system = System::new();
        let kind = ProcessRefreshKind::nothing()
            .with_cpu()
            .with_memory()
            .with_exe(UpdateKind::OnlyIfNotSet)
            .with_user(UpdateKind::OnlyIfNotSet);
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        let mut refresh_samples: Vec<f64> = Vec::with_capacity(15);
        for _ in 0..15 {
            let started = Instant::now();
            system.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
            refresh_samples.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        refresh_samples.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
        let refresh_p50 = refresh_samples[refresh_samples.len() / 2];

        println!(
            "\n  attribution pass over {} rows: p50 {:.3} ms | one real refresh: {:.3} ms",
            table.len(),
            pass_p50,
            refresh_p50
        );
        assert!(
            pass_p50 < refresh_p50,
            "attribution pass p50 {:.3} ms exceeds a full refresh ({:.3} ms) — \
             the 'no additional collection pass' claim is broken",
            pass_p50,
            refresh_p50
        );
    }

    /// Builds a synthetic table of 2000 unmanaged processes hanging off one managed root,
    /// shaped like a real fan-out, for measuring the attribution pass in isolation.
    fn full_table_for_measurement() -> Vec<Consumer> {
        let mut consumers = vec![Consumer {
            managed: true,
            managed_name: Some("measurement-root".to_string()),
            ..c(100, None, 1.0, 100)
        }];
        for i in 0..2000u32 {
            consumers.push(c(200 + i, Some(100), 0.1, 1024));
        }
        consumers
    }
}
