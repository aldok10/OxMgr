//! Core process-domain types shared across the CLI, daemon, storage, and
//! configuration layers.
//!
//! Lint-level cleanup: casts here convert bounded process counters (u64 bytes,
//! small rates) to f64 for display and wire format; values < 2^53 are exact.

use oxmgr_core::numeric::u64_to_f64;
use std::collections::HashMap;
use std::path::PathBuf;

use oxmgr_core::events::EventProcessInfo;
use serde::{Deserialize, Serialize};

mod fingerprint;

/// Default number of daemon-triggered restarts allowed inside the crash-loop
/// window before Oxmgr stops retrying automatically.
pub const DEFAULT_CRASH_RESTART_LIMIT: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
/// Automatic restart behaviour applied after a managed process exits.
pub enum RestartPolicy {
    /// Restart after every exit, including clean exits.
    Always,
    /// Restart only when the process exits unsuccessfully.
    OnFailure,
    /// Never restart automatically.
    Never,
}

impl std::fmt::Display for RestartPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            RestartPolicy::Always => "always",
            RestartPolicy::OnFailure => "on-failure",
            RestartPolicy::Never => "never",
        };
        write!(f, "{value}")
    }
}

impl RestartPolicy {
    /// Returns whether this policy permits a restart for the given exit result.
    pub fn should_restart(&self, exited_successfully: bool) -> bool {
        match self {
            RestartPolicy::Always => true,
            RestartPolicy::OnFailure => !exited_successfully,
            RestartPolicy::Never => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
/// Persisted runtime status reported for a managed process.
pub enum ProcessStatus {
    /// The process is currently running.
    Running,
    /// The process is not running and is not scheduled to restart immediately.
    Stopped,
    /// The process exited unexpectedly and has not yet been recovered.
    Crashed,
    /// The daemon is in the middle of restarting the process.
    Restarting,
    /// The daemon could not start or manage the process successfully.
    Errored,
}

impl std::fmt::Display for ProcessStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            ProcessStatus::Running => "running",
            ProcessStatus::Stopped => "stopped",
            ProcessStatus::Crashed => "crashed",
            ProcessStatus::Restarting => "restarting",
            ProcessStatus::Errored => "errored",
        };
        write!(f, "{value}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
/// Desired steady-state requested by the user or configuration.
pub enum DesiredState {
    /// The process should be running whenever policy permits it.
    Running,
    /// The process should remain stopped until explicitly started again.
    Stopped,
}

impl std::fmt::Display for DesiredState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            DesiredState::Running => "running",
            DesiredState::Stopped => "stopped",
        };
        write!(f, "{value}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
/// Result of the most recent health-check evaluation.
pub enum HealthStatus {
    /// No health verdict is available yet.
    #[default]
    Unknown,
    /// The last completed health check succeeded.
    Healthy,
    /// The last completed health check failed or timed out.
    Unhealthy,
}

impl std::fmt::Display for HealthStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            HealthStatus::Unknown => "unknown",
            HealthStatus::Healthy => "healthy",
            HealthStatus::Unhealthy => "unhealthy",
        };
        write!(f, "{value}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
/// External command used by the daemon to determine whether a process is healthy.
pub struct HealthCheck {
    pub command: String,
    pub interval_secs: u64,
    pub timeout_secs: u64,
    pub max_failures: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
/// Optional runtime limits and isolation flags associated with a process.
pub struct ResourceLimits {
    #[serde(default)]
    pub max_memory_mb: Option<u64>,
    #[serde(default)]
    pub max_cpu_percent: Option<u64>,
    #[serde(default)]
    pub cgroup_enforce: bool,
    #[serde(default)]
    pub deny_gpu: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// User-supplied process definition before the daemon assigns runtime identity
/// and concrete log files.
pub struct StartProcessSpec {
    pub command: String,
    pub name: Option<String>,
    #[serde(default)]
    pub pre_reload_cmd: Option<String>,
    pub restart_policy: RestartPolicy,
    pub max_restarts: u32,
    #[serde(default = "default_crash_restart_limit")]
    pub crash_restart_limit: u32,
    pub cwd: Option<PathBuf>,
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub health_check: Option<HealthCheck>,
    #[serde(default)]
    pub stop_signal: Option<String>,
    pub stop_timeout_secs: u64,
    pub restart_delay_secs: u64,
    pub start_delay_secs: u64,
    #[serde(default)]
    pub watch: bool,
    #[serde(default)]
    pub watch_paths: Vec<PathBuf>,
    #[serde(default)]
    pub ignore_watch: Vec<String>,
    #[serde(default)]
    pub watch_delay_secs: u64,
    #[serde(default)]
    pub cluster_mode: bool,
    #[serde(default)]
    pub cluster_instances: Option<u32>,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub resource_limits: Option<ResourceLimits>,
    #[serde(default)]
    pub git_repo: Option<String>,
    #[serde(default)]
    pub git_ref: Option<String>,
    #[serde(default)]
    pub pull_secret_hash: Option<String>,
    #[serde(default)]
    pub reuse_port: bool,
    #[serde(default)]
    pub wait_ready: bool,
    #[serde(default = "default_ready_timeout_secs")]
    pub ready_timeout_secs: u64,
    #[serde(default)]
    pub log_date_format: Option<String>,
    #[serde(default)]
    pub unified_logs: bool,
    #[serde(default)]
    pub cron_restart: Option<String>,
    #[serde(default)]
    pub stdout_log_override: Option<PathBuf>,
    #[serde(default)]
    pub stderr_log_override: Option<PathBuf>,
    /// Names this process declares a dependency on.
    ///
    /// Carried onto the runtime record so failure correlation can follow declared edges. Ordering
    /// and duplicates are preserved as declared rather than normalised here: the declaration is the
    /// operator's statement, and `DependencyGraph` is where it gets interpreted.
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Full runtime record maintained by the daemon for each managed process.
///
/// This structure combines the original process specification with generated
/// identifiers, runtime metrics, log locations, and bookkeeping required for
/// restart and health-check logic.
pub struct ManagedProcess {
    pub id: u64,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    #[serde(default)]
    pub pre_reload_cmd: Option<String>,
    pub cwd: Option<PathBuf>,
    pub env: HashMap<String, String>,
    pub restart_policy: RestartPolicy,
    pub max_restarts: u32,
    pub restart_count: u32,
    #[serde(default = "default_crash_restart_limit")]
    pub crash_restart_limit: u32,
    #[serde(default)]
    pub auto_restart_history: Vec<u64>,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub git_repo: Option<String>,
    #[serde(default)]
    pub git_ref: Option<String>,
    #[serde(default)]
    pub pull_secret_hash: Option<String>,
    #[serde(default)]
    pub reuse_port: bool,
    #[serde(default)]
    pub stop_signal: Option<String>,
    #[serde(default = "default_stop_timeout_secs")]
    pub stop_timeout_secs: u64,
    #[serde(default)]
    pub restart_delay_secs: u64,
    #[serde(default)]
    pub restart_backoff_cap_secs: u64,
    #[serde(default)]
    pub restart_backoff_reset_secs: u64,
    #[serde(default)]
    pub restart_backoff_attempt: u32,
    #[serde(default)]
    pub start_delay_secs: u64,
    #[serde(default)]
    pub watch: bool,
    #[serde(default)]
    pub watch_paths: Vec<PathBuf>,
    #[serde(default)]
    pub ignore_watch: Vec<String>,
    #[serde(default)]
    pub watch_delay_secs: u64,
    #[serde(default)]
    pub cluster_mode: bool,
    #[serde(default)]
    pub cluster_instances: Option<u32>,
    #[serde(default)]
    pub resource_limits: Option<ResourceLimits>,
    #[serde(default)]
    pub cgroup_path: Option<String>,
    pub pid: Option<u32>,
    pub status: ProcessStatus,
    pub desired_state: DesiredState,
    pub last_exit_code: Option<i32>,
    pub stdout_log: PathBuf,
    pub stderr_log: PathBuf,
    #[serde(default)]
    pub health_check: Option<HealthCheck>,
    #[serde(default)]
    pub health_status: HealthStatus,
    #[serde(default)]
    pub health_failures: u32,
    #[serde(default)]
    pub last_health_check: Option<u64>,
    #[serde(default)]
    pub next_health_check: Option<u64>,
    #[serde(default)]
    pub last_health_error: Option<String>,
    /// Last error message from crash or failed start (stderr tail).
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub wait_ready: bool,
    #[serde(default = "default_ready_timeout_secs")]
    pub ready_timeout_secs: u64,
    #[serde(default)]
    pub cpu_percent: f32,
    #[serde(default)]
    pub memory_bytes: u64,
    /// Bytes read from disk since the previous metrics refresh. An amount, not a
    /// rate: divide by [`Self::metrics_interval_ms`], or call
    /// [`Self::disk_read_rate_bps`], which refuses to divide when the interval is
    /// missing or too short to be usable.
    ///
    /// Zero here is a real reading only when `metrics_interval_ms` is `Some`;
    /// otherwise no measurement was taken and the zero carries no information.
    /// Counts only the I/O the tracked pid performed itself — see
    /// `docs/PROCESS-IO-METRICS.md` for the attribution boundary.
    #[serde(default)]
    pub disk_read_bytes: u64,
    /// Bytes written to disk since the previous metrics refresh. See
    /// [`Self::disk_read_bytes`] for how to read a zero.
    #[serde(default)]
    pub disk_write_bytes: u64,
    /// Milliseconds the disk amounts above cover, so a consumer can derive a rate
    /// from the interval actually observed. The maintenance tick uses
    /// `MissedTickBehavior::Skip`, so that interval is at least 2s and may be more:
    /// assuming the nominal value overstates the rate. `None` when no valid
    /// interval exists yet (first sample, or the pid changed).
    #[serde(default)]
    pub metrics_interval_ms: Option<u64>,
    /// The pid the last metrics sample was taken for. Runtime-only: after a daemon
    /// restart there is no previous sample to difference against, so persisting it
    /// would claim an interval that was never observed.
    #[serde(skip)]
    pub metrics_pid: Option<u32>,
    /// Bytes read since this managed process was first started, accumulated across
    /// restarts. sysinfo's own totals are scoped to a pid, so copying them here
    /// made the value fall backwards on restart; summing the per-refresh amounts
    /// keeps it monotonic for the life of the managed process. A lower bound: I/O
    /// between refreshes, or after the last refresh before an exit, is not seen.
    #[serde(default)]
    pub disk_read_total: u64,
    /// Bytes written since this managed process was first started. See
    /// [`Self::disk_read_total`].
    #[serde(default)]
    pub disk_write_total: u64,
    #[serde(default)]
    pub last_metrics_at: Option<u64>,
    #[serde(default)]
    pub last_started_at: Option<u64>,
    #[serde(default)]
    pub last_stopped_at: Option<u64>,
    #[serde(default)]
    pub config_fingerprint: String,
    #[serde(default)]
    pub log_date_format: Option<String>,
    #[serde(default)]
    pub unified_logs: bool,
    #[serde(default)]
    pub cron_restart: Option<String>,
    #[serde(skip)]
    pub next_cron_restart: Option<u64>,
    /// Names this process declares a dependency on, as declared.
    ///
    /// `#[serde(default)]` so a state file written before this field existed loads as "declares
    /// none" rather than failing recovery. Persisted, so the declaration survives a restart without
    /// needing the original configuration file to still be present.
    ///
    /// Deliberately NOT filtered to currently-managed processes: a dependency on something not
    /// managed is retained and reported as unresolved, because silently dropping it would make an
    /// operator's typo indistinguishable from a dependency they never declared.
    #[serde(default)]
    pub depends_on: Vec<String>,
}

/// Projection of a managed process onto the event-bus payload type. Lives on
/// this side of the boundary because `ManagedProcess` is a manager-layer
/// type; `oxmgr-core` must not know it (workspace-crate-layout, reactive
/// data-flow ownership).
impl From<&ManagedProcess> for EventProcessInfo {
    fn from(p: &ManagedProcess) -> Self {
        let mut command = p.command.clone();
        if !p.args.is_empty() {
            command.push(' ');
            command.push_str(&p.args.join(" "));
        }
        Self {
            id: p.id,
            name: p.name.clone(),
            namespace: p.namespace.clone(),
            pid: p.pid,
            command,
            cwd: p.cwd.as_ref().map(|c| c.display().to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Event sent back to the process manager when a child process exits.
pub struct ProcessExitEvent {
    pub name: String,
    pub pid: u32,
    pub exit_code: Option<i32>,
    /// POSIX signal name that killed the process, e.g. `"SIGSEGV"`. `None` on
    /// Windows or when the process exited normally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    pub success: bool,
    pub wait_error: bool,
}

impl ManagedProcess {
    /// Returns a compact label suitable for user-facing messages.
    pub fn target_label(&self) -> String {
        format!("{} ({})", self.name, self.id)
    }

    /// Zeroes every live resource reading.
    ///
    /// A process that is not running has no usage, and the readings are cleared
    /// in a dozen places (stop, crash, exit, reap, adopt-failure). Doing it
    /// field-by-field at each site is how a newly added metric ends up stale in
    /// one branch and correct in the rest, so every site calls this instead.
    ///
    /// [`Self::disk_read_total`] and [`Self::disk_write_total`] are deliberately
    /// *not* cleared: they are scoped to the managed process, not to the pid, so
    /// clearing them here would reset the counter on every restart — the same
    /// backwards jump this accounting exists to remove. Nothing pid-scoped is
    /// left behind either way, because the pid-scoped totals sysinfo reports are
    /// no longer copied onto the record at all.
    pub fn clear_resource_metrics(&mut self) {
        self.cpu_percent = 0.0;
        self.memory_bytes = 0;
        self.disk_read_bytes = 0;
        self.disk_write_bytes = 0;
        // No interval is valid once sampling stops: the next amount must not be
        // divided by the gap across a stop.
        self.metrics_interval_ms = None;
        // The pid this sample belonged to is gone. Leaving it set was how a
        // stopped process kept a dead pid on its record, and if the OS ever
        // recycled that number the next refresh would difference the new
        // process against the old one's readings and claim an interval that
        // spanned the stop.
        self.metrics_pid = None;
    }

    /// Folds one disk I/O sample into the record.
    ///
    /// `interval_ms` is the wall time since the previous sample of *any* process,
    /// as measured by the collector; `pid` is the process the sample was read
    /// from. Both are needed to decide whether this sample is a difference
    /// against the previous one or merely the first sighting of a new pid, and
    /// only a difference may be published or accumulated.
    ///
    /// Kept here rather than inline in the collector so the pid-change,
    /// first-sample and short-interval branches can be exercised without waiting
    /// on a real workload to perform real I/O.
    pub fn record_io_sample(
        &mut self,
        read_bytes: u64,
        written_bytes: u64,
        pid: u32,
        interval_ms: Option<u64>,
    ) {
        // The first sample after a pid appears has nothing to difference against:
        // sysinfo's first reading for a pid is that pid's lifetime figure, not an
        // amount since our last look. On a fresh spawn that is near zero and
        // harmless; on an adopted process it can be gigabytes, and accumulating it
        // would credit the managed process with I/O from before it was watched.
        let continues_same_pid = self.metrics_pid == Some(pid);

        // An interval is only claimed for a sample that differences against the
        // previous one, and only when it is long enough to divide by. Below the
        // floor the quotient is noise, so no rate is offered rather than a wrong one.
        self.metrics_interval_ms = interval_ms
            .filter(|_| continues_same_pid)
            .filter(|elapsed| *elapsed >= MIN_RATE_INTERVAL_MS);
        self.metrics_pid = Some(pid);

        if self.metrics_interval_ms.is_none() {
            // Without a usable interval there is no measurement to report.
            // Publishing the seed sample would present it as covering a known
            // period, which is the zero-that-means-unknown this accounting removes.
            self.disk_read_bytes = 0;
            self.disk_write_bytes = 0;
            return;
        }

        self.disk_read_bytes = read_bytes;
        self.disk_write_bytes = written_bytes;
        // Accumulate rather than copy sysinfo's totals: those are scoped to the
        // pid, so a restart made them fall backwards (measured: pid 53658
        // write_total 33353728 -> pid 4032 write_total 1814528). Summing the
        // per-sample amounts keeps the counter monotonic for the managed process's
        // lifetime, and it survives a restart because nothing resets it here.
        self.disk_read_total = self.disk_read_total.saturating_add(read_bytes);
        self.disk_write_total = self.disk_write_total.saturating_add(written_bytes);
    }

    /// Bytes read per second over the interval the last sample actually covered,
    /// or `None` when no rate can be derived from it.
    ///
    /// `None` and `Some(0)` are different answers and callers must keep them
    /// apart: the first means the daemon cannot say, the second means the
    /// process genuinely read nothing. `None` covers the first sample after a
    /// pid appears (nothing to difference against), a sample taken across a pid
    /// change, a stopped process, and an interval too short to divide by.
    pub fn disk_read_rate_bps(&self) -> Option<f64> {
        rate_bytes_per_sec(self.disk_read_bytes, self.metrics_interval_ms)
    }

    /// Bytes written per second over the interval the last sample covered. See
    /// [`Self::disk_read_rate_bps`] for what `None` means.
    pub fn disk_write_rate_bps(&self) -> Option<f64> {
        rate_bytes_per_sec(self.disk_write_bytes, self.metrics_interval_ms)
    }

    // No `reset_io_totals` here on purpose: deleting a process drops its whole
    // record, so the accumulators go with it. A separate reset would only be needed
    // for an explicit "reset metrics" command, which does not exist yet — and an
    // unused reset is a reset nobody has tested.
}

/// Shortest interval a rate may be divided by, in milliseconds.
///
/// The maintenance tick is nominally 2s, so an interval this short means
/// something anomalous — two refreshes landing back to back as the daemon
/// catches up after a stall. Dividing a 2-second amount by a few milliseconds
/// reports a rate orders of magnitude above the truth, which is worse than
/// reporting nothing, so anything under this floor is treated as unusable.
pub const MIN_RATE_INTERVAL_MS: u64 = 100;

/// Converts a per-sample amount and the interval it covers into bytes per second.
fn rate_bytes_per_sec(amount: u64, interval_ms: Option<u64>) -> Option<f64> {
    let interval_ms = interval_ms?;
    if interval_ms < MIN_RATE_INTERVAL_MS {
        return None;
    }
    Some(u64_to_f64(amount) * 1000.0 / u64_to_f64(interval_ms))
}

/// Returns the default crash-loop threshold used when older persisted state
/// does not include an explicit value.
pub fn default_crash_restart_limit() -> u32 {
    DEFAULT_CRASH_RESTART_LIMIT
}

fn default_stop_timeout_secs() -> u64 {
    5
}

pub fn default_ready_timeout_secs() -> u64 {
    30
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::{
        DEFAULT_CRASH_RESTART_LIMIT, DesiredState, HealthCheck, HealthStatus, MIN_RATE_INTERVAL_MS,
        ManagedProcess, ProcessStatus, ResourceLimits, RestartPolicy, StartProcessSpec,
        default_crash_restart_limit, default_ready_timeout_secs, default_stop_timeout_secs,
    };

    #[test]
    fn restart_policy_always_restarts_on_success_and_failure() {
        assert!(RestartPolicy::Always.should_restart(true));
        assert!(RestartPolicy::Always.should_restart(false));
    }

    #[test]
    fn restart_policy_on_failure_only_restarts_failed_processes() {
        assert!(!RestartPolicy::OnFailure.should_restart(true));
        assert!(RestartPolicy::OnFailure.should_restart(false));
    }

    #[test]
    fn restart_policy_never_does_not_restart() {
        assert!(!RestartPolicy::Never.should_restart(true));
        assert!(!RestartPolicy::Never.should_restart(false));
    }

    #[test]
    fn display_impls_use_expected_strings() {
        assert_eq!(RestartPolicy::Always.to_string(), "always");
        assert_eq!(RestartPolicy::OnFailure.to_string(), "on-failure");
        assert_eq!(RestartPolicy::Never.to_string(), "never");

        assert_eq!(ProcessStatus::Running.to_string(), "running");
        assert_eq!(ProcessStatus::Stopped.to_string(), "stopped");
        assert_eq!(ProcessStatus::Crashed.to_string(), "crashed");
        assert_eq!(ProcessStatus::Restarting.to_string(), "restarting");
        assert_eq!(ProcessStatus::Errored.to_string(), "errored");

        assert_eq!(HealthStatus::Unknown.to_string(), "unknown");
        assert_eq!(HealthStatus::Healthy.to_string(), "healthy");
        assert_eq!(HealthStatus::Unhealthy.to_string(), "unhealthy");
    }

    #[test]
    fn health_status_default_is_unknown() {
        assert_eq!(HealthStatus::default(), HealthStatus::Unknown);
    }

    #[test]
    fn managed_process_target_label_contains_name_and_id() {
        let process = fixture_process();
        assert_eq!(process.target_label(), "api (42)");
    }

    #[test]
    fn stop_timeout_default_is_five_seconds() {
        assert_eq!(default_stop_timeout_secs(), 5);
    }

    #[test]
    fn crash_restart_limit_default_is_three() {
        assert_eq!(default_crash_restart_limit(), 3);
        assert_eq!(DEFAULT_CRASH_RESTART_LIMIT, 3);
    }

    #[test]
    fn config_fingerprint_matches_between_start_spec_and_managed_process() {
        let mut env = HashMap::new();
        env.insert("NODE_ENV".to_string(), "production".to_string());

        let health_check = Some(HealthCheck {
            command: format!(
                "curl -f {}/health",
                oxmgr_core::constants::DEFAULT_HEALTH_URL
            ),
            interval_secs: 10,
            timeout_secs: 2,
            max_failures: 3,
        });
        let resource_limits = Some(ResourceLimits {
            max_memory_mb: Some(512),
            max_cpu_percent: Some(75),
            cgroup_enforce: true,
            deny_gpu: false,
        });

        let spec = StartProcessSpec {
            command: "node server.js --port 3000".to_string(),
            name: Some("api".to_string()),
            pre_reload_cmd: Some("npm run build".to_string()),
            restart_policy: RestartPolicy::OnFailure,
            max_restarts: 5,
            crash_restart_limit: DEFAULT_CRASH_RESTART_LIMIT,
            cwd: Some(PathBuf::from("/srv/api")),
            env: env.clone(),
            health_check: health_check.clone(),
            stop_signal: Some("SIGTERM".to_string()),
            stop_timeout_secs: 15,
            restart_delay_secs: 4,
            start_delay_secs: 2,
            watch: true,
            watch_paths: vec![PathBuf::from("src")],
            ignore_watch: vec!["node_modules".to_string()],
            watch_delay_secs: 2,
            cluster_mode: true,
            cluster_instances: Some(2),
            namespace: Some("prod".to_string()),
            resource_limits: resource_limits.clone(),
            git_repo: Some("https://example.com/repo.git".to_string()),
            git_ref: Some("main".to_string()),
            pull_secret_hash: Some("abc123".to_string()),
            reuse_port: true,
            wait_ready: true,
            ready_timeout_secs: 45,
            log_date_format: None,
            unified_logs: false,
            cron_restart: None,
            stdout_log_override: None,
            stderr_log_override: None,
            depends_on: Vec::new(),
        };

        let mut process = fixture_process();
        process.command = "node".to_string();
        process.args = vec![
            "server.js".to_string(),
            "--port".to_string(),
            "3000".to_string(),
        ];
        process.cwd = spec.cwd.clone();
        process.env = env;
        process.pre_reload_cmd = spec.pre_reload_cmd.clone();
        process.max_restarts = spec.max_restarts;
        process.health_check = health_check;
        process.stop_timeout_secs = spec.stop_timeout_secs;
        process.restart_delay_secs = spec.restart_delay_secs;
        process.start_delay_secs = spec.start_delay_secs;
        process.watch = spec.watch;
        process.watch_paths = spec.watch_paths.clone();
        process.ignore_watch = spec.ignore_watch.clone();
        process.watch_delay_secs = spec.watch_delay_secs;
        process.cluster_mode = spec.cluster_mode;
        process.cluster_instances = spec.cluster_instances;
        process.namespace = spec.namespace.clone();
        process.resource_limits = resource_limits;
        process.git_repo = spec.git_repo.clone();
        process.git_ref = spec.git_ref.clone();
        process.pull_secret_hash = spec.pull_secret_hash.clone();
        process.reuse_port = spec.reuse_port;
        process.wait_ready = spec.wait_ready;
        process.ready_timeout_secs = spec.ready_timeout_secs;
        process.log_date_format = spec.log_date_format.clone();
        process.unified_logs = spec.unified_logs;
        process.cron_restart = spec.cron_restart.clone();

        assert_eq!(spec.config_fingerprint(), process.config_fingerprint());
    }

    #[test]
    fn config_fingerprint_is_stable_across_env_insertion_order() {
        let mut env_a = HashMap::new();
        env_a.insert("PORT".to_string(), "3000".to_string());
        env_a.insert("NODE_ENV".to_string(), "production".to_string());

        let mut env_b = HashMap::new();
        env_b.insert("NODE_ENV".to_string(), "production".to_string());
        env_b.insert("PORT".to_string(), "3000".to_string());

        let mut spec_a = fixture_start_spec();
        spec_a.env = env_a;

        let mut spec_b = fixture_start_spec();
        spec_b.env = env_b;

        assert_eq!(spec_a.config_fingerprint(), spec_b.config_fingerprint());
    }

    #[test]
    fn config_fingerprint_falls_back_to_raw_command_when_shell_parsing_fails() {
        let mut broken = fixture_start_spec();
        broken.command = "node \"unterminated".to_string();

        let mut changed = fixture_start_spec();
        changed.command = "node \"unterminated extra".to_string();

        assert_ne!(broken.config_fingerprint(), changed.config_fingerprint());
    }

    #[test]
    fn config_fingerprint_changes_when_resource_limits_change() {
        let mut baseline = fixture_start_spec();
        baseline.resource_limits = Some(ResourceLimits {
            max_memory_mb: Some(512),
            max_cpu_percent: Some(50),
            cgroup_enforce: false,
            deny_gpu: false,
        });

        let mut changed = baseline.clone();
        changed.resource_limits = Some(ResourceLimits {
            max_memory_mb: Some(1024),
            max_cpu_percent: Some(50),
            cgroup_enforce: false,
            deny_gpu: false,
        });

        assert_ne!(baseline.config_fingerprint(), changed.config_fingerprint());
    }

    #[test]
    fn redacted_for_transport_clears_sensitive_fields_and_populates_fingerprint() {
        let mut process = fixture_process();
        process
            .env
            .insert("API_KEY".to_string(), "secret".to_string());
        process.pull_secret_hash = Some("top-secret".to_string());

        let redacted = process.redacted_for_transport();

        assert!(redacted.env.is_empty());
        assert_eq!(redacted.pull_secret_hash.as_deref(), Some("<redacted>"));
        assert_eq!(redacted.config_fingerprint, process.config_fingerprint());
    }

    #[test]
    fn redacted_for_transport_preserves_existing_fingerprint_without_mutating_original() {
        let mut process = fixture_process();
        process.config_fingerprint = "precomputed".to_string();
        process
            .env
            .insert("TOKEN".to_string(), "secret".to_string());
        process.pull_secret_hash = Some("hashed-secret".to_string());

        let redacted = process.redacted_for_transport();

        assert_eq!(redacted.config_fingerprint, "precomputed");
        assert!(redacted.env.is_empty());
        assert_eq!(redacted.pull_secret_hash.as_deref(), Some("<redacted>"));

        assert_eq!(process.config_fingerprint, "precomputed");
        assert_eq!(process.env.get("TOKEN").map(String::as_str), Some("secret"));
        assert_eq!(process.pull_secret_hash.as_deref(), Some("hashed-secret"));
    }

    #[test]
    fn refresh_config_fingerprint_updates_stored_value() {
        let mut process = fixture_process();
        process.config_fingerprint = "stale".to_string();

        process.refresh_config_fingerprint();

        assert_eq!(process.config_fingerprint, process.config_fingerprint());
        assert_ne!(process.config_fingerprint, "stale");
    }

    /// A valid sampling interval: comfortably above `MIN_RATE_INTERVAL_MS`, and the
    /// daemon's nominal maintenance tick, so the arithmetic below reads like production.
    const TICK: Option<u64> = Some(2000);

    /// Drives one process through a seed sample and one real difference, which is the
    /// shortest path to a state where amounts and totals are both populated.
    fn sampled_process(pid: u32, read: u64, written: u64) -> ManagedProcess {
        let mut process = fixture_process();
        // First sighting of the pid: seeds state, publishes nothing.
        process.record_io_sample(read, written, pid, TICK);
        // Second sample differences against the first.
        process.record_io_sample(read, written, pid, TICK);
        process
    }

    #[test]
    fn the_first_sample_after_a_pid_appears_publishes_no_amount() {
        let mut process = fixture_process();
        // 4 GB: what an adopted process's lifetime figure can look like. Accumulating it
        // would credit the managed process with I/O from before oxmgr watched it.
        process.record_io_sample(4_000_000_000, 4_000_000_000, 1234, TICK);

        assert_eq!(process.disk_read_bytes, 0);
        assert_eq!(process.disk_write_bytes, 0);
        assert_eq!(process.disk_read_total, 0, "seed must not accumulate");
        assert_eq!(process.disk_write_total, 0, "seed must not accumulate");
        // Unavailable, not "a measured zero over a known period".
        assert_eq!(process.metrics_interval_ms, None);
        assert_eq!(process.disk_read_rate_bps(), None);
        assert_eq!(process.disk_write_rate_bps(), None);
    }

    #[test]
    fn a_pid_change_carries_the_total_forward_and_restarts_differencing() {
        let mut process = sampled_process(1234, 1_000, 2_000);
        let (read_before, write_before) = (process.disk_read_total, process.disk_write_total);
        assert_eq!((read_before, write_before), (1_000, 2_000));

        // Restart: new pid, and sysinfo's counters for it start from that pid's own
        // lifetime. This is the sample that used to make the total fall backwards.
        process.record_io_sample(50, 60, 4032, TICK);

        assert_eq!(process.disk_read_total, read_before, "carried forward");
        assert_eq!(process.disk_write_total, write_before, "carried forward");
        assert_eq!(process.metrics_pid, Some(4032));
        assert_eq!(
            process.metrics_interval_ms, None,
            "an interval spanning a pid change is not a measurement"
        );
    }

    #[test]
    fn the_total_is_non_decreasing_across_a_restart_and_grows_after_it() {
        let mut process = sampled_process(53658, 0, 33_353_728);
        let before = process.disk_write_total;

        // The measured regression from the proposal: pid 53658 write_total 33353728
        // -> pid 4032 write_total 1814528. A copied total would drop here.
        process.record_io_sample(0, 1_814_528, 4032, TICK);
        assert!(
            process.disk_write_total >= before,
            "total fell from {before} to {} across a restart",
            process.disk_write_total
        );

        // Further I/O on the new pid differences normally and adds to the lifetime figure.
        process.record_io_sample(0, 1_814_528 + 4_096, 4032, TICK);
        assert!(
            process.disk_write_total > before,
            "total did not grow after the restart"
        );
    }

    #[test]
    fn stopping_clears_live_readings_but_keeps_the_lifetime_total() {
        let mut process = sampled_process(1234, 8_192, 16_384);
        let (read_total, write_total) = (process.disk_read_total, process.disk_write_total);

        process.clear_resource_metrics();

        // Nothing that looks like a current measurement survives the pid.
        assert_eq!(process.disk_read_bytes, 0);
        assert_eq!(process.disk_write_bytes, 0);
        assert_eq!(process.metrics_interval_ms, None);
        assert_eq!(process.metrics_pid, None, "a dead pid must not linger");
        assert_eq!(process.disk_read_rate_bps(), None, "unavailable, not zero");
        assert_eq!(process.disk_write_rate_bps(), None);

        // The lifetime total is scoped to the managed process, not to the pid, and the
        // spec requires it to be non-decreasing across restarts — every restart passes
        // through this call, so clearing it here would reinstate the backwards jump.
        assert_eq!(process.disk_read_total, read_total);
        assert_eq!(process.disk_write_total, write_total);
    }

    #[test]
    fn a_stretched_interval_lowers_the_rate_it_reports() {
        // The maintenance tick uses MissedTickBehavior::Skip, so under load the interval
        // stretches. The same amount over a longer period is a lower rate, and the figure
        // has to follow the interval actually observed rather than the nominal 2s.
        let mut nominal = fixture_process();
        nominal.record_io_sample(0, 0, 1234, Some(2000));
        nominal.record_io_sample(2_000_000, 0, 1234, Some(2000));

        let mut stretched = fixture_process();
        stretched.record_io_sample(0, 0, 1234, Some(8000));
        stretched.record_io_sample(2_000_000, 0, 1234, Some(8000));

        assert_eq!(nominal.disk_read_rate_bps(), Some(1_000_000.0));
        assert_eq!(stretched.disk_read_rate_bps(), Some(250_000.0));
    }

    #[test]
    fn an_unusable_interval_yields_no_rate() {
        for interval in [None, Some(0), Some(MIN_RATE_INTERVAL_MS - 1)] {
            let mut process = fixture_process();
            process.record_io_sample(0, 0, 1234, interval);
            process.record_io_sample(4_096, 4_096, 1234, interval);
            assert_eq!(
                process.disk_read_rate_bps(),
                None,
                "interval {interval:?} must not produce a rate"
            );
        }
    }

    #[test]
    fn a_running_process_with_no_io_reports_zero_rather_than_unavailable() {
        // The distinction the whole tri-state exists for: this process was measured over a
        // valid interval and genuinely did nothing, which is not the same answer as
        // "the daemon cannot say".
        let process = sampled_process(1234, 0, 0);

        assert_eq!(process.metrics_interval_ms, TICK);
        assert_eq!(process.disk_read_rate_bps(), Some(0.0));
        assert_eq!(process.disk_write_rate_bps(), Some(0.0));
    }

    fn fixture_process() -> ManagedProcess {
        ManagedProcess {
            id: 42,
            name: "api".to_string(),
            command: "node".to_string(),
            args: vec!["server.js".to_string()],
            pre_reload_cmd: None,
            cwd: None,
            env: HashMap::new(),
            restart_policy: RestartPolicy::OnFailure,
            max_restarts: 10,
            restart_count: 0,
            crash_restart_limit: DEFAULT_CRASH_RESTART_LIMIT,
            auto_restart_history: Vec::new(),
            namespace: None,
            git_repo: None,
            git_ref: None,
            pull_secret_hash: None,
            reuse_port: false,
            stop_signal: Some("SIGTERM".to_string()),
            stop_timeout_secs: 5,
            restart_delay_secs: 0,
            restart_backoff_cap_secs: 0,
            restart_backoff_reset_secs: 0,
            restart_backoff_attempt: 0,
            start_delay_secs: 0,
            watch: false,
            watch_paths: Vec::new(),
            ignore_watch: Vec::new(),
            watch_delay_secs: 0,
            cluster_mode: false,
            cluster_instances: None,
            resource_limits: None,
            cgroup_path: None,
            pid: Some(1234),
            status: ProcessStatus::Running,
            desired_state: DesiredState::Running,
            last_exit_code: None,
            stdout_log: PathBuf::from("/tmp/api.out.log"),
            stderr_log: PathBuf::from("/tmp/api.err.log"),
            health_check: None,
            health_status: HealthStatus::Unknown,
            health_failures: 0,
            last_health_check: None,
            next_health_check: None,
            last_health_error: None,
            wait_ready: false,
            ready_timeout_secs: default_ready_timeout_secs(),
            cpu_percent: 0.0,
            memory_bytes: 0,
            disk_read_bytes: 0,
            disk_write_bytes: 0,
            metrics_interval_ms: None,
            metrics_pid: None,
            disk_read_total: 0,
            disk_write_total: 0,
            last_metrics_at: None,
            last_started_at: None,
            last_stopped_at: None,
            config_fingerprint: String::new(),
            log_date_format: Some("%Y-%m-%d %H:%M:%S".to_string()),
            unified_logs: false,
            cron_restart: None,
            next_cron_restart: None,
            last_error: None,
            depends_on: Vec::new(),
        }
    }

    fn fixture_start_spec() -> StartProcessSpec {
        StartProcessSpec {
            command: "node server.js".to_string(),
            name: Some("api".to_string()),
            pre_reload_cmd: None,
            restart_policy: RestartPolicy::OnFailure,
            max_restarts: 10,
            crash_restart_limit: DEFAULT_CRASH_RESTART_LIMIT,
            cwd: Some(PathBuf::from("/srv/api")),
            env: HashMap::new(),
            health_check: None,
            stop_signal: Some("SIGTERM".to_string()),
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
            resource_limits: None,
            git_repo: None,
            git_ref: None,
            pull_secret_hash: None,
            reuse_port: false,
            wait_ready: false,
            ready_timeout_secs: default_ready_timeout_secs(),
            log_date_format: None,
            unified_logs: false,
            cron_restart: None,
            stdout_log_override: None,
            stderr_log_override: None,
            depends_on: Vec::new(),
        }
    }
}
