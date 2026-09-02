//! In-memory orchestration of managed processes, including persistence,
//! restarts, health checks, file watching, and metric collection.
//!
//! Lint-level cleanup: casts here are wire-format conversions (timestamps,
//! memory values, interval durations) for IPC and API responses; all bounded
//! by realistic process metrics (≪ 2^53). See design decision 8.

use oxmgr_core::numeric::{duration_millis, u64_to_f64};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant as StdInstant};

use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Local;
use croner::parser::{CronParser, Seconds};
use sysinfo::{
    Pid as SysPid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::{Instant as TokioInstant, sleep};
use tracing::{debug, error, info, warn};

use oxmgr_core::events::{BusEvent, EventProcessInfo};

use self::git::{
    PullOutcome, constant_time_eq, ensure_origin_remote, ensure_repo_checkout, git_rev_parse_head,
    run_git, sha256_hex, short_commit,
};
use self::health::execute_health_check;
#[cfg(test)]
use self::restart::CRASH_RESTART_WINDOW_SECS;
use self::restart::{
    can_auto_restart, clear_health_state, compute_restart_delay_secs, crash_loop_limit_reached,
    crash_loop_limit_reached_at, exit_event_matches_process, mark_restarting,
    maybe_reset_backoff_attempt, now_epoch_secs, record_auto_restart, reset_auto_restart_state,
    terminal_exit_status,
};
#[cfg(all(test, unix))]
use self::runtime::graceful_wait_before_force_kill;
#[cfg(test)]
use self::runtime::{args_match_expected, program_matches_expected};
use self::runtime::{
    cleanup_process_cgroup, pid_matches_expected_process, process_exists, terminate_pid,
};
use self::spawn::{
    normalize_cluster_instances, parse_command_line, resolve_spawn_program, sanitize_name,
    validate_process_name,
};
use self::watch::watch_fingerprint_for_process;
#[cfg(test)]
use self::watch::{watch_fingerprint_for_dir, watch_fingerprint_for_roots};
use crate::logging::{LogRotationPolicy, ProcessLogs, prepare_log_files, process_logs_for_mode};
use crate::storage::{
    PersistedBaselineStore, PersistedDismissals, PersistedState, baseline_store_path,
    dismissal_store_path, load_baselines, load_dismissals, load_state, save_baselines,
    save_dismissals, save_state,
};
use oxmgr_core::errors::OxmgrError;
use oxmgr_metrics::cgroup;
use oxmgr_metrics::process::{
    DesiredState, HealthStatus, ManagedProcess, ProcessExitEvent, ProcessStatus, StartProcessSpec,
};

/// Findings and decisions as reported over IPC.
///
/// A flattened view rather than the `Finding` type itself: the CLI renders a table, and shipping the
/// full evidence tree to draw six columns would put the daemon's internal representation in the
/// client's parsing path. The HTTP endpoint serves the complete finding for anything that needs it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct FindingsReport {
    pub findings: Vec<FindingRow>,
    pub decisions: Vec<DecisionRow>,
    /// Processes whose baselines are still warming, so an empty report is explainable: a warming
    /// process has not been checked and found healthy, it has not been checked at all.
    #[serde(default)]
    pub warming: Vec<String>,
    /// Findings withheld by tuning, by scope. Distinguishes a quiet daemon from a blind one.
    #[serde(default)]
    pub suppressed_global: u64,
    #[serde(default)]
    pub suppressed_detector: u64,
    #[serde(default)]
    pub suppressed_process: u64,
}

/// One finding, flattened for display.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FindingRow {
    pub process: String,
    pub detector: String,
    pub metric: String,
    /// "active" or "cleared". A cleared finding is retained and shown, because "this resolved" is
    /// only observable if the clearing is visible for a while.
    pub status: String,
    pub confidence: f64,
    pub occurrence: u32,
    pub raised_at: u64,
    pub last_seen: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Operator guidance steps for this finding, from the same source as
    /// `/api/findings`. `None` when the detector is unknown to guidance — a build
    /// that meets a new detector name must not invent advice for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guidance: Option<Vec<String>>,
}

/// One decision, flattened for display.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DecisionRow {
    pub process: String,
    pub at: u64,
    pub rule: String,
    /// The action that would be taken. `None` when the matching rule declined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withheld: Option<String>,
}

mod git;
mod health;
mod restart;
mod runtime;
mod spawn;
mod watch;

/// Maximum number of stderr lines kept per process for crash diagnostics.
const STDERR_TAIL_CAPACITY: usize = 30;

/// Forwards one pipe (stdout or stderr) to a log file and emits line events on
/// the bus. When `stderr_buf` is provided, each stderr line is also pushed into
/// the ring buffer so `handle_exit_event` can attach the tail to crash events.
/// How long a pipe may sit idle before buffered output is flushed to disk. Short
/// enough that `oxmgr logs` reflects a quiet process almost immediately, long
/// enough that a burst amortises into few syscalls.
const LOG_FLUSH_IDLE: Duration = Duration::from_millis(200);
/// Upper bound between flushes while output is continuous, so a process that
/// never goes idle still reaches disk promptly.
const LOG_FLUSH_MAX_INTERVAL: Duration = Duration::from_secs(1);

/// Configuration for a single `forward_log_pipe` invocation. Grouped to keep
/// the forwarding function itself within the seven-argument clippy limit.
pub(crate) struct LogForwardParams {
    pub(crate) log_path: std::path::PathBuf,
    pub(crate) date_format: Option<String>,
    pub(crate) rotation: crate::logging::LogRotationPolicy,
    pub(crate) event_tx: broadcast::Sender<Arc<BusEvent>>,
    pub(crate) process_info: EventProcessInfo,
    pub(crate) is_stderr: bool,
    pub(crate) stderr_buf: Option<Arc<Mutex<VecDeque<String>>>>,
}

fn forward_log_pipe<R>(pipe: R, params: LogForwardParams)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    // Destructure once so the body keeps the original local-variable style — no
    // `params.` noise threaded through the loop.
    let LogForwardParams {
        log_path,
        date_format,
        rotation,
        event_tx,
        process_info,
        is_stderr,
        stderr_buf,
    } = params;

    tokio::spawn(async move {
        let mut reader = BufReader::new(pipe);
        let mut buffer = String::new();

        // One writer held for the pipe's lifetime. The previous implementation
        // opened and closed the file for every single line, which dominated cost
        // on a busy process and left rotation unevaluated after spawn.
        let writer = tokio::task::spawn_blocking({
            let path = log_path.clone();
            move || crate::logging::RotatingLogWriter::open(path, rotation)
        })
        .await;
        let mut writer = match writer {
            Ok(Ok(writer)) => Some(writer),
            _ => {
                warn!("failed to open log writer for {}", log_path.display());
                None
            }
        };
        let mut dirty = false;
        let mut last_flush = TokioInstant::now();
        // Reused across iterations so a timestamped line costs no allocation per line.
        // `format!` here allocated a fresh String for every line of every process.
        let mut framed = String::new();

        loop {
            buffer.clear();
            // Idle timeout doubles as the flush trigger: a process that stops
            // talking has its output on disk within LOG_FLUSH_IDLE.
            let read = tokio::time::timeout(LOG_FLUSH_IDLE, reader.read_line(&mut buffer)).await;
            let read = match read {
                Ok(result) => result,
                Err(_) => {
                    if dirty {
                        if let Some(handle) = writer.as_mut() {
                            // The discard is deliberate: best-effort flush on a best-effort channel
                            #[expect(
                                clippy::let_underscore_must_use,
                                reason = "best-effort flush on a best-effort channel"
                            )]
                            let _ = handle.flush();
                        }
                        dirty = false;
                        last_flush = TokioInstant::now();
                    }
                    continue;
                }
            };

            match read {
                Ok(0) => break,
                Ok(_) => {
                    // Borrow when there is no prefix to add: the previous `buffer.clone()`
                    // copied a string that was about to be written and dropped.
                    let file_line: &str = match &date_format {
                        Some(fmt) => {
                            framed.clear();
                            use std::fmt::Write as _;
                            // Writing into the reused buffer cannot fail for a String; if
                            // it somehow did, fall back to the unprefixed line rather than
                            // losing the output.
                            if write!(framed, "{}: {}", Local::now().format(fmt), buffer).is_ok() {
                                framed.as_str()
                            } else {
                                buffer.as_str()
                            }
                        }
                        None => buffer.as_str(),
                    };

                    if let Some(handle) = writer.as_mut() {
                        if handle.write(file_line.as_bytes()).is_err() {
                            warn!("failed writing to log {}", log_path.display());
                        } else {
                            dirty = true;
                        }
                        // Bound the wait even when output never pauses.
                        if dirty && last_flush.elapsed() >= LOG_FLUSH_MAX_INTERVAL {
                            // The discard is deliberate: best-effort flush on a best-effort channel
                            #[expect(
                                clippy::let_underscore_must_use,
                                reason = "best-effort flush on a best-effort channel"
                            )]
                            let _ = handle.flush();
                            dirty = false;
                            last_flush = TokioInstant::now();
                        }
                    }

                    // Publish the same text that was written to the file. Publishing
                    // the raw buffer instead made a line look different depending on
                    // which path delivered it: the tail (read from the file, on open
                    // and refresh) carried the configured timestamp prefix while the
                    // live stream (from this bus) did not, so timestamps appeared to
                    // stop after the first screenful. The file is the authoritative
                    // record, so the bus matches it.
                    // Publish the same text that was written to the file, so a line looks
                    // identical whether it arrived via the file tail or the event bus.
                    let trimmed = file_line.trim_end_matches(['\n', '\r']);
                    if !trimmed.is_empty() {
                        // Nobody subscribed means nobody will receive it, so constructing
                        // the event — a String allocation plus an Arc — is pure waste. On a
                        // process emitting thousands of lines a second that is thousands of
                        // allocations a second for no reader. The two subscribe sites are
                        // both per-client (the SSE stream and the event socket), so a zero
                        // count genuinely means no consumer; there is no permanent internal
                        // subscriber to starve.
                        let watched = event_tx.receiver_count() > 0;

                        // The stderr ring buffer is NOT conditional on subscribers: it feeds
                        // crash diagnostics on exit, which must work whether or not anyone
                        // happened to be watching at the time.
                        if let Some(ref buf) = stderr_buf
                            && let Ok(mut guard) = buf.lock()
                        {
                            if guard.len() >= STDERR_TAIL_CAPACITY {
                                guard.pop_front();
                            }
                            guard.push_back(trimmed.to_string());
                        }

                        if watched {
                            let line = trimmed.to_string();
                            let event = if is_stderr {
                                BusEvent::log_err(process_info.clone(), line)
                            } else {
                                BusEvent::log_out(process_info.clone(), line)
                            };
                            // The discard is deliberate: a send error means the receiver is gone (shutdown); nothing left to deliver to
                            #[expect(
                                clippy::let_underscore_must_use,
                                reason = "a send error means the receiver is gone (shutdown); nothing left to deliver to"
                            )]
                            let _ = event_tx.send(Arc::new(event));
                        }
                    }
                }
                Err(_) => break,
            }
        }

        // The pipe ended: nothing buffered may be lost.
        if let Some(mut handle) = writer.take() {
            // The discard is deliberate: best-effort flush on a best-effort channel
            #[expect(
                clippy::let_underscore_must_use,
                reason = "best-effort flush on a best-effort channel"
            )]
            let _ = handle.flush();
        }
    });
}

/// Validates a cron expression string and returns the next execution time (epoch seconds).
/// Returns an error if the cron expression is invalid.
pub(crate) fn calculate_next_cron_restart(cron_expr: &str, from_time: Option<u64>) -> Result<u64> {
    let cron = CronParser::builder()
        .seconds(Seconds::Required)
        .build()
        .parse(cron_expr)
        .map_err(|e| anyhow::anyhow!("invalid cron expression '{}': {}", cron_expr, e))?;

    let now = if let Some(timestamp) = from_time {
        let ts = i64::try_from(timestamp)
            .map_err(|_| anyhow::anyhow!("timestamp {timestamp} exceeds i64 range"))?;
        chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
            .ok_or_else(|| anyhow::anyhow!("invalid timestamp"))?
    } else {
        chrono::Utc::now()
    };

    cron.find_next_occurrence(&now, false)
        .map_err(|e| {
            anyhow::anyhow!(
                "no next execution time for cron expression '{}': {}",
                cron_expr,
                e
            )
        })
        .map(|dt| u64::try_from(dt.timestamp()).unwrap_or(0))
}

/// Returns the POSIX signal name for a process that was killed by a signal.
/// Returns `None` on Windows or when the process exited normally.
#[cfg(unix)]
fn exit_signal_name(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(|sig| {
        match sig {
            1 => "SIGHUP",
            2 => "SIGINT",
            3 => "SIGQUIT",
            4 => "SIGILL",
            5 => "SIGTRAP",
            6 => "SIGABRT",
            7 => "SIGBUS",
            8 => "SIGFPE",
            9 => "SIGKILL",
            10 => "SIGUSR1",
            11 => "SIGSEGV",
            12 => "SIGUSR2",
            13 => "SIGPIPE",
            14 => "SIGALRM",
            15 => "SIGTERM",
            _ => return format!("SIG{sig}"),
        }
        .to_string()
    })
}

#[cfg(not(unix))]
fn exit_signal_name(_status: &std::process::ExitStatus) -> Option<String> {
    None
}

/// Computes uptime in seconds from `last_started_at` to now.
fn uptime_secs_since(last_started_at: Option<u64>) -> u64 {
    let Some(started) = last_started_at else {
        return 0;
    };
    now_epoch_secs().saturating_sub(started)
}

/// Drains a stderr ring buffer into a `Vec<String>`, preserving order.
fn drain_stderr_buf(buf: &Arc<Mutex<VecDeque<String>>>) -> Vec<String> {
    buf.lock()
        .map(|mut g| g.drain(..).collect())
        .unwrap_or_default()
}

/// The slice of runtime configuration the manager actually reads: where state
/// and logs live, and how logs rotate.
///
/// Deliberately a plain-data struct rather than the daemon's `AppConfig`:
/// `AppConfig` fuses its model with env-var I/O (`load()` reads the process
/// environment and creates directories) and lives above this crate, so the
/// manager cannot name it without an upward dependency edge. The daemon
/// constructs a `ManagerConfig` from its loaded `AppConfig` at startup; the
/// manager never touches the environment itself.
#[derive(Debug, Clone)]
pub struct ManagerConfig {
    /// Base directory of the local Oxmgr installation (`OXMGR_HOME`).
    pub base_dir: std::path::PathBuf,
    /// Path of the persisted daemon state file.
    pub state_path: std::path::PathBuf,
    /// Directory holding per-process log files.
    pub log_dir: std::path::PathBuf,
    /// Log rotation policy applied to every managed process's logs.
    pub log_rotation: LogRotationPolicy,
}

/// Coordinates process lifecycle operations for one local Oxmgr daemon.
pub struct ProcessManager {
    config: ManagerConfig,
    processes: HashMap<String, ManagedProcess>,
    watch_fingerprints: HashMap<String, u64>,
    pending_watch_restarts: HashMap<String, PendingWatchRestart>,
    scheduled_restarts: HashMap<String, TokioInstant>,
    next_id: u64,
    exit_tx: UnboundedSender<ProcessExitEvent>,
    event_tx: broadcast::Sender<Arc<BusEvent>>,
    /// Per-process ring buffers of recent stderr lines, used to attach crash
    /// context (stack traces, panics, tracebacks) to exit events.
    stderr_buffers: HashMap<String, Arc<Mutex<VecDeque<String>>>>,
    system: System,
    /// When the previous metrics refresh ran, at millisecond resolution. Used to
    /// report the interval each per-refresh I/O amount actually covers, since the
    /// maintenance tick skips missed ticks and so is not a fixed 2s.
    metrics_sampled_at: Option<std::time::Instant>,
    /// Bounded per-process metric history, fed from `refresh_resource_metrics`.
    ///
    /// Deliberately fed from the existing sampling path rather than a second collection pass: the
    /// readings are already in hand there, so history costs a ring push per process per tick and no
    /// extra syscall. Bounded at ~193 KiB per process (900 raw + 720 minute slots), flat once the
    /// tiers fill.
    metric_history: oxmgr_analytics::metrics_history::MetricHistoryStore,
    /// Advisory dismissals, keyed by process name then rule id.
    ///
    /// Held in memory and persisted to its own file. A dismissal is an operator's statement that a
    /// configuration is deliberate, so it has to survive a restart — otherwise every daemon restart
    /// would resurrect warnings someone had already answered.
    dismissals: PersistedDismissals,
    /// Findings, decisions and detector state, advanced once per maintenance tick.
    ///
    /// Owned by the manager rather than living beside it, because the two things it needs — the
    /// metric history and the baselines — are already here, and passing them across a boundary
    /// every tick would mean either cloning them or holding a second lock on the supervision path.
    analysis: oxmgr_analytics::analysis::Engine,
    /// Per-process metric baselines, restored from a store separate from `state.json`.
    ///
    /// Keyed by process name. Each entry carries the config fingerprint it was learned under, so a
    /// restart with unchanged configuration keeps its warm baseline while a reconfigured process
    /// starts warming again — the decision lives in `ProcessBaselines::restore`, not here.
    baselines: HashMap<String, oxmgr_analytics::baseline::ProcessBaselines>,
    /// Bounded per-process lifecycle event history, fed from `emit`.
    ///
    /// Behind a `Mutex` rather than held as `&mut`: `emit` takes `&self` and several call sites
    /// publish while `self.processes` is already borrowed mutably (`emit_terminal_exit_event`, the
    /// health-check loop). Taking `&mut self` on `emit` would force those to be restructured, and
    /// restructuring live supervision code to add an observer is the wrong trade. The lock is
    /// uncontended in practice — every publication is on the daemon task — and held only for the
    /// ring push.
    event_history: Mutex<oxmgr_store::event_retention::EventRetention>,
    /// The kernel's memory ceiling for THIS daemon, read once at startup.
    ///
    /// `Some` only inside a container that actually imposes a limit (2.4): a cgroup
    /// `memory.max` is the level at which growth stops being possible, so a leak
    /// forecast projects toward it when it is lower than the configured limit — and
    /// toward it even when no limit is configured at all. `None` on a host, where
    /// there is no enforced ceiling distinct from the machine itself.
    container_memory_ceiling: Option<u64>,
    /// Scratch buffer for the pid list passed to sysinfo each refresh, held so the
    /// allocation happens once rather than on every 2s maintenance cycle.
    tracked_pid_scratch: Vec<SysPid>,
}

#[derive(Debug, Clone, Copy)]
struct PendingWatchRestart {
    due_at: TokioInstant,
    fingerprint: u64,
}

impl ProcessManager {
    /// Rebuilds the manager from persisted state and prepares runtime-only
    /// bookkeeping such as health scheduling and system metrics.
    pub fn new(
        config: ManagerConfig,
        exit_tx: UnboundedSender<ProcessExitEvent>,
        container_memory_ceiling: Option<u64>,
    ) -> Result<Self> {
        let state = load_state(&config.state_path)?;

        let mut processes = HashMap::new();
        let mut next_id = state.next_id.max(1);
        for mut process in state.processes {
            next_id = next_id.max(process.id + 1);
            if process.restart_backoff_cap_secs == 0 {
                process.restart_backoff_cap_secs = 300;
            }
            if process.restart_backoff_reset_secs == 0 {
                process.restart_backoff_reset_secs = 60;
            }
            if process.ready_timeout_secs == 0 {
                process.ready_timeout_secs = oxmgr_metrics::process::default_ready_timeout_secs();
            }
            process.health_status = HealthStatus::Unknown;
            process.health_failures = 0;
            process.next_health_check = process
                .health_check
                .as_ref()
                .map(|check| now_epoch_secs().saturating_add(check.interval_secs.max(1)));
            process.clear_resource_metrics();
            process.last_metrics_at = None;
            process.cgroup_path = None;
            process.refresh_config_fingerprint();
            processes.insert(process.name.clone(), process);
        }

        // Restored after the loop, so every process's fingerprint has been recomputed from its
        // current record first. Restoring inside the loop would compare against a fingerprint from
        // the previous run and keep a baseline whose workload had changed.
        //
        // `load_baselines` cannot fail — a missing, unreadable or corrupt store returns the default
        // — so a bad baseline file can never stop the daemon from recovering processes. That is the
        // whole reason it is a separate file from `state.json`.
        let baselines = Self::restore_baselines(&config.state_path, &processes);
        // Same forgiving load as baselines: a corrupt suppression list must not stop the daemon, and
        // the worst case of losing it is that some already-answered advisories reappear.
        let dismissals = load_dismissals(&dismissal_store_path(&config.state_path));

        Ok(Self {
            config,
            processes,
            dismissals,
            analysis: oxmgr_analytics::analysis::Engine::default(),
            baselines,
            watch_fingerprints: HashMap::new(),
            pending_watch_restarts: HashMap::new(),
            scheduled_restarts: HashMap::new(),
            next_id,
            exit_tx,
            // The manager owns its bus: `new_bus` stayed in the binary crate
            // because oxmgr-core is tokio-free, and the bus capacity constant
            // lives in core. Constructing inline keeps the fan-out point where
            // the events are produced (workspace-crate-layout reactive table).
            event_tx: broadcast::channel(oxmgr_core::events::BUS_CAPACITY).0,
            stderr_buffers: HashMap::new(),
            // Narrowed from `System::new_all()`, which is `RefreshKind::everything()`:
            // it enumerated every process on the host and loaded memory, CPU, disk,
            // network, component and user data at startup. The daemon reads exactly one
            // thing from this handle — `.process(pid)` for its own tracked pids — and the
            // first `refresh_processes` call passes `remove_dead_processes: true`, so the
            // host-wide table was built and then immediately discarded.
            //
            // Host-level metrics are a separate change (`host-metrics`) with its own
            // collection cadence; when that lands it should own its own handle rather than
            // widening this one back out.
            system: System::new_with_specifics(
                RefreshKind::nothing().with_processes(
                    ProcessRefreshKind::nothing()
                        .with_memory()
                        .with_cpu()
                        .with_disk_usage()
                        .with_exe(UpdateKind::OnlyIfNotSet)
                        .with_cmd(UpdateKind::OnlyIfNotSet)
                        .with_cwd(UpdateKind::OnlyIfNotSet),
                ),
            ),
            metrics_sampled_at: None,
            metric_history: oxmgr_analytics::metrics_history::MetricHistoryStore::new(),
            event_history: Mutex::new(oxmgr_store::event_retention::EventRetention::default()),
            container_memory_ceiling,
            tracked_pid_scratch: Vec::new(),
        })
    }

    /// Returns a cloned sender for the event broadcast channel.
    ///
    /// Use this to subscribe (`sender.subscribe()`) or to emit daemon-level
    /// events from outside the manager.
    pub fn event_tx(&self) -> broadcast::Sender<Arc<BusEvent>> {
        self.event_tx.clone()
    }

    /// Publishes a bus event and records it in retention.
    ///
    /// Retention is fed here, at the one publication point every lifecycle event already travels
    /// through, rather than from a second call beside each `emit`. A parallel path is how the
    /// retained history and the live stream drift apart: an event added later would reach
    /// subscribers and be missing from history, and nothing would fail. `record_bus_event` decides
    /// for itself what is lifecycle, so log lines and daemon-level events cost one match here and
    /// are not retained.
    fn emit(&self, event: BusEvent) {
        if let Ok(mut history) = self.event_history.lock() {
            history.record_bus_event(&event);
        }
        // The discard is deliberate: a send error means the receiver is gone (shutdown); nothing left to deliver to
        #[expect(
            clippy::let_underscore_must_use,
            reason = "a send error means the receiver is gone (shutdown); nothing left to deliver to"
        )]
        let _ = self.event_tx.send(Arc::new(event));
    }

    /// Declared dependencies for one process, each marked resolved or unresolved.
    ///
    /// `None` when the process is not managed at all, which is distinct from a managed process that
    /// declares nothing (an empty vector).
    ///
    /// Read by tests today; the findings endpoints (task 10.3) are the production caller.
    #[cfg(test)]
    pub fn declared_dependencies(
        &self,
        name: &str,
    ) -> Option<Vec<oxmgr_analytics::failure_patterns::DeclaredDependency>> {
        let process = self.processes.get(name)?;
        let managed: std::collections::BTreeSet<String> = self.processes.keys().cloned().collect();
        Some(oxmgr_analytics::failure_patterns::resolve_dependencies(
            &process.depends_on,
            &managed,
        ))
    }

    /// Declared dependency edges across every managed process.
    ///
    /// Unresolved names are kept as edges: correlation walks them, finds no failures, and reports
    /// nothing — which is right. Dropping them would narrow the graph silently.
    ///
    /// Read by tests today; `correlate_dependencies` is the production caller once findings are
    /// evaluated on the maintenance tick.
    #[cfg(test)]
    pub fn dependency_graph(&self) -> oxmgr_analytics::failure_patterns::DependencyGraph {
        oxmgr_analytics::failure_patterns::dependency_graph(
            self.processes
                .iter()
                .map(|(name, process)| (name.as_str(), process.depends_on.as_slice())),
        )
    }

    /// Releases one process's retained events.
    ///
    /// Called from both delete paths for the same reason `metric_history.release` is: a delete ends
    /// the managed identity, so a later process reusing the name must not inherit a stranger's
    /// crash history. A restart keeps it — same workload.
    fn forget_event_history(&self, name: &str) {
        if let Ok(mut history) = self.event_history.lock() {
            history.forget(name);
        }
    }

    /// Retained lifecycle events for one process, oldest first.
    ///
    /// The closure runs under the retention lock because [`oxmgr_store::event_retention::EventQuery`]
    /// borrows the store; callers map it to owned values inside.
    ///
    /// Only tests read this today. The findings endpoints (task 10.3) are its production caller;
    /// the accessor exists now because retention without a way to read it is unverifiable.
    #[cfg(test)]
    pub fn with_event_history<T>(
        &self,
        f: impl FnOnce(&oxmgr_store::event_retention::EventRetention) -> T,
    ) -> Option<T> {
        self.event_history.lock().ok().map(|history| f(&history))
    }

    /// Reconciles persisted process state with the live machine and respawns
    /// processes whose desired state is running.
    pub async fn recover_processes(&mut self) -> Result<()> {
        let stale: Vec<ManagedProcess> = self
            .processes
            .values()
            .filter(|process| process.pid.is_some())
            .cloned()
            .collect();

        let mut set = tokio::task::JoinSet::new();
        for process in stale {
            let name = process.name.clone();
            let Some(pid) = process.pid else {
                continue;
            };
            if process_exists(pid) {
                if !self.pid_matches_managed_process(pid, &process).await {
                    warn!(
                        "skipping stale pid cleanup for process {} because pid {} no longer matches expected command",
                        name, pid
                    );
                    continue;
                }
                warn!("cleaning stale pid {pid} for process {name}");
                let timeout = Duration::from_secs(process.stop_timeout_secs.max(1));
                let signal = process.stop_signal.clone();
                set.spawn(async move {
                    if let Err(err) = terminate_pid(pid, signal.as_deref(), timeout).await {
                        error!("failed to terminate stale pid {pid} for process {name}: {err}");
                    }
                });
            }
        }
        while let Some(res) = set.join_next().await {
            if let Err(err) = res {
                error!("task panicked during process cleanup: {err}");
            }
        }

        let should_start: Vec<String> = self
            .processes
            .values()
            .filter(|process| process.desired_state == DesiredState::Running)
            .map(|process| process.name.clone())
            .collect();

        for process in self.processes.values_mut() {
            cleanup_process_cgroup(process);
            process.pid = None;
            process.status = ProcessStatus::Stopped;
            process.health_status = HealthStatus::Unknown;
            process.health_failures = 0;
            process.next_health_check = process
                .health_check
                .as_ref()
                .map(|check| now_epoch_secs().saturating_add(check.interval_secs.max(1)));

            // Initialize next cron restart if configured
            if let Some(cron_expr) = &process.cron_restart {
                match calculate_next_cron_restart(cron_expr, Some(now_epoch_secs())) {
                    Ok(next_restart) => {
                        process.next_cron_restart = Some(next_restart);
                    }
                    Err(err) => {
                        warn!(
                            "failed to calculate next cron restart for process {}: {}",
                            process.name, err
                        );
                    }
                }
            }
        }
        self.watch_fingerprints.clear();
        self.pending_watch_restarts.clear();
        self.scheduled_restarts.clear();
        self.save()?;

        for name in should_start {
            if let Err(err) = self.spawn_existing(&name).await {
                error!("failed to recover process {name}: {err}");
                if let Some(process) = self.processes.get_mut(&name) {
                    process.status = ProcessStatus::Errored;
                }
            }
        }

        self.save()
    }

    /// Runs the daemon's periodic maintenance tasks.
    pub async fn run_periodic_tasks(&mut self) -> Result<()> {
        self.run_scheduled_restarts().await?;
        self.run_cron_restarts().await?;
        self.run_due_watch_restarts().await?;
        self.refresh_resource_metrics();
        // Between the metrics refresh and the health checks, and that position is load-bearing at
        // both ends. After the refresh, because analysis reads the readings it just took — running
        // before would analyse the previous tick's values and report a lag it invented. Before the
        // health checks, because a health failure joins the findings active NOW (task 8.6), and a
        // finding raised later in the same tick would miss the event it belongs to.
        self.run_analysis();
        self.run_resource_limit_checks().await?;
        self.run_watch_checks().await?;
        self.run_health_checks().await
    }

    /// Advances the analysis engine by one cycle.
    ///
    /// Synchronous and infallible on purpose. It sits on the supervision path, so it must not be
    /// able to delay a restart by awaiting, and it must not be able to fail a tick: a detector
    /// producing nothing is an absence of findings, never a reason to abandon the maintenance
    /// cycle that also runs health checks and scheduled restarts.
    ///
    /// A deferral is logged rather than swallowed. `spec.md` requires deferral not to be silent,
    /// and at `debug` rather than `warn` because deferring under load is the designed behaviour,
    /// not a fault.
    fn run_analysis(&mut self) {
        let now = now_epoch_secs();
        let observations: Vec<oxmgr_analytics::analysis::ProcessObservation> = self
            .processes
            .values()
            .map(|process| {
                let running = process.status == ProcessStatus::Running && process.pid.is_some();
                oxmgr_analytics::analysis::ProcessObservation {
                    process: process.name.clone(),
                    config_fingerprint: process.config_fingerprint.clone(),
                    // `None` for a process that is not running, not zero. A stopped process has no
                    // CPU reading; reporting 0.0 would feed the baseline a value nobody measured
                    // and drag its centre toward zero every tick it stayed stopped.
                    cpu_percent: running.then(|| f64::from(process.cpu_percent)),
                    memory_bytes: running.then_some(u64_to_f64(process.memory_bytes)),
                    // The forecast target for leak detection. Taken from the configured limit
                    // because that is the level an operator cares about arriving at; with no limit
                    // configured a leak is still reported, just without a time-to-threshold.
                    memory_limit_bytes: {
                        let configured = process
                            .resource_limits
                            .as_ref()
                            .and_then(|limits| limits.max_memory_mb)
                            .map(|mb| u64_to_f64(mb * 1024 * 1024));
                        match (configured, self.container_memory_ceiling) {
                            (Some(a), Some(b)) => Some(a.min(u64_to_f64(b))),
                            (Some(a), None) => Some(a),
                            (None, Some(b)) => Some(u64_to_f64(b)),
                            (None, None) => None,
                        }
                    },
                    facts: oxmgr_core::rules::ProcessFacts {
                        desired_stopped: process.desired_state == DesiredState::Stopped,
                        at_crash_loop_limit: crash_loop_limit_reached_at(process, now),
                        // Declared dependencies implicated in a correlated failure, from the
                        // previous pattern pass. `analyse_patterns` replaces these each cycle,
                        // so a correlation that stops appearing stops withholding a tick later.
                        implicated_dependencies: self
                            .analysis
                            .implicated_dependencies_for(&process.name),
                    },
                }
            })
            .collect();

        let report = self.analysis.analyse(
            &observations,
            &mut self.baselines,
            &self.metric_history,
            now,
        );

        if report.deferred > 0 {
            debug!(
                "analysis deferred {} of {} processes after {:?} (budget reached); resuming next cycle",
                report.deferred,
                report.deferred + report.analysed,
                report.elapsed
            );
        }
        // Findings and decisions reach the bus through the same `emit` seam every lifecycle event
        // uses, so a subscriber's existing filter narrows them with no filter change, and retention
        // sees them by the same path.
        for event in &report.events {
            let Some(process) = self.processes.get(&event.process) else {
                continue;
            };
            let info = EventProcessInfo::from(process);
            let data = oxmgr_core::events::AnomalyData {
                id: event.id.clone(),
                key: event.key.clone(),
                detector: event.detector.clone(),
                metric: event.metric.clone(),
                confidence: event.confidence,
                occurrence: event.occurrence,
                summary: event.summary.clone(),
            };
            self.emit(if event.raised {
                BusEvent::anomaly_detected(info, data)
            } else {
                BusEvent::anomaly_cleared(info, data)
            });
        }

        // Failure-pattern analysis: crash loops, restart acceleration, repeated exits, storms
        // and dependency correlations. Runs off the retained lifecycle events rather than the
        // process table, so a process that is currently gone can still be named a pattern's
        // subject. Only events inside the widest detector window are fed, which keeps the
        // per-tick cost bounded by the same retention caps the rest of analysis respects.
        let lookback = oxmgr_analytics::failure_patterns::ACCELERATION_WINDOW_SECS;
        let pattern_config = oxmgr_analytics::failure_patterns::PatternConfig::default();
        let mut failure_events: Vec<oxmgr_analytics::failure_patterns::FailureEvent> = Vec::new();
        if let Ok(history) = self.event_history.lock() {
            for process in history.processes() {
                let namespace = self
                    .processes
                    .get(process)
                    .and_then(|p| p.namespace.clone())
                    .unwrap_or_default();
                for retained in history
                    .query(process, now.saturating_sub(lookback)..)
                    .events
                {
                    if !retained.kind.is_exit() {
                        continue;
                    }
                    let status = match retained.exit_status() {
                        Some(oxmgr_store::event_retention::ExitStatus::Code(code)) => {
                            oxmgr_analytics::failure_patterns::ExitStatus::Code(code)
                        }
                        Some(oxmgr_store::event_retention::ExitStatus::Signal(signal)) => {
                            oxmgr_analytics::failure_patterns::ExitStatus::Signal(
                                signal.to_string(),
                            )
                        }
                        None => oxmgr_analytics::failure_patterns::ExitStatus::Unknown,
                    };
                    failure_events.push(oxmgr_analytics::failure_patterns::FailureEvent {
                        at_secs: retained.at,
                        process: process.to_string(),
                        namespace: namespace.clone(),
                        kind: oxmgr_analytics::failure_patterns::FailureEventKind::Exited {
                            status,
                        },
                    });
                }
            }
        }

        let pattern_events =
            self.analysis
                .analyse_patterns(&failure_events, now, lookback, &pattern_config);

        // Pattern findings join the bus through the same seam as resource findings, with one
        // difference: a storm is a group finding with no owning process, so its keyed scope
        // (`namespace:<name>`, which no real process name can collide with) gets a synthetic
        // info instead of being dropped by the process lookup.
        for event in pattern_events {
            let info = match self.processes.get(&event.process) {
                Some(process) => EventProcessInfo::from(process),
                None => EventProcessInfo {
                    id: 0,
                    name: event.process.clone(),
                    namespace: event.process.strip_prefix("namespace:").map(str::to_string),
                    pid: None,
                    command: String::new(),
                    cwd: None,
                },
            };
            let data = oxmgr_core::events::AnomalyData {
                id: event.id.clone(),
                key: event.key.clone(),
                detector: event.detector.clone(),
                metric: event.metric.clone(),
                confidence: event.confidence,
                occurrence: event.occurrence,
                summary: event.summary.clone(),
            };
            self.emit(if event.raised {
                BusEvent::anomaly_detected(info, data)
            } else {
                BusEvent::anomaly_cleared(info, data)
            });
        }

        for decision in &report.decisions {
            debug!("analysis decision: {}", decision.summary());
            let Some(process) = self.processes.get(&decision.process) else {
                continue;
            };
            let Some(rule) = decision.rule else {
                continue;
            };
            self.emit(BusEvent::remediation_decided(
                EventProcessInfo::from(process),
                oxmgr_core::events::RemediationData {
                    rule: rule.to_string(),
                    action: decision.action.map(|action| action.to_string()),
                    withheld: decision.withheld.as_ref().map(|w| w.reason()),
                    // Always false today: nothing calls the protection gate from here, so observe-only
                    // is structural rather than a flag this path has to respect.
                    acted: false,
                    findings: decision
                        .findings
                        .iter()
                        .map(|key| key.as_string())
                        .collect(),
                },
            ));
        }
    }

    /// Registers a new process, persists it, and starts it immediately.
    pub async fn start_process(&mut self, spec: StartProcessSpec) -> Result<ManagedProcess> {
        let StartProcessSpec {
            command: command_line,
            name,
            pre_reload_cmd,
            restart_policy,
            max_restarts,
            crash_restart_limit,
            cwd,
            env,
            health_check,
            stop_signal,
            stop_timeout_secs,
            restart_delay_secs,
            start_delay_secs,
            watch,
            watch_paths,
            ignore_watch,
            watch_delay_secs,
            cluster_mode,
            cluster_instances,
            namespace,
            resource_limits,
            git_repo,
            git_ref,
            pull_secret_hash,
            reuse_port,
            wait_ready,
            ready_timeout_secs,
            log_date_format,
            unified_logs,
            cron_restart,
            stdout_log_override,
            stderr_log_override,
            depends_on,
        } = spec;

        let (command, args) = parse_command_line(&command_line)?;

        let resolved_name = match name {
            Some(given) => {
                validate_process_name(&given)?;
                if self.processes.contains_key(&given) {
                    return Err(OxmgrError::DuplicateProcessName(given).into());
                }
                given
            }
            None => self.generate_auto_name(&command),
        };

        let logs = {
            let mut base =
                process_logs_for_mode(&self.config.log_dir, &resolved_name, unified_logs);
            if let Some(path) = stdout_log_override {
                base.stdout = path;
            }
            if let Some(path) = stderr_log_override {
                base.stderr = path;
            }
            base
        };
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);

        let mut process = ManagedProcess {
            id,
            name: resolved_name.clone(),
            command,
            args,
            pre_reload_cmd,
            cwd,
            env,
            restart_policy,
            max_restarts,
            restart_count: 0,
            crash_restart_limit,
            auto_restart_history: Vec::new(),
            namespace,
            git_repo,
            git_ref,
            pull_secret_hash,
            reuse_port,
            stop_signal,
            stop_timeout_secs: stop_timeout_secs.max(1),
            restart_delay_secs,
            restart_backoff_cap_secs: 300,
            restart_backoff_reset_secs: 60,
            restart_backoff_attempt: 0,
            start_delay_secs,
            watch,
            watch_paths,
            ignore_watch,
            watch_delay_secs,
            cluster_mode,
            cluster_instances: normalize_cluster_instances(cluster_instances),
            resource_limits,
            cgroup_path: None,
            pid: None,
            status: ProcessStatus::Stopped,
            desired_state: DesiredState::Running,
            last_exit_code: None,
            stdout_log: logs.stdout,
            stderr_log: logs.stderr,
            health_check,
            health_status: HealthStatus::Unknown,
            health_failures: 0,
            last_health_check: None,
            next_health_check: None,
            last_health_error: None,
            wait_ready,
            ready_timeout_secs: ready_timeout_secs.max(1),
            cpu_percent: 0.0,
            memory_bytes: 0,
            disk_read_bytes: 0,
            disk_write_bytes: 0,
            metrics_interval_ms: None,
            metrics_pid: None,
            disk_read_total: 0,
            disk_write_total: 0,
            last_metrics_at: None,
            last_started_at: Some(now_epoch_secs()),
            last_stopped_at: None,
            config_fingerprint: String::new(),
            log_date_format,
            unified_logs,
            cron_restart,
            next_cron_restart: None,
            last_error: None,
            depends_on,
        };
        process.refresh_config_fingerprint();

        if process.start_delay_secs > 0 {
            sleep(Duration::from_secs(process.start_delay_secs)).await;
        }

        self.emit(BusEvent::process_started(EventProcessInfo::from(&process)));
        let pid = self.spawn_child_with_readiness(&mut process).await?;
        process.pid = Some(pid);
        process.status = ProcessStatus::Running;
        process.last_error = None; // Clear last error on successful start
        process.next_health_check = process
            .health_check
            .as_ref()
            .map(|check| now_epoch_secs().saturating_add(check.interval_secs.max(1)));

        // Initialize next cron restart time if configured
        if let Some(cron_expr) = &process.cron_restart {
            match calculate_next_cron_restart(cron_expr, Some(now_epoch_secs())) {
                Ok(next_restart) => {
                    process.next_cron_restart = Some(next_restart);
                }
                Err(err) => {
                    warn!(
                        "failed to calculate next cron restart for process {}: {}",
                        process.name, err
                    );
                }
            }
        }

        info!(
            "started process {} with pid {}",
            process.target_label(),
            pid
        );

        self.emit(BusEvent::process_online(EventProcessInfo::from(&process)));
        self.processes.insert(process.name.clone(), process.clone());
        self.update_watch_fingerprint(&process);
        self.save()?;
        Ok(process)
    }

    /// Stops a managed process and marks its desired state as stopped.
    pub async fn stop_process(&mut self, target: &str) -> Result<ManagedProcess> {
        let name = self.resolve_target(target)?;
        let mut process = self
            .processes
            .get(&name)
            .cloned()
            .ok_or_else(|| OxmgrError::ProcessNotFound(target.to_string()))?;

        process.desired_state = DesiredState::Stopped;
        if let Some(pid) = process.pid {
            let timeout = Duration::from_secs(process.stop_timeout_secs.max(1));
            terminate_pid(pid, process.stop_signal.as_deref(), timeout).await?;
        }

        process.pid = None;
        cleanup_process_cgroup(&mut process);
        process.status = ProcessStatus::Stopped;
        process.restart_backoff_attempt = 0;
        process.health_status = HealthStatus::Unknown;
        process.health_failures = 0;
        process.next_health_check = None;
        process.clear_resource_metrics();
        reset_auto_restart_state(&mut process);

        self.watch_fingerprints.remove(&name);
        self.pending_watch_restarts.remove(&name);
        self.scheduled_restarts.remove(&name);
        self.emit(BusEvent::process_stopped(EventProcessInfo::from(&process)));
        self.processes.insert(name, process.clone());
        self.save()?;
        Ok(process)
    }

    /// Stops every managed process and returns the list of processes that were
    /// affected. Processes that are already stopped are marked as desired-stopped
    /// but otherwise left unchanged. State is persisted once after all stops.
    pub async fn stop_all_processes(&mut self) -> Result<Vec<ManagedProcess>> {
        let names: Vec<String> = self.processes.keys().cloned().collect();
        let mut stopped = Vec::with_capacity(names.len());

        for name in &names {
            let Some(mut process) = self.processes.get(name).cloned() else {
                continue;
            };
            process.desired_state = DesiredState::Stopped;
            if let Some(pid) = process.pid {
                let timeout = Duration::from_secs(process.stop_timeout_secs.max(1));
                // The discard is deliberate: best-effort termination during cleanup; delivery failures are logged inside terminate_pid
                #[expect(
                    clippy::let_underscore_must_use,
                    reason = "best-effort termination during cleanup; delivery failures are logged inside terminate_pid"
                )]
                let _ = terminate_pid(pid, process.stop_signal.as_deref(), timeout).await;
            }
            process.pid = None;
            cleanup_process_cgroup(&mut process);
            process.status = ProcessStatus::Stopped;
            process.restart_backoff_attempt = 0;
            process.health_status = HealthStatus::Unknown;
            process.health_failures = 0;
            process.next_health_check = None;
            process.clear_resource_metrics();
            reset_auto_restart_state(&mut process);

            self.watch_fingerprints.remove(name);
            self.pending_watch_restarts.remove(name);
            self.scheduled_restarts.remove(name);
            self.processes.insert(name.clone(), process.clone());
            stopped.push(process);
        }

        self.save()?;
        Ok(stopped)
    }

    /// Restarts a managed process, resetting restart backoff state before the
    /// fresh spawn.
    pub async fn restart_process(&mut self, target: &str) -> Result<ManagedProcess> {
        self.restart_process_internal(target, true).await
    }

    /// Restarts every managed process and returns the refreshed process list.
    pub async fn restart_all_processes(&mut self) -> Result<Vec<ManagedProcess>> {
        let names: Vec<String> = self.processes.keys().cloned().collect();
        let mut restarted = Vec::with_capacity(names.len());

        for name in names {
            restarted.push(self.restart_process_internal(&name, true).await?);
        }

        Ok(restarted)
    }

    async fn restart_process_internal(
        &mut self,
        target: &str,
        reset_restart_count: bool,
    ) -> Result<ManagedProcess> {
        let name = self.resolve_target(target)?;

        let existing = self
            .processes
            .get(&name)
            .cloned()
            .ok_or_else(|| OxmgrError::ProcessNotFound(target.to_string()))?;

        if let Some(pid) = existing.pid {
            let timeout = Duration::from_secs(existing.stop_timeout_secs.max(1));
            terminate_pid(pid, existing.stop_signal.as_deref(), timeout).await?;
        }

        {
            let process = self
                .processes
                .get_mut(&name)
                .ok_or_else(|| OxmgrError::ProcessNotFound(target.to_string()))?;
            if reset_restart_count {
                process.restart_count = 0;
                reset_auto_restart_state(process);
            }
            process.restart_backoff_attempt = 0;
            process.last_exit_code = None;
            process.desired_state = DesiredState::Running;
            process.status = ProcessStatus::Restarting;
            process.pid = None;
            self.watch_fingerprints.remove(&name);
            self.pending_watch_restarts.remove(&name);
            cleanup_process_cgroup(process);
            process.health_status = HealthStatus::Unknown;
            process.health_failures = 0;
            process.next_health_check = process
                .health_check
                .as_ref()
                .map(|check| now_epoch_secs().saturating_add(check.interval_secs.max(1)));
        }

        self.scheduled_restarts.remove(&name);
        self.pending_watch_restarts.remove(&name);
        match self.spawn_existing(&name).await {
            Ok(process) => Ok(process),
            Err(err) => {
                if let Some(process) = self.processes.get_mut(&name) {
                    process.status = ProcessStatus::Errored;
                    process.desired_state = DesiredState::Stopped;
                    process.last_health_error = Some(format!("restart failed: {err}"));
                }
                self.save()?;
                Err(err)
            }
        }
    }

    /// Reloads a managed process, preferring replacement semantics over a full
    /// downtime window when the process is already running.
    pub async fn reload_process(&mut self, target: &str) -> Result<ManagedProcess> {
        let name = self.resolve_target(target)?;

        let existing = self
            .processes
            .get(&name)
            .cloned()
            .ok_or_else(|| OxmgrError::ProcessNotFound(target.to_string()))?;

        self.run_pre_reload_cmd(&existing).await?;

        if existing.pid.is_none() {
            return self.restart_process(target).await;
        }

        let old_pid = existing.pid.context("missing old pid for reload")?;
        let old_cgroup = existing.cgroup_path.clone();

        let mut replacement = existing.clone();
        let new_pid = self.spawn_child_with_readiness(&mut replacement).await?;
        replacement.pid = Some(new_pid);
        replacement.status = ProcessStatus::Running;
        replacement.desired_state = DesiredState::Running;
        replacement.last_exit_code = None;
        replacement.health_status = HealthStatus::Unknown;
        replacement.health_failures = 0;
        reset_auto_restart_state(&mut replacement);
        replacement.next_health_check = replacement
            .health_check
            .as_ref()
            .map(|check| now_epoch_secs().saturating_add(check.interval_secs.max(1)));

        self.scheduled_restarts.remove(&name);
        self.pending_watch_restarts.remove(&name);
        self.processes.insert(name.clone(), replacement.clone());
        self.update_watch_fingerprint(&replacement);
        self.save()?;

        let timeout = Duration::from_secs(existing.stop_timeout_secs.max(1));
        if let Err(err) = terminate_pid(old_pid, existing.stop_signal.as_deref(), timeout).await {
            warn!(
                "reload for process {} started new pid {} but failed to stop old pid {}: {}",
                name, new_pid, old_pid, err
            );
        }
        if let Some(path) = old_cgroup.as_deref()
            && let Err(err) = cgroup::cleanup(path)
        {
            warn!("failed to cleanup cgroup for process {}: {}", name, err);
        }

        Ok(replacement)
    }

    async fn run_pre_reload_cmd(&self, process: &ManagedProcess) -> Result<()> {
        let Some(command_line) = process.pre_reload_cmd.as_ref() else {
            return Ok(());
        };
        let trimmed = command_line.trim();
        if trimmed.is_empty() {
            anyhow::bail!(
                "pre_reload_cmd cannot be empty for process {}",
                process.name
            );
        }

        info!("running pre_reload_cmd for process {}", process.name);
        let mut command = if cfg!(windows) {
            let mut cmd = Command::new("cmd");
            cmd.arg("/C").arg(trimmed);
            cmd
        } else {
            let mut cmd = Command::new("sh");
            cmd.arg("-lc").arg(trimmed);
            cmd
        };

        if let Some(cwd) = &process.cwd {
            command.current_dir(cwd);
        }
        if !process.env.is_empty() {
            command.envs(&process.env);
        }
        command.env("OXMGR_PROCESS", &process.name);

        let output = command
            .output()
            .await
            .with_context(|| format!("pre_reload_cmd failed to start for {}", process.name))?;
        if output.status.success() {
            return Ok(());
        }

        let code = output
            .status
            .code()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "signal".to_string());
        let mut detail = String::new();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stdout.trim().is_empty() {
            detail.push_str("stdout: ");
            detail.push_str(stdout.trim());
        }
        if !stderr.trim().is_empty() {
            if !detail.is_empty() {
                detail.push_str(" | ");
            }
            detail.push_str("stderr: ");
            detail.push_str(stderr.trim());
        }
        if detail.len() > 2000 {
            detail.truncate(2000);
            detail.push_str("...");
        }

        if detail.is_empty() {
            anyhow::bail!(
                "pre_reload_cmd failed for {} (exit code {})",
                process.name,
                code
            );
        }
        anyhow::bail!(
            "pre_reload_cmd failed for {} (exit code {}): {}",
            process.name,
            code,
            detail
        );
    }

    /// Pulls Git updates for one or more managed processes and applies the
    /// corresponding reload or restart only when the checked-out revision changed.
    pub async fn pull_processes(&mut self, target: Option<&str>) -> Result<String> {
        let mut targets = if let Some(target) = target {
            vec![self.resolve_target(target)?]
        } else {
            let mut names: Vec<String> = self
                .processes
                .values()
                .filter(|process| process.git_repo.is_some())
                .map(|process| process.name.clone())
                .collect();
            names.sort();
            names
        };

        if targets.is_empty() {
            anyhow::bail!("no services configured with git_repo");
        }

        targets.sort();
        targets.dedup();

        let mut changed_count = 0_usize;
        let mut unchanged_count = 0_usize;
        let mut restarted_count = 0_usize;
        let mut failures = Vec::new();
        let mut details = Vec::new();

        for name in targets {
            match self.pull_single_process(&name).await {
                Ok(outcome) => {
                    if outcome.changed {
                        changed_count = changed_count.saturating_add(1);
                    } else {
                        unchanged_count = unchanged_count.saturating_add(1);
                    }
                    if outcome.restarted_or_reloaded {
                        restarted_count = restarted_count.saturating_add(1);
                    }
                    details.push(outcome.message);
                }
                Err(err) => {
                    failures.push(format!("{name}: {err}"));
                }
            }
        }

        if !failures.is_empty() {
            let mut lines = vec!["pull completed with failures:".to_string()];
            for failure in failures {
                lines.push(format!("- {failure}"));
            }
            anyhow::bail!(lines.join("\n"));
        }

        let mut summary = format!(
            "Pull complete: {} updated, {} unchanged, {} reloaded/restarted",
            changed_count, unchanged_count, restarted_count
        );
        if !details.is_empty() {
            summary.push('\n');
            summary.push_str(&details.join("\n"));
        }

        Ok(summary)
    }

    /// Verifies that a webhook secret matches the stored digest for the target
    /// process.
    pub fn verify_pull_webhook_secret(&self, target: &str, provided_secret: &str) -> Result<()> {
        let name = self.resolve_target(target)?;
        let process = self
            .processes
            .get(&name)
            .ok_or_else(|| OxmgrError::ProcessNotFound(target.to_string()))?;

        let expected_hash = process
            .pull_secret_hash
            .as_deref()
            .context("pull webhook secret is not configured for this service")?;
        let provided_hash = sha256_hex(provided_secret.trim());

        if !constant_time_eq(expected_hash.as_bytes(), provided_hash.as_bytes()) {
            anyhow::bail!("invalid pull webhook secret");
        }
        Ok(())
    }

    /// Deletes a managed process and removes its persisted metadata.
    pub async fn delete_process(&mut self, target: &str) -> Result<ManagedProcess> {
        let name = self.resolve_target(target)?;

        if let Some(process) = self.processes.get(&name).cloned() {
            if let Some(pid) = process.pid {
                let timeout = Duration::from_secs(process.stop_timeout_secs.max(1));
                // The discard is deliberate: best-effort termination during cleanup; delivery failures are logged inside terminate_pid
                #[expect(
                    clippy::let_underscore_must_use,
                    reason = "best-effort termination during cleanup; delivery failures are logged inside terminate_pid"
                )]
                let _ = terminate_pid(pid, process.stop_signal.as_deref(), timeout).await;
            }
            if let Some(path) = process.cgroup_path.as_deref()
                && let Err(err) = cgroup::cleanup(path)
            {
                warn!("failed to cleanup cgroup for process {}: {}", name, err);
            }
        }

        let removed = self
            .processes
            .remove(&name)
            .ok_or_else(|| OxmgrError::ProcessNotFound(target.to_string()))?;
        self.watch_fingerprints.remove(&name);
        self.pending_watch_restarts.remove(&name);
        self.scheduled_restarts.remove(&name);
        // Same rule as `delete_all_processes`: history belongs to the managed process, and a delete
        // ends that identity. There are two removal paths and only wiring one of them left history
        // surviving a single-process delete — caught by a test rather than by review.
        self.metric_history.release(&name);
        self.forget_event_history(&name);
        // Baselines go with the process for the same reason its history does, and the persisted
        // copy is rewritten immediately: leaving it on disk would let a later process reusing the
        // name inherit a stranger's notion of normal after the next restart.
        self.baselines.remove(&name);
        self.save_baselines_now();
        // Dismissals go with the process too: a later process reusing the name must not inherit a
        // suppression somebody granted to a different workload.
        if self.dismissals.processes.remove(&name).is_some() {
            self.save_dismissals_now();
        }
        self.save()?;
        Ok(removed)
    }

    /// Terminates and removes every managed process. Returns the list of
    /// deleted processes. State is persisted once after all deletions.
    pub async fn delete_all_processes(&mut self) -> Result<Vec<ManagedProcess>> {
        let names: Vec<String> = self.processes.keys().cloned().collect();
        let mut deleted = Vec::with_capacity(names.len());

        for name in &names {
            let Some(process) = self.processes.get(name).cloned() else {
                continue;
            };
            if let Some(pid) = process.pid {
                let timeout = Duration::from_secs(process.stop_timeout_secs.max(1));
                // The discard is deliberate: best-effort termination during cleanup; delivery failures are logged inside terminate_pid
                #[expect(
                    clippy::let_underscore_must_use,
                    reason = "best-effort termination during cleanup; delivery failures are logged inside terminate_pid"
                )]
                let _ = terminate_pid(pid, process.stop_signal.as_deref(), timeout).await;
            }
            if let Some(path) = process.cgroup_path.as_deref()
                && let Err(err) = cgroup::cleanup(path)
            {
                warn!("failed to cleanup cgroup for process {}: {}", name, err);
            }
            if let Some(removed) = self.processes.remove(name) {
                self.watch_fingerprints.remove(name);
                self.pending_watch_restarts.remove(name);
                self.scheduled_restarts.remove(name);
                // Retained history goes with the process. A restart keeps it — the managed process
                // is the same workload, as `process-io-metrics` established for its lifetime
                // counters — but a delete ends that identity, and keeping the samples would let a
                // later process reusing the name inherit a stranger's history.
                self.metric_history.release(name);
                self.forget_event_history(name);
                self.baselines.remove(name);
                self.dismissals.processes.remove(name);
                deleted.push(removed);
            }
        }

        // One rewrite after the loop rather than one per process: the file is written whole, so
        // N deletions would otherwise cost N full rewrites of a shrinking file.
        self.save_baselines_now();
        self.save_dismissals_now();
        self.save()?;
        Ok(deleted)
    }

    async fn pull_single_process(&mut self, name: &str) -> Result<PullOutcome> {
        let snapshot = self
            .processes
            .get(name)
            .cloned()
            .ok_or_else(|| OxmgrError::ProcessNotFound(name.to_string()))?;

        let repo = snapshot
            .git_repo
            .clone()
            .context("git_repo is not configured for this service")?;
        let cwd = snapshot
            .cwd
            .clone()
            .context("pull requires cwd to be set for the service")?;
        let git_ref = snapshot.git_ref.clone();

        ensure_repo_checkout(&cwd, &repo, git_ref.as_deref()).await?;
        ensure_origin_remote(&cwd, &repo).await?;

        let before = git_rev_parse_head(&cwd).await?;
        if let Some(git_ref) = git_ref.as_deref() {
            run_git(
                &cwd,
                &["pull", "--ff-only", "origin", git_ref],
                "pull repository from remote ref",
            )
            .await?;
        } else {
            run_git(&cwd, &["pull", "--ff-only"], "pull repository").await?;
        }
        let after = git_rev_parse_head(&cwd).await?;
        let changed = before != after;

        let mut action = "up-to-date".to_string();
        let mut restarted_or_reloaded = false;

        if changed {
            if snapshot.status == ProcessStatus::Running && snapshot.pid.is_some() {
                self.reload_process(name).await?;
                action = "reloaded".to_string();
                restarted_or_reloaded = true;
            } else if snapshot.desired_state == DesiredState::Running {
                self.restart_process(name).await?;
                action = "restarted".to_string();
                restarted_or_reloaded = true;
            } else {
                action = "updated (service stopped)".to_string();
            }
        }

        Ok(PullOutcome {
            changed,
            restarted_or_reloaded,
            message: format!(
                "{}: {} ({} -> {})",
                name,
                action,
                short_commit(&before),
                short_commit(&after)
            ),
        })
    }

    /// Returns an ordered snapshot of all managed processes.
    pub fn list_processes(&self) -> Vec<ManagedProcess> {
        let mut list: Vec<ManagedProcess> = self.processes.values().cloned().collect();
        list.sort_by_key(|process| process.id);
        list
    }

    /// Resolves a name or numeric id to a managed process name.
    ///
    /// Public wrapper over the private `resolve_target`, so the IPC layer can refuse an unknown
    /// target before building a report for it — rather than returning an empty report, which reads
    /// as "this process is fine".
    pub fn resolve_target_name(&self, target: &str) -> Result<String> {
        self.resolve_target(target)
    }

    /// Dismisses one advisory rule for one process.
    ///
    /// Refuses an unrecognised rule id rather than storing it. A dismissal for a misspelled rule
    /// would look accepted and suppress nothing, and the operator would only discover that the next
    /// time the real advisory fired — the worst possible moment.
    ///
    /// Refuses an unknown process for the same reason: silently accepting a dismissal for a typo'd
    /// name reads as success while doing nothing.
    ///
    /// Idempotent. Reports whether the dismissal was newly added, so a caller can tell "done" from
    /// "already was".
    pub fn dismiss_advisory(&mut self, target: &str, rule_id: &str) -> Result<bool> {
        let name = self.resolve_target(target)?;
        if crate::advisories::AdvisoryRule::from_id(rule_id).is_none() {
            return Err(anyhow::anyhow!(
                "unknown advisory rule {rule_id:?}; expected one of: {}",
                crate::advisories::AdvisoryRule::ALL
                    .iter()
                    .map(|rule| rule.id())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let added = self
            .dismissals
            .processes
            .entry(name)
            .or_default()
            .insert(rule_id.to_string());
        if added {
            self.save_dismissals_now();
        }
        Ok(added)
    }

    /// Restores a dismissed advisory. Reports whether one was in effect.
    pub fn restore_advisory(&mut self, target: &str, rule_id: &str) -> Result<bool> {
        let name = self.resolve_target(target)?;
        let Some(rules) = self.dismissals.processes.get_mut(&name) else {
            return Ok(false);
        };
        let removed = rules.remove(rule_id);
        if rules.is_empty() {
            // Pruned so a process with no dismissals does not linger as an empty set and read as
            // "configured" in a report.
            self.dismissals.processes.remove(&name);
        }
        if removed {
            self.save_dismissals_now();
        }
        Ok(removed)
    }

    /// Typical (median) values per process per metric, over a fixed recent window.
    ///
    /// 15 minutes, matching the window the `severity` module's own tests use. Long enough that one
    /// startup spike cannot dominate a median, short enough that "typical" still describes now
    /// rather than an hour ago.
    ///
    /// A STOPPED process keeps its typical value while its current figures are reported unavailable
    /// (task 4b.7). That asymmetry is the point: the typical value describes what the process did
    /// when it was running, which is exactly the context an operator needs while looking at a
    /// process that has stopped — and it is a different claim from its current CPU, which genuinely
    /// is unmeasurable.
    pub fn typical_values(&self) -> std::collections::BTreeMap<String, TypicalReport> {
        const WINDOW_SECS: u64 = 900;
        let now_ms = now_epoch_secs().saturating_mul(1000);
        let from_ms = now_ms.saturating_sub(WINDOW_SECS * 1000);

        self.processes
            .keys()
            .filter_map(|name| {
                let history = self.metric_history.history(name)?;
                let typical_for = |kind: oxmgr_analytics::metrics_history::MetricKind| {
                    let series = history.query(kind, from_ms, now_ms).ok()?;
                    // A summary contributes its mean; a raw point its value. Both are legitimate
                    // inputs to a median over the window — the alternative, refusing to use the
                    // downsampled tier, would make the typical value vanish exactly when the window
                    // is long enough to be interesting.
                    let samples: Vec<f64> = series
                        .points
                        .iter()
                        .map(|point| match point {
                            oxmgr_analytics::metrics_history::SeriesPoint::Sample {
                                value, ..
                            } => *value,
                            oxmgr_analytics::metrics_history::SeriesPoint::Summary {
                                mean, ..
                            } => *mean,
                        })
                        .collect();
                    Some(oxmgr_core::severity::typical_from_samples(
                        &samples,
                        WINDOW_SECS,
                    ))
                };

                Some((
                    name.clone(),
                    TypicalReport {
                        cpu: typical_for(oxmgr_analytics::metrics_history::MetricKind::Cpu),
                        memory: typical_for(oxmgr_analytics::metrics_history::MetricKind::Memory),
                        window_secs: WINDOW_SECS,
                    },
                ))
            })
            .collect()
    }

    /// Every dismissal, for the snapshot the HTTP handlers read.
    pub fn dismissal_map(
        &self,
    ) -> std::collections::BTreeMap<String, std::collections::BTreeSet<String>> {
        self.dismissals.processes.clone()
    }

    /// Rule ids dismissed for one process.
    ///
    /// Read by tests; production reads the whole map through `dismissal_map` for the snapshot, since
    /// the endpoint needs every process in one pass rather than one lookup per row.
    #[cfg(test)]
    pub fn dismissed_advisories(&self, process: &str) -> Vec<String> {
        self.dismissals
            .processes
            .get(process)
            .map(|rules| rules.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Persists dismissals, logging and swallowing a failure.
    ///
    /// A failed write loses a suppression, which means an advisory reappears — annoying, not
    /// dangerous. Failing the operator's dismiss request because the disk was busy would be worse.
    fn save_dismissals_now(&self) {
        if let Err(err) = save_dismissals(
            &dismissal_store_path(&self.config.state_path),
            &self.dismissals,
        ) {
            warn!("failed to persist advisory dismissals: {err}");
        }
    }

    /// Findings and decisions flattened for the CLI.
    ///
    /// `target: None` means every process. A flattened view rather than the `Finding` type: the CLI
    /// draws a table, and shipping the whole evidence tree to render six columns would put the
    /// daemon's internal representation in the client's parsing path. `/api/findings` serves the
    /// complete finding for anything that needs it.
    pub fn findings_report(&self, target: Option<&str>) -> FindingsReport {
        let wanted = target.and_then(|target| self.resolve_target(target).ok());
        let snapshot = self.analysis_snapshot();
        let matches = |process: &str| wanted.as_deref().is_none_or(|name| name == process);

        FindingsReport {
            findings: snapshot
                .findings
                .iter()
                .filter(|finding| matches(&finding.key.process))
                .map(|finding| FindingRow {
                    process: finding.key.process.clone(),
                    detector: finding.key.detector.to_string(),
                    metric: finding.key.metric.to_string(),
                    status: if finding.is_active() {
                        "active"
                    } else {
                        "cleared"
                    }
                    .to_string(),
                    confidence: finding.confidence.score,
                    occurrence: finding.occurrence,
                    raised_at: finding.raised_at_unix,
                    last_seen: finding.last_seen_unix,
                    summary: finding.summary.clone(),
                    guidance: oxmgr_core::findings::guidance_for(finding),
                })
                .collect(),
            decisions: snapshot
                .decisions
                .iter()
                .filter(|decision| matches(&decision.process))
                .filter_map(|decision| {
                    // A decision with no rule matched nothing, and showing "no rule matched" rows
                    // would bury the ones that did.
                    let rule = decision.rule?;
                    Some(DecisionRow {
                        process: decision.process.clone(),
                        at: decision.at_unix,
                        rule: rule.to_string(),
                        action: decision.action.map(|action| action.to_string()),
                        withheld: decision.withheld.as_ref().map(|w| w.reason()),
                    })
                })
                .collect(),
            warming: snapshot
                .warming
                .iter()
                .filter(|name| matches(name))
                .cloned()
                .collect(),
            suppressed_global: snapshot.suppressed.blocked_globally,
            suppressed_detector: snapshot.suppressed.blocked_by_detector,
            suppressed_process: snapshot.suppressed.blocked_by_process,
        }
    }

    /// A read-only copy of analysis output, for the HTTP surface and the CLI.
    ///
    /// The warming list is computed here rather than inside the engine, because baseline readiness
    /// lives on the manager's own baselines. It is what makes "no findings" explainable: a process
    /// still warming has not been checked and found healthy, it has not been checked at all.
    pub fn analysis_snapshot(&self) -> oxmgr_analytics::analysis::AnalysisSnapshot {
        let warming: Vec<String> = self
            .processes
            .keys()
            .filter(|name| {
                self.baselines
                    .get(*name)
                    // Absent baselines count as warming: a process the engine has not yet seen is
                    // in exactly that state, and reporting it as ready would be a false claim.
                    .map(|set| set.snapshots().iter().any(|s| !s.readiness.is_ready()))
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        self.analysis.snapshot(warming)
    }

    /// Returns one managed process identified by name or numeric id.
    pub fn get_process(&self, target: &str) -> Result<ManagedProcess> {
        let name = self.resolve_target(target)?;
        self.processes
            .get(&name)
            .cloned()
            .ok_or_else(|| OxmgrError::ProcessNotFound(target.to_string()).into())
    }

    /// Returns the stdout and stderr log paths for one managed process.
    pub fn logs_for(&self, target: &str) -> Result<ProcessLogs> {
        let process = self.get_process(target)?;
        Ok(ProcessLogs {
            stdout: process.stdout_log,
            stderr: process.stderr_log,
        })
    }

    /// Updates internal state after a child process exits and schedules an
    /// automatic restart when policy allows.
    pub async fn handle_exit_event(&mut self, event: ProcessExitEvent) -> Result<()> {
        let Some(mut process) = self.processes.get(&event.name).cloned() else {
            return Ok(());
        };

        if !exit_event_matches_process(&process, &event) {
            return Ok(());
        }

        let now = now_epoch_secs();
        let uptime = uptime_secs_since(process.last_started_at);
        self.clear_runtime_state(&mut process, &event, now);
        let stderr_tail = self.take_stderr_tail(&process.name);

        if process.desired_state == DesiredState::Stopped {
            return self.finish_requested_stop(process);
        }

        let exited_successfully = event.success && !event.wait_error;
        if can_auto_restart(&process, &event, exited_successfully) {
            return self
                .handle_auto_restart(process, &event, now, uptime, stderr_tail)
                .await;
        }

        self.finish_terminal_exit(process, &event, exited_successfully, uptime, stderr_tail)
    }

    /// Clears pid, watch, metric, and cgroup bookkeeping for a process that is
    /// no longer running.
    fn clear_runtime_state(
        &mut self,
        process: &mut ManagedProcess,
        event: &ProcessExitEvent,
        now: u64,
    ) {
        process.pid = None;
        self.watch_fingerprints.remove(&process.name);
        self.pending_watch_restarts.remove(&process.name);
        self.scheduled_restarts.remove(&process.name);
        cleanup_process_cgroup(process);
        process.clear_resource_metrics();
        process.last_exit_code = event.exit_code;
        process.last_stopped_at = Some(now);
    }

    /// Drains the buffered stderr tail used to annotate crash diagnostics.
    fn take_stderr_tail(&self, name: &str) -> Vec<String> {
        self.stderr_buffers
            .get(name)
            .map(drain_stderr_buf)
            .unwrap_or_default()
    }

    /// Finalizes a process that exited because a stop was requested.
    fn finish_requested_stop(&mut self, mut process: ManagedProcess) -> Result<()> {
        process.status = ProcessStatus::Stopped;
        process.restart_backoff_attempt = 0;
        process.health_status = HealthStatus::Unknown;
        process.next_health_check = None;
        reset_auto_restart_state(&mut process);
        self.processes.insert(process.name.clone(), process);
        self.save()
    }

    /// Restarts a process after an unexpected exit, unless it is crash looping.
    async fn handle_auto_restart(
        &mut self,
        mut process: ManagedProcess,
        event: &ProcessExitEvent,
        now: u64,
        uptime: u64,
        stderr_tail: Vec<String>,
    ) -> Result<()> {
        if crash_loop_limit_reached(&mut process, now) {
            return self.finish_crash_loop(process, stderr_tail);
        }

        maybe_reset_backoff_attempt(&mut process);
        let restart_delay = compute_restart_delay_secs(&process);
        record_auto_restart(&mut process, now);
        mark_restarting(&mut process);

        let process_name = process.name.clone();
        self.emit(BusEvent::process_restarting(
            EventProcessInfo::from(&process),
            event.exit_code,
            event.signal.clone(),
            uptime,
            process.restart_count,
            restart_delay,
        ));
        self.processes.insert(process_name.clone(), process);

        if restart_delay == 0 {
            return self.respawn_now(&process_name).await;
        }

        self.scheduled_restarts.insert(
            process_name,
            TokioInstant::now() + Duration::from_secs(restart_delay),
        );
        self.save()
    }

    /// Parks a crash-looping process in the errored state so it requires a
    /// manual restart.
    fn finish_crash_loop(
        &mut self,
        mut process: ManagedProcess,
        stderr_tail: Vec<String>,
    ) -> Result<()> {
        process.status = ProcessStatus::Errored;
        process.desired_state = DesiredState::Stopped;
        process.restart_backoff_attempt = 0;
        clear_health_state(&mut process);

        let error_msg = format!(
            "crash loop detected after {} auto restarts in 5 minutes; manual restart required",
            process.crash_restart_limit
        );
        process.last_health_error = Some(error_msg.clone());
        process.last_error = Some(if stderr_tail.is_empty() {
            error_msg
        } else {
            format!("{}\n\nLast stderr:\n{}", error_msg, stderr_tail.join("\n"))
        });

        self.emit(BusEvent::process_errored(EventProcessInfo::from(&process)));
        self.processes.insert(process.name.clone(), process);
        self.save()
    }

    /// Respawns a process immediately, marking it errored when the spawn fails.
    async fn respawn_now(&mut self, name: &str) -> Result<()> {
        let Err(err) = self.spawn_existing(name).await else {
            return Ok(());
        };

        error!(
            "failed to restart process {} immediately after exit: {}",
            name, err
        );

        let errored_info = self.processes.get_mut(name).map(|process| {
            process.status = ProcessStatus::Errored;
            process.desired_state = DesiredState::Stopped;
            let error_msg = format!("restart failed: {err}");
            process.last_health_error = Some(error_msg.clone());
            process.last_error = Some(error_msg);
            EventProcessInfo::from(process as &ManagedProcess)
        });
        if let Some(info) = errored_info {
            self.emit(BusEvent::process_errored(info));
        }
        self.save()
    }

    /// Finalizes a process that exited and will not be restarted, emitting the
    /// matching lifecycle event.
    fn finish_terminal_exit(
        &mut self,
        mut process: ManagedProcess,
        event: &ProcessExitEvent,
        exited_successfully: bool,
        uptime: u64,
        stderr_tail: Vec<String>,
    ) -> Result<()> {
        process.status = terminal_exit_status(event, exited_successfully);
        process.restart_backoff_attempt = 0;
        process.health_status = HealthStatus::Unknown;
        process.next_health_check = None;
        reset_auto_restart_state(&mut process);

        let failed = matches!(
            process.status,
            ProcessStatus::Crashed | ProcessStatus::Errored
        );
        // Attach the stderr tail so crash diagnostics survive the exit.
        if failed && !stderr_tail.is_empty() {
            process.last_error = Some(stderr_tail.join("\n"));
        }

        self.emit_terminal_exit_event(&process, event, uptime, stderr_tail);
        self.processes.insert(process.name.clone(), process);
        self.save()
    }

    /// Emits the crashed, exited, or errored event for a terminal exit.
    fn emit_terminal_exit_event(
        &self,
        process: &ManagedProcess,
        event: &ProcessExitEvent,
        uptime: u64,
        stderr_tail: Vec<String>,
    ) {
        match process.status {
            ProcessStatus::Crashed => self.emit(BusEvent::process_crashed(
                EventProcessInfo::from(process),
                event.exit_code,
                event.signal.clone(),
                uptime,
                process.restart_count,
                stderr_tail,
            )),
            ProcessStatus::Stopped => self.emit(BusEvent::process_exited(
                EventProcessInfo::from(process),
                event.exit_code,
                event.signal.clone(),
                uptime,
                process.restart_count,
                vec![],
            )),
            ProcessStatus::Errored => {
                self.emit(BusEvent::process_errored(EventProcessInfo::from(process)))
            }
            _ => {}
        }
    }

    /// Stops every managed process as part of daemon shutdown.
    pub async fn shutdown_all(&mut self) -> Result<()> {
        let names: Vec<String> = self.processes.keys().cloned().collect();
        for name in names {
            if let Some(mut process) = self.processes.get(&name).cloned() {
                process.desired_state = DesiredState::Stopped;
                if let Some(pid) = process.pid {
                    let timeout = Duration::from_secs(process.stop_timeout_secs.max(1));
                    // The discard is deliberate: best-effort termination during cleanup; delivery failures are logged inside terminate_pid
                    #[expect(
                        clippy::let_underscore_must_use,
                        reason = "best-effort termination during cleanup; delivery failures are logged inside terminate_pid"
                    )]
                    let _ = terminate_pid(pid, process.stop_signal.as_deref(), timeout).await;
                }
                process.pid = None;
                cleanup_process_cgroup(&mut process);
                process.status = ProcessStatus::Stopped;
                process.restart_backoff_attempt = 0;
                process.health_status = HealthStatus::Unknown;
                process.next_health_check = None;
                process.clear_resource_metrics();
                reset_auto_restart_state(&mut process);
                self.watch_fingerprints.remove(&name);
                self.pending_watch_restarts.remove(&name);
                self.scheduled_restarts.remove(&name);
                self.processes.insert(name, process);
            }
        }
        self.save()
    }

    async fn spawn_existing(&mut self, name: &str) -> Result<ManagedProcess> {
        let mut process = self
            .processes
            .get(name)
            .cloned()
            .ok_or_else(|| OxmgrError::ProcessNotFound(name.to_string()))?;

        let pid = self.spawn_child_with_readiness(&mut process).await?;
        process.pid = Some(pid);
        process.status = ProcessStatus::Running;
        process.desired_state = DesiredState::Running;
        process.last_started_at = Some(now_epoch_secs());
        process.next_health_check = process
            .health_check
            .as_ref()
            .map(|check| now_epoch_secs().saturating_add(check.interval_secs.max(1)));

        self.scheduled_restarts.remove(name);
        self.pending_watch_restarts.remove(name);
        self.emit(BusEvent::process_online(EventProcessInfo::from(&process)));
        self.processes.insert(name.to_string(), process.clone());
        self.update_watch_fingerprint(&process);
        self.save()?;
        Ok(process)
    }

    async fn spawn_child_with_readiness(&mut self, process: &mut ManagedProcess) -> Result<u32> {
        let pid = self.spawn_child(process).await?;
        if !process.wait_ready {
            return Ok(pid);
        }

        let Some(check) = process.health_check.clone() else {
            let timeout = Duration::from_secs(process.stop_timeout_secs.max(1));
            // The discard is deliberate: best-effort termination during cleanup; delivery failures are logged inside terminate_pid
            #[expect(
                clippy::let_underscore_must_use,
                reason = "best-effort termination during cleanup; delivery failures are logged inside terminate_pid"
            )]
            let _ = terminate_pid(pid, process.stop_signal.as_deref(), timeout).await;
            anyhow::bail!(
                "wait_ready requires a health check for process {}",
                process.name
            );
        };

        let mut snapshot = process.clone();
        snapshot.pid = Some(pid);
        let deadline = StdInstant::now() + Duration::from_secs(process.ready_timeout_secs.max(1));
        let detail = loop {
            if !process_exists(pid) {
                anyhow::bail!("process {} exited before becoming ready", process.name);
            }

            match execute_health_check(&snapshot, &check).await {
                Ok(()) => return Ok(pid),
                Err(err) => {
                    if StdInstant::now() >= deadline {
                        break err.to_string();
                    }
                }
            }

            sleep(Duration::from_millis(250)).await;
        };

        let timeout = Duration::from_secs(process.stop_timeout_secs.max(1));
        // The discard is deliberate: best-effort termination during cleanup; delivery failures are logged inside terminate_pid
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort termination during cleanup; delivery failures are logged inside terminate_pid"
        )]
        let _ = terminate_pid(pid, process.stop_signal.as_deref(), timeout).await;
        anyhow::bail!(
            "process {} did not become ready within {}s: {}",
            process.name,
            process.ready_timeout_secs.max(1),
            detail
        );
    }

    async fn spawn_child(&mut self, process: &mut ManagedProcess) -> Result<u32> {
        let logs = ProcessLogs {
            stdout: process.stdout_log.clone(),
            stderr: process.stderr_log.clone(),
        };
        let spawn = resolve_spawn_program(process, &self.config.base_dir).await?;

        let mut command = Command::new(&spawn.program);
        #[cfg(unix)]
        {
            // SAFETY: The closure runs in the forked child process between `fork`
            // and `exec`, where only async-signal-safe operations are permitted:
            // the child has a single thread and may hold locks the parent's
            // other threads left locked. `nix::libc::setpgid` is a direct syscall
            // and safe there. `std::io::Error::last_os_error` is reachable only
            // on the failure path and reads `errno`; this is the standard pattern
            // the `pre_exec` documentation itself uses.
            unsafe {
                command.pre_exec(|| {
                    if nix::libc::setpgid(0, 0) == 0 {
                        Ok(())
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                });
            }
        }
        command.args(&spawn.args).stdin(Stdio::null());

        prepare_log_files(&logs, self.config.log_rotation)?;
        command.stdout(Stdio::piped()).stderr(Stdio::piped());

        if let Some(cwd) = &process.cwd {
            command.current_dir(cwd);
        }
        if !process.env.is_empty() {
            command.envs(&process.env);
        }
        if !spawn.extra_env.is_empty() {
            command.envs(&spawn.extra_env);
        }
        if process.reuse_port {
            #[cfg(unix)]
            {
                command.env("OXMGR_REUSEPORT", "1");
                command.env("SO_REUSEPORT", "1");
            }
            #[cfg(windows)]
            {
                warn!(
                    "process {} requested reuse_port but SO_REUSEPORT is not supported on Windows",
                    process.name
                );
            }
        }
        if process
            .resource_limits
            .as_ref()
            .map(|limits| limits.deny_gpu)
            .unwrap_or(false)
        {
            command.env("CUDA_VISIBLE_DEVICES", "");
            command.env("NVIDIA_VISIBLE_DEVICES", "none");
            command.env("HIP_VISIBLE_DEVICES", "");
            command.env("ROCR_VISIBLE_DEVICES", "");
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn {}", process.command))?;
        let pid = child.id().context("spawned child has no pid")?;

        let process_info = EventProcessInfo::from(process as &ManagedProcess);
        // Refresh pid in the info we pass to log pipes (process.pid set after spawn).
        let process_info = EventProcessInfo {
            pid: Some(pid),
            ..process_info
        };

        // Create (or reset) the per-process stderr ring buffer.
        let stderr_buf = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_CAPACITY)));
        self.stderr_buffers
            .insert(process.name.clone(), Arc::clone(&stderr_buf));

        if let Some(stdout) = child.stdout.take() {
            forward_log_pipe(
                stdout,
                LogForwardParams {
                    log_path: logs.stdout.clone(),
                    date_format: process.log_date_format.clone(),
                    rotation: self.config.log_rotation,
                    event_tx: self.event_tx.clone(),
                    process_info: process_info.clone(),
                    is_stderr: false,
                    stderr_buf: None,
                },
            );
        }
        if let Some(stderr) = child.stderr.take() {
            forward_log_pipe(
                stderr,
                LogForwardParams {
                    log_path: logs.stderr.clone(),
                    date_format: process.log_date_format.clone(),
                    rotation: self.config.log_rotation,
                    event_tx: self.event_tx.clone(),
                    process_info,
                    is_stderr: true,
                    stderr_buf: Some(stderr_buf),
                },
            );
        }
        process.cgroup_path = None;
        if let Some(limits) = process.resource_limits.as_ref() {
            match cgroup::apply_limits(&process.name, process.id, pid, limits) {
                Ok(path) => {
                    process.cgroup_path = path;
                }
                Err(err) => {
                    // The discard is deliberate: best-effort termination during cleanup; delivery failures are logged inside terminate_pid
                    #[expect(
                        clippy::let_underscore_must_use,
                        reason = "best-effort termination during cleanup; delivery failures are logged inside terminate_pid"
                    )]
                    let _ = terminate_pid(
                        pid,
                        process.stop_signal.as_deref(),
                        Duration::from_secs(process.stop_timeout_secs.max(1)),
                    )
                    .await;
                    anyhow::bail!(
                        "failed to apply resource controls for process {}: {}",
                        process.name,
                        err
                    );
                }
            }
        }

        let tx = self.exit_tx.clone();
        let name = process.name.clone();
        tokio::spawn(async move {
            let event = match child.wait().await {
                Ok(status) => ProcessExitEvent {
                    name,
                    pid,
                    exit_code: status.code(),
                    signal: exit_signal_name(&status),
                    success: status.success(),
                    wait_error: false,
                },
                Err(err) => {
                    error!("child wait failed: {err}");
                    ProcessExitEvent {
                        name,
                        pid,
                        exit_code: None,
                        signal: None,
                        success: false,
                        wait_error: true,
                    }
                }
            };

            // The discard is deliberate: a send error means the receiver is gone (shutdown); nothing left to deliver to
            #[expect(
                clippy::let_underscore_must_use,
                reason = "a send error means the receiver is gone (shutdown); nothing left to deliver to"
            )]
            let _ = tx.send(event);
        });

        Ok(pid)
    }

    fn update_watch_fingerprint(&mut self, process: &ManagedProcess) {
        if !process.watch || process.status != ProcessStatus::Running {
            self.watch_fingerprints.remove(&process.name);
            self.pending_watch_restarts.remove(&process.name);
            return;
        }

        match watch_fingerprint_for_process(process) {
            Ok(fingerprint) => {
                self.watch_fingerprints
                    .insert(process.name.clone(), fingerprint);
                self.pending_watch_restarts.remove(&process.name);
            }
            Err(err) => {
                warn!(
                    "failed to initialize watch fingerprint for process {}: {}",
                    process.name, err
                );
                self.watch_fingerprints.remove(&process.name);
                self.pending_watch_restarts.remove(&process.name);
            }
        }
    }

    /// Executes delayed restarts whose scheduled timestamp has already passed.
    pub async fn run_scheduled_restarts(&mut self) -> Result<()> {
        let now = TokioInstant::now();
        let mut due: Vec<(String, TokioInstant)> = self
            .scheduled_restarts
            .iter()
            .filter(|(_, due_at)| **due_at <= now)
            .map(|(name, due_at)| (name.clone(), *due_at))
            .collect();
        due.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));

        for (name, _) in due {
            self.scheduled_restarts.remove(&name);

            let Some(snapshot) = self.processes.get(&name).cloned() else {
                continue;
            };

            if snapshot.desired_state != DesiredState::Running
                || snapshot.status != ProcessStatus::Restarting
            {
                continue;
            }

            if let Err(err) = self.spawn_existing(&name).await {
                error!("failed to restart process {}: {err}", name);
                if let Some(process) = self.processes.get_mut(&name) {
                    process.status = ProcessStatus::Errored;
                    process.desired_state = DesiredState::Stopped;
                    process.last_health_error = Some(format!("restart failed: {err}"));
                }
                self.save()?;
            }
        }

        Ok(())
    }

    async fn run_cron_restarts(&mut self) -> Result<()> {
        let now_secs = now_epoch_secs();
        let mut due: Vec<String> = self
            .processes
            .iter()
            .filter(|(_, process)| {
                process.cron_restart.is_some()
                    && process
                        .next_cron_restart
                        .is_some_and(|next| next <= now_secs)
                    && process.status == ProcessStatus::Running
                    && process.desired_state == DesiredState::Running
            })
            .map(|(name, _)| name.clone())
            .collect();
        due.sort();

        for name in due {
            let Some(process) = self.processes.get(&name).cloned() else {
                continue;
            };

            info!("triggering cron-scheduled restart for process {}", name);

            match self.restart_process_internal(&name, false).await {
                Ok(_) => {
                    if let Some(p) = self.processes.get_mut(&name) {
                        p.restart_count = process.restart_count.saturating_add(1);
                        p.last_health_error = Some("cron-scheduled restart".to_string());

                        // Calculate next cron restart
                        if let Some(cron_expr) = p.cron_restart.as_ref() {
                            match calculate_next_cron_restart(cron_expr, Some(now_secs)) {
                                Ok(next_restart) => {
                                    p.next_cron_restart = Some(next_restart);
                                }
                                Err(err) => {
                                    warn!(
                                        "failed to calculate next cron restart for {}: {}",
                                        name, err
                                    );
                                }
                            }
                        }
                    }
                    self.save()?;
                }
                Err(err) => {
                    error!("cron restart failed for process {}: {}", name, err);
                    if let Some(process) = self.processes.get_mut(&name) {
                        process.status = ProcessStatus::Errored;
                        process.last_health_error = Some(format!("cron restart failed: {err}"));
                    }
                    self.save()?;
                }
            }
        }

        Ok(())
    }

    async fn run_due_watch_restarts(&mut self) -> Result<()> {
        let now = TokioInstant::now();
        let mut due: Vec<(String, PendingWatchRestart)> = self
            .pending_watch_restarts
            .iter()
            .filter(|(_, state)| state.due_at <= now)
            .map(|(name, state)| (name.clone(), *state))
            .collect();
        due.sort_by(|left, right| {
            left.1
                .due_at
                .cmp(&right.1.due_at)
                .then_with(|| left.0.cmp(&right.0))
        });

        for (name, pending) in due {
            self.pending_watch_restarts.remove(&name);

            let Some(snapshot) = self.processes.get(&name).cloned() else {
                continue;
            };
            if !snapshot.watch
                || snapshot.status != ProcessStatus::Running
                || snapshot.pid.is_none()
            {
                continue;
            }

            warn!(
                "watch delay elapsed for process {}; triggering restart",
                name
            );

            match self.restart_process_internal(&name, false).await {
                Ok(_) => {
                    if let Some(process) = self.processes.get_mut(&name) {
                        process.restart_count = snapshot.restart_count.saturating_add(1);
                        process.last_health_error = Some("watch-triggered restart".to_string());
                    }
                    self.watch_fingerprints
                        .insert(name.clone(), pending.fingerprint);
                    self.save()?;
                }
                Err(err) => {
                    error!("watch restart failed for process {}: {}", name, err);
                    if let Some(process) = self.processes.get_mut(&name) {
                        process.status = ProcessStatus::Errored;
                        process.last_health_error = Some(format!("watch restart failed: {err}"));
                    }
                    self.save()?;
                }
            }
        }

        Ok(())
    }

    pub fn next_scheduled_restart_at(&self) -> Option<TokioInstant> {
        let now_secs = now_epoch_secs();
        let cron_restarts = self
            .processes
            .values()
            .filter_map(|process| process.next_cron_restart)
            .filter(|&next_restart| next_restart >= now_secs)
            .map(|next_restart| {
                let secs_from_now = next_restart.saturating_sub(now_secs);
                TokioInstant::now() + Duration::from_secs(secs_from_now)
            });

        self.scheduled_restarts
            .values()
            .copied()
            .chain(
                self.pending_watch_restarts
                    .values()
                    .map(|state| state.due_at),
            )
            .chain(cron_restarts)
            .min()
    }

    fn save(&self) -> Result<()> {
        let mut values: Vec<ManagedProcess> = self.processes.values().cloned().collect();
        values.sort_by_key(|process| process.id);

        let state = PersistedState {
            next_id: self.next_id,
            processes: values,
        };

        save_state(&self.config.state_path, &state)
    }

    /// Restores baselines for the processes just loaded from state.
    ///
    /// Every decision about whether a persisted baseline may be adopted belongs to
    /// `ProcessBaselines::restore` — fingerprint match, unlabelled payload, unknown metric key.
    /// This function only supplies the current fingerprint and drops entries for processes that no
    /// longer exist, which is what satisfies "deleted process not restored": a process removed
    /// while the daemon was down leaves its baselines in the file, and they must not come back.
    fn restore_baselines(
        state_path: &Path,
        processes: &HashMap<String, ManagedProcess>,
    ) -> HashMap<String, oxmgr_analytics::baseline::ProcessBaselines> {
        let persisted = load_baselines(&baseline_store_path(state_path));
        let mut restored = HashMap::new();

        for (name, process) in processes {
            let Some(stored) = persisted.processes.get(name) else {
                continue;
            };
            let (baselines, outcome) = oxmgr_analytics::baseline::ProcessBaselines::restore(
                &process.config_fingerprint,
                stored,
            );
            match outcome {
                oxmgr_analytics::baseline::RestoreOutcome::Restored { metrics } if metrics > 0 => {
                    restored.insert(name.clone(), baselines);
                }
                // A discard is reported rather than silently swallowed: "re-warming after a
                // restart" and "re-warming because the config changed" are different operational
                // facts, and only one of them is worth investigating.
                oxmgr_analytics::baseline::RestoreOutcome::DiscardedConfigChanged { .. } => {
                    info!(
                        "process {name} baselines discarded: configuration changed since they were learned"
                    );
                }
                oxmgr_analytics::baseline::RestoreOutcome::DiscardedUnknownFingerprint => {
                    warn!(
                        "process {name} baselines discarded: stored payload carried no fingerprint"
                    );
                }
                // Restored but empty: nothing to carry, and inserting an empty set would only make
                // "warming" indistinguishable from "restored with no metrics".
                oxmgr_analytics::baseline::RestoreOutcome::Restored { .. } => {}
            }
        }

        restored
    }

    /// Persists baselines to their own file.
    ///
    /// Separate from [`Self::save`] and deliberately NOT called from it. `save` runs on every
    /// lifecycle transition; baselines change on every metric tick and are a reconstructible
    /// cache, so writing them on the lifecycle path would add a file rewrite to operations that
    /// have no reason to pay for one. A failure here is logged and swallowed for the same reason:
    /// losing a statistics cache must not fail a start or stop.
    fn save_baselines_now(&self) {
        let store = PersistedBaselineStore {
            processes: self
                .baselines
                .iter()
                .map(|(name, baselines)| (name.clone(), baselines.to_persisted()))
                .collect(),
        };

        if let Err(err) = save_baselines(&baseline_store_path(&self.config.state_path), &store) {
            warn!("failed to persist process baselines: {err}");
        }
    }

    async fn pid_matches_managed_process(&self, pid: u32, process: &ManagedProcess) -> bool {
        let spawn = match resolve_spawn_program(process, &self.config.base_dir).await {
            Ok(spawn) => spawn,
            Err(err) => {
                warn!(
                    "failed to resolve expected spawn program for process {} while verifying stale pid {}: {}",
                    process.name, pid, err
                );
                return false;
            }
        };
        pid_matches_expected_process(pid, &spawn.program, &spawn.args, process.cwd.as_deref())
    }

    fn resolve_target(&self, target: &str) -> Result<String> {
        if self.processes.contains_key(target) {
            return Ok(target.to_string());
        }

        if let Ok(id) = target.parse::<u64>()
            && let Some(name) = self
                .processes
                .values()
                .find(|process| process.id == id)
                .map(|process| process.name.clone())
        {
            return Ok(name);
        }

        Err(OxmgrError::ProcessNotFound(target.to_string()).into())
    }

    fn generate_auto_name(&self, command: &str) -> String {
        let stem = Path::new(command)
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("process");
        let base = sanitize_name(stem);

        if !self.processes.contains_key(&base) {
            return base;
        }

        let mut suffix = 1_u64;
        loop {
            let candidate = format!("{base}-{suffix}");
            if !self.processes.contains_key(&candidate) {
                return candidate;
            }
            suffix = suffix.saturating_add(1);
        }
    }

    fn refresh_resource_metrics(&mut self) {
        let now = now_epoch_secs();
        // Millisecond-resolution interval for rate derivation. `last_metrics_at` is
        // in seconds, so two refreshes inside one second would divide by zero.
        let sampled_at = std::time::Instant::now();
        let interval_ms = self
            .metrics_sampled_at
            .map(|previous| duration_millis(sampled_at.duration_since(previous)))
            .filter(|elapsed| *elapsed > 0);
        self.metrics_sampled_at = Some(sampled_at);

        // Reused across cycles: this ran every 2s and allocated a fresh Vec each time.
        // `clear` keeps the capacity, so a steady process count settles on zero
        // allocations here.
        // Samples are collected here and recorded after the loop: the loop borrows
        // `self.processes` mutably, so `self.metric_history` cannot be reached from inside it.
        let mut history_samples: Vec<(String, oxmgr_analytics::metrics_history::MetricSample)> =
            Vec::new();

        let mut tracked_pids = std::mem::take(&mut self.tracked_pid_scratch);
        tracked_pids.clear();
        tracked_pids.extend(
            self.processes
                .values()
                .filter(|process| process.status == ProcessStatus::Running)
                .filter_map(|process| process.pid.map(SysPid::from_u32)),
        );

        // Deduplicated before the refresh, and this is not defensive tidying: a repeated pid in
        // the slice makes sysinfo return NOTHING for ANY of them. Measured directly against
        // sysinfo 0.39 — refreshing `[pid]` finds the process, refreshing `[pid, pid]` finds
        // nothing at all.
        //
        // The failure would be silent and total: every managed process would have its metrics
        // cleared on the same tick and report "running, no reading". Duplicates need pid reuse
        // across two records to arise, which is rare, but one sort costs microseconds against
        // losing every figure on the dashboard.
        tracked_pids.sort_unstable();
        tracked_pids.dedup();

        if !tracked_pids.is_empty() {
            self.system
                .refresh_processes(ProcessesToUpdate::Some(&tracked_pids), true);
        }

        for process in self.processes.values_mut() {
            if process.status != ProcessStatus::Running {
                process.clear_resource_metrics();
                continue;
            }

            let Some(pid) = process.pid else {
                process.clear_resource_metrics();
                continue;
            };

            if let Some(proc_info) = self.system.process(SysPid::from_u32(pid)) {
                process.cpu_percent = proc_info.cpu_usage();
                process.memory_bytes = proc_info.memory();
                // `refresh_processes` already collects disk usage, so this costs
                // nothing extra to read.
                let io = proc_info.disk_usage();
                process.record_io_sample(io.read_bytes, io.written_bytes, pid, interval_ms);
                process.last_metrics_at = Some(now);

                // History rides the same readings rather than a second collection pass. Recorded
                // after `record_io_sample`, so the disk figures are the published amounts and an
                // absent measurement (first sample for a pid, or a pid change) stays absent here
                // instead of being written as a zero.
                let mut sample = oxmgr_analytics::metrics_history::MetricSample::cpu_memory(
                    now.saturating_mul(1_000),
                    process.cpu_percent,
                    process.memory_bytes,
                );
                if let Some(elapsed) = process.metrics_interval_ms {
                    sample = sample.with_disk(
                        process.disk_read_bytes,
                        process.disk_write_bytes,
                        elapsed,
                    );
                }
                history_samples.push((process.name.clone(), sample));
            } else {
                // Running by our record but absent from sysinfo's refresh. Not a
                // zero reading — no reading at all, so clearing also drops the pid
                // and the next sample starts differencing afresh.
                process.clear_resource_metrics();
                process.last_metrics_at = Some(now);
            }
        }

        for (name, sample) in history_samples {
            self.metric_history.record(&name, sample);
        }

        // Hand the buffer back with its capacity intact. Without this the `take` above
        // would leave the field empty and the next cycle would allocate again — the
        // allocation would have moved rather than gone.
        self.tracked_pid_scratch = tracked_pids;
    }

    async fn run_resource_limit_checks(&mut self) -> Result<()> {
        let violating: Vec<(String, bool, bool)> = self
            .processes
            .values()
            .filter_map(|process| {
                if process.status != ProcessStatus::Running || process.pid.is_none() {
                    return None;
                }

                let limits = process.resource_limits.as_ref()?;

                let memory_exceeded = limits
                    .max_memory_mb
                    .map(|max_mb| process.memory_bytes > max_mb.saturating_mul(1024 * 1024))
                    .unwrap_or(false);
                let cpu_exceeded = limits
                    .max_cpu_percent
                    .map(|max_cpu| u64_to_f64(max_cpu) > f64::from(process.cpu_percent))
                    .unwrap_or(false);

                if memory_exceeded || cpu_exceeded {
                    Some((process.name.clone(), memory_exceeded, cpu_exceeded))
                } else {
                    None
                }
            })
            .collect();

        let mut should_save = false;

        for (name, memory_exceeded, cpu_exceeded) in violating {
            let Some(snapshot) = self.processes.get(&name).cloned() else {
                continue;
            };

            if snapshot.restart_count >= snapshot.max_restarts {
                warn!(
                    "resource limits exceeded for process {} and max_restarts reached; stopping process",
                    name
                );
                if let Some(pid) = snapshot.pid {
                    let timeout = Duration::from_secs(snapshot.stop_timeout_secs.max(1));
                    // The discard is deliberate: best-effort termination during cleanup; delivery failures are logged inside terminate_pid
                    #[expect(
                        clippy::let_underscore_must_use,
                        reason = "best-effort termination during cleanup; delivery failures are logged inside terminate_pid"
                    )]
                    let _ = terminate_pid(pid, snapshot.stop_signal.as_deref(), timeout).await;
                }

                if let Some(process) = self.processes.get_mut(&name) {
                    process.pid = None;
                    cleanup_process_cgroup(process);
                    process.desired_state = DesiredState::Stopped;
                    process.status = ProcessStatus::Errored;
                    process.clear_resource_metrics();
                    process.last_health_error =
                        Some("resource limit exceeded and max_restarts reached".to_string());
                }
                should_save = true;
                continue;
            }

            warn!(
                "resource limit exceeded for process {} (memory_exceeded={}, cpu_exceeded={}); restarting",
                name, memory_exceeded, cpu_exceeded
            );

            match self.restart_process_internal(&name, false).await {
                Ok(_) => {
                    if let Some(process) = self.processes.get_mut(&name) {
                        process.restart_count = snapshot.restart_count.saturating_add(1);
                        process.last_health_error = Some(format!(
                            "resource limit restart (memory_exceeded={}, cpu_exceeded={})",
                            memory_exceeded, cpu_exceeded
                        ));
                    }
                    self.save()?;
                }
                Err(err) => {
                    error!(
                        "resource-limit restart failed for process {}: {}",
                        name, err
                    );
                    if let Some(process) = self.processes.get_mut(&name) {
                        process.status = ProcessStatus::Errored;
                        process.last_health_error =
                            Some(format!("resource-limit restart failed: {err}"));
                    }
                    should_save = true;
                }
            }
        }

        if should_save {
            self.save()?;
        }

        Ok(())
    }

    async fn run_watch_checks(&mut self) -> Result<()> {
        let candidates: Vec<String> = self
            .processes
            .values()
            .filter(|process| {
                process.watch && process.status == ProcessStatus::Running && process.pid.is_some()
            })
            .map(|process| process.name.clone())
            .collect();

        for name in candidates {
            let Some(snapshot) = self.processes.get(&name).cloned() else {
                continue;
            };

            let current_fingerprint = match watch_fingerprint_for_process(&snapshot) {
                Ok(value) => value,
                Err(err) => {
                    warn!("watch scan failed for process {}: {}", name, err);
                    continue;
                }
            };

            let Some(previous_fingerprint) = self.watch_fingerprints.get(&name).copied() else {
                self.watch_fingerprints
                    .insert(name.clone(), current_fingerprint);
                continue;
            };

            if previous_fingerprint == current_fingerprint {
                self.pending_watch_restarts.remove(&name);
                continue;
            }

            if snapshot.watch_delay_secs > 0 {
                let due_at = TokioInstant::now() + Duration::from_secs(snapshot.watch_delay_secs);
                self.pending_watch_restarts.insert(
                    name.clone(),
                    PendingWatchRestart {
                        due_at,
                        fingerprint: current_fingerprint,
                    },
                );
                continue;
            }

            warn!(
                "filesystem change detected for process {}; triggering restart",
                name
            );

            match self.restart_process_internal(&name, false).await {
                Ok(_) => {
                    if let Some(process) = self.processes.get_mut(&name) {
                        process.restart_count = snapshot.restart_count.saturating_add(1);
                        process.last_health_error = Some("watch-triggered restart".to_string());
                    }
                    self.watch_fingerprints
                        .insert(name.clone(), current_fingerprint);
                    self.pending_watch_restarts.remove(&name);
                    self.save()?;
                }
                Err(err) => {
                    error!("watch restart failed for process {}: {}", name, err);
                    if let Some(process) = self.processes.get_mut(&name) {
                        process.status = ProcessStatus::Errored;
                        process.last_health_error = Some(format!("watch restart failed: {err}"));
                    }
                    self.pending_watch_restarts.remove(&name);
                    self.save()?;
                }
            }
        }

        Ok(())
    }

    async fn run_health_checks(&mut self) -> Result<()> {
        let now = now_epoch_secs();
        let due_names: Vec<String> = self
            .processes
            .values()
            .filter(|process| {
                process.status == ProcessStatus::Running
                    && process.pid.is_some()
                    && process.health_check.is_some()
                    && process
                        .next_health_check
                        .map(|next| next <= now)
                        .unwrap_or(true)
            })
            .map(|process| process.name.clone())
            .collect();

        let mut should_save = false;

        for name in due_names {
            let Some(snapshot) = self.processes.get(&name).cloned() else {
                continue;
            };

            let Some(check) = snapshot.health_check.clone() else {
                continue;
            };

            let outcome = execute_health_check(&snapshot, &check).await;
            let mut should_restart = false;

            // Read BEFORE the mutable borrow of `self.processes` below, because that borrow lasts
            // until the event is built and the engine lives on the same `self`. Collected
            // unconditionally rather than inside the `Err` arm for the same reason: doing it there
            // would need a second borrow of `self` while `process` is still held.
            //
            // Cheap in the healthy case — `active_for` is a range scan over a `BTreeMap` keyed by
            // process, so a process with no findings walks nothing.
            let active_findings: Vec<String> = self
                .analysis
                .registry()
                .active_for(&name)
                .iter()
                .map(|finding| finding.key.as_string())
                .collect();

            let health_event = {
                let Some(process) = self.processes.get_mut(&name) else {
                    continue;
                };

                if process.pid != snapshot.pid {
                    continue;
                }

                process.last_health_check = Some(now);
                process.next_health_check = Some(now.saturating_add(check.interval_secs.max(1)));

                let event = match &outcome {
                    Ok(()) => {
                        process.health_status = HealthStatus::Healthy;
                        process.health_failures = 0;
                        process.last_health_error = None;
                        Some(BusEvent::health_healthy(EventProcessInfo::from(
                            process as &ManagedProcess,
                        )))
                    }
                    Err(err) => {
                        process.health_status = HealthStatus::Unhealthy;
                        process.health_failures = process.health_failures.saturating_add(1);
                        process.last_health_error = Some(err.to_string());
                        // The correlating form: any resource finding active right now travels with
                        // the failure. A health failure caused by memory pressure and one caused by
                        // a broken endpoint need different responses, and a bare "health check
                        // failed" makes an operator correlate by hand against a dashboard that may
                        // already have moved on.
                        let ev = BusEvent::health_unhealthy_with_findings(
                            EventProcessInfo::from(process as &ManagedProcess),
                            err.to_string(),
                            process.health_failures,
                            active_findings,
                        );

                        if process.health_failures >= check.max_failures.max(1) {
                            should_restart = true;
                            process.health_failures = 0;
                        }
                        Some(ev)
                    }
                };
                should_save = true;
                event
            };

            if let Some(event) = health_event {
                self.emit(event);
            }

            if should_restart {
                if snapshot.restart_count >= snapshot.max_restarts {
                    warn!(
                        "health checks failed for process {} and max_restarts reached; stopping process",
                        name
                    );
                    if let Some(pid) = snapshot.pid {
                        let timeout = Duration::from_secs(snapshot.stop_timeout_secs.max(1));
                        // The discard is deliberate: best-effort termination during cleanup; delivery failures are logged inside terminate_pid
                        #[expect(
                            clippy::let_underscore_must_use,
                            reason = "best-effort termination during cleanup; delivery failures are logged inside terminate_pid"
                        )]
                        let _ = terminate_pid(pid, snapshot.stop_signal.as_deref(), timeout).await;
                    }
                    if let Some(process) = self.processes.get_mut(&name) {
                        process.pid = None;
                        cleanup_process_cgroup(process);
                        process.desired_state = DesiredState::Stopped;
                        process.status = ProcessStatus::Errored;
                        process.clear_resource_metrics();
                        process.last_health_error =
                            Some("health checks failed and max_restarts reached".to_string());
                    }
                    should_save = true;
                    continue;
                }

                warn!(
                    "health checks failed for process {} repeatedly; restarting process",
                    name
                );
                match self.restart_process_internal(&name, false).await {
                    Err(err) => {
                        error!("health-check restart failed for process {}: {}", name, err);
                        if let Some(process) = self.processes.get_mut(&name) {
                            process.status = ProcessStatus::Errored;
                            process.last_health_error =
                                Some(format!("health restart failed after max failures: {err}"));
                        }
                        should_save = true;
                    }
                    _ => {
                        if let Some(process) = self.processes.get_mut(&name) {
                            process.restart_count = snapshot.restart_count.saturating_add(1);
                        }
                    }
                }
            }
        }

        if should_save {
            self.save()?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests;

/// Typical values for one process, over a stated window.
///
/// `Option<Typical>` per metric rather than `Typical` directly: `None` means the metric was never
/// retained for this process at all, which is a different absence from `Typical::Unavailable`, where
/// history exists but holds too few samples to derive a median from.
#[derive(Debug, Clone, PartialEq)]
pub struct TypicalReport {
    pub cpu: Option<oxmgr_core::severity::Typical>,
    pub memory: Option<oxmgr_core::severity::Typical>,
    /// The window the medians summarise, carried so the claim is interpretable: "typical over
    /// fifteen minutes" and "typical over a day" are different statements.
    pub window_secs: u64,
}
