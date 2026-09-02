//! Integration tests for the process manager. Lint-level cleanup: test fixtures
//! use bounded fake values; casts are exact for the values under test.

use oxmgr_core::numeric::u64_to_f64;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc::unbounded_channel;
use tokio::time::Instant as TokioInstant;

#[cfg(unix)]
use super::graceful_wait_before_force_kill;
use super::{
    CRASH_RESTART_WINDOW_SECS, ProcessManager, args_match_expected, compute_restart_delay_secs,
    constant_time_eq, crash_loop_limit_reached, maybe_reset_backoff_attempt, now_epoch_secs,
    process_exists, program_matches_expected, resolve_spawn_program, sha256_hex, short_commit,
    watch_fingerprint_for_dir, watch_fingerprint_for_roots,
};
use oxmgr_metrics::process::{
    DesiredState, HealthCheck, HealthStatus, ManagedProcess, ProcessExitEvent, ProcessStatus,
    RestartPolicy, StartProcessSpec,
};

mod cron;
mod forwarding;
mod git;
mod lifecycle;
mod restart;
mod spawn;
mod stop_delete_all;
mod watch;

fn fixture_process() -> ManagedProcess {
    ManagedProcess {
        id: 1,
        name: "api".to_string(),
        command: "node".to_string(),
        args: vec!["server.js".to_string()],
        pre_reload_cmd: None,
        cwd: None,
        env: HashMap::new(),
        restart_policy: RestartPolicy::OnFailure,
        max_restarts: 10,
        restart_count: 0,
        crash_restart_limit: 3,
        auto_restart_history: Vec::new(),
        namespace: None,
        git_repo: None,
        git_ref: None,
        pull_secret_hash: None,
        reuse_port: false,
        stop_signal: Some("SIGTERM".to_string()),
        stop_timeout_secs: 5,
        restart_delay_secs: 1,
        restart_backoff_cap_secs: 300,
        restart_backoff_reset_secs: 60,
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
        stdout_log: PathBuf::from("/tmp/out.log"),
        stderr_log: PathBuf::from("/tmp/err.log"),
        health_check: None,
        health_status: HealthStatus::Unknown,
        health_failures: 0,
        last_health_check: None,
        next_health_check: None,
        last_health_error: None,
        wait_ready: false,
        ready_timeout_secs: oxmgr_metrics::process::default_ready_timeout_secs(),
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
        log_date_format: Some("%Y-%m-%d %H:%M:%S".to_string()),
        unified_logs: false,
        cron_restart: None,
        next_cron_restart: None,
        last_error: None,
        depends_on: Vec::new(),
    }
}

fn spawnable_fixture_process() -> ManagedProcess {
    let mut process = fixture_process();
    process.command = std::env::current_exe()
        .expect("failed to resolve current test executable")
        .display()
        .to_string();
    process.args = vec!["--help".to_string()];
    process
}

fn long_running_fixture_process() -> ManagedProcess {
    let mut process = fixture_process();
    #[cfg(windows)]
    {
        // Keep the fixture alive long enough for parallel CI runs on slower
        // Windows runners to complete reload/crash assertions reliably.
        process.command = "powershell".to_string();
        process.args = vec![
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "Start-Sleep -Seconds 30".to_string(),
        ];
    }
    #[cfg(not(windows))]
    {
        // Keep the fixture alive long enough for parallel CI runs to
        // finish the assertion phase before the process exits naturally.
        process.command = "sh".to_string();
        process.args = vec!["-c".to_string(), "sleep 30".to_string()];
    }
    process
}

fn empty_manager(prefix: &str) -> ProcessManager {
    manager_with_config(test_config(prefix))
}

/// Builds a manager over an existing config, so a second call with the same config reads the same
/// `state.json` and baseline store — which is what "across a restart" means for these tests.
fn manager_with_config(config: crate::process_manager::ManagerConfig) -> ProcessManager {
    let (exit_tx, _exit_rx) = unbounded_channel();
    ProcessManager::new(config, exit_tx, None).expect("failed to create test process manager")
}

/// A manager with a fixed container ceiling, so forecast tests can drive 2.4 deterministically
/// without depending on the host this suite runs on.
fn manager_with_ceiling(
    config: crate::process_manager::ManagerConfig,
    memory_ceiling: Option<u64>,
) -> ProcessManager {
    let (exit_tx, _exit_rx) = unbounded_channel();
    ProcessManager::new(config, exit_tx, memory_ceiling)
        .expect("failed to create test process manager")
}

fn test_config(prefix: &str) -> crate::process_manager::ManagerConfig {
    let base = temp_watch_dir(prefix);
    let log_dir = base.join("logs");
    fs::create_dir_all(&log_dir).expect("failed to create test log directory");
    crate::process_manager::ManagerConfig {
        base_dir: base.clone(),
        state_path: base.join("state.json"),
        log_dir,
        log_rotation: crate::logging::LogRotationPolicy {
            max_size_bytes: 1024 * 1024,
            max_files: 2,
            max_age_days: 1,
            max_age_secs: None,
        },
    }
}

struct GitFixture {
    root: PathBuf,
    remote_dir: PathBuf,
    source_dir: PathBuf,
    clone_dir: PathBuf,
}

fn setup_git_fixture(prefix: &str) -> GitFixture {
    let root = temp_watch_dir(prefix);
    let remote_dir = root.join("remote.git");
    let source_dir = root.join("source");
    let clone_dir = root.join("clone");

    fs::create_dir_all(&root).expect("failed to create git fixture root");
    fs::create_dir_all(&source_dir).expect("failed to create git source dir");
    run_git_sync(
        &root,
        &["init", "--bare", remote_dir.to_str().unwrap_or_default()],
    );
    run_git_sync(&source_dir, &["init"]);
    run_git_sync(&source_dir, &["config", "user.email", "tests@oxmgr.local"]);
    run_git_sync(&source_dir, &["config", "user.name", "Oxmgr Tests"]);
    fs::write(source_dir.join("app.js"), "console.log('v1');\n")
        .expect("failed to write initial source file");
    run_git_sync(&source_dir, &["add", "."]);
    run_git_sync(&source_dir, &["commit", "-m", "initial"]);
    run_git_sync(&source_dir, &["branch", "-M", "main"]);
    run_git_sync(
        &source_dir,
        &[
            "remote",
            "add",
            "origin",
            remote_dir.to_str().unwrap_or_default(),
        ],
    );
    run_git_sync(&source_dir, &["push", "-u", "origin", "main"]);
    run_git_sync(
        &root,
        &[
            "clone",
            remote_dir.to_str().unwrap_or_default(),
            clone_dir.to_str().unwrap_or_default(),
        ],
    );
    run_git_sync(&clone_dir, &["checkout", "main"]);

    GitFixture {
        root,
        remote_dir,
        source_dir,
        clone_dir,
    }
}

fn write_commit_and_push(source_dir: &Path, file_name: &str, content: &str, message: &str) {
    fs::write(source_dir.join(file_name), content).expect("failed writing updated source file");
    run_git_sync(source_dir, &["add", "."]);
    run_git_sync(source_dir, &["commit", "-m", message]);
    run_git_sync(source_dir, &["push", "origin", "main"]);
}

fn git_head(repo_dir: &Path) -> String {
    let output = StdCommand::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(repo_dir)
        .output()
        .expect("failed running git rev-parse");
    assert!(
        output.status.success(),
        "git rev-parse failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn run_git_sync(cwd: &Path, args: &[&str]) {
    let output = StdCommand::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("failed to launch git in test");
    assert!(
        output.status.success(),
        "git {:?} failed in {}: {}",
        args,
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn cleanup_git_fixture(fixture: GitFixture) {
    let _ = fs::remove_dir_all(fixture.root);
}

fn temp_watch_dir(prefix: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock failure")
        .as_nanos();
    std::env::temp_dir().join(format!("oxmgr-{prefix}-{nonce}"))
}

fn command_line(program: &str, args: &[String]) -> String {
    let mut parts = vec![shell_words::quote(program).to_string()];
    for arg in args {
        parts.push(shell_words::quote(arg).to_string());
    }
    parts.join(" ")
}

#[cfg(windows)]
fn failing_readiness_check_command() -> String {
    "powershell -NoProfile -Command \"exit 1\"".to_string()
}

#[cfg(not(windows))]
fn failing_readiness_check_command() -> String {
    "sh -c 'exit 1'".to_string()
}

#[cfg(windows)]
fn successful_readiness_check_command() -> String {
    "powershell -NoProfile -Command \"exit 0\"".to_string()
}

#[cfg(not(windows))]
fn successful_readiness_check_command() -> String {
    "sh -c 'exit 0'".to_string()
}

fn wait_for_process_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !process_exists(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !process_exists(pid)
}

/// Integration of the scaffolded analysis modules with the manager's own sampling path.
///
/// The modules are unit-tested in isolation; these tests answer the different question of whether
/// the daemon actually feeds them. A module with passing unit tests that nothing calls is not a
/// working feature, and that gap is exactly what an integration step closes.
mod intelligence {
    use super::*;
    use oxmgr_analytics::metrics_history::MetricKind;
    use oxmgr_core::events::{BusEvent, EventProcessInfo};

    /// A running process with a pid, so `refresh_resource_metrics` treats it as sampleable.
    fn running_process(name: &str, id: u64, pid: u32) -> ManagedProcess {
        let mut process = fixture_process();
        process.id = id;
        process.name = name.to_string();
        process.status = ProcessStatus::Running;
        process.pid = Some(pid);
        process
    }

    #[test]
    fn history_is_recorded_from_the_existing_sampling_path() {
        let mut manager = empty_manager("history-from-sampling");
        // This test's own pid: sysinfo will find it, so the sample is a real reading rather than
        // the absent case. Using a made-up pid would exercise the `clear_resource_metrics` branch
        // and prove nothing about recording.
        let pid = std::process::id();
        manager
            .processes
            .insert("api".to_string(), running_process("api", 1, pid));

        assert!(
            manager.metric_history.history("api").is_none(),
            "no history before the first sample"
        );

        // Two refreshes: the first seeds the interval, the second has one to difference against.
        manager.refresh_resource_metrics();
        manager.refresh_resource_metrics();

        let history = manager
            .metric_history
            .history("api")
            .expect("sampling must record history");
        assert!(
            history.total_samples() >= 2,
            "expected both refreshes recorded, got {}",
            history.total_samples()
        );

        // And the samples are queryable, which is the point of retaining them.
        let series = history
            .query(MetricKind::Memory, 0, u64::MAX)
            .expect("memory is a known metric");
        assert!(
            !series.points.is_empty(),
            "recorded samples must be queryable, got an empty series"
        );
    }

    #[test]
    fn a_stopped_process_records_no_history() {
        let mut manager = empty_manager("history-stopped");
        let mut process = running_process("api", 1, std::process::id());
        process.status = ProcessStatus::Stopped;
        manager.processes.insert("api".to_string(), process);

        manager.refresh_resource_metrics();
        manager.refresh_resource_metrics();

        // A stopped process is not sampled at all, so there is nothing to retain. Recording a zero
        // here would be the "absent is not zero" failure the whole module guards against.
        assert!(
            manager.metric_history.history("api").is_none(),
            "a process that is not running must not accumulate history"
        );
    }

    #[test]
    fn history_is_kept_per_process() {
        let mut manager = empty_manager("history-per-process");
        let pid = std::process::id();
        manager
            .processes
            .insert("api".to_string(), running_process("api", 1, pid));
        manager
            .processes
            .insert("worker".to_string(), running_process("worker", 2, pid));

        manager.refresh_resource_metrics();
        manager.refresh_resource_metrics();

        for name in ["api", "worker"] {
            assert!(
                manager.metric_history.history(name).is_some(),
                "{name} has no history of its own"
            );
        }
        assert_eq!(
            manager.metric_history.len(),
            2,
            "one history per process, not one shared"
        );
    }

    #[test]
    fn a_repeated_pid_does_not_wipe_every_process_metric() {
        // Regression guard for a real defect found while writing these tests, not a hypothetical.
        //
        // `refresh_processes(ProcessesToUpdate::Some(&pids))` returns NOTHING for ANY pid when the
        // slice contains a duplicate. Measured against sysinfo 0.39: refreshing `[pid]` finds the
        // process, refreshing `[pid, pid]` finds nothing at all. The pid slice is built from
        // process records without deduplication, so two records holding the same pid — pid reuse
        // across a stale record, say — silently cleared the metrics of EVERY managed process on
        // that tick, each reported as "running, no reading".
        //
        // Two processes deliberately share one pid here. Both must still get a reading.
        let mut manager = empty_manager("history-duplicate-pid");
        let pid = std::process::id();
        manager
            .processes
            .insert("api".to_string(), running_process("api", 1, pid));
        manager
            .processes
            .insert("worker".to_string(), running_process("worker", 2, pid));

        manager.refresh_resource_metrics();
        manager.refresh_resource_metrics();

        for name in ["api", "worker"] {
            let process = manager.processes.get(name).expect("present");
            // `memory_bytes`, not `metrics_interval_ms`: two refreshes back to back elapse ~0ms and
            // the interval is filtered on `> 0`, so `None` there is correct and asserting on it
            // proved nothing. Memory is the honest signal — `clear_resource_metrics` zeroes it, so
            // a non-zero value means sysinfo actually returned this process.
            assert!(
                process.memory_bytes > 0,
                "{name} got no reading: a duplicate pid wiped the refresh"
            );
            assert!(
                manager.metric_history.history(name).is_some(),
                "{name} recorded no history"
            );
        }
    }

    #[tokio::test]
    async fn deleting_a_process_releases_its_history() {
        let mut manager = empty_manager("history-released-on-delete");
        // NOT this test's own pid. The other tests here use it so sysinfo finds a real process, but
        // `delete_process` sends a real termination signal — pointing it at the test runner killed
        // the whole suite with SIGTERM. History is seeded directly instead, which is what this test
        // is actually about.
        manager
            .processes
            .insert("api".to_string(), running_process("api", 1, 999_001));
        manager
            .processes
            .insert("worker".to_string(), running_process("worker", 2, 999_002));

        let sample = oxmgr_analytics::metrics_history::MetricSample::cpu_memory(1_000, 1.0, 1_024);
        assert!(manager.metric_history.record("api", sample));
        assert!(manager.metric_history.record("worker", sample));
        assert_eq!(manager.metric_history.len(), 2);

        manager
            .delete_process("api")
            .await
            .expect("delete should succeed");

        // Gone, so a later process reusing the name cannot inherit a stranger's history.
        assert!(
            manager.metric_history.history("api").is_none(),
            "deleted process kept its history"
        );
        // And the other process is untouched: release is scoped to one name.
        assert!(
            manager.metric_history.history("worker").is_some(),
            "deleting one process must not clear another's history"
        );
    }

    /// Retained lifecycle event count for one process.
    fn retained_events(manager: &ProcessManager, name: &str) -> usize {
        manager
            .with_event_history(|history| history.events_for(name).count())
            .expect("retention lock must not be poisoned")
    }

    #[test]
    fn events_are_retained_from_the_existing_publication_point() {
        // The point of this test is the SEAM, not the ring — `event_retention` has its own 16
        // tests for capacity and ordering. What is asserted here is that publishing through the
        // manager's own `emit` reaches retention, so retention cannot be fed by a parallel path
        // that a later event would bypass.
        let manager = empty_manager("events-from-emit");
        let process = running_process("api", 1, 999_101);

        assert_eq!(
            retained_events(&manager, "api"),
            0,
            "no events retained before anything is published"
        );

        manager.emit(BusEvent::process_started(EventProcessInfo::from(&process)));
        manager.emit(BusEvent::process_online(EventProcessInfo::from(&process)));

        assert_eq!(
            retained_events(&manager, "api"),
            2,
            "events published through emit must be retained"
        );
    }

    #[test]
    fn events_are_retained_with_no_subscriber_present() {
        // `empty_manager` holds the sender but nothing calls `subscribe`, so `event_tx.send`
        // returns `Err(SendError)` — no receivers. Retention must not depend on that: an
        // unobserved daemon has to retain exactly what an observed one does, or the history is
        // empty precisely when nobody was watching and it is needed most.
        let manager = empty_manager("events-no-subscriber");
        let process = running_process("api", 1, 999_102);

        manager.emit(BusEvent::process_crashed(
            EventProcessInfo::from(&process),
            Some(1),
            None,
            5,
            1,
            Vec::new(),
        ));

        assert_eq!(
            retained_events(&manager, "api"),
            1,
            "retention must not depend on a subscriber being attached"
        );
    }

    #[test]
    fn log_lines_are_published_but_not_retained() {
        // Log output travels the same bus, at one event per line. Retaining it would blow the ring
        // in seconds and evict the lifecycle events that failure analysis actually reads, so the
        // filter lives in `record_bus_event` and is asserted here at the seam.
        let manager = empty_manager("events-log-not-retained");
        let process = running_process("api", 1, 999_103);

        manager.emit(BusEvent::log_out(
            EventProcessInfo::from(&process),
            "listening on 8080".to_string(),
        ));
        manager.emit(BusEvent::log_err(
            EventProcessInfo::from(&process),
            "warn: slow query".to_string(),
        ));

        assert_eq!(
            retained_events(&manager, "api"),
            0,
            "log lines must not consume lifecycle retention"
        );

        // And a lifecycle event on the same process still lands, so the filter is by event kind
        // rather than the process having been excluded outright.
        manager.emit(BusEvent::process_stopped(EventProcessInfo::from(&process)));
        assert_eq!(
            retained_events(&manager, "api"),
            1,
            "lifecycle events must still be retained after log lines are filtered"
        );
    }

    /// Seeds an established-enough baseline set for one process.
    ///
    /// One observation per metric is enough for these tests: what is being asserted is that the
    /// store round-trips and that the fingerprint gate is applied, not how a baseline warms up —
    /// `src/baseline.rs` owns that with its own tests.
    fn seed_baselines(manager: &mut ProcessManager, name: &str, fingerprint: &str) {
        let mut baselines = oxmgr_analytics::baseline::ProcessBaselines::new(fingerprint);
        baselines.observe(oxmgr_analytics::baseline::Metric::CpuPercent, 12.5);
        baselines.observe(
            oxmgr_analytics::baseline::Metric::MemoryBytes,
            64.0 * 1024.0 * 1024.0,
        );
        manager.baselines.insert(name.to_string(), baselines);
    }

    #[test]
    fn baselines_are_restored_across_a_restart() {
        let config = test_config("baselines-restart");
        let mut manager = manager_with_config(config.clone());

        let mut process = running_process("api", 1, 999_201);
        process.refresh_config_fingerprint();
        let fingerprint = process.config_fingerprint.clone();
        manager.processes.insert("api".to_string(), process);
        manager.save().expect("state must persist");

        seed_baselines(&mut manager, "api", &fingerprint);
        manager.save_baselines_now();

        // A second manager over the SAME config is the restart: it re-reads state.json and the
        // baseline store from disk, with nothing carried over in memory.
        let restarted = manager_with_config(config);

        let restored = restarted
            .baselines
            .get("api")
            .expect("an unchanged process must keep its baselines across a restart");
        assert_eq!(
            restored.config_fingerprint(),
            fingerprint,
            "restored baselines must belong to the fingerprint they were learned under"
        );
        assert_eq!(
            restored.snapshots().len(),
            2,
            "both observed metrics must come back"
        );
    }

    #[test]
    fn a_reconfigured_process_does_not_restore_its_baselines() {
        // The other half of the restart story: a restart keeps the baseline, a reconfiguration must
        // not. Otherwise a changed binary is measured against the old one's normal, which is not a
        // finding about the process — it is a finding about the edit.
        let config = test_config("baselines-reconfigured");
        let mut manager = manager_with_config(config.clone());

        let mut process = running_process("api", 1, 999_202);
        process.refresh_config_fingerprint();
        manager.processes.insert("api".to_string(), process);
        manager.save().expect("state must persist");

        // Learned under a fingerprint that is deliberately not the process's current one.
        seed_baselines(&mut manager, "api", "fingerprint-from-a-different-config");
        manager.save_baselines_now();

        let restarted = manager_with_config(config);

        assert!(
            !restarted.baselines.contains_key("api"),
            "baselines learned under a different configuration must be discarded"
        );
    }

    #[test]
    fn a_corrupt_baseline_store_still_starts_and_recovers_processes() {
        // The reason baselines live in their own file. A statistics cache must never be able to
        // stop process recovery, so this asserts the process comes back even though the baseline
        // file is unparseable — not merely that loading returned a default.
        let config = test_config("baselines-corrupt");
        let mut manager = manager_with_config(config.clone());
        let mut process = running_process("api", 1, 999_203);
        process.refresh_config_fingerprint();
        manager.processes.insert("api".to_string(), process);
        manager.save().expect("state must persist");

        let store_path = crate::storage::baseline_store_path(&config.state_path);
        fs::write(&store_path, b"{ this is not json at all")
            .expect("failed to write corrupt store");

        let restarted = manager_with_config(config);

        assert!(
            restarted.processes.contains_key("api"),
            "a corrupt baseline store must not block process recovery"
        );
        assert!(
            !restarted.baselines.contains_key("api"),
            "an unreadable baseline store must be equivalent to absent state"
        );
        // Moved aside rather than deleted, so the bad payload can still be inspected.
        assert!(
            !store_path.exists(),
            "the corrupt store should have been moved out of the way"
        );
    }

    #[test]
    fn a_process_deleted_while_the_daemon_was_down_is_not_restored() {
        // Deletion drops the persisted entry, but a delete can also happen by the state file being
        // rewritten without a process — a config apply, say. The restore path must not resurrect
        // baselines for a name that is no longer managed.
        let config = test_config("baselines-deleted-offline");
        let mut manager = manager_with_config(config.clone());

        let mut kept = running_process("api", 1, 999_204);
        kept.refresh_config_fingerprint();
        let kept_fingerprint = kept.config_fingerprint.clone();
        let mut gone = running_process("worker", 2, 999_205);
        gone.refresh_config_fingerprint();
        let gone_fingerprint = gone.config_fingerprint.clone();

        manager.processes.insert("api".to_string(), kept);
        manager.processes.insert("worker".to_string(), gone);
        seed_baselines(&mut manager, "api", &kept_fingerprint);
        seed_baselines(&mut manager, "worker", &gone_fingerprint);
        manager.save_baselines_now();

        // Only `api` survives in state, while the baseline store still holds both.
        manager.processes.remove("worker");
        manager.save().expect("state must persist");

        let restarted = manager_with_config(config);

        assert!(
            restarted.baselines.contains_key("api"),
            "a still-managed process must keep its baselines"
        );
        assert!(
            !restarted.baselines.contains_key("worker"),
            "baselines must not be restored for a process that is no longer managed"
        );
    }

    #[test]
    fn declared_dependencies_survive_a_daemon_restart() {
        // The spec asks for the declaration to survive a restart without the original config file
        // still being present, so this asserts against a rebuilt manager rather than the live one.
        let config = test_config("deps-restart");
        let mut manager = manager_with_config(config.clone());

        let mut process = running_process("api", 1, 999_301);
        process.depends_on = vec!["db".to_string(), "cache".to_string()];
        manager.processes.insert("api".to_string(), process);
        manager.save().expect("state must persist");

        let restarted = manager_with_config(config);

        let recovered = restarted
            .processes
            .get("api")
            .expect("process must recover");
        assert_eq!(
            recovered.depends_on,
            vec!["db".to_string(), "cache".to_string()],
            "declared dependencies must survive a restart, in declaration order"
        );
    }

    #[test]
    fn a_process_without_dependencies_reports_none() {
        let mut manager = empty_manager("deps-none");
        manager
            .processes
            .insert("api".to_string(), running_process("api", 1, 999_302));

        let declared = manager
            .declared_dependencies("api")
            .expect("a managed process must report a dependency set");
        assert!(
            declared.is_empty(),
            "a process declaring nothing must report an empty set, got {declared:?}"
        );
        // And an unmanaged name reports nothing at all, which is a different fact from "declares
        // none" — hence Option rather than an empty vector for both.
        assert!(manager.declared_dependencies("ghost").is_none());
    }

    #[test]
    fn an_unmanaged_dependency_is_retained_but_marked_unresolved() {
        let mut manager = empty_manager("deps-unresolved");
        let mut process = running_process("api", 1, 999_303);
        process.depends_on = vec!["db".to_string(), "not-managed".to_string()];
        manager.processes.insert("api".to_string(), process);
        manager
            .processes
            .insert("db".to_string(), running_process("db", 2, 999_304));

        let declared = manager
            .declared_dependencies("api")
            .expect("managed process reports its dependencies");

        assert_eq!(declared.len(), 2, "the unresolved declaration is retained");
        assert_eq!(declared[0].name, "db");
        assert!(declared[0].resolved, "a managed dependency resolves");
        assert_eq!(declared[1].name, "not-managed");
        assert!(
            !declared[1].resolved,
            "a dependency naming nothing managed must be marked unresolved, not dropped"
        );

        // The graph keeps the unresolved edge too: correlation walks it and finds no failures,
        // which is the correct answer. Dropping it here would narrow the graph silently.
        let graph = manager.dependency_graph();
        assert_eq!(
            graph.get("api").map(Vec::as_slice),
            Some(["db".to_string(), "not-managed".to_string()].as_slice()),
        );
        // A process declaring nothing contributes no entry, so "no edges" and "an empty edge list"
        // do not both have to be handled downstream.
        assert!(!graph.contains_key("db"));
    }

    #[test]
    fn save_state_cost_does_not_grow_with_retention() {
        // Asserted structurally rather than by timing, because the structural property is the one
        // that actually matters and a wall-clock assertion would be flaky on CI anyway.
        //
        // `save_state` serialises `PersistedState` — next_id plus the process records. Retention
        // (metric history, the event ring, baselines) is held in separate fields and, for
        // baselines, a separate file. So the claim is not "the write is fast", it is "retention is
        // not in the payload at all", and the way to prove that is byte-for-byte: fill retention
        // hard and the state file must not move by a single byte.
        let config = test_config("save-cost-retention");
        let mut manager = manager_with_config(config.clone());

        let mut process = running_process("api", 1, 999_208);
        process.refresh_config_fingerprint();
        let fingerprint = process.config_fingerprint.clone();
        manager.processes.insert("api".to_string(), process.clone());

        manager.save().expect("state must persist");
        let empty_retention_bytes = fs::metadata(&config.state_path)
            .expect("state file must exist")
            .len();

        // Fill every retention structure well past a trivial amount: 2,000 metric samples, 500
        // lifecycle events (past the 128-event ring cap, so it has wrapped), and a baseline set.
        for tick in 0..2_000u64 {
            manager.metric_history.record(
                "api",
                oxmgr_analytics::metrics_history::MetricSample::cpu_memory(
                    tick * 1_000,
                    1.5,
                    2_048,
                ),
            );
        }
        for _ in 0..500 {
            manager.emit(BusEvent::process_restarting(
                EventProcessInfo::from(&process),
                Some(1),
                None,
                5,
                1,
                0,
            ));
        }
        seed_baselines(&mut manager, "api", &fingerprint);

        // Retention really is populated, so the comparison below is not vacuous.
        assert!(manager.metric_history.history("api").is_some());
        assert_eq!(
            retained_events(&manager, "api"),
            oxmgr_store::event_retention::DEFAULT_PER_PROCESS_CAPACITY,
            "the event ring should be full, having wrapped"
        );

        manager.save().expect("state must persist");
        let full_retention_bytes = fs::metadata(&config.state_path)
            .expect("state file must exist")
            .len();

        assert_eq!(
            empty_retention_bytes, full_retention_bytes,
            "state.json changed size as retention grew: retention has leaked into the recovery payload"
        );
    }

    #[tokio::test]
    async fn deleting_a_process_removes_its_persisted_baselines() {
        let config = test_config("baselines-deleted");
        let mut manager = manager_with_config(config.clone());

        for (name, id, pid) in [("api", 1, 999_206), ("worker", 2, 999_207)] {
            let mut process = running_process(name, id, pid);
            process.refresh_config_fingerprint();
            let fingerprint = process.config_fingerprint.clone();
            manager.processes.insert(name.to_string(), process);
            seed_baselines(&mut manager, name, &fingerprint);
        }
        manager.save().expect("state must persist");
        manager.save_baselines_now();

        manager
            .delete_process("api")
            .await
            .expect("delete should succeed");

        // Gone from memory and from disk: leaving the persisted copy would let a later process
        // reusing the name inherit a stranger's notion of normal on the next restart.
        assert!(!manager.baselines.contains_key("api"));
        let persisted = crate::storage::load_baselines(&crate::storage::baseline_store_path(
            &config.state_path,
        ));
        assert!(
            !persisted.processes.contains_key("api"),
            "a deleted process must not keep persisted baselines"
        );
        assert!(
            persisted.processes.contains_key("worker"),
            "deleting one process must not drop another's persisted baselines"
        );
    }

    #[tokio::test]
    async fn deleting_a_process_releases_its_retained_events() {
        // Same rule as metric history, and it needs its own assertion because it is a second
        // release path: wiring only one of the two delete paths is exactly the defect the metric
        // history comment above records.
        let mut manager = empty_manager("events-released-on-delete");
        for (name, id, pid) in [("api", 1, 999_104), ("worker", 2, 999_105)] {
            let process = running_process(name, id, pid);
            manager.processes.insert(name.to_string(), process.clone());
            manager.emit(BusEvent::process_started(EventProcessInfo::from(&process)));
        }
        assert_eq!(retained_events(&manager, "api"), 1);
        assert_eq!(retained_events(&manager, "worker"), 1);

        manager
            .delete_process("api")
            .await
            .expect("delete should succeed");

        assert_eq!(
            retained_events(&manager, "api"),
            0,
            "a deleted process must not keep its event history"
        );
        assert_eq!(
            retained_events(&manager, "worker"),
            1,
            "deleting one process must not clear another's events"
        );
    }

    // ── Analysis engine on the maintenance tick (8.6, 8.7) ──────────────────────────────────────

    /// Warms a process's baselines through the real observe path, so the warm-up gate and the
    /// spread floor behave exactly as they do in production.
    /// The memory baseline is warmed at the process's ACTUAL `memory_bytes`, so only CPU departs.
    ///
    /// This mattered: warming memory at an arbitrary centre while the fixture reported 0 bytes
    /// produced a second, entirely correct finding — memory departing BELOW its baseline — and the
    /// first version of these tests read that as a bug in the engine. It was a bug in the fixture.
    /// Anchoring memory to the reading under test isolates the metric the test is about.
    fn warm_baselines(manager: &mut ProcessManager, name: &str, centre: f64) {
        let memory = manager
            .processes
            .get(name)
            .map(|process| u64_to_f64(process.memory_bytes))
            .unwrap_or(0.0);
        let mut set = oxmgr_analytics::baseline::ProcessBaselines::new("fingerprint");
        for _ in 0..60 {
            set.observe(oxmgr_analytics::baseline::Metric::CpuPercent, centre);
            set.observe(oxmgr_analytics::baseline::Metric::MemoryBytes, memory);
        }
        manager.baselines.insert(name.to_string(), set);
    }

    #[test]
    fn analysis_runs_on_the_maintenance_tick_and_raises_from_real_readings() {
        // The end-to-end claim: a running daemon's own sampling path produces a finding. Every
        // piece of this was tested in isolation before; nothing had joined them to the tick.
        let mut manager = empty_manager("analysis-on-tick");
        let mut process = running_process("api", 1, std::process::id());
        process.cpu_percent = 95.0;
        manager.processes.insert("api".to_string(), process);
        warm_baselines(&mut manager, "api", 5.0);

        // Three cycles: `min_consecutive_samples` defaults to 3, so two would be `Building` — the
        // anti-spike suppression, not a finding.
        for _ in 0..3 {
            manager.run_analysis();
        }

        let active = manager.analysis.registry().active_for("api");
        assert_eq!(
            active.len(),
            1,
            "a sustained departure from a warm baseline must raise exactly one finding"
        );
        assert!(
            active[0].confidence_is_reproducible(),
            "the finding's score must follow from its evidence"
        );
    }

    #[test]
    fn a_container_ceiling_anchors_the_leak_forecast_below_the_configured_limit() {
        // 2.4. Inside a container the ceiling that actually stops growth is the cgroup's, not the
        // configured `max_memory_mb` — so a forecast must project toward the LOWER of the two, and
        // toward the ceiling alone when no limit is configured at all. Before this task, a missing
        // configured limit meant `memory_limit_bytes = None` and therefore NO ETA, even inside a
        // container about to run out of memory.
        //
        // The shape of the assertion: a 220 MB ceiling with a 512 MB configured limit must produce
        // a forecast whose threshold IS the ceiling. If the wiring regressed to the configured
        // limit alone, the threshold would read 512 MB — and the test would fail. That is what
        // makes this test bite: it does not observe the min() helper, it observes the anchor the
        // forecast was built on.
        let mut manager =
            manager_with_ceiling(test_config("forecast-ceiling"), Some(220 * 1024 * 1024));
        let mut process = running_process("api", 1, std::process::id());
        process.resource_limits = Some(oxmgr_metrics::process::ResourceLimits {
            max_memory_mb: Some(512),
            max_cpu_percent: None,
            cgroup_enforce: false,
            deny_gpu: false,
        });
        // Anchored BEFORE the baseline warms, so the memory baseline matches the starting reading
        // and only the CLIMB departs — a 60 MB first sample against a 0 MB baseline would raise a
        // legitimate level-departure finding beside the leak and muddy the assertion.
        process.memory_bytes = 60 * 1024 * 1024;
        manager.processes.insert("api".to_string(), process);
        warm_baselines(&mut manager, "api", 5.0);

        // 15 ticks at 60s spacing, +8 MB each: 900s of window (past the 600s gate), a slope of
        // 0.133 MB/s, and an arrival at the 220 MB ceiling ~360s out — comfortably inside the
        // 3x-window horizon. The trend detector fires on the 15th tick (`DEFAULT_TREND_EVERY`).
        let start_ms = now_epoch_secs() * 1_000;
        for tick in 0..15u64 {
            let at_ms = start_ms.saturating_sub((14 - tick) * 60_000);
            manager.metric_history.record(
                "api",
                oxmgr_analytics::metrics_history::MetricSample::cpu_memory(
                    at_ms,
                    10.0,
                    60u64 * 1024 * 1024 + 8 * 1024 * 1024 * tick,
                ),
            );
            manager
                .processes
                .get_mut("api")
                .expect("process exists")
                .memory_bytes = 60u64 * 1024 * 1024 + 8 * 1024 * 1024 * tick;
            manager.run_analysis();
        }

        let active = manager.analysis.registry().active_for("api");
        let leak = active
            .iter()
            .find(|f| f.key.detector == oxmgr_core::findings::Detector::ResourceLeak)
            .expect("a monotonic climb must raise a leak finding");
        let forecast = leak
            .evidence
            .forecast
            .as_ref()
            .expect("a ceiling must anchor a forecast even without a configured limit");
        assert!(
            (forecast.threshold_value - (220.0 * 1024.0 * 1024.0)).abs() < 1024.0,
            "the forecast must project toward the ENFORCED ceiling (220 MB), not the configured \
             512 MB, got {} bytes",
            forecast.threshold_value
        );
    }

    #[test]
    fn a_stopped_process_feeds_no_reading_to_the_baseline() {
        // `None`, not zero. A stopped process has no CPU reading, and recording 0.0 would drag its
        // baseline centre toward zero every tick it stayed stopped — so that when it restarted, its
        // normal load would read as a departure.
        let mut manager = empty_manager("analysis-stopped");
        let mut process = running_process("api", 1, 999_401);
        process.status = ProcessStatus::Stopped;
        process.cpu_percent = 0.0;
        manager.processes.insert("api".to_string(), process);

        let mut set = oxmgr_analytics::baseline::ProcessBaselines::new("fingerprint");
        for _ in 0..60 {
            set.observe(oxmgr_analytics::baseline::Metric::CpuPercent, 50.0);
        }
        let before = set
            .get(oxmgr_analytics::baseline::Metric::CpuPercent)
            .expect("seeded")
            .snapshot()
            .centre;
        manager.baselines.insert("api".to_string(), set);

        for _ in 0..10 {
            manager.run_analysis();
        }

        let after = manager
            .baselines
            .get("api")
            .and_then(|s| s.get(oxmgr_analytics::baseline::Metric::CpuPercent))
            .map(|b| b.snapshot().centre)
            .expect("baseline still present");
        assert_eq!(
            before, after,
            "a stopped process must not move its own baseline"
        );
    }

    #[tokio::test]
    async fn a_health_failure_reports_the_resource_findings_active_with_it() {
        // Task 8.6. A health failure caused by resource pressure and one caused by a broken
        // endpoint need different responses; the join is what lets an operator tell them apart
        // without correlating by hand against a dashboard that may have moved on.
        let mut manager = empty_manager("health-with-findings");
        let mut process = running_process("api", 1, std::process::id());
        process.cpu_percent = 95.0;
        // A command that always fails, so the health check reports unhealthy deterministically.
        process.health_check = Some(HealthCheck {
            #[cfg(windows)]
            command: "cmd /C exit 1".to_string(),
            #[cfg(not(windows))]
            command: "false".to_string(),
            interval_secs: 1,
            timeout_secs: 5,
            max_failures: 10,
        });
        process.next_health_check = Some(0);
        manager.processes.insert("api".to_string(), process);
        warm_baselines(&mut manager, "api", 5.0);

        for _ in 0..3 {
            manager.run_analysis();
        }
        assert_eq!(
            manager.analysis.registry().active_for("api").len(),
            1,
            "a finding must be active before the health check runs"
        );

        let mut events = manager.event_tx().subscribe();
        manager
            .run_health_checks()
            .await
            .expect("health checks should run");

        // Drain until the unhealthy event, which is the one carrying the correlation.
        let mut found = None;
        while let Ok(event) = events.try_recv() {
            if let BusEvent::HealthUnhealthy { data, .. } = event.as_ref() {
                found = Some(data.clone());
                break;
            }
        }
        let data = found.expect("a health:unhealthy event must have been published");
        assert_eq!(
            data.resource_findings.len(),
            1,
            "the active finding must travel with the failure, got {:?}",
            data.resource_findings
        );
        // Named by key, so a consumer can fetch the finding rather than re-deriving it.
        assert!(
            data.resource_findings[0].starts_with("api/level_departure/"),
            "the finding is named by its key, got {:?}",
            data.resource_findings[0]
        );
    }

    #[tokio::test]
    async fn a_health_failure_with_no_findings_reports_alone() {
        // The other half of 8.6, and the common case: nothing implicated, so nothing is claimed.
        // An empty list here is the answer to "was this resource pressure" — no.
        let mut manager = empty_manager("health-without-findings");
        let mut process = running_process("api", 1, std::process::id());
        process.health_check = Some(HealthCheck {
            #[cfg(windows)]
            command: "cmd /C exit 1".to_string(),
            #[cfg(not(windows))]
            command: "false".to_string(),
            interval_secs: 1,
            timeout_secs: 5,
            max_failures: 10,
        });
        process.next_health_check = Some(0);
        manager.processes.insert("api".to_string(), process);

        let mut events = manager.event_tx().subscribe();
        manager
            .run_health_checks()
            .await
            .expect("health checks should run");

        let mut found = None;
        while let Ok(event) = events.try_recv() {
            if let BusEvent::HealthUnhealthy { data, .. } = event.as_ref() {
                found = Some(data.clone());
                break;
            }
        }
        let data = found.expect("a health:unhealthy event must have been published");
        assert!(
            data.resource_findings.is_empty(),
            "no finding active means no correlation is claimed, got {:?}",
            data.resource_findings
        );
    }

    #[tokio::test]
    async fn health_check_behaviour_is_unchanged_by_analysis() {
        // Task 8.7. Analysis is an observer, and the way to show that is to assert the supervision
        // state it must not touch: the failure counter, the scheduled next check, and the recorded
        // error are exactly what they would be without it.
        let mut manager = empty_manager("health-unchanged");
        let mut process = running_process("api", 1, std::process::id());
        process.cpu_percent = 95.0;
        process.health_check = Some(HealthCheck {
            #[cfg(windows)]
            command: "cmd /C exit 1".to_string(),
            #[cfg(not(windows))]
            command: "false".to_string(),
            interval_secs: 7,
            timeout_secs: 5,
            max_failures: 10,
        });
        process.next_health_check = Some(0);
        manager.processes.insert("api".to_string(), process);
        warm_baselines(&mut manager, "api", 5.0);

        // Analysis active, with a finding raised.
        for _ in 0..3 {
            manager.run_analysis();
        }
        assert_eq!(manager.analysis.registry().active_for("api").len(), 1);

        manager
            .run_health_checks()
            .await
            .expect("health checks should run");

        let process = manager.processes.get("api").expect("present");
        assert_eq!(
            process.health_status,
            HealthStatus::Unhealthy,
            "the check still reports its real outcome"
        );
        assert_eq!(
            process.health_failures, 1,
            "the failure counter advances exactly once, unaffected by analysis"
        );
        assert!(
            process.last_health_error.is_some(),
            "the error is still recorded"
        );
        // And the next check is still scheduled from the configured interval, not perturbed.
        let next = process.next_health_check.expect("rescheduled");
        let last = process.last_health_check.expect("recorded");
        assert_eq!(
            next - last,
            7,
            "the configured interval still governs cadence"
        );
    }

    #[test]
    fn crash_loop_state_is_read_without_being_mutated_by_analysis() {
        // The read-only `crash_loop_limit_reached_at` exists so analysis cannot change supervision
        // state from a read path. Asserted by driving the tick and checking the history survives:
        // the restart path's pruning form would have emptied it.
        let mut manager = empty_manager("analysis-crash-loop");
        let mut process = running_process("api", 1, std::process::id());
        process.crash_restart_limit = 3;
        // Two recent restarts, inside the five-minute window.
        let now = crate::process_manager::restart::now_epoch_secs();
        process.auto_restart_history = vec![now.saturating_sub(10), now.saturating_sub(5)];
        manager.processes.insert("api".to_string(), process);
        warm_baselines(&mut manager, "api", 5.0);

        for _ in 0..3 {
            manager.run_analysis();
        }

        let process = manager.processes.get("api").expect("present");
        assert_eq!(
            process.auto_restart_history.len(),
            2,
            "analysis must not prune the restart history it reads"
        );
    }

    // ── Advisory dismissal (resource-awareness 3.3, 3.4) ────────────────────────────────────────

    /// A process whose configuration produces a critical advisory: crash-loop protection off plus
    /// always-restart with no delay.
    fn risky_process(name: &str, id: u64) -> ManagedProcess {
        let mut process = running_process(name, id, 999_500 + u32::try_from(id).unwrap_or(0));
        process.restart_policy = oxmgr_metrics::process::RestartPolicy::Always;
        process.crash_restart_limit = 0;
        process.restart_delay_secs = 0;
        process
    }

    #[test]
    fn a_dismissal_is_scoped_to_one_process() {
        // The load-bearing property. Two processes with the SAME risky configuration: dismissing the
        // advisory for one must leave it in force for the other, or the feature becomes a blanket
        // mute and an operator loses the warning on a process they never looked at.
        let mut manager = empty_manager("dismiss-scope");
        manager
            .processes
            .insert("api".to_string(), risky_process("api", 1));
        manager
            .processes
            .insert("worker".to_string(), risky_process("worker", 2));

        let added = manager
            .dismiss_advisory("api", "crash_loop_protection_disabled")
            .expect("a known rule on a known process is accepted");
        assert!(added, "the first dismissal is newly added");

        assert_eq!(
            manager.dismissed_advisories("api"),
            vec!["crash_loop_protection_disabled".to_string()]
        );
        assert!(
            manager.dismissed_advisories("worker").is_empty(),
            "dismissing for one process must not dismiss for another"
        );
    }

    #[test]
    fn a_dismissal_does_not_hide_a_different_advisory() {
        // The other half of the scoping requirement, and the one a naive "dismissed: true" flag on
        // the process would break: dismissing one RULE must leave every other rule reporting.
        let mut manager = empty_manager("dismiss-other-rule");
        manager
            .processes
            .insert("api".to_string(), risky_process("api", 1));

        manager
            .dismiss_advisory("api", "crash_loop_protection_disabled")
            .expect("accepted");

        let dismissed = manager.dismissed_advisories("api");
        assert_eq!(dismissed.len(), 1, "only the named rule is dismissed");
        assert!(!dismissed.contains(&"immediate_restart_loop".to_string()));

        // And the configuration still produces the other advisory, so it is available to report.
        let report = crate::advisories::evaluate(
            &crate::advisories::ProcessConfig::from(manager.processes.get("api").expect("present")),
            None,
        );
        assert!(
            report
                .advisories
                .iter()
                .any(|advisory| advisory.id == "immediate_restart_loop"),
            "the undismissed rule must still be produced by evaluation"
        );
    }

    #[test]
    fn an_unknown_rule_is_refused_rather_than_stored() {
        // A dismissal for a misspelled rule would look accepted and suppress nothing, and the
        // operator would find out the next time the real advisory fired — the worst moment.
        let mut manager = empty_manager("dismiss-unknown-rule");
        manager
            .processes
            .insert("api".to_string(), risky_process("api", 1));

        let error = manager
            .dismiss_advisory("api", "crash_loop_protection_disbaled")
            .expect_err("a misspelled rule must be refused");
        // The message names the valid ids, so the operator can fix it without reading the source.
        assert!(
            error.to_string().contains("crash_loop_protection_disabled"),
            "the refusal should list the valid rules, got: {error}"
        );
        assert!(
            manager.dismissed_advisories("api").is_empty(),
            "nothing may be stored for a rule that does not exist"
        );
    }

    #[test]
    fn an_unknown_process_is_refused() {
        // Same reasoning as the rule: silently accepting a dismissal for a typo'd process name reads
        // as success while doing nothing at all.
        let mut manager = empty_manager("dismiss-unknown-process");
        manager
            .processes
            .insert("api".to_string(), risky_process("api", 1));

        assert!(
            manager
                .dismiss_advisory("ghost", "crash_loop_protection_disabled")
                .is_err()
        );
    }

    #[test]
    fn dismissal_is_idempotent_and_reports_which_it_was() {
        let mut manager = empty_manager("dismiss-idempotent");
        manager
            .processes
            .insert("api".to_string(), risky_process("api", 1));

        assert!(
            manager
                .dismiss_advisory("api", "immediate_restart_loop")
                .expect("accepted")
        );
        // Second time reports false — "already was", not "done". A client showing a confirmation
        // should not claim it changed something it did not.
        assert!(
            !manager
                .dismiss_advisory("api", "immediate_restart_loop")
                .expect("accepted")
        );
        assert_eq!(manager.dismissed_advisories("api").len(), 1);
    }

    #[test]
    fn restoring_a_dismissal_brings_the_advisory_back() {
        let mut manager = empty_manager("dismiss-restore");
        manager
            .processes
            .insert("api".to_string(), risky_process("api", 1));

        manager
            .dismiss_advisory("api", "immediate_restart_loop")
            .expect("accepted");
        assert!(
            manager
                .restore_advisory("api", "immediate_restart_loop")
                .expect("accepted")
        );
        assert!(manager.dismissed_advisories("api").is_empty());
        // Idempotent in this direction too.
        assert!(
            !manager
                .restore_advisory("api", "immediate_restart_loop")
                .expect("accepted")
        );
        // Pruned rather than left as an empty set, so a process with no dismissals does not read as
        // "configured" in the map the endpoint serves.
        assert!(!manager.dismissal_map().contains_key("api"));
    }

    #[test]
    fn dismissals_survive_a_daemon_restart() {
        // A dismissal is an operator's statement that a configuration is deliberate, so it has to
        // outlive the daemon — otherwise every restart resurrects warnings someone already answered.
        let config = test_config("dismiss-restart");
        let mut manager = manager_with_config(config.clone());
        let mut process = risky_process("api", 1);
        process.refresh_config_fingerprint();
        manager.processes.insert("api".to_string(), process);
        manager.save().expect("state must persist");
        manager
            .dismiss_advisory("api", "crash_loop_protection_disabled")
            .expect("accepted");

        let restarted = manager_with_config(config);
        assert_eq!(
            restarted.dismissed_advisories("api"),
            vec!["crash_loop_protection_disabled".to_string()],
            "a dismissal must survive a restart"
        );
    }

    #[tokio::test]
    async fn deleting_a_process_drops_its_dismissals() {
        // A later process reusing the name must not inherit a suppression granted to a different
        // workload — the same rule metric history, baselines and findings follow.
        let mut manager = empty_manager("dismiss-delete");
        manager
            .processes
            .insert("api".to_string(), risky_process("api", 1));
        manager
            .processes
            .insert("worker".to_string(), risky_process("worker", 2));
        manager
            .dismiss_advisory("api", "immediate_restart_loop")
            .expect("accepted");
        manager
            .dismiss_advisory("worker", "immediate_restart_loop")
            .expect("accepted");

        manager
            .delete_process("api")
            .await
            .expect("delete should succeed");

        assert!(manager.dismissed_advisories("api").is_empty());
        assert_eq!(
            manager.dismissed_advisories("worker").len(),
            1,
            "deleting one process must not clear another's dismissals"
        );
    }
}
