// Integration tests are exempt from the panic-freedom lints (§D3 of
// rust-panic-discipline): `unwrap()`/`expect()` is how a test fails, and this
// crate is test-only by construction — it never ships in the binary.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::let_underscore_must_use,
    reason = "test-only crate; a panicking assertion is the test failing loudly"
)]

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;
use serial_test::serial;

struct TestEnv {
    home: PathBuf,
    daemon_addr: String,
    /// The daemon's HTTP endpoint. Set explicitly rather than left to the default, so a
    /// test can reach the dashboard, log and metrics endpoints on a known port.
    api_addr: String,
}

static COMMAND_SEQ: AtomicU64 = AtomicU64::new(0);

impl TestEnv {
    fn new(prefix: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock failure")
            .as_nanos();
        let home = std::env::temp_dir().join(format!("oxmgr-e2e-{prefix}-{nonce}"));
        fs::create_dir_all(&home).expect("failed to create temporary home");

        let port = free_port();
        let api_port = free_port();

        Self {
            home,
            daemon_addr: format!("127.0.0.1:{port}"),
            api_addr: format!("127.0.0.1:{api_port}"),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, None)
    }

    fn run_in_dir(&self, args: &[&str], cwd: &Path) -> Output {
        self.run_with(args, Some(cwd))
    }

    fn run_with(&self, args: &[&str], cwd: Option<&Path>) -> Output {
        let bin = env!("CARGO_BIN_EXE_oxmgr");
        let command_id = COMMAND_SEQ.fetch_add(1, Ordering::Relaxed);
        let stdout_path = self.home.join(format!("cmd-{command_id}.stdout.log"));
        let stderr_path = self.home.join(format!("cmd-{command_id}.stderr.log"));
        let stdout_file = fs::File::create(&stdout_path).expect("failed to create stdout capture");
        let stderr_file = fs::File::create(&stderr_path).expect("failed to create stderr capture");

        let mut command = Command::new(bin);
        command
            .args(args)
            .env("OXMGR_HOME", &self.home)
            .env("OXMGR_DAEMON_ADDR", &self.daemon_addr)
            .env("OXMGR_API_ADDR", &self.api_addr)
            .env("OXMGR_LOG_MAX_SIZE_MB", "1")
            .env("OXMGR_LOG_MAX_FILES", "3")
            .env("OXMGR_LOG_MAX_DAYS", "1")
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file));
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }

        let mut child = command.spawn().expect("failed to spawn oxmgr command");

        let timeout = Duration::from_secs(60);
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => {
                    let status = child.wait().expect("failed to wait for oxmgr command");
                    return read_command_output(status, &stdout_path, &stderr_path);
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let status = child
                            .wait()
                            .expect("failed to wait for timed out oxmgr command");
                        let output = read_command_output(status, &stdout_path, &stderr_path);
                        panic!(
                            "oxmgr command timed out after {:?}: {:?}\nstdout:\n{}\nstderr:\n{}",
                            timeout,
                            args,
                            String::from_utf8_lossy(&output.stdout),
                            String::from_utf8_lossy(&output.stderr)
                        );
                    }
                    sleep(Duration::from_millis(100));
                }
                Err(err) => {
                    panic!("failed while waiting for oxmgr command {:?}: {err}", args);
                }
            }
        }
    }

    fn run_vec(&self, args: Vec<String>) -> Output {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&refs)
    }

    fn write_file(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.home.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("failed to create parent directory");
        }
        fs::write(&path, contents).expect("failed to write fixture file");
        path
    }
}

fn read_command_output(status: ExitStatus, stdout_path: &Path, stderr_path: &Path) -> Output {
    let stdout = fs::read(stdout_path).expect("failed to read captured stdout");
    let stderr = fs::read(stderr_path).expect("failed to read captured stderr");
    let _ = fs::remove_file(stdout_path);
    let _ = fs::remove_file(stderr_path);

    Output {
        status,
        stdout,
        stderr,
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = self.run(&["daemon", "stop"]);
        let _ = fs::remove_dir_all(&self.home);
    }
}

/// One HTTP response: status, headers (lowercased names) and body.
struct HttpReply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl HttpReply {
    fn header(&self, name: &str) -> Option<&str> {
        let wanted = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == wanted)
            .map(|(_, value)| value.as_str())
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|err| panic!("body was not JSON ({err}): {}", self.body))
    }
}

/// Issues `GET path` against the daemon's HTTP endpoint and reads the whole reply.
///
/// Deliberately no HTTP client dependency: the daemon closes the connection after each
/// response, so "read to end" is the whole protocol handling required here.
fn http_get(addr: &str, path: &str) -> HttpReply {
    http_get_limited(addr, path, usize::MAX)
}

/// Issues `POST path` with no body against the daemon's HTTP endpoint.
///
/// The daemon's mutating routes take their arguments in the path rather than a body, so there is
/// nothing to send — but the method still has to be POST, since the handler refuses anything else
/// with 405.
fn http_post(addr: &str, path: &str) -> HttpReply {
    http_request("POST", addr, path, usize::MAX)
}

/// As `http_get`, but stops after `max_bytes` of body. Streaming endpoints never close the
/// connection, so a full read would hang.
fn http_get_limited(addr: &str, path: &str, max_bytes: usize) -> HttpReply {
    http_request("GET", addr, path, max_bytes)
}

/// One request/response exchange.
///
/// Factored out when POST was needed rather than copied: fifty lines of socket handling duplicated
/// for a three-character difference is fifty lines that can drift apart.
fn http_request(method: &str, addr: &str, path: &str, max_bytes: usize) -> HttpReply {
    let mut stream =
        TcpStream::connect(addr).unwrap_or_else(|err| panic!("failed connecting to {addr}: {err}"));
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("failed setting read timeout");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .unwrap_or_else(|err| panic!("failed writing request for {path}: {err}"));

    let mut raw = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                raw.extend_from_slice(&chunk[..read]);
                // Enough of the body for the assertion: streaming endpoints would
                // otherwise never end.
                if raw.len() >= max_bytes {
                    break;
                }
            }
            // A timeout on a stream that never closes is the expected end of a bounded read.
            Err(_) => break,
        }
    }

    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = match text.find("\r\n\r\n") {
        Some(idx) => (text[..idx].to_string(), text[idx + 4..].to_string()),
        None => (text.clone(), String::new()),
    };

    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default().to_string();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("malformed status line: {status_line:?}"));

    let headers = lines
        .filter_map(|line| {
            line.split_once(':')
                .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();

    HttpReply {
        status,
        headers,
        body,
    }
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind random port");
    let port = listener
        .local_addr()
        .expect("failed to resolve local addr")
        .port();
    drop(listener);
    port
}

fn should_run_e2e(test_name: &str) -> bool {
    if std::env::var("OXMGR_RUN_E2E").ok().as_deref() == Some("1") {
        true
    } else {
        eprintln!("skipping {test_name} (set OXMGR_RUN_E2E=1 to run)");
        false
    }
}

fn wait_until<F>(timeout: Duration, mut predicate: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        sleep(Duration::from_millis(150));
    }
    predicate()
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn normalize_path_for_compare(path: &Path) -> String {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    normalize_path_text_for_compare(&canonical.to_string_lossy())
}

fn normalize_path_text_for_compare(value: &str) -> String {
    let trimmed = value.trim().trim_matches('"');
    let canonical = fs::canonicalize(trimmed).unwrap_or_else(|_| PathBuf::from(trimmed));
    let rendered = canonical.to_string_lossy();

    #[cfg(windows)]
    {
        rendered
            .trim_start_matches(r"\\?\")
            .replace('/', "\\")
            .to_ascii_lowercase()
    }

    #[cfg(not(windows))]
    {
        rendered.into_owned()
    }
}

fn logs_contain_cwd_env_marker(log_output: &str, expected_cwd: &str, env_value: &str) -> bool {
    log_output.lines().any(|line| {
        let Some((cwd, logged_env_value)) = line.rsplit_once('|') else {
            return false;
        };

        if logged_env_value.trim() != env_value {
            return false;
        }

        let raw_cwd = cwd.trim();
        if normalize_path_text_for_compare(raw_cwd) == expected_cwd {
            return true;
        }

        // Log lines may be prefixed as "<timestamp>: <payload>" when log_date_format is set.
        if let Some((_, prefixed_cwd)) = raw_cwd.split_once(": ") {
            return normalize_path_text_for_compare(prefixed_cwd) == expected_cwd;
        }

        false
    })
}

fn status_field_value<'a>(status_output: &'a str, field: &str) -> Option<&'a str> {
    status_output.lines().find_map(|line| {
        let (label, value) = line.split_once(':')?;
        (label.trim() == field).then_some(value.trim())
    })
}

fn escape_toml_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn output_contains(output: &Output, needle: &str) -> bool {
    String::from_utf8_lossy(&output.stdout).contains(needle)
        || String::from_utf8_lossy(&output.stderr).contains(needle)
}

#[cfg(windows)]
fn sleep_command(seconds: u64) -> String {
    format!("powershell -NoProfile -Command \"Start-Sleep -Seconds {seconds}\"")
}

#[cfg(not(windows))]
fn sleep_command(seconds: u64) -> String {
    format!("sh -c 'sleep {seconds}'")
}

#[cfg(windows)]
fn echo_and_sleep_command(marker: &str, seconds: u64) -> String {
    format!(
        "powershell -NoProfile -Command \"Write-Output {marker}; Start-Sleep -Seconds {seconds}\""
    )
}

#[cfg(not(windows))]
fn echo_and_sleep_command(marker: &str, seconds: u64) -> String {
    format!("sh -c 'echo {marker}; sleep {seconds}'")
}

#[cfg(windows)]
fn echo_two_lines_and_sleep_command(first: &str, second: &str, seconds: u64) -> String {
    format!(
        "powershell -NoProfile -Command \"Write-Output {first}; Write-Output {second}; Start-Sleep -Seconds {seconds}\""
    )
}

#[cfg(not(windows))]
fn echo_two_lines_and_sleep_command(first: &str, second: &str, seconds: u64) -> String {
    format!("sh -c 'printf \"%s\\n%s\\n\" \"{first}\" \"{second}\"; sleep {seconds}'")
}

#[cfg(windows)]
fn print_pwd_and_env_then_sleep_command(env_key: &str, seconds: u64) -> String {
    format!(
        "powershell -NoProfile -Command \"$cwd = (Get-Location).Path; $value = [Environment]::GetEnvironmentVariable('{env_key}'); Write-Output \\\"$cwd|$value\\\"; Start-Sleep -Seconds {seconds}\""
    )
}

#[cfg(not(windows))]
fn print_pwd_and_env_then_sleep_command(env_key: &str, seconds: u64) -> String {
    format!("sh -c 'printf \"%s|%s\\n\" \"$PWD\" \"${{{env_key}}}\"; sleep {seconds}'")
}

fn parse_pid_from_status(output: &str) -> Option<u32> {
    output.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.trim() != "PID" {
            return None;
        }
        let value = value.trim();
        if value == "-" {
            None
        } else {
            value.parse::<u32>().ok()
        }
    })
}

fn wait_for_pid(env: &TestEnv, target: &str, timeout: Duration) -> Option<u32> {
    let mut pid = None;
    let found = wait_until(timeout, || {
        let output = env.run(&["status", target]);
        if !output.status.success() {
            return false;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        pid = parse_pid_from_status(&stdout);
        pid.is_some()
    });

    if found { pid } else { None }
}

/// Whether a pid is still alive, by outcome rather than by mechanism.
///
/// Task 4.1 and 4.2 both ask for termination asserted BY OUTCOME on every platform, which rules out
/// checking that a particular signal was sent or that `taskkill /T` was invoked — those are the
/// mechanisms, and they are exactly what differs. "Is this pid gone" is the same question everywhere.
#[cfg(windows)]
fn pid_is_alive(pid: u32) -> bool {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .expect("failed to run tasklist");
    // tasklist exits 0 whether or not it matched, and prints an INFO line when it did not, so the
    // presence of the pid in stdout is the only reliable signal.
    String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
}

#[cfg(not(windows))]
fn pid_is_alive(pid: u32) -> bool {
    // `kill -0` tests for existence without delivering a signal. Non-zero also covers EPERM, which
    // would mean the pid exists but belongs to another user — impossible for a child this test
    // spawned, so treating it as alive would be the safer reading and treating it as dead is fine
    // here.
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Reads one labelled field out of `oxmgr status` output.
fn status_field(env: &TestEnv, target: &str, label: &str) -> Option<String> {
    let output = env.run(&["status", target]);
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    stdout.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == label).then(|| value.trim().to_string())
    })
}

#[cfg(windows)]
fn force_kill_pid(pid: u32) {
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status()
        .expect("failed to run taskkill");
    assert!(status.success(), "taskkill failed for pid {pid}: {status}");
}

#[cfg(not(windows))]
fn force_kill_pid(pid: u32) {
    let status = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("failed to run kill -9");
    assert!(status.success(), "kill -9 failed for pid {pid}: {status}");
}

#[test]
#[serial]
fn e2e_process_lifecycle() {
    if !should_run_e2e("e2e_process_lifecycle") {
        return;
    }

    let env = TestEnv::new("lifecycle");
    let command = sleep_command(15);

    let start = env.run_vec(vec![
        "start".to_string(),
        command,
        "--name".to_string(),
        "e2e".to_string(),
        "--restart".to_string(),
        "never".to_string(),
        "--stop-timeout".to_string(),
        "1".to_string(),
    ]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );

    let list = env.run(&["list"]);
    assert!(
        list.status.success(),
        "list failed: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        list_stdout.contains("e2e"),
        "unexpected list output: {list_stdout}"
    );

    let restart = env.run(&["restart", "e2e"]);
    assert!(
        restart.status.success(),
        "restart failed: {}",
        String::from_utf8_lossy(&restart.stderr)
    );

    let stop = env.run(&["stop", "e2e"]);
    assert!(
        stop.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );

    let delete = env.run(&["delete", "e2e"]);
    assert!(
        delete.status.success(),
        "delete failed: {}",
        String::from_utf8_lossy(&delete.stderr)
    );
}

#[test]
#[serial]
fn e2e_restart_all_and_config_lifecycle_targets() {
    if !should_run_e2e("e2e_restart_all_and_config_lifecycle_targets") {
        return;
    }

    let env = TestEnv::new("lifecycle-config-targets");
    let command = escape_toml_string(&sleep_command(25));
    let oxfile_path = env.write_file(
        "fixtures/oxfile.lifecycle.toml",
        &format!(
            r#"version = 1

[[apps]]
name = "web"
command = "{command}"
restart_policy = "never"
stop_timeout_secs = 1

[[apps]]
name = "worker"
command = "{command}"
restart_policy = "never"
stop_timeout_secs = 1
"#
        ),
    );

    let apply = env.run_vec(vec!["apply".to_string(), path_string(&oxfile_path)]);
    assert!(
        apply.status.success(),
        "apply failed: {}",
        String::from_utf8_lossy(&apply.stderr)
    );

    let first_web_pid =
        wait_for_pid(&env, "web", Duration::from_secs(8)).expect("expected web pid after apply");
    let first_worker_pid = wait_for_pid(&env, "worker", Duration::from_secs(8))
        .expect("expected worker pid after apply");

    let restart_all = env.run(&["restart", "all"]);
    assert!(
        restart_all.status.success(),
        "restart all failed: {}",
        String::from_utf8_lossy(&restart_all.stderr)
    );

    let web_restarted = wait_until(Duration::from_secs(8), || {
        let output = env.run(&["status", "web"]);
        output.status.success()
            && parse_pid_from_status(&String::from_utf8_lossy(&output.stdout))
                != Some(first_web_pid)
    });
    assert!(
        web_restarted,
        "expected web pid to change after restart all"
    );

    let worker_restarted = wait_until(Duration::from_secs(8), || {
        let output = env.run(&["status", "worker"]);
        output.status.success()
            && parse_pid_from_status(&String::from_utf8_lossy(&output.stdout))
                != Some(first_worker_pid)
    });
    assert!(
        worker_restarted,
        "expected worker pid to change after restart all"
    );

    let stop_from_config = env.run_vec(vec!["stop".to_string(), path_string(&oxfile_path)]);
    assert!(
        stop_from_config.status.success(),
        "stop from config failed: {}",
        String::from_utf8_lossy(&stop_from_config.stderr)
    );

    for name in ["web", "worker"] {
        let output = env.run(&["status", name]);
        assert!(
            output.status.success(),
            "status failed for {name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("Status:      stopped"),
            "expected {name} to be stopped, got:\n{stdout}"
        );
    }

    let delete_from_config = env.run_vec(vec!["delete".to_string(), path_string(&oxfile_path)]);
    assert!(
        delete_from_config.status.success(),
        "delete from config failed: {}",
        String::from_utf8_lossy(&delete_from_config.stderr)
    );

    for name in ["web", "worker"] {
        let output = env.run(&["status", name]);
        assert!(
            !output.status.success(),
            "expected {name} to be deleted, got stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
#[serial]
fn e2e_validate_oxfile() {
    if !should_run_e2e("e2e_validate_oxfile") {
        return;
    }

    let env = TestEnv::new("validate");
    let oxfile = format!(
        "{}/../../docs/examples/oxfile.web-stack.toml",
        env!("CARGO_MANIFEST_DIR")
    );

    let output = env.run(&["validate", &oxfile]);
    assert!(
        output.status.success(),
        "validate failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Config validation: OK") && stdout.contains("Format: oxfile.toml"),
        "unexpected validate output: {stdout}"
    );
}

#[test]
#[serial]
fn e2e_validate_accepts_ecosystem_json_input() {
    if !should_run_e2e("e2e_validate_accepts_ecosystem_json_input") {
        return;
    }

    let env = TestEnv::new("validate-ecosystem-json");
    let ecosystem_path = env.write_file(
        "ecosystem.config.json",
        r#"{"apps":[{"name":"api","script":"server.js"}]}"#,
    );
    let output = env.run_vec(vec!["validate".to_string(), path_string(&ecosystem_path)]);

    assert!(
        output.status.success(),
        "validate failed for ecosystem input: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Config validation: OK") && stdout.contains("Format: ecosystem config"),
        "unexpected validate output\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[serial]
fn e2e_validate_accepts_ecosystem_js_input() {
    if !should_run_e2e("e2e_validate_accepts_ecosystem_js_input") {
        return;
    }

    let env = TestEnv::new("validate-ecosystem-js");
    let ecosystem_path = env.write_file(
        "ecosystem.config.js",
        r#"
module.exports = {
  apps: [
    {
      name: "api",
      cmd: "node server.js",
      cwd: "/srv/api",
      watch: ["src"],
      ignore_watch: ["node_modules"],
      watch_delay: 1000,
      health_cmd: "curl -fsS http://127.0.0.1:3000/health",
      wait_ready: true,
      listen_timeout: 5000
    }
  ]
};
"#,
    );
    let output = env.run_vec(vec!["validate".to_string(), path_string(&ecosystem_path)]);

    assert!(
        output.status.success(),
        "validate failed for ecosystem js input: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Config validation: OK") && stdout.contains("Format: ecosystem config"),
        "unexpected validate output\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[serial]
fn e2e_convert_ecosystem_to_oxfile_and_validate() {
    if !should_run_e2e("e2e_convert_ecosystem_to_oxfile_and_validate") {
        return;
    }

    let env = TestEnv::new("convert");
    let ecosystem_payload = json!({
        "apps": [
            {
                "name": "converted-app",
                "cmd": sleep_command(20),
                "autorestart": false,
                "max_restarts": 0,
                "stop_timeout": 1
            }
        ]
    });
    let ecosystem_path = env.write_file(
        "fixtures/ecosystem.config.json",
        &serde_json::to_string_pretty(&ecosystem_payload)
            .expect("failed to serialize ecosystem fixture"),
    );
    let oxfile_path = env.home.join("fixtures/oxfile.converted.toml");

    let convert = env.run_vec(vec![
        "convert".to_string(),
        path_string(&ecosystem_path),
        "--out".to_string(),
        path_string(&oxfile_path),
    ]);
    assert!(
        convert.status.success(),
        "convert failed: {}",
        String::from_utf8_lossy(&convert.stderr)
    );

    let generated =
        fs::read_to_string(&oxfile_path).expect("converted oxfile should exist and be readable");
    assert!(
        generated.contains("version = 1") && generated.contains("name = \"converted-app\""),
        "unexpected converted oxfile:\n{generated}"
    );

    let validate = env.run_vec(vec!["validate".to_string(), path_string(&oxfile_path)]);
    assert!(
        validate.status.success(),
        "validate failed: {}",
        String::from_utf8_lossy(&validate.stderr)
    );
}

#[test]
#[serial]
fn e2e_apply_is_idempotent() {
    if !should_run_e2e("e2e_apply_is_idempotent") {
        return;
    }

    let env = TestEnv::new("apply-idempotent");
    let command = escape_toml_string(&sleep_command(25));
    let oxfile = format!(
        r#"version = 1

[[apps]]
name = "idempotent-app"
command = "{command}"
restart_policy = "never"
max_restarts = 0
stop_timeout_secs = 1
"#
    );
    let oxfile_path = env.write_file("fixtures/oxfile.idempotent.toml", &oxfile);

    let first_apply = env.run_vec(vec!["apply".to_string(), path_string(&oxfile_path)]);
    assert!(
        first_apply.status.success(),
        "first apply failed: {}",
        String::from_utf8_lossy(&first_apply.stderr)
    );
    let first_stdout = String::from_utf8_lossy(&first_apply.stdout);
    assert!(
        first_stdout.contains("Apply complete:") && first_stdout.contains("1 created"),
        "unexpected first apply output: {first_stdout}"
    );

    let second_apply = env.run_vec(vec!["apply".to_string(), path_string(&oxfile_path)]);
    assert!(
        second_apply.status.success(),
        "second apply failed: {}",
        String::from_utf8_lossy(&second_apply.stderr)
    );
    let second_stdout = String::from_utf8_lossy(&second_apply.stdout);
    assert!(
        second_stdout.contains("Apply complete:") && second_stdout.contains("1 unchanged"),
        "apply was not idempotent, output: {second_stdout}"
    );

    let _ = env.run(&["delete", "idempotent-app"]);
}

#[test]
#[serial]
fn e2e_apply_accepts_multiple_config_files() {
    if !should_run_e2e("e2e_apply_accepts_multiple_config_files") {
        return;
    }

    let env = TestEnv::new("apply-multi-file");
    let command = escape_toml_string(&sleep_command(25));
    let core_path = env.write_file(
        "fixtures/oxfile.core.toml",
        &format!(
            r#"version = 1

[[apps]]
name = "db"
command = "{command}"
restart_policy = "never"
stop_timeout_secs = 1
"#
        ),
    );
    let worker_path = env.write_file(
        "fixtures/oxfile.worker.toml",
        &format!(
            r#"version = 1

[[apps]]
name = "worker"
command = "{command}"
restart_policy = "never"
stop_timeout_secs = 1
depends_on = ["db"]
"#
        ),
    );

    let validate = env.run_vec(vec![
        "validate".to_string(),
        path_string(&core_path),
        path_string(&worker_path),
    ]);
    assert!(
        validate.status.success(),
        "validate failed: {}",
        String::from_utf8_lossy(&validate.stderr)
    );
    let validate_stdout = String::from_utf8_lossy(&validate.stdout);
    assert!(
        validate_stdout.contains("Config validation: OK")
            && validate_stdout.contains("Format: multiple configs")
            && validate_stdout.contains("Apps: 2")
            && validate_stdout.contains("Expanded Processes: 2"),
        "unexpected validate output: {validate_stdout}"
    );

    let apply = env.run_vec(vec![
        "apply".to_string(),
        path_string(&core_path),
        path_string(&worker_path),
    ]);
    assert!(
        apply.status.success(),
        "apply failed: {}",
        String::from_utf8_lossy(&apply.stderr)
    );
    let apply_stdout = String::from_utf8_lossy(&apply.stdout);
    assert!(
        apply_stdout.contains("Apply complete:") && apply_stdout.contains("2 created"),
        "unexpected apply output: {apply_stdout}"
    );

    wait_for_pid(&env, "db", Duration::from_secs(8)).expect("expected db pid after apply");
    wait_for_pid(&env, "worker", Duration::from_secs(8)).expect("expected worker pid after apply");

    let list = env.run(&["list"]);
    assert!(
        list.status.success(),
        "list failed: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        list_stdout.contains("db") && list_stdout.contains("worker"),
        "unexpected list output: {list_stdout}"
    );

    let _ = env.run(&["delete", "db"]);
    let _ = env.run(&["delete", "worker"]);
}

#[test]
#[serial]
fn e2e_reload_replaces_pid() {
    if !should_run_e2e("e2e_reload_replaces_pid") {
        return;
    }

    let env = TestEnv::new("reload");
    let command = sleep_command(30);
    let start = env.run_vec(vec![
        "start".to_string(),
        command,
        "--name".to_string(),
        "reload-app".to_string(),
        "--restart".to_string(),
        "never".to_string(),
        "--stop-timeout".to_string(),
        "1".to_string(),
    ]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );

    let old_pid = wait_for_pid(&env, "reload-app", Duration::from_secs(8))
        .expect("expected pid after starting process");

    let reload = env.run(&["reload", "reload-app"]);
    assert!(
        reload.status.success(),
        "reload failed: {}",
        String::from_utf8_lossy(&reload.stderr)
    );

    let mut new_pid = None;
    let replaced = wait_until(Duration::from_secs(8), || {
        let output = env.run(&["status", "reload-app"]);
        if !output.status.success() {
            return false;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        new_pid = parse_pid_from_status(&stdout);
        new_pid.is_some() && new_pid != Some(old_pid) && stdout.contains("Status:      running")
    });
    assert!(
        replaced,
        "expected reload to replace pid (old={old_pid}, new={new_pid:?})"
    );

    let _ = env.run(&["delete", "reload-app"]);
}

#[test]
#[serial]
fn e2e_crash_auto_restart_replaces_pid() {
    if !should_run_e2e("e2e_crash_auto_restart_replaces_pid") {
        return;
    }

    let env = TestEnv::new("crash-restart");
    let start = env.run_vec(vec![
        "start".to_string(),
        sleep_command(30),
        "--name".to_string(),
        "crash-app".to_string(),
        "--restart".to_string(),
        "on-failure".to_string(),
        "--max-restarts".to_string(),
        "5".to_string(),
        "--restart-delay".to_string(),
        "0".to_string(),
        "--stop-timeout".to_string(),
        "1".to_string(),
    ]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );

    let old_pid = wait_for_pid(&env, "crash-app", Duration::from_secs(8))
        .expect("expected crash-app pid after startup");
    force_kill_pid(old_pid);

    let mut new_pid = None;
    let mut last_status = String::new();
    let restarted = wait_until(Duration::from_secs(8), || {
        let output = env.run(&["status", "crash-app"]);
        if !output.status.success() {
            last_status = format!(
                "stdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return false;
        }

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        last_status = stdout.clone();
        new_pid = parse_pid_from_status(&stdout);
        new_pid.is_some() && new_pid != Some(old_pid) && stdout.contains("Status:      running")
    });
    assert!(
        restarted,
        "expected auto-restart to replace pid after crash (old={old_pid}, new={new_pid:?})\n{last_status}"
    );

    let _ = env.run(&["delete", "crash-app"]);
}

#[test]
#[serial]
fn e2e_logs_show_stdout_content() {
    if !should_run_e2e("e2e_logs_show_stdout_content") {
        return;
    }

    let env = TestEnv::new("logs");
    let marker = "OXMGR_E2E_LOG_MARKER";
    let command = echo_and_sleep_command(marker, 15);
    let start = env.run_vec(vec![
        "start".to_string(),
        command,
        "--name".to_string(),
        "logs-app".to_string(),
        "--restart".to_string(),
        "never".to_string(),
        "--stop-timeout".to_string(),
        "1".to_string(),
    ]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );

    let found = wait_until(Duration::from_secs(8), || {
        let logs = env.run(&["logs", "logs-app", "--lines", "50"]);
        if !logs.status.success() {
            return false;
        }
        let stdout = String::from_utf8_lossy(&logs.stdout);
        stdout.contains(marker)
    });
    assert!(found, "expected marker to be present in logs output");

    let _ = env.run(&["delete", "logs-app"]);
}

#[test]
#[serial]
fn e2e_apply_prune_removes_unmanaged_process() {
    if !should_run_e2e("e2e_apply_prune_removes_unmanaged_process") {
        return;
    }

    let env = TestEnv::new("apply-prune");

    let start_orphan = env.run_vec(vec![
        "start".to_string(),
        sleep_command(25),
        "--name".to_string(),
        "orphan-app".to_string(),
        "--restart".to_string(),
        "never".to_string(),
        "--stop-timeout".to_string(),
        "1".to_string(),
    ]);
    assert!(
        start_orphan.status.success(),
        "failed to start orphan app: {}",
        String::from_utf8_lossy(&start_orphan.stderr)
    );
    wait_for_pid(&env, "orphan-app", Duration::from_secs(8))
        .expect("expected orphan app pid after startup");

    let oxfile = format!(
        r#"version = 1

[[apps]]
name = "managed-app"
command = "{command}"
restart_policy = "never"
max_restarts = 0
stop_timeout_secs = 1
"#,
        command = escape_toml_string(&sleep_command(25))
    );
    let oxfile_path = env.write_file("fixtures/oxfile.prune.toml", &oxfile);

    let apply = env.run_vec(vec![
        "apply".to_string(),
        path_string(&oxfile_path),
        "--prune".to_string(),
    ]);
    assert!(
        apply.status.success(),
        "apply --prune failed: {}",
        String::from_utf8_lossy(&apply.stderr)
    );
    let apply_stdout = String::from_utf8_lossy(&apply.stdout);
    assert!(
        apply_stdout.contains("Apply complete:")
            && apply_stdout.contains("1 created")
            && apply_stdout.contains("1 pruned"),
        "unexpected apply --prune output: {apply_stdout}"
    );

    wait_for_pid(&env, "managed-app", Duration::from_secs(8))
        .expect("expected managed app pid after apply");

    let orphan_removed = wait_until(Duration::from_secs(8), || {
        let status = env.run(&["status", "orphan-app"]);
        !status.status.success()
    });
    assert!(orphan_removed, "expected orphan-app to be pruned");

    let list = env.run(&["list"]);
    assert!(
        list.status.success(),
        "list failed after apply --prune: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        list_stdout.contains("managed-app") && !list_stdout.contains("orphan-app"),
        "unexpected list output after prune: {list_stdout}"
    );

    let _ = env.run(&["delete", "managed-app"]);
}

#[test]
#[serial]
fn e2e_export_import_bundle_roundtrip() {
    if !should_run_e2e("e2e_export_import_bundle_roundtrip") {
        return;
    }

    let env = TestEnv::new("bundle-roundtrip");

    let start = env.run_vec(vec![
        "start".to_string(),
        sleep_command(25),
        "--name".to_string(),
        "bundle-app".to_string(),
        "--restart".to_string(),
        "never".to_string(),
        "--stop-timeout".to_string(),
        "1".to_string(),
        "--namespace".to_string(),
        "bundle-ns".to_string(),
        "--max-memory-mb".to_string(),
        "64".to_string(),
        "--max-cpu-percent".to_string(),
        "25".to_string(),
    ]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    wait_for_pid(&env, "bundle-app", Duration::from_secs(8))
        .expect("expected bundle-app pid after startup");

    let bundle_path = env.home.join("exports/bundle-app.oxpkg");
    let export = env.run_vec(vec![
        "export".to_string(),
        "bundle-app".to_string(),
        "--out".to_string(),
        path_string(&bundle_path),
    ]);
    assert!(
        export.status.success(),
        "export failed: {}",
        String::from_utf8_lossy(&export.stderr)
    );
    let export_stdout = String::from_utf8_lossy(&export.stdout);
    assert!(
        export_stdout.contains("Exported service bundle:")
            && export_stdout.contains(&path_string(&bundle_path)),
        "unexpected export output: {export_stdout}"
    );
    let bundle_bytes = fs::read(&bundle_path).expect("expected exported bundle to exist");
    assert!(
        !bundle_bytes.is_empty(),
        "expected exported bundle to contain data"
    );

    let delete_original = env.run(&["delete", "bundle-app"]);
    assert!(
        delete_original.status.success(),
        "failed to delete original bundle-app: {}",
        String::from_utf8_lossy(&delete_original.stderr)
    );

    let import = env.run_vec(vec!["import".to_string(), path_string(&bundle_path)]);
    assert!(
        import.status.success(),
        "import failed: {}",
        String::from_utf8_lossy(&import.stderr)
    );
    let import_stdout = String::from_utf8_lossy(&import.stdout);
    assert!(
        import_stdout.contains("Imported: 1 started, 0 failed"),
        "unexpected import output: {import_stdout}"
    );
    wait_for_pid(&env, "bundle-app", Duration::from_secs(8))
        .expect("expected bundle-app pid after import");

    let status = env.run(&["status", "bundle-app"]);
    assert!(
        status.status.success(),
        "status failed after import: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status_stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        status_stdout.contains("bundle-ns")
            && status_stdout.contains("memory=64 MB")
            && status_stdout.contains("cpu=25%")
            && status_stdout.contains("Policy:      never"),
        "unexpected imported status output:\n{status_stdout}"
    );

    let _ = env.run(&["delete", "bundle-app"]);
}

#[test]
#[serial]
fn e2e_start_applies_cwd_env_namespace_and_limits() {
    if !should_run_e2e("e2e_start_applies_cwd_env_namespace_and_limits") {
        return;
    }

    let env = TestEnv::new("start-options");
    let working_dir = env.home.join("workspace/service-a");
    fs::create_dir_all(&working_dir).expect("failed to create working directory fixture");

    let env_key = "OXMGR_E2E_MARKER";
    let env_value = "cwd-env-check";
    let start = env.run_vec(vec![
        "start".to_string(),
        print_pwd_and_env_then_sleep_command(env_key, 20),
        "--name".to_string(),
        "options-app".to_string(),
        "--restart".to_string(),
        "never".to_string(),
        "--stop-timeout".to_string(),
        "1".to_string(),
        "--cwd".to_string(),
        path_string(&working_dir),
        "--env".to_string(),
        format!("{env_key}={env_value}"),
        "--namespace".to_string(),
        "ops".to_string(),
        "--max-memory-mb".to_string(),
        "96".to_string(),
        "--max-cpu-percent".to_string(),
        "12".to_string(),
    ]);
    assert!(
        start.status.success(),
        "start with options failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    wait_for_pid(&env, "options-app", Duration::from_secs(8))
        .expect("expected options-app pid after startup");

    let status = env.run(&["status", "options-app"]);
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status_stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        status_stdout.contains("Namespace:") && status_stdout.contains("ops"),
        "namespace missing from status output:\n{status_stdout}"
    );
    let expected_cwd = normalize_path_for_compare(&working_dir);
    let actual_cwd = status_field_value(&status_stdout, "Working Dir")
        .expect("Working Dir missing from status output");
    assert!(
        normalize_path_text_for_compare(actual_cwd) == expected_cwd,
        "working dir missing from status output:\n{status_stdout}"
    );
    assert!(
        status_stdout.contains("Limits:")
            && status_stdout.contains("memory=96 MB")
            && status_stdout.contains("cpu=12%"),
        "limits missing from status output:\n{status_stdout}"
    );

    let mut last_logs = String::new();
    let found_log_line = wait_until(Duration::from_secs(8), || {
        let logs = env.run(&["logs", "options-app", "--lines", "50"]);
        if !logs.status.success() {
            last_logs = format!(
                "stdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&logs.stdout),
                String::from_utf8_lossy(&logs.stderr)
            );
            return false;
        }
        let stdout = String::from_utf8_lossy(&logs.stdout).into_owned();
        last_logs = format!(
            "stdout:\n{}\nstderr:\n{}",
            stdout,
            String::from_utf8_lossy(&logs.stderr)
        );
        logs_contain_cwd_env_marker(&stdout, &expected_cwd, env_value)
    });
    assert!(
        found_log_line,
        "expected cwd/env marker in logs for {expected_cwd}|{env_value}\n{last_logs}"
    );

    let _ = env.run(&["delete", "options-app"]);
}

#[test]
#[serial]
fn e2e_start_defaults_cwd_to_invocation_directory() {
    if !should_run_e2e("e2e_start_defaults_cwd_to_invocation_directory") {
        return;
    }

    let env = TestEnv::new("start-default-cwd");
    let working_dir = env.home.join("workspace/service-b");
    fs::create_dir_all(&working_dir).expect("failed to create working directory fixture");

    let env_key = "OXMGR_E2E_DEFAULT_CWD";
    let env_value = "cwd-default-check";
    let start = env.run_in_dir(
        &[
            "start",
            &print_pwd_and_env_then_sleep_command(env_key, 20),
            "--name",
            "default-cwd-app",
            "--restart",
            "never",
            "--stop-timeout",
            "1",
            "--env",
            &format!("{env_key}={env_value}"),
        ],
        &working_dir,
    );
    assert!(
        start.status.success(),
        "start with implicit cwd failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    wait_for_pid(&env, "default-cwd-app", Duration::from_secs(8))
        .expect("expected default-cwd-app pid after startup");

    let status = env.run(&["status", "default-cwd-app"]);
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status_stdout = String::from_utf8_lossy(&status.stdout);
    let expected_cwd = normalize_path_for_compare(&working_dir);
    let actual_cwd = status_field_value(&status_stdout, "Working Dir")
        .expect("Working Dir missing from status output");
    assert!(
        normalize_path_text_for_compare(actual_cwd) == expected_cwd,
        "working dir missing from status output:\n{status_stdout}"
    );

    let mut last_logs = String::new();
    let found_log_line = wait_until(Duration::from_secs(8), || {
        let logs = env.run(&["logs", "default-cwd-app", "--lines", "50"]);
        if !logs.status.success() {
            last_logs = format!(
                "stdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&logs.stdout),
                String::from_utf8_lossy(&logs.stderr)
            );
            return false;
        }
        let stdout = String::from_utf8_lossy(&logs.stdout).into_owned();
        last_logs = format!(
            "stdout:\n{}\nstderr:\n{}",
            stdout,
            String::from_utf8_lossy(&logs.stderr)
        );
        logs_contain_cwd_env_marker(&stdout, &expected_cwd, env_value)
    });
    assert!(
        found_log_line,
        "expected cwd/env marker in logs for {expected_cwd}|{env_value}\n{last_logs}"
    );

    let _ = env.run(&["delete", "default-cwd-app"]);
}

#[test]
#[serial]
#[cfg(not(windows))]
fn e2e_start_reuse_port_flag() {
    if !should_run_e2e("e2e_start_reuse_port_flag") {
        return;
    }

    let env = TestEnv::new("reuse-port");
    let start = env.run_vec(vec![
        "start".to_string(),
        sleep_command(20),
        "--name".to_string(),
        "reuse-port-app".to_string(),
        "--restart".to_string(),
        "never".to_string(),
        "--reuse-port".to_string(),
    ]);
    assert!(
        start.status.success(),
        "start with --reuse-port failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    wait_for_pid(&env, "reuse-port-app", Duration::from_secs(8))
        .expect("expected reuse-port-app pid after startup");

    let status = env.run(&["status", "reuse-port-app"]);
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status_stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        status_stdout.contains("Reuse Port") && status_stdout.contains("enabled"),
        "reuse port missing from status output:\n{status_stdout}"
    );

    let _ = env.run(&["delete", "reuse-port-app"]);
}

#[test]
#[serial]
#[cfg(not(windows))]
fn e2e_pre_reload_cmd_runs_on_reload() {
    if !should_run_e2e("e2e_pre_reload_cmd_runs_on_reload") {
        return;
    }

    let env = TestEnv::new("pre-reload-cmd");
    let marker_path = env.home.join("pre-reload/marker.txt");
    let marker_parent = marker_path.parent().expect("marker should have parent dir");
    fs::create_dir_all(marker_parent).expect("failed to create marker dir");
    if marker_path.exists() {
        fs::remove_file(&marker_path).expect("failed to cleanup marker file");
    }

    let pre_cmd = format!("sh -c \"echo pre_reload > {}\"", path_string(&marker_path));

    let start = env.run_vec(vec![
        "start".to_string(),
        sleep_command(20),
        "--name".to_string(),
        "pre-reload-app".to_string(),
        "--restart".to_string(),
        "never".to_string(),
        "--pre-reload-cmd".to_string(),
        pre_cmd,
    ]);
    assert!(
        start.status.success(),
        "start with --pre-reload-cmd failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    wait_for_pid(&env, "pre-reload-app", Duration::from_secs(8))
        .expect("expected pre-reload-app pid after startup");

    assert!(
        !marker_path.exists(),
        "marker file should not exist before reload"
    );

    let reload = env.run(&["reload", "pre-reload-app"]);
    assert!(
        reload.status.success(),
        "reload failed: {}",
        String::from_utf8_lossy(&reload.stderr)
    );

    let created = wait_until(Duration::from_secs(8), || marker_path.exists());
    assert!(created, "marker file was not created by pre_reload_cmd");

    let _ = env.run(&["delete", "pre-reload-app"]);
}

#[test]
#[serial]
#[cfg(windows)]
fn e2e_pre_reload_cmd_runs_on_reload_windows() {
    if !should_run_e2e("e2e_pre_reload_cmd_runs_on_reload_windows") {
        return;
    }

    let env = TestEnv::new("pre-reload-cmd-win");
    let marker_path = env.home.join("pre-reload/marker.txt");
    let marker_parent = marker_path.parent().expect("marker should have parent dir");
    fs::create_dir_all(marker_parent).expect("failed to create marker dir");
    if marker_path.exists() {
        fs::remove_file(&marker_path).expect("failed to cleanup marker file");
    }

    let marker_path_str = path_string(&marker_path);
    let pre_cmd = format!("echo pre_reload > {marker_path_str}");

    let start = env.run_vec(vec![
        "start".to_string(),
        sleep_command(20),
        "--name".to_string(),
        "pre-reload-app".to_string(),
        "--restart".to_string(),
        "never".to_string(),
        "--pre-reload-cmd".to_string(),
        pre_cmd,
    ]);
    assert!(
        start.status.success(),
        "start with --pre-reload-cmd failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    wait_for_pid(&env, "pre-reload-app", Duration::from_secs(8))
        .expect("expected pre-reload-app pid after startup");

    assert!(
        !marker_path.exists(),
        "marker file should not exist before reload"
    );

    let reload = env.run(&["reload", "pre-reload-app"]);
    assert!(
        reload.status.success(),
        "reload failed: {}",
        String::from_utf8_lossy(&reload.stderr)
    );

    let created = wait_until(Duration::from_secs(8), || marker_path.exists());
    assert!(created, "marker file was not created by pre_reload_cmd");

    let _ = env.run(&["delete", "pre-reload-app"]);
}

#[test]
#[serial]
fn e2e_validate_profile_and_only_reports_expanded_processes() {
    if !should_run_e2e("e2e_validate_profile_and_only_reports_expanded_processes") {
        return;
    }

    let env = TestEnv::new("validate-profile-only");
    let oxfile = format!(
        "{}/../../docs/examples/oxfile.profiles.toml",
        env!("CARGO_MANIFEST_DIR")
    );

    let output = env.run(&["validate", &oxfile, "--env", "prod", "--only", "api"]);
    assert!(
        output.status.success(),
        "validate with profile/only failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Config validation: OK")
            && stdout.contains("Profile: prod")
            && stdout.contains("Apps: 1")
            && stdout.contains("Format: oxfile.toml")
            && stdout.contains("Expanded Processes: 4"),
        "unexpected validate profile/only output:\n{stdout}"
    );
}

#[test]
#[serial]
fn e2e_validate_rejects_only_filter_without_matches() {
    if !should_run_e2e("e2e_validate_rejects_only_filter_without_matches") {
        return;
    }

    let env = TestEnv::new("validate-only-miss");
    let oxfile = format!(
        "{}/../../docs/examples/oxfile.profiles.toml",
        env!("CARGO_MANIFEST_DIR")
    );

    let output = env.run(&["validate", &oxfile, "--only", "missing-app"]);
    assert!(
        !output.status.success(),
        "validate unexpectedly succeeded for missing --only match"
    );
    assert!(
        output_contains(&output, "no apps matched --only filter (missing-app)"),
        "unexpected validate --only failure\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[serial]
fn e2e_validate_rejects_invalid_cluster_command() {
    if !should_run_e2e("e2e_validate_rejects_invalid_cluster_command") {
        return;
    }

    let env = TestEnv::new("validate-bad-cluster");
    let oxfile = env.write_file(
        "fixtures/oxfile.bad-cluster.toml",
        r#"version = 1

[[apps]]
name = "bad-cluster"
command = "python worker.py"
cluster_mode = true
cluster_instances = 2
"#,
    );

    let output = env.run_vec(vec!["validate".to_string(), path_string(&oxfile)]);
    assert!(
        !output.status.success(),
        "validate unexpectedly succeeded for invalid cluster command"
    );
    assert!(
        output_contains(&output, "cluster_mode but command is not Node.js"),
        "unexpected invalid cluster validation output\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[serial]
fn e2e_import_oxfile_profile_and_only_expands_instances() {
    if !should_run_e2e("e2e_import_oxfile_profile_and_only_expands_instances") {
        return;
    }

    let env = TestEnv::new("import-profile-only");
    let sleep = escape_toml_string(&sleep_command(25));
    let oxfile = format!(
        r#"version = 1

[[apps]]
name = "api"
command = "{sleep}"
restart_policy = "never"
max_restarts = 0
stop_timeout_secs = 1

[apps.profiles.prod]
instances = 2
namespace = "blue"

[apps.profiles.prod.env]
MODE = "prod"

[[apps]]
name = "worker"
command = "{sleep}"
restart_policy = "never"
max_restarts = 0
stop_timeout_secs = 1

[apps.profiles.prod]
disabled = true
"#
    );
    let oxfile_path = env.write_file("fixtures/oxfile.import-profile.toml", &oxfile);

    let import = env.run_vec(vec![
        "import".to_string(),
        path_string(&oxfile_path),
        "--env".to_string(),
        "prod".to_string(),
        "--only".to_string(),
        "api-0,api-1".to_string(),
    ]);
    assert!(
        import.status.success(),
        "import failed: {}",
        String::from_utf8_lossy(&import.stderr)
    );
    let import_stdout = String::from_utf8_lossy(&import.stdout);
    assert!(
        import_stdout.contains("Imported: 2 started, 0 failed"),
        "unexpected import output:\n{import_stdout}"
    );

    for target in ["api-0", "api-1"] {
        wait_for_pid(&env, target, Duration::from_secs(8))
            .unwrap_or_else(|| panic!("expected {target} pid after import"));
    }

    let list = env.run(&["list"]);
    assert!(
        list.status.success(),
        "list failed after import: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        list_stdout.contains("api-0")
            && list_stdout.contains("api-1")
            && !list_stdout.contains("worker"),
        "unexpected import list output:\n{list_stdout}"
    );

    let status = env.run(&["status", "api-0"]);
    assert!(
        status.status.success(),
        "status failed for imported api-0: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status_stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        status_stdout.contains("Namespace:") && status_stdout.contains("blue"),
        "expected namespace from profile in imported process status:\n{status_stdout}"
    );

    for target in ["api-0", "api-1"] {
        let _ = env.run(&["delete", target]);
    }
}

#[test]
#[serial]
fn e2e_export_rejects_existing_output_file() {
    if !should_run_e2e("e2e_export_rejects_existing_output_file") {
        return;
    }

    let env = TestEnv::new("export-existing-file");
    let start = env.run_vec(vec![
        "start".to_string(),
        sleep_command(25),
        "--name".to_string(),
        "export-app".to_string(),
        "--restart".to_string(),
        "never".to_string(),
        "--stop-timeout".to_string(),
        "1".to_string(),
    ]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    wait_for_pid(&env, "export-app", Duration::from_secs(8))
        .expect("expected export-app pid after startup");

    let bundle_path = env.write_file("exports/existing.oxpkg", "already here");
    let export = env.run_vec(vec![
        "export".to_string(),
        "export-app".to_string(),
        "--out".to_string(),
        path_string(&bundle_path),
    ]);
    assert!(
        !export.status.success(),
        "export unexpectedly succeeded despite existing output file"
    );
    assert!(
        output_contains(&export, "failed to create bundle file"),
        "unexpected export failure output\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&export.stdout),
        String::from_utf8_lossy(&export.stderr)
    );

    let _ = env.run(&["delete", "export-app"]);
}

#[test]
#[serial]
fn e2e_doctor_reports_running_daemon_and_processes() {
    if !should_run_e2e("e2e_doctor_reports_running_daemon_and_processes") {
        return;
    }

    let env = TestEnv::new("doctor");
    let start = env.run_vec(vec![
        "start".to_string(),
        sleep_command(20),
        "--name".to_string(),
        "doctor-app".to_string(),
        "--restart".to_string(),
        "never".to_string(),
        "--stop-timeout".to_string(),
        "1".to_string(),
    ]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    wait_for_pid(&env, "doctor-app", Duration::from_secs(8))
        .expect("expected doctor-app pid after startup");

    let doctor = env.run(&["doctor"]);
    assert!(
        doctor.status.success(),
        "doctor failed: {}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    let stdout = String::from_utf8_lossy(&doctor.stdout);
    assert!(
        stdout.contains("Oxmgr doctor")
            && stdout.contains("[OK] daemon_ping")
            && stdout.contains("[OK] daemon_list")
            && stdout.contains("1 managed process(es)"),
        "unexpected doctor output:\n{stdout}"
    );

    let _ = env.run(&["delete", "doctor-app"]);
}

#[test]
#[serial]
fn e2e_start_rejects_cluster_instances_without_cluster() {
    if !should_run_e2e("e2e_start_rejects_cluster_instances_without_cluster") {
        return;
    }

    let env = TestEnv::new("start-bad-cluster");
    let output = env.run_vec(vec![
        "start".to_string(),
        sleep_command(10),
        "--name".to_string(),
        "bad-cluster".to_string(),
        "--cluster-instances".to_string(),
        "2".to_string(),
    ]);

    assert!(
        !output.status.success(),
        "start unexpectedly succeeded without --cluster"
    );
    assert!(
        output_contains(&output, "--cluster-instances requires --cluster"),
        "unexpected cluster validation output\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[serial]
fn e2e_list_empty_prints_no_managed_processes() {
    if !should_run_e2e("e2e_list_empty_prints_no_managed_processes") {
        return;
    }

    let env = TestEnv::new("list-empty");
    let list = env.run(&["list"]);
    assert!(
        list.status.success(),
        "list failed on empty env: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        stdout.contains("No managed processes."),
        "unexpected empty list output:\n{stdout}"
    );
}

#[test]
#[serial]
fn e2e_list_json_emits_array_of_processes() {
    if !should_run_e2e("e2e_list_json_emits_array_of_processes") {
        return;
    }

    let env = TestEnv::new("list-json");

    // Empty environment: --json must emit a valid empty JSON array and exit cleanly.
    let empty = env.run(&["ls", "--json"]);
    assert!(
        empty.status.success(),
        "ls --json failed on empty env: {}",
        String::from_utf8_lossy(&empty.stderr)
    );
    let empty_stdout = String::from_utf8_lossy(&empty.stdout);
    let empty_value: serde_json::Value =
        serde_json::from_str(empty_stdout.trim()).expect("expected valid JSON for empty list");
    assert!(
        empty_value.is_array(),
        "expected JSON array for empty list, got:\n{empty_stdout}"
    );
    assert!(
        empty_value.as_array().unwrap().is_empty(),
        "expected empty JSON array, got:\n{empty_stdout}"
    );

    let start = env.run_vec(vec![
        "start".to_string(),
        sleep_command(25),
        "--name".to_string(),
        "json-app".to_string(),
        "--restart".to_string(),
        "never".to_string(),
        "--stop-timeout".to_string(),
        "1".to_string(),
    ]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    wait_for_pid(&env, "json-app", Duration::from_secs(8))
        .expect("expected json-app pid after startup");

    let list = env.run(&["ls", "--json"]);
    assert!(
        list.status.success(),
        "ls --json failed: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    let processes: Vec<serde_json::Value> =
        serde_json::from_str(&String::from_utf8_lossy(&list.stdout))
            .expect("expected stdout to be a JSON array of processes");

    let json_app = processes
        .iter()
        .find(|p| p.get("name").and_then(|v| v.as_str()) == Some("json-app"))
        .unwrap_or_else(|| panic!("json-app missing from JSON output:\n{processes:?}"));
    for field in [
        "id",
        "name",
        "command",
        "status",
        "pid",
        "cpu_percent",
        "memory_bytes",
        "restart_count",
        "cluster_mode",
        "health_status",
    ] {
        assert!(
            json_app.get(field).is_some(),
            "JSON process record missing field `{field}`: {json_app}"
        );
    }
    assert_eq!(
        json_app["name"].as_str(),
        Some("json-app"),
        "unexpected name field: {json_app}"
    );

    // Plain `ls` (no --json) still renders the human-readable table, not JSON.
    let table = env.run(&["ls"]);
    assert!(
        table.status.success(),
        "ls table failed: {}",
        String::from_utf8_lossy(&table.stderr)
    );
    let table_stdout = String::from_utf8_lossy(&table.stdout);
    assert!(
        table_stdout.contains("NAME") && table_stdout.contains("json-app"),
        "expected human-readable table headers and process name, got:\n{table_stdout}"
    );

    let _ = env.run(&["delete", "json-app"]);
}

#[test]
#[serial]
fn e2e_stop_clears_pid_and_marks_process_stopped() {
    if !should_run_e2e("e2e_stop_clears_pid_and_marks_process_stopped") {
        return;
    }

    let env = TestEnv::new("stop-status");
    let start = env.run_vec(vec![
        "start".to_string(),
        sleep_command(25),
        "--name".to_string(),
        "stop-app".to_string(),
        "--restart".to_string(),
        "never".to_string(),
        "--stop-timeout".to_string(),
        "1".to_string(),
    ]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    wait_for_pid(&env, "stop-app", Duration::from_secs(8))
        .expect("expected stop-app pid after startup");

    let stop = env.run(&["stop", "stop-app"]);
    assert!(
        stop.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(
        output_contains(&stop, "stopped stop-app"),
        "unexpected stop output\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&stop.stdout),
        String::from_utf8_lossy(&stop.stderr)
    );

    let status = env.run(&["status", "stop-app"]);
    assert!(
        status.status.success(),
        "status failed after stop: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.contains("Status:      stopped") && stdout.contains("PID:         -"),
        "unexpected stopped status output:\n{stdout}"
    );

    let list = env.run(&["list"]);
    assert!(
        list.status.success(),
        "list failed after stop: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        list_stdout.contains("stop-app") && list_stdout.contains("stopped"),
        "unexpected list output after stop:\n{list_stdout}"
    );

    let _ = env.run(&["delete", "stop-app"]);
}

#[test]
#[serial]
fn e2e_doctor_warns_when_daemon_not_running() {
    if !should_run_e2e("e2e_doctor_warns_when_daemon_not_running") {
        return;
    }

    let env = TestEnv::new("doctor-no-daemon");
    let doctor = env.run(&["doctor"]);
    assert!(
        doctor.status.success(),
        "doctor failed unexpectedly: {}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    let stdout = String::from_utf8_lossy(&doctor.stdout);
    assert!(
        stdout.contains("Oxmgr doctor")
            && stdout.contains("[WARN] daemon_ping")
            && stdout.contains("daemon not reachable")
            && stdout.contains("Summary:")
            && stdout.contains("warning(s), 0 failure(s)"),
        "unexpected doctor no-daemon output:\n{stdout}"
    );
}

#[test]
#[serial]
fn e2e_daemon_stop_reports_not_running_when_idle() {
    if !should_run_e2e("e2e_daemon_stop_reports_not_running_when_idle") {
        return;
    }

    let env = TestEnv::new("daemon-stop-idle");
    let output = env.run(&["daemon", "stop"]);
    assert!(
        output.status.success(),
        "daemon stop should succeed when idle: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Daemon is not running."),
        "unexpected daemon stop output:\n{stdout}"
    );
}

#[test]
#[serial]
fn e2e_apply_profile_with_all_apps_disabled_reports_no_apps() {
    if !should_run_e2e("e2e_apply_profile_with_all_apps_disabled_reports_no_apps") {
        return;
    }

    let env = TestEnv::new("apply-no-apps");
    let oxfile = env.write_file(
        "fixtures/oxfile.no-apps.toml",
        &format!(
            r#"version = 1

[[apps]]
name = "disabled-app"
command = "{command}"
restart_policy = "never"
max_restarts = 0
stop_timeout_secs = 1

[apps.profiles.prod]
disabled = true
"#,
            command = escape_toml_string(&sleep_command(10))
        ),
    );

    let apply = env.run_vec(vec![
        "apply".to_string(),
        path_string(&oxfile),
        "--env".to_string(),
        "prod".to_string(),
    ]);
    assert!(
        apply.status.success(),
        "apply should succeed when profile disables all apps: {}",
        String::from_utf8_lossy(&apply.stderr)
    );
    let stdout = String::from_utf8_lossy(&apply.stdout);
    assert!(
        stdout.contains("No apps found in"),
        "unexpected apply no-apps output:\n{stdout}"
    );
}

#[test]
#[serial]
fn e2e_logs_lines_returns_only_tail() {
    if !should_run_e2e("e2e_logs_lines_returns_only_tail") {
        return;
    }

    let env = TestEnv::new("logs-tail");
    let first = "OXMGR_E2E_FIRST";
    let second = "OXMGR_E2E_SECOND";
    let start = env.run_vec(vec![
        "start".to_string(),
        echo_two_lines_and_sleep_command(first, second, 15),
        "--name".to_string(),
        "tail-app".to_string(),
        "--restart".to_string(),
        "never".to_string(),
        "--stop-timeout".to_string(),
        "1".to_string(),
    ]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );

    let found = wait_until(Duration::from_secs(8), || {
        let logs = env.run(&["logs", "tail-app", "--lines", "1"]);
        if !logs.status.success() {
            return false;
        }
        let stdout = String::from_utf8_lossy(&logs.stdout);
        stdout.contains(second) && !stdout.contains(first)
    });
    assert!(found, "expected logs --lines 1 to show only the tail line");

    let _ = env.run(&["delete", "tail-app"]);
}

#[test]
#[serial]
fn e2e_missing_target_commands_report_process_not_found() {
    if !should_run_e2e("e2e_missing_target_commands_report_process_not_found") {
        return;
    }

    let env = TestEnv::new("missing-targets");
    let commands = [
        vec!["status".to_string(), "missing-app".to_string()],
        vec!["logs".to_string(), "missing-app".to_string()],
        vec!["stop".to_string(), "missing-app".to_string()],
        vec!["restart".to_string(), "missing-app".to_string()],
        vec!["reload".to_string(), "missing-app".to_string()],
        vec!["delete".to_string(), "missing-app".to_string()],
        vec!["export".to_string(), "missing-app".to_string()],
    ];

    for args in commands {
        let output = env.run_vec(args.clone());
        assert!(
            !output.status.success(),
            "command unexpectedly succeeded for missing target: {:?}",
            args
        );
        assert!(
            output_contains(&output, "process not found: missing-app"),
            "unexpected missing-target output for {:?}\nstdout:\n{}\nstderr:\n{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Exercises the daemon's whole HTTP surface against a real daemon over a real socket.
///
/// Consolidated into one test on purpose. Each `TestEnv` starts and stops a daemon, and the
/// e2e job runs `--test-threads=1` on Windows, so one test per endpoint would pay full
/// daemon startup a dozen times on the slowest runner. Related endpoints share one
/// lifecycle instead.
///
/// This closes the gap that prompted the coverage work: the suite had 33 tests and not one
/// real HTTP request, leaving the dashboard, log and metrics endpoints — the newest and
/// most platform-sensitive code — verified only by unit tests against a snapshot.
#[test]
#[serial]
fn e2e_http_surface_serves_dashboard_logs_and_metrics() {
    if !should_run_e2e("e2e_http_surface_serves_dashboard_logs_and_metrics") {
        return;
    }

    let env = TestEnv::new("http-surface");
    let marker = "OXMGR_E2E_HTTP_MARKER";
    let command = echo_and_sleep_command(marker, 30);
    let start = env.run_vec(vec![
        "start".to_string(),
        command,
        "--name".to_string(),
        "http-app".to_string(),
        "--restart".to_string(),
        "never".to_string(),
        "--stop-timeout".to_string(),
        "1".to_string(),
    ]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );

    let api = env.api_addr.clone();

    // The daemon binds its HTTP endpoint after the IPC one, so wait for it rather than
    // assuming it is up the moment `start` returned.
    let reachable = wait_until(Duration::from_secs(15), || TcpStream::connect(&api).is_ok());
    assert!(reachable, "daemon HTTP endpoint never became reachable");

    // Wait for the line to reach the log file, so the tail assertion is not racing the
    // writer's idle flush.
    let logged = wait_until(Duration::from_secs(15), || {
        let reply = http_get(&api, "/api/processes/http-app/logs?stream=stdout&lines=50");
        reply.status == 200 && reply.body.contains(marker)
    });
    assert!(logged, "log tail never reported the marker line");

    // --- dashboard page ---
    let page = http_get(&api, "/");
    assert_eq!(page.status, 200, "dashboard page should be served");
    assert!(
        page.body.contains("OxMgr Dashboard"),
        "dashboard body should be the rendered page"
    );
    assert!(
        !page.body.contains("{{OXMGR_"),
        "template tokens should have been substituted"
    );
    assert!(
        page.body.contains("href=\"/dashboard.css\"")
            && page.body.contains("src=\"/dashboard.js\""),
        "styles and script should be referenced by URL"
    );

    // --- config ---
    let config = http_get(&api, "/api/config");
    assert_eq!(config.status, 200);
    let config_body = config.json();
    assert!(
        config_body["log_tail_lines"].as_u64().is_some(),
        "config should carry the tail length the viewer uses: {}",
        config.body
    );

    // --- process list ---
    let list = http_get(&api, "/api/processes");
    assert_eq!(list.status, 200);
    let names: Vec<String> = list
        .json()
        .as_array()
        .expect("process list should be an array")
        .iter()
        .filter_map(|item| item["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        names.contains(&"http-app".to_string()),
        "process list should include the started process, got {names:?}"
    );

    // --- log tail, with the byte size the dashboard's estimate depends on ---
    let tail = http_get(&api, "/api/processes/http-app/logs?stream=stdout&lines=50");
    assert_eq!(tail.status, 200);
    let tail_body = tail.json();
    assert_eq!(tail_body["stream"], "stdout");
    assert!(
        tail_body["bytes"].as_u64().is_some(),
        "tail should report its measured byte size: {}",
        tail.body
    );

    // --- range read: the paging contract ---
    let ranged = http_get(
        &api,
        "/api/processes/http-app/logs?stream=stdout&lines=1&before=0",
    );
    assert_eq!(ranged.status, 200);
    assert!(
        ranged.json()["reached_start"].as_bool().is_some(),
        "a range read should say whether it reached the start of the file"
    );

    // --- unknown stream is refused rather than silently answered with stdout ---
    let bad_stream = http_get(&api, "/api/processes/http-app/logs?stream=error");
    assert_eq!(
        bad_stream.status, 400,
        "an unknown stream should be refused, got {}",
        bad_stream.body
    );

    // --- log file listing ---
    let files = http_get(&api, "/api/processes/http-app/logs/files");
    assert_eq!(files.status, 200);
    let listing = files.json();
    let entries = listing["files"]
        .as_array()
        .expect("listing should carry a files array");
    assert!(
        entries
            .iter()
            .any(|entry| entry["stream"] == "stdout" && entry["index"] == 0),
        "listing should include the active stdout log: {}",
        files.body
    );
    assert!(
        entries.iter().all(
            |entry| entry["size"].as_u64().is_some() && entry["modified_at"].as_u64().is_some()
        ),
        "every entry needs the size and mtime the dashboard renders: {}",
        files.body
    );

    // --- download, as an attachment ---
    let download = http_get(&api, "/api/processes/http-app/logs/download?stream=stdout");
    assert_eq!(download.status, 200);
    let disposition = download
        .header("content-disposition")
        .expect("download should be an attachment");
    assert!(
        disposition.starts_with("attachment;"),
        "unexpected disposition: {disposition}"
    );
    assert!(
        download.body.contains(marker),
        "download should carry the log's content"
    );

    // --- traversal attempt is refused ---
    let traversal = http_get(
        &api,
        "/api/processes/http-app/logs/download?stream=stdout&index=../../etc/passwd",
    );
    assert_eq!(
        traversal.status, 400,
        "a non-integer archive index must be refused, got {}",
        traversal.body
    );

    // --- standalone log page ---
    let log_page = http_get(&api, "/logs/http-app?stream=stdout");
    assert_eq!(log_page.status, 200);
    assert!(
        log_page.body.contains("OxMgr Dashboard"),
        "the log page serves the dashboard document"
    );
    let unknown_page = http_get(&api, "/logs/does-not-exist");
    assert_eq!(
        unknown_page.status, 404,
        "the log page should 404 for an unknown process"
    );

    // --- Prometheus metrics ---
    let metrics = http_get(&api, "/metrics");
    assert_eq!(metrics.status, 200);
    assert!(
        metrics
            .header("content-type")
            .is_some_and(|value| value.contains("text/plain")),
        "metrics should be served as Prometheus text"
    );
    for expected in [
        "# HELP oxmgr_managed_processes",
        "# TYPE oxmgr_managed_processes gauge",
        "oxmgr_process_up",
    ] {
        assert!(
            metrics.body.contains(expected),
            "metrics output missing {expected:?}:\n{}",
            metrics.body
        );
    }

    // --- streaming: read a bounded prefix, since these never close the connection ---
    let process_stream = http_get_limited(&api, "/api/processes/stream?interval_ms=200", 512);
    assert_eq!(process_stream.status, 200);
    assert!(
        process_stream
            .header("content-type")
            .is_some_and(|value| value.contains("text/event-stream")),
        "the process stream should be Server-Sent Events"
    );
    assert!(
        process_stream.body.contains("data: "),
        "the process stream should have pushed at least one frame:\n{}",
        process_stream.body
    );

    let log_stream = http_get_limited(
        &api,
        "/api/processes/http-app/logs/stream?stream=stdout",
        512,
    );
    assert_eq!(log_stream.status, 200);
    assert!(
        log_stream.body.contains("data: "),
        "the log stream should have pushed the existing tail:\n{}",
        log_stream.body
    );

    // --- unknown process is refused across the surface ---
    for path in [
        "/api/processes/missing-app/logs?stream=stdout",
        "/api/processes/missing-app/logs/files",
        "/api/processes/missing-app/logs/download?stream=stdout",
        // Added with the findings surface: an unknown process must be refused there too, or a typo
        // reads as "this process is healthy".
        "/api/processes/missing-app/findings",
        "/api/processes/missing-app/decisions",
    ] {
        let reply = http_get(&api, path);
        assert_eq!(
            reply.status, 404,
            "expected 404 for {path}, got {} with {}",
            reply.status, reply.body
        );
    }

    // --- the event stream (5.2) ---
    //
    // The third SSE endpoint, and the one the earlier version of this test missed. Read as a bounded
    // prefix like the others, since none of them close the connection.
    let event_stream = http_get_limited(&api, "/api/events/stream?subscribe=process:*", 256);
    assert_eq!(event_stream.status, 200);
    assert!(
        event_stream
            .header("content-type")
            .is_some_and(|value| value.contains("text/event-stream")),
        "the event stream should be Server-Sent Events, got {:?}",
        event_stream.header("content-type")
    );

    let host_stream = http_get_limited(&api, "/api/host/stream", 256);
    assert_eq!(host_stream.status, 200);
    assert!(
        host_stream
            .header("content-type")
            .is_some_and(|value| value.contains("text/event-stream")),
        "the host stream should be Server-Sent Events"
    );

    // --- the analysis and resource surfaces (5.1) ---
    //
    // Every one of these was added after this test was first written, so each is asserted here rather
    // than only in a unit test: a route can be correct in isolation and unreachable in a real daemon
    // — wrong prefix, missing auth branch, never wired into the match.
    let findings = http_get(&api, "/api/findings");
    assert_eq!(findings.status, 200, "findings: {}", findings.body);
    let findings_body = findings.json();
    for key in ["findings", "active", "total", "suppressed", "warming"] {
        assert!(
            !findings_body[key].is_null(),
            "findings response is missing {key:?}: {}",
            findings.body
        );
    }
    // Every finding carries a `guidance` field — null when the detector is unknown to this build,
    // otherwise an array of strings. The dashboard and the CLI render this field verbatim, so a
    // finding without it would render guidance-less on both surfaces while the local function that
    // used to exist is gone: the field is load-bearing, not cosmetic.
    if let Some(items) = findings_body["findings"].as_array() {
        for item in items {
            let guidance = &item["guidance"];
            assert!(
                guidance.is_null() || guidance.is_array(),
                "each finding must carry a guidance array or null, got: {}",
                item
            );
            if let Some(steps) = guidance.as_array() {
                assert!(
                    steps.iter().all(|s| s.is_string()),
                    "guidance steps must be strings: {}",
                    item
                );
            }
        }
    }

    let decisions = http_get(&api, "/api/decisions");
    assert_eq!(decisions.status, 200, "decisions: {}", decisions.body);
    assert_eq!(
        decisions.json()["acting_enabled"],
        false,
        "observe-only must be stated on the wire so a decision log is not misread as actions taken"
    );

    let typical = http_get(&api, "/api/typical");
    assert_eq!(typical.status, 200, "typical: {}", typical.body);
    let typical_body = typical.json();
    assert!(
        typical_body["processes"].is_array(),
        "typical should carry a process array: {}",
        typical.body
    );
    assert!(
        !typical_body["host"]["current"].is_null(),
        "typical should carry host-level figures alongside per-process ones"
    );

    let advisories = http_get(&api, "/api/advisories");
    assert_eq!(advisories.status, 200, "advisories: {}", advisories.body);
    let advisory_body = advisories.json();
    for key in [
        "processes",
        "total",
        "dismissed_total",
        "capacity_available",
    ] {
        assert!(
            !advisory_body[key].is_null(),
            "advisories response is missing {key:?}: {}",
            advisories.body
        );
    }

    // Host consumers: 200 once sampled, or 503 before the first sample. Both are correct, and an
    // empty listing is not — "sampling has not run yet" and "this host has no processes" are
    // different claims and the second is never true.
    let consumers = http_get(&api, "/api/host/consumers");
    assert!(
        consumers.status == 200 || consumers.status == 503,
        "host consumers should be served or reported unavailable, got {} with {}",
        consumers.status,
        consumers.body
    );
    if consumers.status == 200 {
        let body = consumers.json();
        assert!(
            body["by_cpu"].is_array() && body["by_memory"].is_array(),
            "consumers should carry both dimensions: {}",
            consumers.body
        );
        assert!(
            body["by_cpu_trees"].is_array() && body["by_memory_trees"].is_array(),
            "consumers should carry the attributed tree listings: {}",
            consumers.body
        );
        assert_eq!(
            body["command_lines_included"], false,
            "command lines must be redacted by default: another process's argv can carry credentials"
        );
    }

    // --- the findings series reach /metrics ---
    //
    // Re-fetched rather than reusing the earlier response, because the analysis series are rendered
    // from a snapshot that is only published after a maintenance tick.
    let metrics_again = http_get(&api, "/metrics");
    assert_eq!(metrics_again.status, 200);
    for expected in [
        "# TYPE oxmgr_finding_active gauge",
        "# TYPE oxmgr_findings_suppressed_total counter",
        "# TYPE oxmgr_process_baseline_warming gauge",
    ] {
        assert!(
            metrics_again.body.contains(expected),
            "metrics output missing {expected:?}; the analysis series are not reaching the scrape"
        );
    }

    // Every sample line must parse with a finite value. A NaN is accepted by a scrape and then
    // poisons an aggregation, so it is worth asserting on the whole body rather than spot-checking.
    for line in metrics_again.body.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((_, value)) = line.rsplit_once(' ') else {
            panic!("metrics line has no value: {line}");
        };
        let parsed: f64 = value
            .parse()
            .unwrap_or_else(|_| panic!("unparseable metrics value in: {line}"));
        assert!(parsed.is_finite(), "non-finite metrics value in: {line}");
    }

    // --- an advisory dismissal round-trips over HTTP ---
    let dismiss = http_post(
        &api,
        "/api/processes/http-app/dismiss/crash_loop_protection_disabled",
    );
    assert_eq!(
        dismiss.status, 200,
        "dismissing a known rule on a known process should succeed: {}",
        dismiss.body
    );
    let bad_rule = http_post(&api, "/api/processes/http-app/dismiss/not_a_real_rule");
    assert_eq!(
        bad_rule.status, 400,
        "an unknown rule must be refused rather than stored: {}",
        bad_rule.body
    );
    let blanket = http_post(&api, "/api/processes/all/dismiss/immediate_restart_loop");
    assert_eq!(
        blanket.status, 400,
        "dismissal is per process; `all` must be refused: {}",
        blanket.body
    );
}

/// Termination, orphans and log paths — the claimed-parity assertions (4.1, 4.2, 4.6).
///
/// One test and one daemon lifecycle, per task 5.3: Windows CI runs `--test-threads=1`, so three
/// separate tests would be three more daemon spawns for assertions that share a fixture.
///
/// Every assertion here is by OUTCOME rather than mechanism. Process-tree termination is declared
/// degraded on Windows and supported on Unix, and the mechanisms differ completely — `taskkill /T`
/// versus a process-group signal — so a test that checked the mechanism would have to be written twice
/// and would prove nothing about the result. "Is the child gone" is the same question on every
/// platform.
#[test]
#[serial]
fn e2e_termination_orphans_and_log_paths() {
    if !should_run_e2e("e2e_termination_orphans_and_log_paths") {
        return;
    }

    let env = TestEnv::new("parity");

    // ── 4.1 a managed process with children is fully stopped ────────────────────────────────────
    //
    // A parent that spawns a child and then waits. The child outlives its parent's own exit unless
    // something terminates the tree, which is precisely what is being asserted.
    #[cfg(windows)]
    let tree_command = "powershell -NoProfile -Command \"Start-Process -NoNewWindow powershell '-NoProfile','-Command','Start-Sleep -Seconds 120'; Start-Sleep -Seconds 120\"".to_string();
    #[cfg(not(windows))]
    let tree_command = "sh -c 'sleep 120 & echo child $!; wait'".to_string();

    let start = env.run_vec(vec![
        "start".to_string(),
        tree_command,
        "--name".to_string(),
        "tree-app".to_string(),
        "--restart".to_string(),
        "never".to_string(),
        "--stop-timeout".to_string(),
        "2".to_string(),
    ]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );

    let parent = wait_for_pid(&env, "tree-app", Duration::from_secs(15))
        .expect("the managed process never reported a pid");
    assert!(pid_is_alive(parent), "the parent should be running");

    let stop = env.run(&["stop", "tree-app"]);
    assert!(
        stop.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );

    // The parent must be gone. Asserted with a wait rather than immediately: stop is graceful first,
    // so the process gets its stop-timeout before being forced.
    let parent_gone = wait_until(Duration::from_secs(15), || !pid_is_alive(parent));
    assert!(
        parent_gone,
        "pid {parent} survived stop; termination did not take effect"
    );

    // ── 4.2 repeated restarts leave no orphans from previous incarnations ────────────────────────
    //
    // The failure this guards against is a restart that spawns a new incarnation without reaping the
    // old one, so each cycle leaks a process. Collecting every pid and asserting only the last is
    // alive is what catches it — checking the current pid alone would pass while N-1 orphans ran.
    let start = env.run_vec(vec![
        "start".to_string(),
        echo_and_sleep_command("OXMGR_PARITY_RESTART", 120),
        "--name".to_string(),
        "restart-app".to_string(),
        "--restart".to_string(),
        "never".to_string(),
        "--stop-timeout".to_string(),
        "2".to_string(),
    ]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );

    let mut incarnations = Vec::new();
    incarnations.push(
        wait_for_pid(&env, "restart-app", Duration::from_secs(15))
            .expect("restart-app never reported a pid"),
    );

    for cycle in 0..3 {
        let restart = env.run(&["restart", "restart-app"]);
        assert!(
            restart.status.success(),
            "restart {cycle} failed: {}",
            String::from_utf8_lossy(&restart.stderr)
        );
        let previous = *incarnations.last().expect("at least one incarnation");
        // Wait for the pid to actually CHANGE. Reading it immediately can return the old one and make
        // the orphan check trivially pass.
        let mut current = previous;
        let replaced = wait_until(Duration::from_secs(15), || {
            current = wait_for_pid(&env, "restart-app", Duration::from_secs(5)).unwrap_or(previous);
            current != previous
        });
        assert!(replaced, "restart {cycle} did not replace pid {previous}");
        incarnations.push(current);
    }

    let live = *incarnations.last().expect("at least one incarnation");
    for (index, pid) in incarnations.iter().enumerate() {
        if *pid == live {
            continue;
        }
        let reaped = wait_until(Duration::from_secs(15), || !pid_is_alive(*pid));
        assert!(
            reaped,
            "incarnation {index} (pid {pid}) is still alive after {} restarts; a restart left an \
             orphan",
            incarnations.len() - 1
        );
    }
    assert!(
        pid_is_alive(live),
        "the current incarnation (pid {live}) should be running"
    );

    // ── 4.6 output lands at the paths the daemon reports ─────────────────────────────────────────
    //
    // Not "a log file exists somewhere" — the daemon REPORTS a path, and output must be at that path.
    // A daemon writing correctly to a path it misreports is indistinguishable from a broken one to an
    // operator following the trail.
    let reported = status_field(&env, "restart-app", "Stdout Log")
        .expect("status must report the stdout log path");
    let path = PathBuf::from(&reported);
    assert!(
        path.is_absolute(),
        "the reported log path must be absolute so it is usable from any cwd: {reported}"
    );

    let landed = wait_until(Duration::from_secs(15), || {
        fs::read_to_string(&path)
            .map(|body| body.contains("OXMGR_PARITY_RESTART"))
            .unwrap_or(false)
    });
    assert!(
        landed,
        "the marker never appeared at the reported path {reported}"
    );

    // And the API agrees with the CLI about where it is: two surfaces reporting different paths for
    // one process is the drift this asserts against.
    let api = env.api_addr.clone();
    let reachable = wait_until(Duration::from_secs(15), || TcpStream::connect(&api).is_ok());
    assert!(reachable, "daemon HTTP endpoint never became reachable");
    let files = http_get(&api, "/api/processes/restart-app/logs/files");
    assert_eq!(files.status, 200);
    let listed = files.json()["files"]
        .as_array()
        .expect("listing carries a files array")
        .iter()
        .any(|entry| {
            entry["stream"] == "stdout"
                && entry["index"] == 0
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| entry["filename"] == name)
        });
    assert!(
        listed,
        "the API listing must name the same active stdout file the CLI reported ({reported}): {}",
        files.body
    );

    let cleanup = env.run(&["delete", "all"]);
    assert!(
        cleanup.status.success(),
        "cleanup failed: {}",
        String::from_utf8_lossy(&cleanup.stderr)
    );
}

#[test]
#[serial]
fn e2e_referenced_asset_route_test() {
    if !should_run_e2e("e2e_referenced_asset_route_test") {
        return;
    }

    let env = TestEnv::new("referenced-asset");
    // The daemon only binds its HTTP endpoint once something brings it up, so start a
    // trivial sleep-target and wait for the socket (same pattern as
    // e2e_http_surface_serves_dashboard_logs_and_metrics) instead of assuming an
    // unrelated test left one running on our random port.
    let command = echo_and_sleep_command("OXMGR_E2E_ASSET_ROUTE", 30);
    let start = env.run_vec(vec![
        "start".to_string(),
        command,
        "--name".to_string(),
        "asset-app".to_string(),
        "--restart".to_string(),
        "never".to_string(),
        "--stop-timeout".to_string(),
        "1".to_string(),
    ]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );

    let api = env.api_addr.clone();
    let reachable = wait_until(Duration::from_secs(15), || TcpStream::connect(&api).is_ok());
    assert!(reachable, "daemon HTTP endpoint never became reachable");

    // Every asset the dashboard document references must resolve to a route and never
    // return a not-found status (dashboard-interaction-safety: "Every referenced asset
    // has a route"). The asset set comes from the document itself, not from memory:
    // each `href`/`src` the page carries, minus the in-page `#process-list` anchor.
    let html = http_get(&api, "/").body;
    let mut referenced: Vec<String> = Vec::new();
    for cap in [r##"href="(/[^"#][^"]*)""##, r##"src="(/[^"][^"]*)""##] {
        for m in regex::Regex::new(cap).unwrap().captures_iter(&html) {
            let asset = m[1].to_string();
            if !referenced.contains(&asset) {
                referenced.push(asset);
            }
        }
    }
    assert!(
        !referenced.is_empty(),
        "dashboard document references no assets — the regex scan is broken"
    );

    let mut not_found: Vec<String> = Vec::new();
    for asset in &referenced {
        let reply = http_get(&api, asset);
        assert!(
            reply.status < 400,
            "referenced asset {asset} returns HTTP {}",
            reply.status
        );
        if reply.status == 404 {
            not_found.push(asset.clone());
        }
    }
    assert!(
        not_found.is_empty(),
        "referenced assets 404: {}",
        not_found.join(", ")
    );

    // And the css/js specifically must serve with their content types, since the page
    // hard-depends on both rendering paths.
    let reply = http_get(&api, "/dashboard.css");
    assert_eq!(reply.status, 200, "dashboard.css should be served");
    assert!(
        reply
            .header("content-type")
            .unwrap_or("")
            .contains("text/css")
    );
}
