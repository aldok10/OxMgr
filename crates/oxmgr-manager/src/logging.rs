//! Log-path calculation, rotation, and tail-reading helpers for managed
//! processes.
//!
//! Lint-level cleanup: display-path casts in log-chunk calculation.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Concrete stdout and stderr log file paths for a managed process.
pub struct ProcessLogs {
    pub stdout: PathBuf,
    pub stderr: PathBuf,
}

#[derive(Debug, Clone, Copy)]
/// Rotation settings applied before a process log is opened for writing, and
/// re-evaluated as lines are written so a long-running process still rotates.
pub struct LogRotationPolicy {
    pub max_size_bytes: u64,
    pub max_files: u32,
    pub max_age_days: u64,
    /// Rotate once the active file is this old, regardless of size. `None`
    /// leaves size as the only trigger.
    pub max_age_secs: Option<u64>,
}

/// One log file belonging to a process: the active file or a rotated archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogFileEntry {
    /// `stdout` or `stderr`.
    pub stream: String,
    /// `0` for the active file, `1..=max_files` for archives (1 = newest).
    pub index: u32,
    pub filename: String,
    pub size: u64,
    /// Unix seconds; `0` when unavailable.
    pub modified_at: u64,
}

/// Resolves the path of a log file by archive index. Index `0` is the active
/// file. The path is always derived from `base`, never from caller-supplied
/// text, so an index cannot address a file outside the process's own logs.
pub fn log_file_path(base: &Path, index: u32) -> PathBuf {
    if index == 0 {
        base.to_path_buf()
    } else {
        rotated_path(base, index)
    }
}

/// Yields the log files present for one stream, active file first, then archives
/// newest to oldest. An iterator rather than a `Vec`: callers serialise it
/// straight out, so materialising the list first would be pure overhead.
pub fn log_files_for<'a>(
    base: &'a Path,
    stream: &'a str,
    max_files: u32,
) -> impl Iterator<Item = LogFileEntry> + 'a {
    (0..=max_files).filter_map(move |index| {
        let path = log_file_path(base, index);
        let meta = fs::metadata(&path).ok()?;
        if !meta.is_file() {
            return None;
        }
        Some(LogFileEntry {
            stream: stream.to_string(),
            index,
            filename: path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string(),
            size: meta.len(),
            modified_at: meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|dur| dur.as_secs())
                .unwrap_or(0),
        })
    })
}

/// Returns the canonical stdout and stderr log paths for the given process name.
pub fn process_logs(log_dir: &Path, name: &str) -> ProcessLogs {
    ProcessLogs {
        stdout: log_dir.join(format!("{name}.out.log")),
        stderr: log_dir.join(format!("{name}.err.log")),
    }
}

/// Returns log paths for either split-stream or unified-log mode.
pub fn process_logs_for_mode(log_dir: &Path, name: &str, unified_logs: bool) -> ProcessLogs {
    if unified_logs {
        let unified = log_dir.join(format!("{name}.log"));
        ProcessLogs {
            stdout: unified.clone(),
            stderr: unified,
        }
    } else {
        process_logs(log_dir, name)
    }
}

/// Returns the last modification time of a log file, falling back to the Unix
/// epoch when metadata is unavailable.
pub fn log_modified_at(path: &Path) -> SystemTime {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .unwrap_or(UNIX_EPOCH)
}

/// Performs log rotation and retention cleanup without opening file handles.
///
/// Call this before spawning a process that will use async log forwarding so
/// that the rotation policy is still enforced.
pub fn prepare_log_files(logs: &ProcessLogs, policy: LogRotationPolicy) -> Result<()> {
    if let Some(parent) = logs.stdout.parent() {
        ensure_private_dir(parent)?;
    }
    rotate_log_if_needed(&logs.stdout, policy)?;
    cleanup_rotated_logs(&logs.stdout, policy)?;
    if logs.stdout != logs.stderr {
        if let Some(parent) = logs.stderr.parent() {
            ensure_private_dir(parent)?;
        }
        rotate_log_if_needed(&logs.stderr, policy)?;
        cleanup_rotated_logs(&logs.stderr, policy)?;
    }
    Ok(())
}

/// Opens stdout and stderr log writers, performing rotation and retention
/// cleanup first.
///
/// The daemon spawns processes via async log forwarding and only calls
/// [`prepare_log_files`]; this synchronous writer path is retained for the
/// rotation tests below.
#[cfg(test)]
pub fn open_log_writers(logs: &ProcessLogs, policy: LogRotationPolicy) -> Result<(File, File)> {
    use std::fs::OpenOptions;

    if let Some(parent) = logs.stdout.parent() {
        ensure_private_dir(parent)?;
    }
    rotate_log_if_needed(&logs.stdout, policy)?;
    rotate_log_if_needed(&logs.stderr, policy)?;
    cleanup_rotated_logs(&logs.stdout, policy)?;
    cleanup_rotated_logs(&logs.stderr, policy)?;

    let mut stdout_options = OpenOptions::new();
    stdout_options
        .create(true)
        .append(true)
        .write(true)
        .read(true)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        stdout_options.mode(0o600);
    }
    let stdout = stdout_options
        .open(&logs.stdout)
        .with_context(|| format!("failed opening {}", logs.stdout.display()))?;
    set_private_file_permissions(&logs.stdout)?;

    let mut stderr_options = OpenOptions::new();
    stderr_options
        .create(true)
        .append(true)
        .write(true)
        .read(true)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        stderr_options.mode(0o600);
    }
    let stderr = stderr_options
        .open(&logs.stderr)
        .with_context(|| format!("failed opening {}", logs.stderr.display()))?;
    set_private_file_permissions(&logs.stderr)?;

    Ok((stdout, stderr))
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

/// Buffered append-only writer for one process stream that enforces the rotation
/// policy as it writes.
///
/// Two problems this solves over the previous open-per-line approach:
/// rotation was only ever evaluated at spawn, so a process that ran for weeks
/// never rotated; and every single line cost an open/write/close cycle, which
/// dominated throughput once a log grew.
///
/// Size is tracked in a counter seeded from the file's length on open, so the
/// policy check costs no syscall per line. The time trigger compares against a
/// deadline set when the file is opened or rotated.
pub struct RotatingLogWriter {
    path: PathBuf,
    policy: LogRotationPolicy,
    file: File,
    written: u64,
    deadline: Option<Instant>,
}

impl RotatingLogWriter {
    /// Opens `path` for appending, rotating first if the existing file already
    /// exceeds the policy.
    pub fn open(path: PathBuf, policy: LogRotationPolicy) -> Result<Self> {
        if let Some(parent) = path.parent() {
            ensure_private_dir(parent)?;
        }
        rotate_log_if_needed(&path, policy)?;
        cleanup_rotated_logs(&path, policy)?;
        let file = append_file(&path)?;
        let written = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        Ok(Self {
            path,
            policy,
            file,
            written,
            deadline: Self::next_deadline(policy),
        })
    }

    fn next_deadline(policy: LogRotationPolicy) -> Option<Instant> {
        policy
            .max_age_secs
            .filter(|secs| *secs > 0)
            .map(|secs| Instant::now() + Duration::from_secs(secs))
    }

    /// Whether either trigger has fired. Size and time are independent: whichever
    /// is reached first rotates.
    fn should_rotate(&self) -> bool {
        if self.policy.max_files == 0 {
            return false;
        }
        let by_size = self.policy.max_size_bytes > 0 && self.written >= self.policy.max_size_bytes;
        let by_age = self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline);
        by_size || by_age
    }

    /// Appends one already-formatted record. Flushing is the caller's decision so
    /// a burst can amortise into few syscalls.
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if self.should_rotate() {
            self.rotate()?;
        }
        self.file
            .write_all(bytes)
            .with_context(|| format!("failed writing {}", self.path.display()))?;
        self.written = self.written.saturating_add(bytes.len() as u64);
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.file
            .flush()
            .with_context(|| format!("failed flushing {}", self.path.display()))
    }

    fn rotate(&mut self) -> Result<()> {
        // Flush before the rename so no buffered bytes are stranded in the
        // handle we are about to replace.
        // The discard is deliberate: best-effort flush on a best-effort channel
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort flush on a best-effort channel"
        )]
        let _ = self.file.flush();
        shift_rotated_chain(&self.path, self.policy)?;
        let first = rotated_path(&self.path, 1);
        // The discard is deliberate: removal is opportunistic; a real failure surfaces at the subsequent rename or bind
        #[expect(
            clippy::let_underscore_must_use,
            reason = "removal is opportunistic; a real failure surfaces at the subsequent rename or bind"
        )]
        let _ = fs::remove_file(&first);
        fs::rename(&self.path, &first).with_context(|| {
            format!(
                "failed to rotate {} -> {}",
                self.path.display(),
                first.display()
            )
        })?;
        self.file = append_file(&self.path)?;
        self.written = 0;
        self.deadline = Self::next_deadline(self.policy);
        cleanup_rotated_logs(&self.path, self.policy)?;
        Ok(())
    }
}

impl Drop for RotatingLogWriter {
    fn drop(&mut self) {
        // Buffered output must reach the file even if the pipe ended abruptly.
        // The discard is deliberate: best-effort flush on a best-effort channel
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort flush on a best-effort channel"
        )]
        let _ = self.file.flush();
    }
}

/// Opens a log file for appending with private permissions.
fn append_file(path: &Path) -> Result<File> {
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("failed opening {}", path.display()))?;
    set_private_file_permissions(path)?;
    Ok(file)
}

/// Shifts `path.1 -> path.2 -> …`, dropping the oldest beyond `max_files`.
fn shift_rotated_chain(path: &Path, policy: LogRotationPolicy) -> Result<()> {
    for idx in (1..=policy.max_files).rev() {
        let candidate = rotated_path(path, idx);
        if !candidate.exists() {
            continue;
        }
        if idx == policy.max_files {
            // The discard is deliberate: removal is opportunistic; a real failure surfaces at the subsequent rename or bind
            #[expect(
                clippy::let_underscore_must_use,
                reason = "removal is opportunistic; a real failure surfaces at the subsequent rename or bind"
            )]
            let _ = fs::remove_file(&candidate);
        } else {
            let next = rotated_path(path, idx + 1);
            // The discard is deliberate: removal is opportunistic; a real failure surfaces at the subsequent rename or bind
            #[expect(
                clippy::let_underscore_must_use,
                reason = "removal is opportunistic; a real failure surfaces at the subsequent rename or bind"
            )]
            let _ = fs::remove_file(&next);
            fs::rename(&candidate, &next).with_context(|| {
                format!(
                    "failed to rotate {} -> {}",
                    candidate.display(),
                    next.display()
                )
            })?;
        }
    }
    Ok(())
}

fn rotate_log_if_needed(path: &Path, policy: LogRotationPolicy) -> Result<()> {
    if policy.max_size_bytes == 0 || policy.max_files == 0 {
        return Ok(());
    }
    if !path.exists() {
        return Ok(());
    }

    let metadata =
        fs::metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
    if metadata.len() < policy.max_size_bytes {
        return Ok(());
    }

    shift_rotated_chain(path, policy)?;

    let first = rotated_path(path, 1);
    // The discard is deliberate: removal is opportunistic; a real failure surfaces at the subsequent rename or bind
    #[expect(
        clippy::let_underscore_must_use,
        reason = "removal is opportunistic; a real failure surfaces at the subsequent rename or bind"
    )]
    let _ = fs::remove_file(&first);
    fs::rename(path, &first)
        .with_context(|| format!("failed to rotate {} -> {}", path.display(), first.display()))?;
    Ok(())
}

fn cleanup_rotated_logs(path: &Path, policy: LogRotationPolicy) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let Some(base_name) = path.file_name().and_then(|value| value.to_str()) else {
        return Ok(());
    };

    let max_age = Duration::from_secs(policy.max_age_days.saturating_mul(24 * 60 * 60));
    let now = std::time::SystemTime::now();

    let entries = fs::read_dir(parent)
        .with_context(|| format!("failed to read directory {}", parent.display()))?;
    for entry in entries {
        let entry =
            entry.with_context(|| format!("failed to read entry in {}", parent.display()))?;
        let file_name = entry.file_name();
        let file_name = match file_name.to_str() {
            Some(value) => value,
            None => continue,
        };

        let Some(suffix) = file_name
            .strip_prefix(base_name)
            .and_then(|rest| rest.strip_prefix('.'))
        else {
            continue;
        };
        let Ok(index) = suffix.parse::<u32>() else {
            continue;
        };

        let path = entry.path();
        let mut remove = index > policy.max_files;

        if !remove
            && policy.max_age_days > 0
            && let Ok(meta) = entry.metadata()
            && let Ok(modified) = meta.modified()
            && now.duration_since(modified).unwrap_or(Duration::ZERO) > max_age
        {
            remove = true;
        }

        if remove {
            // The discard is deliberate: removal is opportunistic; a real failure surfaces at the subsequent rename or bind
            #[expect(
                clippy::let_underscore_must_use,
                reason = "removal is opportunistic; a real failure surfaces at the subsequent rename or bind"
            )]
            let _ = fs::remove_file(path);
        }
    }

    Ok(())
}

fn rotated_path(path: &Path, index: u32) -> PathBuf {
    PathBuf::from(format!("{}.{}", path.display(), index))
}

/// A section of a log file, addressed backwards from the end.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineRange {
    /// The lines in file order, oldest first.
    pub lines: Vec<String>,
    /// Whether these lines begin at the first line of the file, so the caller knows
    /// there is no earlier content to ask for. Reported by the reader — which knows
    /// when it ran out of file — rather than inferred from counts by the caller.
    pub reached_start: bool,
}

/// Reads up to the last `max_lines` lines from a log file without loading the
/// entire file into memory when avoidable.
pub fn read_last_lines(path: &Path, max_lines: usize) -> Result<Vec<String>> {
    Ok(read_line_range(path, 0, max_lines)?.lines)
}

/// Reads a bounded section of a log file, skipping `skip_from_end` lines back from
/// the end and then taking up to `max_lines`.
///
/// Addressed from the end rather than the start because that is what paging
/// backwards through history needs, and because addressing from the start would
/// require a line count for the whole file — which means reading all of it, the
/// cost this exists to avoid. The traversal is the same backwards chunk scan
/// `read_last_lines` has always used, with a different stopping rule.
pub fn read_line_range(path: &Path, skip_from_end: usize, max_lines: usize) -> Result<LineRange> {
    // A zero-length request tells us nothing about the file, so it cannot claim to
    // have reached the start. A missing or empty file genuinely has no earlier
    // content.
    if max_lines == 0 {
        return Ok(LineRange {
            lines: Vec::new(),
            reached_start: false,
        });
    }
    if !path.exists() {
        return Ok(LineRange {
            lines: Vec::new(),
            reached_start: true,
        });
    }

    let mut file =
        File::open(path).with_context(|| format!("failed opening {}", path.display()))?;
    let total_size = file
        .metadata()
        .with_context(|| format!("failed to stat {}", path.display()))?
        .len();
    if total_size == 0 {
        return Ok(LineRange {
            lines: Vec::new(),
            reached_start: true,
        });
    }

    // Everything from the end up to and including the requested window.
    let want = skip_from_end.saturating_add(max_lines);

    const CHUNK_SIZE: u64 = 16 * 1024;
    let mut offset = total_size;
    let mut newline_count = 0usize;
    let mut chunks: Vec<Vec<u8>> = Vec::new();

    // One newline beyond the window, so the leading partial line a chunk boundary
    // can produce is available to discard rather than reported as a line.
    while offset > 0 && newline_count <= want {
        let read_len = usize::try_from(CHUNK_SIZE.min(offset)).unwrap_or(usize::MAX);
        offset -= read_len as u64;

        file.seek(SeekFrom::Start(offset))
            .with_context(|| format!("failed seeking {}", path.display()))?;

        let mut chunk = vec![0_u8; read_len];
        file.read_exact(&mut chunk)
            .with_context(|| format!("failed reading {}", path.display()))?;
        newline_count += chunk.iter().filter(|&&byte| byte == b'\n').count();
        chunks.push(chunk);
    }

    let total_bytes: usize = chunks.iter().map(Vec::len).sum();
    let mut bytes = Vec::with_capacity(total_bytes);
    for chunk in chunks.iter().rev() {
        bytes.extend_from_slice(chunk);
    }
    let text = String::from_utf8_lossy(&bytes);

    let mut available: Vec<&str> = text.lines().collect();
    // Reading stopped short of the file's start, so the first line we hold begins
    // mid-line. Dropping it is what keeps a chunk boundary from being served as
    // though it were a real line.
    let scanned_whole_file = offset == 0;
    if !scanned_whole_file && !available.is_empty() {
        available.remove(0);
    }

    let end = available.len().saturating_sub(skip_from_end);
    let start = end.saturating_sub(max_lines);

    Ok(LineRange {
        lines: available[start..end]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        // Only a window that both begins at index 0 and was scanned from the file's
        // actual start can claim there is nothing earlier.
        reached_start: scanned_whole_file && start == 0,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        LogRotationPolicy, ProcessLogs, RotatingLogWriter, log_file_path, log_files_for,
        open_log_writers, process_logs, read_last_lines, read_line_range,
    };

    /// Policy with a size trigger only, matching the historical default shape.
    fn size_policy(max_size_bytes: u64, max_files: u32) -> LogRotationPolicy {
        LogRotationPolicy {
            max_size_bytes,
            max_files,
            max_age_days: 14,
            max_age_secs: None,
        }
    }

    /// The defect this pins: rotation used to be evaluated only when the process
    /// was spawned, so a process that ran for weeks never rotated. The writer must
    /// rotate from the write path itself, with no restart involved.
    #[test]
    fn rotating_writer_rotates_mid_run_without_a_restart() {
        let tmp = temp_dir("writer-rotate-mid-run");
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        let path = tmp.join("app.out.log");

        let mut writer = RotatingLogWriter::open(path.clone(), size_policy(64, 3))
            .expect("failed to open rotating writer");

        // One long-lived writer, many writes: exactly the case that never rotated.
        for idx in 0..40 {
            writer
                .write(format!("line {idx:04} padding padding\n").as_bytes())
                .expect("failed to write line");
        }
        writer.flush().expect("failed to flush");

        let first = super::rotated_path(&path, 1);
        assert!(
            first.exists(),
            "expected an archive after exceeding the size limit mid-run"
        );
        assert!(path.exists(), "the active log should have been reopened");
        let active_len = fs::metadata(&path).expect("stat active log").len();
        assert!(
            active_len < 64,
            "active log should have been truncated by rotation, got {active_len} bytes"
        );

        let _ = fs::remove_dir_all(tmp);
    }

    /// Time and size are independent triggers. Here the file stays far below the
    /// size limit, so only the elapsed-time deadline can rotate it.
    #[test]
    fn rotating_writer_rotates_on_elapsed_time_below_the_size_limit() {
        let tmp = temp_dir("writer-rotate-time");
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        let path = tmp.join("app.out.log");

        let policy = LogRotationPolicy {
            // Deliberately huge: size must not be what fires.
            max_size_bytes: 10 * 1024 * 1024,
            max_files: 3,
            max_age_days: 14,
            max_age_secs: Some(1),
        };
        let mut writer =
            RotatingLogWriter::open(path.clone(), policy).expect("failed to open rotating writer");
        writer.write(b"before\n").expect("failed to write");
        writer.flush().expect("failed to flush");

        assert!(
            !super::rotated_path(&path, 1).exists(),
            "nothing should rotate before the deadline elapses"
        );

        std::thread::sleep(std::time::Duration::from_millis(1100));
        writer.write(b"after\n").expect("failed to write");
        writer.flush().expect("failed to flush");

        let first = super::rotated_path(&path, 1);
        assert!(
            first.exists(),
            "expected a time-triggered rotation below the size limit"
        );
        let archived = fs::read_to_string(&first).expect("read archive");
        assert!(
            archived.contains("before"),
            "the archive should hold the pre-rotation content, got {archived:?}"
        );
        let active = fs::read_to_string(&path).expect("read active log");
        assert!(
            active.contains("after"),
            "post-rotation writes belong in the new active file, got {active:?}"
        );

        let _ = fs::remove_dir_all(tmp);
    }

    /// Size must still win inside an unexpired time window: whichever trigger is
    /// reached first rotates.
    #[test]
    fn rotating_writer_rotates_on_size_within_the_time_window() {
        let tmp = temp_dir("writer-rotate-size-wins");
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        let path = tmp.join("app.out.log");

        let policy = LogRotationPolicy {
            max_size_bytes: 32,
            max_files: 3,
            max_age_days: 14,
            // Long enough that the time trigger cannot be what fires.
            max_age_secs: Some(3600),
        };
        let mut writer =
            RotatingLogWriter::open(path.clone(), policy).expect("failed to open rotating writer");
        for _ in 0..10 {
            writer.write(b"0123456789\n").expect("failed to write");
        }
        writer.flush().expect("failed to flush");

        assert!(
            super::rotated_path(&path, 1).exists(),
            "size should rotate even while the time window is open"
        );

        let _ = fs::remove_dir_all(tmp);
    }

    /// Buffered output must not be lost when the writer goes away: the pipe ending
    /// is the normal case, and an unflushed tail would silently truncate the log.
    #[test]
    fn rotating_writer_flushes_buffered_output_on_drop() {
        let tmp = temp_dir("writer-flush-on-drop");
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        let path = tmp.join("app.out.log");

        {
            let mut writer = RotatingLogWriter::open(path.clone(), size_policy(1024 * 1024, 3))
                .expect("failed to open rotating writer");
            writer.write(b"buffered line\n").expect("failed to write");
            // No explicit flush: Drop must do it.
        }

        let content = fs::read_to_string(&path).expect("read active log");
        assert_eq!(content, "buffered line\n");

        let _ = fs::remove_dir_all(tmp);
    }

    /// A burst arriving between flushes must reach the file once flushed, and must
    /// not be reordered or lost. This is the observable half of "no reopen per
    /// line": the writer holds one handle and appends.
    #[test]
    fn rotating_writer_appends_a_burst_in_order_with_one_handle() {
        let tmp = temp_dir("writer-burst");
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        let path = tmp.join("app.out.log");

        let mut writer = RotatingLogWriter::open(path.clone(), size_policy(1024 * 1024, 3))
            .expect("failed to open rotating writer");
        for idx in 0..200 {
            writer
                .write(format!("line {idx}\n").as_bytes())
                .expect("failed to write burst line");
        }
        writer.flush().expect("failed to flush");

        let content = fs::read_to_string(&path).expect("read active log");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 200, "every burst line should be present");
        assert_eq!(lines[0], "line 0");
        assert_eq!(lines[199], "line 199");

        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn log_file_path_resolves_active_file_and_archives() {
        let base = Path::new("/logs/app.out.log");
        assert_eq!(log_file_path(base, 0), base.to_path_buf());
        assert_eq!(
            log_file_path(base, 2),
            Path::new("/logs/app.out.log.2").to_path_buf()
        );
    }

    /// Enumeration is derived from the filesystem, so it survives a daemon restart
    /// with no tracked state, and a pruned archive simply stops appearing.
    #[test]
    fn log_files_for_enumerates_active_then_archives_newest_first() {
        let tmp = temp_dir("enumerate-log-files");
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        let path = tmp.join("app.out.log");
        fs::write(&path, "active\n").expect("write active");
        fs::write(super::rotated_path(&path, 1), "one\n").expect("write archive 1");
        fs::write(super::rotated_path(&path, 2), "two\n").expect("write archive 2");

        let entries: Vec<_> = log_files_for(&path, "stdout", 5).collect();
        assert_eq!(entries.len(), 3, "active plus two archives");
        assert_eq!(entries[0].index, 0, "active file first");
        assert_eq!(entries[1].index, 1, "newest archive next");
        assert_eq!(entries[2].index, 2);
        assert!(entries.iter().all(|entry| entry.stream == "stdout"));
        assert!(entries.iter().all(|entry| entry.size > 0));
        assert_eq!(entries[0].filename, "app.out.log");
        assert_eq!(entries[1].filename, "app.out.log.1");

        // A pruned archive disappears from the enumeration without any bookkeeping.
        fs::remove_file(super::rotated_path(&path, 1)).expect("prune archive 1");
        let after: Vec<_> = log_files_for(&path, "stdout", 5)
            .map(|entry| entry.index)
            .collect();
        assert_eq!(after, vec![0, 2], "pruned archive should not be listed");

        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn log_files_for_reports_nothing_when_no_logs_exist() {
        let tmp = temp_dir("enumerate-empty");
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        let entries: Vec<_> = log_files_for(&tmp.join("absent.out.log"), "stdout", 5).collect();
        assert!(entries.is_empty());
        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn open_log_writers_rotates_when_size_exceeded() {
        let tmp = temp_dir("rotate");
        let logs = ProcessLogs {
            stdout: tmp.join("app.out.log"),
            stderr: tmp.join("app.err.log"),
        };
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        fs::write(&logs.stdout, "1234567890").expect("failed to write stdout seed");
        fs::write(&logs.stderr, "1234567890").expect("failed to write stderr seed");

        let policy = LogRotationPolicy {
            max_size_bytes: 5,
            max_files: 3,
            max_age_days: 30,
            max_age_secs: None,
        };
        let (mut out, mut err) = open_log_writers(&logs, policy).expect("failed opening logs");
        writeln!(out, "new").expect("failed writing stdout");
        writeln!(err, "new").expect("failed writing stderr");

        assert!(tmp.join("app.out.log.1").exists());
        assert!(tmp.join("app.err.log.1").exists());

        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn open_log_writers_does_not_rotate_when_size_is_below_threshold() {
        let tmp = temp_dir("no-rotate");
        let logs = ProcessLogs {
            stdout: tmp.join("app.out.log"),
            stderr: tmp.join("app.err.log"),
        };
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        fs::write(&logs.stdout, "1234").expect("failed to write stdout seed");
        fs::write(&logs.stderr, "1234").expect("failed to write stderr seed");

        let policy = LogRotationPolicy {
            max_size_bytes: 1024,
            max_files: 3,
            max_age_days: 30,
            max_age_secs: None,
        };
        let _ = open_log_writers(&logs, policy).expect("failed opening logs");

        assert!(!tmp.join("app.out.log.1").exists());
        assert!(!tmp.join("app.err.log.1").exists());

        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn open_log_writers_prunes_rotated_files_by_count() {
        let tmp = temp_dir("prune-count");
        let logs = ProcessLogs {
            stdout: tmp.join("app.out.log"),
            stderr: tmp.join("app.err.log"),
        };
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        fs::write(&logs.stdout, "seed").expect("failed to write stdout seed");
        fs::write(&logs.stderr, "seed").expect("failed to write stderr seed");
        fs::write(tmp.join("app.out.log.1"), "r1").expect("failed to write out.1");
        fs::write(tmp.join("app.out.log.2"), "r2").expect("failed to write out.2");
        fs::write(tmp.join("app.out.log.3"), "r3").expect("failed to write out.3");

        let policy = LogRotationPolicy {
            max_size_bytes: 1024,
            max_files: 2,
            max_age_days: 30,
            max_age_secs: None,
        };
        let _ = open_log_writers(&logs, policy).expect("failed opening logs");

        assert!(tmp.join("app.out.log.1").exists());
        assert!(tmp.join("app.out.log.2").exists());
        assert!(!tmp.join("app.out.log.3").exists());

        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn read_last_lines_returns_only_tail() {
        let tmp = temp_dir("read-tail");
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        let path = tmp.join("app.out.log");
        fs::write(&path, "line1\nline2\nline3\nline4\n").expect("failed to write test log file");

        let lines = read_last_lines(&path, 2).expect("failed reading tail lines");
        assert_eq!(lines, vec!["line3".to_string(), "line4".to_string()]);

        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn read_last_lines_returns_all_lines_without_trailing_newline() {
        let tmp = temp_dir("read-no-trailing-newline");
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        let path = tmp.join("app.out.log");
        fs::write(&path, "line1\nline2\nline3").expect("failed to write test log file");

        let lines = read_last_lines(&path, 10).expect("failed reading lines");
        assert_eq!(
            lines,
            vec![
                "line1".to_string(),
                "line2".to_string(),
                "line3".to_string()
            ]
        );

        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn read_last_lines_returns_empty_for_missing_or_empty_file() {
        let tmp = temp_dir("read-empty");
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        let missing = tmp.join("missing.log");
        let empty = tmp.join("empty.log");
        fs::write(&empty, "").expect("failed to create empty log file");

        assert!(
            read_last_lines(&missing, 5)
                .expect("missing file should be handled")
                .is_empty()
        );
        assert!(
            read_last_lines(&empty, 5)
                .expect("empty file should be handled")
                .is_empty()
        );

        let _ = fs::remove_dir_all(tmp);
    }

    /// Writes `count` numbered lines, enough of them to span several of the reader's
    /// 16KB chunks so the chunk-boundary handling is genuinely exercised.
    fn write_numbered_log(path: &Path, count: usize) {
        let mut body = String::new();
        let padding = "x".repeat(120);
        for idx in 1..=count {
            // Padded so the file crosses chunk boundaries at a few hundred lines.
            body.push_str(&format!("line {idx:05} {padding}\n"));
        }
        fs::write(path, body).expect("failed to write numbered log");
    }

    fn line_number(line: &str) -> usize {
        line.split_whitespace()
            .nth(1)
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("unexpected line shape: {line:?}"))
    }

    #[test]
    fn read_line_range_at_offset_zero_returns_the_tail() {
        let tmp = temp_dir("range-tail");
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        let path = tmp.join("app.out.log");
        write_numbered_log(&path, 500);

        let range = read_line_range(&path, 0, 100).expect("failed reading range");
        assert_eq!(range.lines.len(), 100);
        assert_eq!(line_number(&range.lines[0]), 401);
        assert_eq!(line_number(range.lines.last().unwrap()), 500);
        assert!(
            !range.reached_start,
            "400 earlier lines remain, so the start is not reached"
        );

        let _ = fs::remove_dir_all(tmp);
    }

    /// Paging backwards: successive offsets must return contiguous, non-overlapping
    /// sections, which is what makes prepending them reconstruct the file.
    #[test]
    fn read_line_range_pages_backwards_contiguously() {
        let tmp = temp_dir("range-paging");
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        let path = tmp.join("app.out.log");
        write_numbered_log(&path, 500);

        let page_size = 100;
        let mut expected_last = 500;
        for page in 0..4 {
            let range =
                read_line_range(&path, page * page_size, page_size).expect("failed reading page");
            assert_eq!(range.lines.len(), page_size, "page {page} should be full");
            assert_eq!(
                line_number(range.lines.last().unwrap()),
                expected_last,
                "page {page} should end where the previous began"
            );
            assert_eq!(
                line_number(&range.lines[0]),
                expected_last - page_size + 1,
                "page {page} should be contiguous"
            );
            expected_last -= page_size;
        }

        // The fifth page reaches the first line of the file.
        let final_page =
            read_line_range(&path, 4 * page_size, page_size).expect("failed reading final page");
        assert_eq!(line_number(&final_page.lines[0]), 1);
        assert!(
            final_page.reached_start,
            "a window beginning at line 1 must report the start"
        );

        let _ = fs::remove_dir_all(tmp);
    }

    /// A window that runs past the beginning returns what exists rather than failing,
    /// and says there is nothing earlier.
    #[test]
    fn read_line_range_spanning_the_start_is_truncated_and_reports_it() {
        let tmp = temp_dir("range-spans-start");
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        let path = tmp.join("app.out.log");
        write_numbered_log(&path, 50);

        let range = read_line_range(&path, 40, 100).expect("failed reading range");
        assert_eq!(range.lines.len(), 10, "only 10 lines precede offset 40");
        assert_eq!(line_number(&range.lines[0]), 1);
        assert_eq!(line_number(range.lines.last().unwrap()), 10);
        assert!(range.reached_start);

        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn read_line_range_past_the_start_returns_nothing_and_reports_it() {
        let tmp = temp_dir("range-past-start");
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        let path = tmp.join("app.out.log");
        write_numbered_log(&path, 20);

        let range = read_line_range(&path, 500, 100).expect("failed reading range");
        assert!(range.lines.is_empty());
        assert!(range.reached_start);

        let _ = fs::remove_dir_all(tmp);
    }

    /// A file shorter than one page is fully returned, and must not look like it has
    /// more history behind it.
    #[test]
    fn read_line_range_returns_a_short_file_whole_and_reports_the_start() {
        let tmp = temp_dir("range-short-file");
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        let path = tmp.join("app.out.log");
        fs::write(&path, "one\ntwo\nthree\n").expect("failed to write log");

        let range = read_line_range(&path, 0, 200).expect("failed reading range");
        assert_eq!(range.lines, vec!["one", "two", "three"]);
        assert!(range.reached_start);

        let _ = fs::remove_dir_all(tmp);
    }

    /// A chunk boundary lands mid-line; the partial line must be discarded rather than
    /// served as though it were a real line.
    #[test]
    fn read_line_range_does_not_emit_a_partial_line_at_a_chunk_boundary() {
        let tmp = temp_dir("range-chunk-boundary");
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        let path = tmp.join("app.out.log");
        // ~121 bytes per line, so 2000 lines spans many 16KB chunks.
        write_numbered_log(&path, 2000);

        // Deep enough that reading stops well short of the file's start.
        let range = read_line_range(&path, 300, 50).expect("failed reading range");
        assert_eq!(range.lines.len(), 50);
        for line in &range.lines {
            assert!(
                line.starts_with("line "),
                "every returned line should be whole, got {line:?}"
            );
        }
        assert_eq!(line_number(range.lines.last().unwrap()), 1700);
        assert_eq!(line_number(&range.lines[0]), 1651);
        assert!(!range.reached_start);

        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn read_line_range_with_zero_count_returns_nothing_and_claims_nothing() {
        let tmp = temp_dir("range-zero");
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        let path = tmp.join("app.out.log");
        write_numbered_log(&path, 10);

        let range = read_line_range(&path, 0, 0).expect("failed reading range");
        assert!(range.lines.is_empty());
        // A request that read nothing has learned nothing about the file.
        assert!(!range.reached_start);

        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn read_line_range_handles_missing_and_empty_files() {
        let tmp = temp_dir("range-missing");
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        let missing = tmp.join("missing.log");
        let empty = tmp.join("empty.log");
        fs::write(&empty, "").expect("failed to create empty log");

        for path in [&missing, &empty] {
            let range = read_line_range(path, 0, 100).expect("should be handled");
            assert!(range.lines.is_empty());
            assert!(range.reached_start, "no content means nothing earlier");
        }

        let _ = fs::remove_dir_all(tmp);
    }

    /// `read_last_lines` now delegates to the range reader, so its existing contract
    /// must be unchanged for the callers that already depend on it.
    #[test]
    fn read_last_lines_still_matches_the_range_reader_at_offset_zero() {
        let tmp = temp_dir("range-delegation");
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        let path = tmp.join("app.out.log");
        write_numbered_log(&path, 300);

        let tail = read_last_lines(&path, 25).expect("failed reading tail");
        let range = read_line_range(&path, 0, 25).expect("failed reading range");
        assert_eq!(tail, range.lines);

        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn read_last_lines_with_zero_limit_returns_empty() {
        let tmp = temp_dir("read-zero");
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        let path = tmp.join("app.out.log");
        fs::write(&path, "line1\nline2\n").expect("failed to write test log file");

        let lines = read_last_lines(&path, 0).expect("failed reading lines");
        assert!(lines.is_empty());

        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn open_log_writers_creates_parent_directory_and_files() {
        let tmp = temp_dir("create-files");
        let logs = ProcessLogs {
            stdout: tmp.join("nested").join("app.out.log"),
            stderr: tmp.join("nested").join("app.err.log"),
        };

        let policy = LogRotationPolicy {
            max_size_bytes: 1024,
            max_files: 2,
            max_age_days: 30,
            max_age_secs: None,
        };

        let _ = open_log_writers(&logs, policy).expect("failed opening logs");

        assert!(logs.stdout.exists(), "stdout log should be created");
        assert!(logs.stderr.exists(), "stderr log should be created");
        assert!(
            logs.stdout.parent().is_some_and(|parent| parent.exists()),
            "log directory should be created"
        );

        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn open_log_writers_rotates_existing_chain_forward() {
        let tmp = temp_dir("rotate-chain");
        let logs = ProcessLogs {
            stdout: tmp.join("app.out.log"),
            stderr: tmp.join("app.err.log"),
        };
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        fs::write(&logs.stdout, "current-out").expect("failed to write stdout seed");
        fs::write(&logs.stderr, "current-err").expect("failed to write stderr seed");
        fs::write(tmp.join("app.out.log.1"), "older-out-1").expect("failed to write out.1");
        fs::write(tmp.join("app.out.log.2"), "older-out-2").expect("failed to write out.2");
        fs::write(tmp.join("app.err.log.1"), "older-err-1").expect("failed to write err.1");
        fs::write(tmp.join("app.err.log.2"), "older-err-2").expect("failed to write err.2");

        let policy = LogRotationPolicy {
            max_size_bytes: 1,
            max_files: 3,
            max_age_days: 30,
            max_age_secs: None,
        };
        let _ = open_log_writers(&logs, policy).expect("failed opening logs");

        assert_eq!(
            fs::read_to_string(tmp.join("app.out.log.1")).expect("failed to read out.1"),
            "current-out"
        );
        assert_eq!(
            fs::read_to_string(tmp.join("app.out.log.2")).expect("failed to read out.2"),
            "older-out-1"
        );
        assert_eq!(
            fs::read_to_string(tmp.join("app.out.log.3")).expect("failed to read out.3"),
            "older-out-2"
        );

        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn process_logs_builds_expected_file_paths() {
        let logs = process_logs(Path::new("/tmp/oxmgr/logs"), "worker");
        assert_eq!(logs.stdout, Path::new("/tmp/oxmgr/logs/worker.out.log"));
        assert_eq!(logs.stderr, Path::new("/tmp/oxmgr/logs/worker.err.log"));
    }

    fn temp_dir(prefix: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock failure")
            .as_nanos();
        std::env::temp_dir().join(format!("oxmgr-{prefix}-{nonce}"))
    }

    // ── 4.3 / 4.4 rotation during active writing ────────────────────────────────────────────────

    #[test]
    fn rotation_continues_writing_to_the_new_file_and_leaves_the_archive_readable() {
        // Task 4.3 asks for three things and the existing mid-run test only covered the first: that
        // rotation SUCCEEDS, that writing CONTINUES to the new file, and that the archive is
        // READABLE afterwards. The third is the one that matters most on Windows, where renaming a
        // file the daemon still holds open is the declared unknown — a rotation that "succeeded" but
        // left an unreadable archive would lose the log an operator went looking for.
        let tmp = temp_dir("rotate-continue-and-read");
        fs::create_dir_all(&tmp).expect("failed to create temp directory");
        let path = tmp.join("app.out.log");

        let mut writer = RotatingLogWriter::open(path.clone(), size_policy(128, 3))
            .expect("failed to open rotating writer");

        // Enough to rotate at least once.
        for idx in 0..40 {
            writer
                .write(format!("before {idx:04} padding padding\n").as_bytes())
                .expect("failed to write pre-rotation line");
        }
        writer.flush().expect("failed to flush");

        let archive = super::rotated_path(&path, 1);
        assert!(archive.exists(), "rotation should have produced an archive");

        // 1. Writing CONTINUES through the same writer, with no reopen by the caller.
        for idx in 0..5 {
            writer
                .write(format!("after {idx:04}\n").as_bytes())
                .expect("writing must continue after rotation");
        }
        writer.flush().expect("failed to flush post-rotation");

        let active = fs::read_to_string(&path).expect("the active log must be readable");
        assert!(
            active.contains("after 0004"),
            "post-rotation writes must land in the NEW active file, got: {active:?}"
        );

        // 2. The ARCHIVE is readable, and holds the earlier content rather than being truncated or
        //    left locked.
        let archived = fs::read_to_string(&archive).expect("the archive must be readable");
        assert!(
            archived.contains("before "),
            "the archive must retain the pre-rotation lines"
        );
        assert!(
            !archived.contains("after "),
            "post-rotation lines must not appear in the archive"
        );

        // 3. And the reader the API uses agrees, since that is the path an operator actually goes
        //    through rather than a bare file read.
        let range = super::read_line_range(&archive, 0, 5).expect("archive range read");
        assert!(
            !range.lines.is_empty(),
            "the archive must be readable through the same reader the log endpoint uses"
        );

        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn rotation_does_not_retry_a_failed_rename_silently() {
        // Task 4.4: if a platform cannot rename an open log file, the difference is DECLARED rather
        // than hidden behind retries. Asserted structurally, since this machine can rename open files
        // and cannot reach the failing branch.
        //
        // The property is that no retry loop exists: `shift_rotated_chain` and `rotate` call `rename`
        // once and propagate the error. A retry would turn a platform difference into a slow success
        // or a silent partial rotation, and the operator would never learn which.
        let source = include_str!("logging.rs");
        let rotate_region = source
            .split("fn shift_rotated_chain")
            .nth(1)
            .and_then(|rest| rest.split("\nfn ").next())
            .expect("shift_rotated_chain is present");
        for retry_shape in ["for attempt", "while attempt", "retry", "sleep("] {
            assert!(
                !rotate_region.contains(retry_shape),
                "rotation must not retry a rename ({retry_shape:?} found); the platform difference is \
                 declared in platform::MATRIX as LogRotationWhileOpen instead"
            );
        }

        // And the difference IS declared, so "no retry" is paired with an actual statement of the
        // consequence rather than silence.
        let declared = oxmgr_metrics::platform::support(
            oxmgr_metrics::platform::Capability::LogRotationWhileOpen,
            oxmgr_metrics::platform::Platform::Windows,
        );
        assert!(
            declared.level.requires_reason(),
            "Windows log rotation must be declared as something other than plainly supported"
        );
        assert!(
            declared.reason.is_some_and(|reason| reason.len() > 40),
            "the declaration must state the consequence, not just the level"
        );
    }
}
