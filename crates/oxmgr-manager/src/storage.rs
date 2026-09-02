//! Persistence helpers for Oxmgr daemon state.
//!
//! Lint-level cleanup: display-path casts in persistence.

use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

use oxmgr_analytics::baseline::PersistedProcessBaselines;
use oxmgr_metrics::process::ManagedProcess;

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Serializable daemon state stored on disk between launches.
pub struct PersistedState {
    pub next_id: u64,
    pub processes: Vec<ManagedProcess>,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            next_id: 1,
            processes: Vec::new(),
        }
    }
}

/// Loads the persisted daemon state, recovering gracefully from missing, empty,
/// or corrupted files.
pub fn load_state(path: &Path) -> Result<PersistedState> {
    if !path.exists() {
        return Ok(PersistedState::default());
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read state file {}", path.display()))?;

    if content.trim().is_empty() {
        return Ok(PersistedState::default());
    }

    match serde_json::from_str::<PersistedState>(&content) {
        Ok(state) => Ok(state),
        Err(error) => {
            let backup = corrupted_backup_path(path);
            if let Err(rename_err) = fs::rename(path, &backup) {
                warn!(
                    "failed to move corrupted state file {} -> {}: {rename_err}",
                    path.display(),
                    backup.display()
                );
            } else {
                warn!(
                    "state file {} is corrupted ({error}), moved to {}",
                    path.display(),
                    backup.display()
                );
            }
            Ok(PersistedState::default())
        }
    }
}

/// Atomically writes the daemon state to disk using a temporary file and then
/// replacing the previous state file.
pub fn save_state(path: &Path, state: &PersistedState) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }

    let tmp_path = tmp_state_path(path);

    write_private_json_file(&tmp_path, state)?;
    replace_state_file(&tmp_path, path)?;
    set_private_file_permissions(path)?;

    Ok(())
}

/// Per-process baselines as persisted, keyed by process name.
///
/// A separate file from `state.json` on purpose. `state.json` is what process recovery reads: a
/// corrupt baseline map must not be able to stop a managed process from being recovered, and
/// sharing one file makes that impossible to guarantee — a serialisation problem anywhere in the
/// payload takes the whole file with it. Baselines are a statistics cache and are always
/// reconstructible by observing the process again; managed process records are not.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedBaselineStore {
    /// Baselines per process name.
    #[serde(default)]
    pub processes: std::collections::BTreeMap<String, PersistedProcessBaselines>,
}

/// Path of the baseline store, derived from the state path so it follows `OXMGR_HOME`.
pub fn baseline_store_path(state_path: &Path) -> PathBuf {
    state_path.with_file_name("baselines.json")
}

/// Loads persisted baselines, treating every failure as absent.
///
/// Missing, empty, unreadable and malformed all return the default. This is deliberately more
/// forgiving than [`load_state`]: baselines are a cache whose worst-case loss is a warm-up period,
/// so there is no case in which failing to read them should surface as an error to the caller. A
/// corrupt file is moved aside rather than deleted, so it can still be inspected.
pub fn load_baselines(path: &Path) -> PersistedBaselineStore {
    if !path.exists() {
        return PersistedBaselineStore::default();
    }

    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) => {
            warn!(
                "failed to read baseline store {} ({error}); continuing without baselines",
                path.display()
            );
            return PersistedBaselineStore::default();
        }
    };

    if content.trim().is_empty() {
        return PersistedBaselineStore::default();
    }

    match serde_json::from_str::<PersistedBaselineStore>(&content) {
        Ok(store) => store,
        Err(error) => {
            let backup = corrupted_backup_path(path);
            if let Err(rename_err) = fs::rename(path, &backup) {
                warn!(
                    "failed to move corrupted baseline store {} -> {}: {rename_err}",
                    path.display(),
                    backup.display()
                );
            } else {
                warn!(
                    "baseline store {} is corrupted ({error}), moved to {}",
                    path.display(),
                    backup.display()
                );
            }
            PersistedBaselineStore::default()
        }
    }
}

/// Atomically writes the baseline store, using the same tmp-then-rename path as
/// [`save_state`] so a crash mid-write cannot leave a half-written file in place.
pub fn save_baselines(path: &Path, store: &PersistedBaselineStore) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }

    let tmp_path = tmp_state_path(path);

    write_private_json_file(&tmp_path, store)?;
    replace_state_file(&tmp_path, path)?;
    set_private_file_permissions(path)?;

    Ok(())
}

/// Advisory dismissals, keyed by process name then rule id.
///
/// Its own file for the same reason baselines have one: a corrupt dismissal list must not be able to
/// stop process recovery. Losing dismissals means a few advisories reappear, which is recoverable by
/// dismissing them again; losing `state.json` is not.
///
/// `BTreeMap` for deterministic serialisation — the file is rewritten whole, and a map that reordered
/// itself would produce a spurious diff on every write.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedDismissals {
    /// Process name → dismissed rule ids.
    #[serde(default)]
    pub processes: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
}

/// Path of the dismissal store, derived from the state path so it follows `OXMGR_HOME`.
pub fn dismissal_store_path(state_path: &Path) -> PathBuf {
    state_path.with_file_name("advisory-dismissals.json")
}

/// Loads dismissals, treating every failure as absent.
///
/// Cannot fail, for the same reason `load_baselines` cannot: the worst case of losing this file is
/// that some advisories reappear, and an operator who cannot start their daemon because a
/// suppression list is malformed is strictly worse off. A corrupt file is moved aside rather than
/// deleted so it stays inspectable.
pub fn load_dismissals(path: &Path) -> PersistedDismissals {
    if !path.exists() {
        return PersistedDismissals::default();
    }

    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) => {
            warn!(
                "failed to read advisory dismissals {} ({error}); continuing with none",
                path.display()
            );
            return PersistedDismissals::default();
        }
    };

    if content.trim().is_empty() {
        return PersistedDismissals::default();
    }

    match serde_json::from_str::<PersistedDismissals>(&content) {
        Ok(store) => store,
        Err(error) => {
            let backup = corrupted_backup_path(path);
            if let Err(rename_err) = fs::rename(path, &backup) {
                warn!(
                    "failed to move corrupted dismissal store {} -> {}: {rename_err}",
                    path.display(),
                    backup.display()
                );
            } else {
                warn!(
                    "advisory dismissal store {} is corrupted ({error}), moved to {}",
                    path.display(),
                    backup.display()
                );
            }
            PersistedDismissals::default()
        }
    }
}

/// Atomically writes the dismissal store.
pub fn save_dismissals(path: &Path, store: &PersistedDismissals) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }

    let tmp_path = tmp_state_path(path);
    write_private_json_file(&tmp_path, store)?;
    replace_state_file(&tmp_path, path)?;
    set_private_file_permissions(path)?;

    Ok(())
}

fn corrupted_backup_path(path: &Path) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    path.with_extension(format!("corrupt-{suffix}.json"))
}

fn tmp_state_path(path: &Path) -> PathBuf {
    path.with_extension("tmp")
}

fn replace_state_file(tmp_path: &Path, path: &Path) -> Result<()> {
    match fs::rename(tmp_path, path) {
        Ok(()) => Ok(()),
        Err(rename_err) => {
            #[cfg(windows)]
            {
                if path.exists() {
                    fs::remove_file(path).with_context(|| {
                        format!("failed to remove state file {}", path.display())
                    })?;
                    fs::rename(tmp_path, path).with_context(|| {
                        format!("failed to replace state file {}", path.display())
                    })?;
                    return Ok(());
                }
            }

            Err(rename_err)
                .with_context(|| format!("failed to replace state file {}", path.display()))
        }
    }
}

#[cfg(unix)]
fn ensure_private_dir(path: &Path) -> Result<()> {
    let existed = path.exists();
    fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;

    use std::os::unix::fs::PermissionsExt;

    if !existed {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }
    Ok(())
}

fn write_private_json_file<T: Serialize>(path: &Path, state: &T) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let file = options
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer(&mut writer, state)
        .with_context(|| format!("failed to serialize {}", path.display()))?;
    writer
        .flush()
        .with_context(|| format!("failed to flush {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file_permissions(_: &Path) -> Result<()> {
    Ok(())
}

/// What protection is actually in force on a path, and whether the platform can express it.
///
/// Task 4.7 asks for three things: apply equivalent protection where the platform allows, declare the
/// difference where it does not, and make the applied protection INSPECTABLE. The third was missing —
/// the daemon set `0o600` and nothing could report what had taken effect, so an operator had no way
/// to tell a restricted file from one the platform silently left open.
///
/// Reported rather than asserted, because the honest answer differs per platform: on Unix this reads
/// the real mode bits, and on Windows it states that no restriction was applied at creation. Those are
/// different facts and collapsing them into a boolean would lose the one that matters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileProtection {
    /// The Unix mode, when the platform has one.
    pub mode: Option<u32>,
    /// Whether the file is readable only by its owner.
    ///
    /// `None` where the platform cannot answer, which is NOT the same as `Some(false)`: "we did not
    /// restrict this" and "we cannot tell" call for different responses from an operator.
    pub owner_only: Option<bool>,
    /// One line an operator can read, naming the platform difference where there is one.
    pub summary: String,
}

/// Inspects the protection in force on a file.
pub fn inspect_protection(path: &Path) -> Result<FileProtection> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to stat {} for inspection", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Masked to the permission bits: the raw mode also carries the file type, and reporting
        // `0o100600` as "the mode" would confuse anyone comparing it to what was requested.
        let mode = metadata.permissions().mode() & 0o777;
        let owner_only = mode & 0o077 == 0;
        Ok(FileProtection {
            mode: Some(mode),
            owner_only: Some(owner_only),
            summary: if owner_only {
                format!("mode {mode:04o}: readable and writable by the owner only")
            } else {
                // Stated as a problem rather than a fact, because a state file group- or
                // world-readable is a real exposure: it carries every managed process's environment.
                format!(
                    "mode {mode:04o}: accessible beyond the owner — expected {:04o}",
                    0o600
                )
            },
        })
    }

    #[cfg(not(unix))]
    {
        let _ = metadata;
        Ok(FileProtection {
            mode: None,
            owner_only: None,
            summary: oxmgr_metrics::platform::unavailable_message(
                oxmgr_metrics::platform::Capability::PrivateFilePermissions,
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use oxmgr_core::numeric::usize_to_f64;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{PersistedState, load_state, save_state};

    /// Builds `count` processes by deserialising a minimal record, so the measurement
    /// needs no production-code fixture helper.
    fn synthetic_state(count: usize) -> PersistedState {
        let processes = (0..count)
            .map(|idx| {
                let json = format!(
                    r#"{{
                        "id": {idx},
                        "name": "service-{idx}",
                        "command": "/usr/local/bin/some-service-binary",
                        "args": ["--config", "/etc/service/config.toml", "--verbose"],
                        "cwd": "/srv/app/service-{idx}",
                        "env": {{"NODE_ENV": "production", "PORT": "{port}", "LOG_LEVEL": "info"}},
                        "restart_policy": "always",
                        "max_restarts": 10,
                        "restart_count": 0,
                        "pid": {pid},
                        "status": "running",
                        "desired_state": "running",
                        "last_exit_code": null,
                        "stdout_log": "/var/log/oxmgr/service-{idx}.out.log",
                        "stderr_log": "/var/log/oxmgr/service-{idx}.err.log"
                    }}"#,
                    idx = idx,
                    port = 3000 + idx,
                    pid = 40000 + idx,
                );
                serde_json::from_str(&json).expect("synthetic process should deserialise")
            })
            .collect();
        PersistedState {
            next_id: count as u64 + 1,
            processes,
        }
    }

    /// Measures `save_state` against process count.
    ///
    /// The spec asks for this before changing persistence: whole-file rewriting may be
    /// entirely adequate at the intended scale, and rewriting it on suspicion would add
    /// risk for nothing. Ignored by default because a timing assertion on a shared CI
    /// runner is flaky; run with `cargo test -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing measurement, not an assertion; run explicitly"]
    fn measure_save_state_cost_by_process_count() {
        let base = temp_dir("save-cost");
        fs::create_dir_all(&base).expect("failed to create temp dir");

        println!("\n  processes   bytes    p50 (ms)   per-process (us)");
        for count in [1usize, 10, 50, 100, 250, 500, 1000] {
            let state = synthetic_state(count);
            let path = base.join(format!("state-{count}.json"));

            // Warm the path so directory creation is not counted.
            save_state(&path, &state).expect("warmup save failed");
            let bytes = fs::metadata(&path).expect("failed to stat state").len();

            let mut samples = Vec::new();
            for _ in 0..15 {
                let started = std::time::Instant::now();
                save_state(&path, &state).expect("save failed");
                samples.push(started.elapsed().as_secs_f64() * 1000.0);
            }
            samples.sort_by(|a, b| a.partial_cmp(b).expect("no NaN timings"));
            let p50 = samples[samples.len() / 2];
            let per_process = p50 * 1000.0 / usize_to_f64(count);
            println!("  {count:>9}  {bytes:>7}   {p50:>8.3}   {per_process:>16.1}");

            // A sanity floor rather than a performance claim: whole-file rewriting at the
            // intended scale must not be pathological.
            assert!(
                p50 < 250.0,
                "save_state took {p50:.1}ms at {count} processes, which would make every \
                 state mutation visible to an operator"
            );
        }

        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn load_state_returns_default_when_file_is_missing() {
        let path = temp_state_file("missing");

        let loaded = load_state(&path).expect("missing state file should use defaults");

        assert_eq!(loaded.next_id, 1);
        assert!(loaded.processes.is_empty());
    }

    #[test]
    fn load_state_returns_default_when_file_is_empty() {
        let path = temp_state_file("empty");
        fs::write(&path, "").expect("failed to write empty state file");

        let loaded = load_state(&path).expect("empty state file should use defaults");

        assert_eq!(loaded.next_id, 1);
        assert!(loaded.processes.is_empty());

        let _ = fs::remove_file(path);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let path = temp_state_file("roundtrip");
        let state = PersistedState {
            next_id: 42,
            processes: Vec::new(),
        };

        save_state(&path, &state).expect("failed to save test state");
        let loaded = load_state(&path).expect("failed to load test state");

        assert_eq!(loaded.next_id, 42);
        assert!(loaded.processes.is_empty());

        let _ = fs::remove_file(path);
    }

    #[test]
    fn save_state_overwrites_existing_file() {
        let path = temp_state_file("overwrite");
        let first = PersistedState {
            next_id: 7,
            processes: Vec::new(),
        };
        let second = PersistedState {
            next_id: 9,
            processes: Vec::new(),
        };

        save_state(&path, &first).expect("failed to save first state");
        save_state(&path, &second).expect("failed to overwrite existing state");
        let loaded = load_state(&path).expect("failed to load overwritten state");

        assert_eq!(loaded.next_id, 9);
        assert!(loaded.processes.is_empty());

        let _ = fs::remove_file(path);
    }

    #[test]
    fn save_state_creates_parent_directories() {
        let base = temp_dir("save-parent");
        let path = base.join("nested").join("state.json");
        let state = PersistedState::default();

        save_state(&path, &state).expect("save_state should create missing parent directories");

        assert!(path.exists(), "state file should be created");
        assert!(
            path.parent().is_some_and(|parent| parent.exists()),
            "parent directory should be created"
        );

        let _ = fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn save_state_sets_private_permissions_on_created_paths() {
        use std::os::unix::fs::PermissionsExt;

        let base = temp_dir("save-perms");
        let dir = base.join("state-dir");
        let path = dir.join("state.json");
        let state = PersistedState::default();

        save_state(&path, &state).expect("save_state should write private state file");

        let dir_mode = fs::metadata(&dir)
            .expect("failed to stat parent dir")
            .permissions()
            .mode()
            & 0o777;
        let file_mode = fs::metadata(&path)
            .expect("failed to stat state file")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);

        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn load_state_recovers_from_corruption() {
        let path = temp_state_file("corrupt");
        fs::write(&path, "{ not valid json ]").expect("failed to write corrupted state file");

        let loaded = load_state(&path).expect("load_state should recover from corruption");
        assert_eq!(loaded.next_id, 1);
        assert!(loaded.processes.is_empty());
        assert!(!path.exists(), "corrupted file should have been renamed");

        let original_stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_string();

        let backup_found = path
            .parent()
            .expect("temp file has no parent")
            .read_dir()
            .expect("failed to read temp parent")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .any(|candidate| {
                candidate
                    .file_name()
                    .and_then(|value| value.to_str())
                    .map(|name| name.starts_with(&original_stem) && name.contains(".corrupt-"))
                    .unwrap_or(false)
            });

        assert!(backup_found, "expected renamed corrupt backup state file");

        // Best-effort cleanup.
        if let Some(parent) = path.parent()
            && let Ok(entries) = parent.read_dir()
        {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str()
                    && name.contains("corrupt-")
                {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }

    fn temp_state_file(prefix: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock failure")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{nonce}.state.json"))
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock failure")
            .as_nanos();
        std::env::temp_dir().join(format!("oxmgr-storage-{prefix}-{nonce}"))
    }

    // ── 4.7 file protection is applied, declared, and inspectable ───────────────────────────────

    #[test]
    fn a_written_state_file_is_restricted_and_reports_what_took_effect() {
        // Task 4.7. The three parts: protection is APPLIED where the platform allows, the difference
        // is DECLARED where it does not, and what took effect is INSPECTABLE. The third was missing
        // entirely — the daemon set 0o600 and nothing could report the result, so an operator could
        // not tell a restricted file from one the platform silently left open.
        let base = temp_dir("protection-applied");
        fs::create_dir_all(&base).expect("failed to create temp dir");
        let path = base.join("state.json");
        save_state(&path, &PersistedState::default()).expect("save failed");

        let protection = super::inspect_protection(&path).expect("inspection failed");

        #[cfg(unix)]
        {
            // Applied: owner-only, and the mode is reported so the claim is checkable rather than
            // asserted. Masked to the permission bits, or the file-type bits would make 0o600 read as
            // 0o100600 and confuse anyone comparing it to what was requested.
            assert_eq!(
                protection.mode,
                Some(0o600),
                "the state file must be created owner-only: {}",
                protection.summary
            );
            assert_eq!(protection.owner_only, Some(true));
            assert!(
                protection.summary.contains("owner only"),
                "the summary must say what is in force: {:?}",
                protection.summary
            );
        }

        #[cfg(not(unix))]
        {
            // Declared: `None` rather than `Some(false)`, because "we did not restrict this" and "we
            // cannot tell" are different facts and an operator responds differently to each.
            assert_eq!(protection.mode, None);
            assert_eq!(
                protection.owner_only, None,
                "an inexpressible protection must be unknown rather than reported as absent"
            );
            // And the summary carries the declared platform reason rather than a locally written one.
            assert!(
                protection.summary.contains("Windows"),
                "the summary must name the platform difference: {:?}",
                protection.summary
            );
        }

        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn a_loosened_file_is_reported_as_a_problem_rather_than_a_fact() {
        // The inspection has to be capable of saying NO, or it is decoration. Loosening the file by
        // hand and re-inspecting is the only way to prove that.
        let base = temp_dir("protection-loosened");
        fs::create_dir_all(&base).expect("failed to create temp dir");
        let path = base.join("state.json");
        save_state(&path, &PersistedState::default()).expect("save failed");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
                .expect("failed to loosen permissions");

            let protection = super::inspect_protection(&path).expect("inspection failed");
            assert_eq!(protection.owner_only, Some(false));
            assert_eq!(protection.mode, Some(0o644));
            // Phrased as an exposure rather than a neutral reading: a state file readable beyond its
            // owner carries every managed process's environment.
            assert!(
                protection.summary.contains("beyond the owner"),
                "a loosened file must be reported as a problem: {:?}",
                protection.summary
            );
            assert!(
                protection.summary.contains("0600"),
                "the summary must state what was expected: {:?}",
                protection.summary
            );
        }

        #[cfg(not(unix))]
        {
            // Nothing to loosen: the platform never restricted it, which is the declared difference.
            let protection = super::inspect_protection(&path).expect("inspection failed");
            assert_eq!(protection.owner_only, None);
        }

        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn every_private_store_is_written_through_the_same_restricted_path() {
        // The state file is not the only sensitive store. Baselines carry per-process statistics and
        // dismissals carry operator decisions, and both were added later — so this asserts they went
        // through `write_private_json_file` rather than growing their own `fs::write`, which would
        // have created a world-readable file beside a restricted one.
        let base = temp_dir("protection-all-stores");
        fs::create_dir_all(&base).expect("failed to create temp dir");
        let state = base.join("state.json");

        save_state(&state, &PersistedState::default()).expect("state save failed");
        super::save_baselines(
            &super::baseline_store_path(&state),
            &super::PersistedBaselineStore::default(),
        )
        .expect("baseline save failed");
        super::save_dismissals(
            &super::dismissal_store_path(&state),
            &super::PersistedDismissals::default(),
        )
        .expect("dismissal save failed");

        for path in [
            state.clone(),
            super::baseline_store_path(&state),
            super::dismissal_store_path(&state),
        ] {
            assert!(path.exists(), "{} was not written", path.display());
            let protection = super::inspect_protection(&path).expect("inspection failed");
            #[cfg(unix)]
            assert_eq!(
                protection.owner_only,
                Some(true),
                "{} is not owner-only: {}",
                path.display(),
                protection.summary
            );
            #[cfg(not(unix))]
            assert_eq!(protection.owner_only, None);
        }

        let _ = fs::remove_dir_all(base);
    }
}
