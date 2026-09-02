//! Application-wide configuration derived from environment variables and local
//! filesystem conventions.
//!
//! Lint-level cleanup: display-path casts in config parsing and port hashing.

use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use oxmgr_manager::logging::LogRotationPolicy;

#[derive(Debug, Clone)]
/// Resolved runtime configuration for the local Oxmgr installation.
///
/// The values are loaded from environment variables where available and then
/// normalised into absolute paths and bind addresses used by the CLI and daemon.
pub struct AppConfig {
    pub base_dir: PathBuf,
    pub daemon_addr: String,
    pub api_addr: String,
    pub state_path: PathBuf,
    pub log_dir: PathBuf,
    pub log_rotation: LogRotationPolicy,
    /// Unix socket path for the streaming event bus (Unix only).
    #[cfg_attr(not(unix), allow(dead_code))]
    pub event_socket_path: PathBuf,
}

impl AppConfig {
    /// Loads configuration from the environment and creates the required
    /// directory layout if it does not already exist.
    ///
    /// Recognised environment variables:
    /// - `OXMGR_HOME`
    /// - `OXMGR_DAEMON_ADDR`
    /// - `OXMGR_API_ADDR`
    /// - `OXMGR_LOG_MAX_SIZE_MB`
    /// - `OXMGR_LOG_MAX_FILES`
    /// - `OXMGR_LOG_MAX_DAYS`
    pub fn load() -> Result<Self> {
        let base_dir = env::var("OXMGR_HOME")
            .map(PathBuf::from)
            .ok()
            .unwrap_or_else(|| {
                dirs::data_local_dir()
                    .unwrap_or_else(env::temp_dir)
                    .join("oxmgr")
            });
        let daemon_addr = env::var("OXMGR_DAEMON_ADDR")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| format!("127.0.0.1:{}", daemon_port()));
        let api_addr = env::var("OXMGR_API_ADDR")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| format!("127.0.0.1:{}", api_port()));
        let state_path = base_dir.join("state.json");
        let log_dir = base_dir.join("logs");
        let event_socket_path = base_dir.join("events.sock");
        let log_rotation = LogRotationPolicy {
            max_size_bytes: env_u64("OXMGR_LOG_MAX_SIZE_MB", 20)
                .max(1)
                .saturating_mul(1024 * 1024),
            max_files: env_u64("OXMGR_LOG_MAX_FILES", 5)
                .max(1)
                .try_into()
                .unwrap_or(u32::MAX),
            max_age_days: env_u64("OXMGR_LOG_MAX_DAYS", 14).max(1),
            // Time-based rotation is opt-in: absent or unparseable leaves size as
            // the only trigger rather than silently rotating on a guessed period.
            max_age_secs: rotate_interval_secs(),
        };

        let config = Self {
            base_dir,
            daemon_addr,
            api_addr,
            state_path,
            log_dir,
            log_rotation,
            event_socket_path,
        };
        config.ensure_layout()?;
        Ok(config)
    }

    /// Ensures the base directory and log directory exist with private
    /// permissions where the platform supports them.
    pub fn ensure_layout(&self) -> Result<()> {
        ensure_private_dir(&self.base_dir)?;
        ensure_private_dir(&self.log_dir)?;
        Ok(())
    }
    /// Returns the default health check URL, based on the API address.
    pub fn default_health_url(&self) -> String {
        format!("http://{}/health", self.api_addr)
    }
}

impl From<&AppConfig> for oxmgr_manager::process_manager::ManagerConfig {
    fn from(config: &AppConfig) -> Self {
        Self {
            base_dir: config.base_dir.clone(),
            state_path: config.state_path.clone(),
            log_dir: config.log_dir.clone(),
            log_rotation: config.log_rotation,
        }
    }
}

impl From<AppConfig> for oxmgr_manager::process_manager::ManagerConfig {
    fn from(config: AppConfig) -> Self {
        Self::from(&config)
    }
}

fn ensure_private_dir(path: &std::path::Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }
    Ok(())
}

fn daemon_port() -> u16 {
    let identity = current_identity();
    let mut hash = 2166136261_u32;
    for byte in identity.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(16777619);
    }

    // Keep daemon ports in a high, non-privileged range.
    let range = 20000_u16;
    40000 + u16::try_from(hash % u32::from(range)).unwrap_or(0)
}

fn api_port() -> u16 {
    let daemon = daemon_port();
    if daemon >= 59000 {
        daemon.saturating_sub(5000)
    } else {
        daemon.saturating_add(1000)
    }
}

fn current_identity() -> String {
    #[cfg(unix)]
    {
        format!("uid-{}", nix::unistd::Uid::effective().as_raw())
    }

    #[cfg(windows)]
    {
        let username = env::var("USERNAME").unwrap_or_else(|_| "unknown".to_string());
        format!("win-{username}")
    }

    #[cfg(not(any(unix, windows)))]
    {
        "oxmgr-generic".to_string()
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

/// Reads `OXMGR_LOG_ROTATE_INTERVAL` as a time-based rotation period.
///
/// Accepts a named period (`hourly`, `daily`, `weekly`) or a duration with a
/// unit suffix (`30m`, `12h`, `7d`); a bare number is read as seconds. Returns
/// `None` when unset, empty, `off`, or unparseable, which leaves size as the only
/// rotation trigger — a value we cannot interpret must not silently become a
/// rotation schedule.
pub(crate) fn parse_rotate_interval(raw: &str) -> Option<u64> {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    let value = raw.trim().to_ascii_lowercase();
    if value.is_empty() || value == "off" || value == "none" || value == "0" {
        return None;
    }
    match value.as_str() {
        "hourly" => return Some(HOUR),
        "daily" => return Some(DAY),
        "weekly" => return Some(7 * DAY),
        _ => {}
    }

    let (digits, unit) = value.split_at(
        value
            .find(|ch: char| !ch.is_ascii_digit())
            .unwrap_or(value.len()),
    );
    let amount = digits.parse::<u64>().ok()?;
    if amount == 0 {
        return None;
    }
    let multiplier = match unit.trim() {
        "" | "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => MINUTE,
        "h" | "hr" | "hrs" | "hour" | "hours" => HOUR,
        "d" | "day" | "days" => DAY,
        "w" | "week" | "weeks" => 7 * DAY,
        _ => return None,
    };
    amount.checked_mul(multiplier)
}

fn rotate_interval_secs() -> Option<u64> {
    env::var("OXMGR_LOG_ROTATE_INTERVAL")
        .ok()
        .as_deref()
        .and_then(parse_rotate_interval)
}

#[cfg(test)]
mod tests {
    use serial_test::serial;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{AppConfig, api_port, daemon_port, env_u64};

    #[test]
    fn daemon_port_is_stable_and_in_expected_range() {
        let first = daemon_port();
        let second = daemon_port();
        assert_eq!(first, second, "daemon port should be deterministic");
        assert!(
            (40000..60000).contains(&first),
            "daemon port should stay in non-privileged range, got {first}"
        );
    }

    #[test]
    fn api_port_is_stable_and_in_expected_range() {
        let first = api_port();
        let second = api_port();
        assert_eq!(first, second, "api port should be deterministic");
        assert!(
            (35000..60000).contains(&first),
            "api port should stay in high range, got {first}"
        );
    }

    #[test]
    #[serial]
    fn env_u64_uses_default_for_invalid_values() {
        let _guard = crate::test_utils::EnvGuard::set("OXMGR_TEST_ENV_U64", "not-a-number");

        let parsed = env_u64("OXMGR_TEST_ENV_U64", 42);
        assert_eq!(parsed, 42);
    }

    #[test]
    #[serial]
    fn env_u64_parses_trimmed_numeric_values() {
        let _guard = crate::test_utils::EnvGuard::set("OXMGR_TEST_ENV_U64", " 17 ");

        let parsed = env_u64("OXMGR_TEST_ENV_U64", 42);
        assert_eq!(parsed, 17);
    }

    #[test]
    #[serial]
    fn app_config_load_uses_env_and_creates_layout() {
        let base = temp_dir("config-load");
        let mut _guard =
            crate::test_utils::EnvGuard::set("OXMGR_HOME", &base.display().to_string());
        _guard
            .also_set("OXMGR_DAEMON_ADDR", " ")
            .also_set("OXMGR_API_ADDR", " ")
            .also_set("OXMGR_LOG_MAX_SIZE_MB", "0")
            .also_set("OXMGR_LOG_MAX_FILES", "0")
            .also_set("OXMGR_LOG_MAX_DAYS", "0");

        let config = AppConfig::load().expect("expected config load to succeed");
        assert_eq!(config.base_dir, base);
        assert_eq!(config.state_path, base.join("state.json"));
        assert_eq!(config.log_dir, base.join("logs"));
        assert_eq!(config.log_rotation.max_size_bytes, 1024 * 1024);
        assert_eq!(config.log_rotation.max_files, 1);
        assert_eq!(config.log_rotation.max_age_days, 1);
        assert!(
            config.daemon_addr.starts_with("127.0.0.1:"),
            "expected default daemon address, got {}",
            config.daemon_addr
        );
        assert!(
            config.api_addr.starts_with("127.0.0.1:"),
            "expected default API address, got {}",
            config.api_addr
        );
        assert!(config.base_dir.exists(), "base directory should be created");
        assert!(config.log_dir.exists(), "log directory should be created");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    #[serial]
    fn app_config_load_honors_explicit_addresses() {
        let base = temp_dir("config-explicit-addrs");
        let mut _guard =
            crate::test_utils::EnvGuard::set("OXMGR_HOME", &base.display().to_string());
        _guard
            .also_set("OXMGR_DAEMON_ADDR", "127.0.0.1:40123")
            .also_set("OXMGR_API_ADDR", "127.0.0.1:41123");

        let config = AppConfig::load().expect("expected config load to succeed");
        assert_eq!(config.daemon_addr, "127.0.0.1:40123");
        assert_eq!(config.api_addr, "127.0.0.1:41123");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn ensure_layout_creates_missing_directories() {
        let base = temp_dir("config-layout");
        let log_dir = base.join("custom-logs");
        let cfg = AppConfig {
            base_dir: base.clone(),
            daemon_addr: "127.0.0.1:50000".to_string(),
            api_addr: "127.0.0.1:51000".to_string(),
            state_path: base.join("state.json"),
            log_dir: log_dir.clone(),
            log_rotation: oxmgr_manager::logging::LogRotationPolicy {
                max_size_bytes: 1024,
                max_files: 3,
                max_age_days: 7,
                max_age_secs: None,
            },
            event_socket_path: base.join("events.sock"),
        };

        cfg.ensure_layout()
            .expect("expected ensure_layout to create directories");
        assert!(base.exists(), "base directory should exist");
        assert!(log_dir.exists(), "log directory should exist");

        let _ = fs::remove_dir_all(base);
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock failure")
            .as_nanos();
        std::env::temp_dir().join(format!("oxmgr-{prefix}-{nonce}"))
    }
}
