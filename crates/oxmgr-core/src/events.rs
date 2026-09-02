//! Event bus types and subscription filter logic for the oxmgr streaming
//! event socket.
//!
//! Pure data only — this module is part of `oxmgr-core`, the zero-I/O layer.
//! The broadcast channel that carries these events is constructed on the
//! daemon side (`crates/oxmgr/src/events.rs`), which owns fan-out; nothing
//! below the daemon holds a channel sender.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Capacity of the internal broadcast channel. Owned as a constant here so
/// producers and the daemon-side bus constructor agree on one number without
/// core depending on any runtime.
pub const BUS_CAPACITY: usize = 512;

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Payload types
// ---------------------------------------------------------------------------

/// Process identity and static metadata attached to every process-scoped bus event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventProcessInfo {
    pub id: u64,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// The executable (and arguments) that was launched.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command: String,
    /// Working directory the process was started in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

// `From<&ManagedProcess> for EventProcessInfo` is implemented on the side
// that owns `ManagedProcess` (the manager layer), not here: core must not
// know process-runtime types, and the orphan rule keeps the impl with the
// local type anyway.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessLifecycleData {
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessExitData {
    pub exit_code: Option<i32>,
    /// POSIX signal name if the process was killed by a signal (e.g. `"SIGSEGV"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    /// How long the process ran before exiting (seconds).
    pub uptime_secs: u64,
    pub restart_count: u32,
    /// Last lines written to stderr before the process exited.
    /// Captures stack traces, panic output, etc. for any language.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stderr_tail: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessRestartData {
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    pub uptime_secs: u64,
    pub restart_count: u32,
    pub delay_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogData {
    pub line: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthData {
    pub error: String,
    pub failures: u32,
    /// Resource findings active for this process when the health check failed.
    ///
    /// Rendered as finding keys (`process/detector/metric[/variant]`) rather than whole findings:
    /// the event stream is not the place to republish evidence, and a key is enough for a consumer
    /// to fetch the finding it names.
    ///
    /// Empty means no resource finding was active — which is itself the answer to "was this health
    /// failure caused by resource pressure": no. `skip_serializing_if` keeps the common healthy case
    /// off the wire entirely, so this field costs nothing when there is nothing to say.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resource_findings: Vec<String>,
}

/// Payload for `anomaly:detected` and `anomaly:cleared`.
///
/// Carries the finding's identity and enough of its shape to be actionable without a second
/// request, but NOT its whole evidence: the samples, the baseline and the confidence terms belong
/// behind the findings endpoint. An event stream that republishes every sample of every finding
/// becomes the thing operators turn off.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnomalyData {
    /// `"{process}/{detector}/{metric}[/{variant}]#{occurrence}"`. Stable for the life of an
    /// episode, so a consumer can correlate a `cleared` with the `detected` that opened it.
    pub id: String,
    /// The stable key without the occurrence suffix, for fetching the finding.
    pub key: String,
    /// Which detector produced it.
    pub detector: String,
    /// Which metric it is about.
    pub metric: String,
    /// Confidence 0..=1, so a consumer can filter without re-deriving it from evidence.
    pub confidence: f64,
    /// Which episode of this condition, from 1. Above 1 is a recurrence rather than a first sighting.
    pub occurrence: u32,
    /// One operator-facing sentence. A courtesy, never the only description — the evidence behind
    /// the endpoint is the machine-readable form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Payload for `remediation:decided`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemediationData {
    /// The rule that matched, by wire name.
    pub rule: String,
    /// The action that would be taken, or `None` when the matching rule declined to act.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Why no action was proposed, when a rule matched and withheld.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withheld: Option<String>,
    /// Whether an action was actually taken. Always false while protection mode is observing, which
    /// is the default.
    pub acted: bool,
    /// Keys of the findings that satisfied the rule.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Main event enum
// ---------------------------------------------------------------------------

/// All events that can be emitted on the event bus.
///
/// Serialised as newline-delimited JSON with an `"event"` discriminant field,
/// e.g. `{"event":"process:crashed","at":1714567890,"process":{...},"data":{...}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum BusEvent {
    /// Daemon has begun spawning the process.
    #[serde(rename = "process:started")]
    ProcessStarted {
        at: u64,
        process: EventProcessInfo,
        data: ProcessLifecycleData,
    },
    /// Process is confirmed running (readiness check passed when configured).
    #[serde(rename = "process:online")]
    ProcessOnline {
        at: u64,
        process: EventProcessInfo,
        data: ProcessLifecycleData,
    },
    /// Process was explicitly stopped by the user or daemon.
    #[serde(rename = "process:stopped")]
    ProcessStopped {
        at: u64,
        process: EventProcessInfo,
        data: ProcessLifecycleData,
    },
    /// Process exited cleanly (exit code 0) and will not be restarted.
    #[serde(rename = "process:exited")]
    ProcessExited {
        at: u64,
        process: EventProcessInfo,
        data: ProcessExitData,
    },
    /// Process exited with a non-zero code and restart policy does not apply.
    #[serde(rename = "process:crashed")]
    ProcessCrashed {
        at: u64,
        process: EventProcessInfo,
        data: ProcessExitData,
    },
    /// Process exited; daemon is waiting before the next restart attempt.
    #[serde(rename = "process:restarting")]
    ProcessRestarting {
        at: u64,
        process: EventProcessInfo,
        data: ProcessRestartData,
    },
    /// Crash-loop limit reached; daemon will not retry automatically.
    #[serde(rename = "process:errored")]
    ProcessErrored {
        at: u64,
        process: EventProcessInfo,
        data: ProcessLifecycleData,
    },
    /// One line from the managed process's stdout.
    #[serde(rename = "log:out")]
    LogOut {
        at: u64,
        process: EventProcessInfo,
        data: LogData,
    },
    /// One line from the managed process's stderr.
    #[serde(rename = "log:err")]
    LogErr {
        at: u64,
        process: EventProcessInfo,
        data: LogData,
    },
    /// Health check passed.
    #[serde(rename = "health:healthy")]
    HealthHealthy { at: u64, process: EventProcessInfo },
    /// Health check failed.
    #[serde(rename = "health:unhealthy")]
    HealthUnhealthy {
        at: u64,
        process: EventProcessInfo,
        data: HealthData,
    },
    /// A finding was raised: a condition now holds that did not before.
    ///
    /// Raised once per episode, never re-emitted while the condition holds. That is the whole point
    /// of the registry's identity model — an event per tick for an ongoing condition is noise an
    /// operator learns to filter, at which point the real one is lost too.
    #[serde(rename = "anomaly:detected")]
    AnomalyDetected {
        at: u64,
        process: EventProcessInfo,
        data: AnomalyData,
    },
    /// A finding cleared: the condition stopped holding.
    ///
    /// Emitted so "this resolved" is observable. Without it a consumer cannot distinguish a
    /// condition that ended from one it simply stopped hearing about.
    #[serde(rename = "anomaly:cleared")]
    AnomalyCleared {
        at: u64,
        process: EventProcessInfo,
        data: AnomalyData,
    },
    /// A decision was reached. Whether or not anything acted.
    ///
    /// Published in observe-only mode too, which is what lets an operator watch what protection mode
    /// WOULD do before enabling it.
    #[serde(rename = "remediation:decided")]
    RemediationDecided {
        at: u64,
        process: EventProcessInfo,
        data: RemediationData,
    },
    /// The daemon is shutting down.
    #[serde(rename = "daemon:shutdown")]
    DaemonShutdown { at: u64 },
}

impl BusEvent {
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn event_name(&self) -> &'static str {
        match self {
            BusEvent::ProcessStarted { .. } => "process:started",
            BusEvent::ProcessOnline { .. } => "process:online",
            BusEvent::ProcessStopped { .. } => "process:stopped",
            BusEvent::ProcessExited { .. } => "process:exited",
            BusEvent::ProcessCrashed { .. } => "process:crashed",
            BusEvent::ProcessRestarting { .. } => "process:restarting",
            BusEvent::ProcessErrored { .. } => "process:errored",
            BusEvent::LogOut { .. } => "log:out",
            BusEvent::LogErr { .. } => "log:err",
            BusEvent::HealthHealthy { .. } => "health:healthy",
            BusEvent::HealthUnhealthy { .. } => "health:unhealthy",
            BusEvent::AnomalyDetected { .. } => "anomaly:detected",
            BusEvent::AnomalyCleared { .. } => "anomaly:cleared",
            BusEvent::RemediationDecided { .. } => "remediation:decided",
            BusEvent::DaemonShutdown { .. } => "daemon:shutdown",
        }
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn process_name(&self) -> Option<&str> {
        match self {
            BusEvent::ProcessStarted { process, .. }
            | BusEvent::ProcessOnline { process, .. }
            | BusEvent::ProcessStopped { process, .. }
            | BusEvent::ProcessExited { process, .. }
            | BusEvent::ProcessCrashed { process, .. }
            | BusEvent::ProcessRestarting { process, .. }
            | BusEvent::ProcessErrored { process, .. }
            | BusEvent::LogOut { process, .. }
            | BusEvent::LogErr { process, .. }
            | BusEvent::HealthHealthy { process, .. }
            | BusEvent::HealthUnhealthy { process, .. }
            // Per-process like the rest, so the existing `process` filter narrows them with no
            // change to the filter itself — which is what "following existing naming and
            // filtering" means in practice.
            | BusEvent::AnomalyDetected { process, .. }
            | BusEvent::AnomalyCleared { process, .. }
            | BusEvent::RemediationDecided { process, .. } => Some(&process.name),
            BusEvent::DaemonShutdown { .. } => None,
        }
    }

    // --- convenience constructors ---

    pub fn process_started(process: EventProcessInfo) -> Self {
        Self::ProcessStarted {
            at: now_epoch_secs(),
            data: ProcessLifecycleData {
                status: "starting".into(),
            },
            process,
        }
    }

    pub fn process_online(process: EventProcessInfo) -> Self {
        Self::ProcessOnline {
            at: now_epoch_secs(),
            data: ProcessLifecycleData {
                status: "running".into(),
            },
            process,
        }
    }

    pub fn process_stopped(process: EventProcessInfo) -> Self {
        Self::ProcessStopped {
            at: now_epoch_secs(),
            data: ProcessLifecycleData {
                status: "stopped".into(),
            },
            process,
        }
    }

    pub fn process_exited(
        process: EventProcessInfo,
        exit_code: Option<i32>,
        signal: Option<String>,
        uptime_secs: u64,
        restart_count: u32,
        stderr_tail: Vec<String>,
    ) -> Self {
        Self::ProcessExited {
            at: now_epoch_secs(),
            data: ProcessExitData {
                exit_code,
                signal,
                uptime_secs,
                restart_count,
                stderr_tail,
            },
            process,
        }
    }

    pub fn process_crashed(
        process: EventProcessInfo,
        exit_code: Option<i32>,
        signal: Option<String>,
        uptime_secs: u64,
        restart_count: u32,
        stderr_tail: Vec<String>,
    ) -> Self {
        Self::ProcessCrashed {
            at: now_epoch_secs(),
            data: ProcessExitData {
                exit_code,
                signal,
                uptime_secs,
                restart_count,
                stderr_tail,
            },
            process,
        }
    }

    pub fn process_restarting(
        process: EventProcessInfo,
        exit_code: Option<i32>,
        signal: Option<String>,
        uptime_secs: u64,
        restart_count: u32,
        delay_secs: u64,
    ) -> Self {
        Self::ProcessRestarting {
            at: now_epoch_secs(),
            data: ProcessRestartData {
                exit_code,
                signal,
                uptime_secs,
                restart_count,
                delay_secs,
            },
            process,
        }
    }

    pub fn process_errored(process: EventProcessInfo) -> Self {
        Self::ProcessErrored {
            at: now_epoch_secs(),
            data: ProcessLifecycleData {
                status: "errored".into(),
            },
            process,
        }
    }

    pub fn log_out(process: EventProcessInfo, line: String) -> Self {
        Self::LogOut {
            at: now_epoch_secs(),
            data: LogData { line },
            process,
        }
    }

    pub fn log_err(process: EventProcessInfo, line: String) -> Self {
        Self::LogErr {
            at: now_epoch_secs(),
            data: LogData { line },
            process,
        }
    }

    pub fn health_healthy(process: EventProcessInfo) -> Self {
        Self::HealthHealthy {
            at: now_epoch_secs(),
            process,
        }
    }

    /// A health check failed, with no resource finding active.
    ///
    /// The correlating form is [`Self::health_unhealthy_with_findings`], which is what the
    /// supervision path now calls. This shorter form is kept for tests and for any caller that has
    /// no findings to join — a health failure with nothing implicated reports alone, and expressing
    /// that as `Vec::new()` at every call site would be noise.
    pub fn health_unhealthy(process: EventProcessInfo, error: String, failures: u32) -> Self {
        Self::health_unhealthy_with_findings(process, error, failures, Vec::new())
    }

    /// A health check failed, carrying the resource findings active at that moment.
    ///
    /// The join exists so an operator can tell a health failure caused by resource pressure from one
    /// caused by something else — the two need different responses, and a bare "health check failed"
    /// forces them to go and correlate by hand against a dashboard that may have moved on.
    pub fn health_unhealthy_with_findings(
        process: EventProcessInfo,
        error: String,
        failures: u32,
        resource_findings: Vec<String>,
    ) -> Self {
        Self::HealthUnhealthy {
            at: now_epoch_secs(),
            data: HealthData {
                error,
                failures,
                resource_findings,
            },
            process,
        }
    }

    /// A finding was raised or re-raised.
    pub fn anomaly_detected(process: EventProcessInfo, data: AnomalyData) -> Self {
        Self::AnomalyDetected {
            at: now_epoch_secs(),
            data,
            process,
        }
    }

    /// A finding's condition stopped holding.
    pub fn anomaly_cleared(process: EventProcessInfo, data: AnomalyData) -> Self {
        Self::AnomalyCleared {
            at: now_epoch_secs(),
            data,
            process,
        }
    }

    /// A decision was reached, whether or not it acted.
    pub fn remediation_decided(process: EventProcessInfo, data: RemediationData) -> Self {
        Self::RemediationDecided {
            at: now_epoch_secs(),
            data,
            process,
        }
    }

    pub fn daemon_shutdown() -> Self {
        Self::DaemonShutdown {
            at: now_epoch_secs(),
        }
    }
}

// ---------------------------------------------------------------------------
// Subscription filter
// ---------------------------------------------------------------------------

/// Subscription filter sent by a client when connecting to the event socket.
///
/// The client sends this as the first newline-terminated JSON line. An empty
/// `subscribe` list subscribes to all events. A `null` `process` field matches
/// all processes.
// The event socket subscription protocol is only served on Unix platforms.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EventFilter {
    /// Event name patterns to subscribe to.
    ///
    /// Supports exact matches (`"log:out"`) and prefix wildcards (`"log:*"`,
    /// `"process:*"`, `"*"`). An empty list matches every event.
    #[serde(default)]
    pub subscribe: Vec<String>,
    /// Restrict delivery to events from this process name. `null` = all.
    #[serde(default)]
    pub process: Option<String>,
}

impl EventFilter {
    /// Returns `true` if `event` should be delivered to a client with this filter.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn matches(&self, event: &BusEvent) -> bool {
        let event_name = event.event_name();
        let event_ok =
            self.subscribe.is_empty() || self.subscribe.iter().any(|p| glob_matches(p, event_name));
        if !event_ok {
            return false;
        }
        match &self.process {
            None => true,
            Some(filter_name) => event
                .process_name()
                .map(|name| name == filter_name)
                .unwrap_or(true), // daemon-level events always pass through
        }
    }
}

/// Supports `*` (match all), `prefix:*` (match any event in that namespace),
/// and exact string equality.
#[cfg_attr(not(unix), allow(dead_code))]
fn glob_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix(":*") {
        return value.starts_with(&format!("{prefix}:"));
    }
    pattern == value
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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

    // --- glob_matches ---

    #[test]
    fn glob_exact_match() {
        assert!(glob_matches("log:out", "log:out"));
        assert!(!glob_matches("log:out", "log:err"));
    }

    #[test]
    fn glob_star_matches_everything() {
        assert!(glob_matches("*", "process:crashed"));
        assert!(glob_matches("*", "daemon:shutdown"));
    }

    #[test]
    fn glob_prefix_wildcard() {
        assert!(glob_matches("process:*", "process:crashed"));
        assert!(glob_matches("process:*", "process:started"));
        assert!(!glob_matches("process:*", "log:out"));
        assert!(glob_matches("log:*", "log:out"));
        assert!(glob_matches("log:*", "log:err"));
        assert!(!glob_matches("log:*", "health:healthy"));
    }

    // --- EventFilter::matches ---

    #[test]
    fn filter_empty_subscribe_matches_all() {
        let f = EventFilter::default();
        assert!(f.matches(&BusEvent::process_crashed(
            pinfo("api"),
            Some(1),
            None,
            0,
            3,
            vec![]
        )));
        assert!(f.matches(&BusEvent::log_out(pinfo("api"), "line".into())));
        assert!(f.matches(&BusEvent::daemon_shutdown()));
    }

    #[test]
    fn filter_subscribe_limits_event_types() {
        let f = EventFilter {
            subscribe: vec!["process:*".into()],
            process: None,
        };
        assert!(f.matches(&BusEvent::process_crashed(
            pinfo("api"),
            Some(1),
            None,
            0,
            3,
            vec![]
        )));
        assert!(f.matches(&BusEvent::process_online(pinfo("api"))));
        assert!(!f.matches(&BusEvent::log_out(pinfo("api"), "hello".into())));
        assert!(!f.matches(&BusEvent::health_healthy(pinfo("api"))));
    }

    #[test]
    fn filter_subscribe_exact_event() {
        let f = EventFilter {
            subscribe: vec!["log:out".into()],
            process: None,
        };
        assert!(f.matches(&BusEvent::log_out(pinfo("api"), "line".into())));
        assert!(!f.matches(&BusEvent::log_err(pinfo("api"), "err".into())));
    }

    #[test]
    fn filter_process_name_limits_scope() {
        let f = EventFilter {
            subscribe: vec![],
            process: Some("api".into()),
        };
        assert!(f.matches(&BusEvent::process_crashed(
            pinfo("api"),
            Some(1),
            None,
            0,
            0,
            vec![]
        )));
        assert!(!f.matches(&BusEvent::process_crashed(
            pinfo("worker"),
            Some(1),
            None,
            0,
            0,
            vec![]
        )));
        assert!(!f.matches(&BusEvent::log_out(pinfo("worker"), "line".into())));
    }

    #[test]
    fn filter_daemon_shutdown_always_passes_process_filter() {
        let f = EventFilter {
            subscribe: vec![],
            process: Some("api".into()),
        };
        assert!(f.matches(&BusEvent::daemon_shutdown()));
    }

    #[test]
    fn filter_combined_subscribe_and_process() {
        let f = EventFilter {
            subscribe: vec!["log:*".into()],
            process: Some("api".into()),
        };
        assert!(f.matches(&BusEvent::log_out(pinfo("api"), "line".into())));
        assert!(f.matches(&BusEvent::log_err(pinfo("api"), "err".into())));
        assert!(!f.matches(&BusEvent::log_out(pinfo("worker"), "line".into())));
        assert!(!f.matches(&BusEvent::process_crashed(
            pinfo("api"),
            Some(1),
            None,
            0,
            0,
            vec![]
        )));
    }

    // --- event_name ---

    #[test]
    fn event_names_match_wire_format() {
        assert_eq!(
            BusEvent::process_started(pinfo("x")).event_name(),
            "process:started"
        );
        assert_eq!(
            BusEvent::process_online(pinfo("x")).event_name(),
            "process:online"
        );
        assert_eq!(
            BusEvent::process_stopped(pinfo("x")).event_name(),
            "process:stopped"
        );
        assert_eq!(
            BusEvent::process_exited(pinfo("x"), None, None, 0, 0, vec![]).event_name(),
            "process:exited"
        );
        assert_eq!(
            BusEvent::process_crashed(pinfo("x"), None, None, 0, 0, vec![]).event_name(),
            "process:crashed"
        );
        assert_eq!(
            BusEvent::process_restarting(pinfo("x"), None, None, 0, 0, 0).event_name(),
            "process:restarting"
        );
        assert_eq!(
            BusEvent::process_errored(pinfo("x")).event_name(),
            "process:errored"
        );
        assert_eq!(
            BusEvent::log_out(pinfo("x"), "".into()).event_name(),
            "log:out"
        );
        assert_eq!(
            BusEvent::log_err(pinfo("x"), "".into()).event_name(),
            "log:err"
        );
        assert_eq!(
            BusEvent::health_healthy(pinfo("x")).event_name(),
            "health:healthy"
        );
        assert_eq!(
            BusEvent::health_unhealthy(pinfo("x"), "err".into(), 1).event_name(),
            "health:unhealthy"
        );
        assert_eq!(BusEvent::daemon_shutdown().event_name(), "daemon:shutdown");
    }

    // --- serialization ---

    #[test]
    fn process_crashed_serializes_with_event_tag() {
        let event = BusEvent::process_crashed(
            EventProcessInfo {
                id: 1,
                name: "api".into(),
                namespace: None,
                pid: Some(1234),
                command: "node server.js".into(),
                cwd: Some("/app".into()),
            },
            Some(1),
            Some("SIGSEGV".into()),
            42,
            3,
            vec![
                "Error: something".into(),
                "    at Object.<anonymous>".into(),
            ],
        );
        let json = serde_json::to_value(&event).expect("serialize failed");
        assert_eq!(json["event"], "process:crashed");
        assert_eq!(json["process"]["name"], "api");
        assert_eq!(json["process"]["pid"], 1234);
        assert_eq!(json["process"]["command"], "node server.js");
        assert_eq!(json["process"]["cwd"], "/app");
        assert_eq!(json["data"]["exit_code"], 1);
        assert_eq!(json["data"]["signal"], "SIGSEGV");
        assert_eq!(json["data"]["uptime_secs"], 42);
        assert_eq!(json["data"]["restart_count"], 3);
        assert_eq!(json["data"]["stderr_tail"][0], "Error: something");
    }

    #[test]
    fn log_out_serializes_with_line_field() {
        let event = BusEvent::log_out(pinfo("worker"), "hello world\n".into());
        let json = serde_json::to_value(&event).expect("serialize failed");
        assert_eq!(json["event"], "log:out");
        assert_eq!(json["data"]["line"], "hello world\n");
        assert_eq!(json["process"]["name"], "worker");
    }

    #[test]
    fn daemon_shutdown_has_no_process_field() {
        let event = BusEvent::daemon_shutdown();
        let json = serde_json::to_value(&event).expect("serialize failed");
        assert_eq!(json["event"], "daemon:shutdown");
        assert!(json.get("process").is_none());
    }

    #[test]
    fn null_namespace_is_omitted_from_json() {
        let event = BusEvent::process_online(pinfo("api"));
        let json = serde_json::to_value(&event).expect("serialize failed");
        assert!(json["process"].get("namespace").is_none());
    }

    #[test]
    fn process_restarting_includes_delay() {
        let event = BusEvent::process_restarting(pinfo("api"), Some(1), None, 10, 2, 5);
        let json = serde_json::to_value(&event).expect("serialize failed");
        assert_eq!(json["data"]["delay_secs"], 5);
        assert_eq!(json["data"]["restart_count"], 2);
        assert_eq!(json["data"]["uptime_secs"], 10);
        assert!(json["data"].get("signal").is_none());
    }

    #[test]
    fn event_filter_deserializes_from_json() {
        let raw = r#"{"subscribe":["process:*","log:out"],"process":"api"}"#;
        let f: EventFilter = serde_json::from_str(raw).expect("deserialize failed");
        assert_eq!(f.subscribe, ["process:*", "log:out"]);
        assert_eq!(f.process.as_deref(), Some("api"));
    }

    #[test]
    fn event_filter_deserializes_with_missing_fields() {
        let f: EventFilter = serde_json::from_str("{}").expect("deserialize empty");
        assert!(f.subscribe.is_empty());
        assert!(f.process.is_none());
    }

    // --- analysis events pass the existing filter (10.1, 10.6) ---

    fn anomaly(process: &str) -> BusEvent {
        BusEvent::anomaly_detected(
            pinfo(process),
            AnomalyData {
                id: format!("{process}/level_departure/memory_bytes/above#1"),
                key: format!("{process}/level_departure/memory_bytes/above"),
                detector: "level_departure".to_string(),
                metric: "memory_bytes".to_string(),
                confidence: 0.87,
                occurrence: 1,
                summary: Some("memory far above baseline".to_string()),
            },
        )
    }

    #[test]
    fn analysis_event_names_match_wire_format() {
        assert_eq!(anomaly("x").event_name(), "anomaly:detected");
        assert_eq!(
            BusEvent::anomaly_cleared(
                pinfo("x"),
                AnomalyData {
                    id: "x/level_departure/memory_bytes/above#1".to_string(),
                    key: "x/level_departure/memory_bytes/above".to_string(),
                    detector: "level_departure".to_string(),
                    metric: "memory_bytes".to_string(),
                    confidence: 0.5,
                    occurrence: 1,
                    summary: None,
                },
            )
            .event_name(),
            "anomaly:cleared"
        );
        assert_eq!(
            BusEvent::remediation_decided(
                pinfo("x"),
                RemediationData {
                    rule: "resource_leak".to_string(),
                    action: Some("restart".to_string()),
                    withheld: None,
                    acted: false,
                    findings: vec!["x/resource_leak/memory_bytes".to_string()],
                },
            )
            .event_name(),
            "remediation:decided"
        );
    }

    #[test]
    fn analysis_events_pass_the_existing_subscription_filter() {
        // The point of naming them `anomaly:*` and `remediation:*`: the filter already understands
        // prefix wildcards, so these are subscribable with no filter change at all. A new event
        // family that needed filter code would be a new family the filter could get wrong.
        let event = anomaly("api");

        for pattern in ["anomaly:*", "anomaly:detected", "*"] {
            let filter = EventFilter {
                subscribe: vec![pattern.to_string()],
                process: None,
            };
            assert!(
                filter.matches(&event),
                "{pattern:?} should match anomaly:detected"
            );
        }

        // And a filter for something else does not receive them.
        let other = EventFilter {
            subscribe: vec!["log:*".to_string()],
            process: None,
        };
        assert!(!other.matches(&event), "log:* must not match an anomaly");
    }

    #[test]
    fn analysis_events_are_narrowed_by_the_process_filter() {
        // They carry a process, so `process_name` returns it and the existing per-process filter
        // narrows them exactly as it does a lifecycle event. This is what "following existing
        // filtering" means in practice rather than in intent.
        let event = anomaly("api");
        assert_eq!(event.process_name(), Some("api"));

        let matching = EventFilter {
            subscribe: vec![],
            process: Some("api".to_string()),
        };
        assert!(matching.matches(&event));

        let other = EventFilter {
            subscribe: vec![],
            process: Some("worker".to_string()),
        };
        assert!(
            !other.matches(&event),
            "an anomaly for api must not reach a subscriber filtered to worker"
        );
    }

    #[test]
    fn an_anomaly_event_round_trips_through_json() {
        // The stream is newline-delimited JSON, so a payload that cannot be read back is a payload
        // no consumer can use. Asserted on the wire form rather than on the struct.
        let event = anomaly("api");
        let encoded = serde_json::to_string(&event).expect("serialise");
        assert!(encoded.contains(r#""event":"anomaly:detected""#));

        let decoded: BusEvent = serde_json::from_str(&encoded).expect("deserialise");
        match decoded {
            BusEvent::AnomalyDetected { data, process, .. } => {
                assert_eq!(process.name, "api");
                assert_eq!(data.detector, "level_departure");
                assert_eq!(data.confidence, 0.87);
                assert_eq!(data.occurrence, 1);
            }
            other => panic!("expected an anomaly, got {other:?}"),
        }
    }

    #[test]
    fn a_health_failure_serialises_its_correlated_findings() {
        // Task 8.6 on the wire. The field is skipped when empty, so the healthy case costs nothing;
        // both states are asserted because a `skip_serializing_if` that fires when it should not is
        // silent data loss.
        let with = BusEvent::health_unhealthy_with_findings(
            pinfo("api"),
            "connection refused".to_string(),
            2,
            vec!["api/level_departure/memory_bytes/above".to_string()],
        );
        let encoded = serde_json::to_string(&with).expect("serialise");
        assert!(
            encoded.contains("resource_findings"),
            "the correlation must reach the wire: {encoded}"
        );

        let without = BusEvent::health_unhealthy(pinfo("api"), "boom".to_string(), 1);
        let encoded = serde_json::to_string(&without).expect("serialise");
        assert!(
            !encoded.contains("resource_findings"),
            "an empty correlation must stay off the wire: {encoded}"
        );
    }
}
