use std::collections::HashMap;

use anyhow::Result;
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tokio::sync::mpsc;

use oxmgr_manager::process_manager::ProcessManager;
use oxmgr_metrics::process::ManagedProcess;

use super::{DaemonSnapshot, ManagerCommand};

/// Shared state handed to every axum handler.
#[derive(Clone)]
pub struct AppState {
    pub snapshot: DaemonSnapshot,
    pub command_tx: mpsc::UnboundedSender<ManagerCommand>,
    /// Auth credentials: `Some((user, pass))` when dashboard auth is enabled, `None` otherwise.
    /// Read once at daemon startup (from env), never re-read per-request.
    pub auth_creds: Option<(String, String)>,
    /// Web asset directory for static serving. `None` if static serving is disabled.
    pub static_web_dir: Option<std::path::PathBuf>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Embedded assets (compiled into the binary)
// ─────────────────────────────────────────────────────────────────────────────

/// Embedded dashboard JS files as a compile-time map from path to bytes.
/// Built from the same source list that defines `DASHBOARD_JS_MODULE_ORDER`.
/// The order is significant for on-disk serving (static mode) to preserve
/// module load order via `<script type="module">` import graph.
macro_rules! dashboard_js_sources {
    ($($path:literal),* $(,)?) => {
        &[
            $(($path, include_bytes!(concat!("../../../../../web/", $path)))),*
        ]
    };
}

/// Static array of (path, bytes) for every dashboard JS module.
/// Order matches `DASHBOARD_JS_MODULE_ORDER`; used by the embedded fallback
/// to serve individual files without concatenation.
const DASHBOARD_JS_ASSETS: &[(&str, &[u8])] = dashboard_js_sources!(
    "js/core/const.js",
    "js/core/severity.js",
    "js/core/boot-target.js",
    "js/core/bus.js",
    "js/core/store.js",
    "js/core/api.js",
    "js/core/spin.js",
    "js/core/announce.js",
    "js/format/fmt.js",
    "js/ansi/ansi.js",
    "js/log/LineBuffer.js",
    "js/log/RowPool.js",
    "js/log/CapacityController.js",
    "js/log/ChunkRunner.js",
    "js/log/LogView.js",
    "js/host/shell.js",
    "js/host/metricRenderers.js",
    "js/host/canvas.js",
    "js/modals/modal.js",
    "js/modals/log-modal.js",
    "js/modals/detail-modal.js",
    "js/modals/confirm-modal.js",
    "js/shell/network.js",
    "js/shell/stats.js",
    "js/shell/table.js",
    "js/shell/controls.js",
    "js/shell/app.js",
    "js/logpage/logpage.js",
    "dashboard.js",
);

/// Module paths for static-mode serving (relative to `OXMGR_WEB_DIR`).
/// Order is the topological order of the import graph — modules are served
/// as ES modules, so the browser walks imports; this order only matters for
/// deterministic embedded responses and tests.
#[cfg(test)]
pub(crate) const DASHBOARD_JS_MODULE_ORDER: &[&str] = &[
    "js/core/const.js",
    "js/core/severity.js",
    "js/core/boot-target.js",
    "js/core/bus.js",
    "js/core/store.js",
    "js/core/api.js",
    "js/core/spin.js",
    "js/core/announce.js",
    "js/format/fmt.js",
    "js/ansi/ansi.js",
    "js/log/LineBuffer.js",
    "js/log/RowPool.js",
    "js/log/CapacityController.js",
    "js/log/ChunkRunner.js",
    "js/log/LogView.js",
    "js/host/shell.js",
    "js/host/metricRenderers.js",
    "js/host/canvas.js",
    "js/modals/modal.js",
    "js/modals/log-modal.js",
    "js/modals/detail-modal.js",
    "js/modals/confirm-modal.js",
    "js/shell/network.js",
    "js/shell/stats.js",
    "js/shell/table.js",
    "js/shell/controls.js",
    "js/shell/app.js",
    "js/logpage/logpage.js",
    "dashboard.js",
];

/// The embedded, concatenated script is REMOVED — each module is now served
/// as an independent ES module. Kept as a deprecated constant for test drift
/// guards that still reference it; the bytes are the concatenation of the
/// module files in `DASHBOARD_JS_MODULE_ORDER` and MUST match the on-disk
/// assembly for backward compatibility with tests that haven't been migrated.
#[deprecated(since = "6.0.0", note = "per-file serving replaces bundle")]
#[expect(
    dead_code,
    reason = "drift-guard tests still reference the legacy concat"
)]
const DASHBOARD_JS: &str = concat!(
    include_str!("../../../../../web/js/core/const.js"),
    include_str!("../../../../../web/js/core/severity.js"),
    include_str!("../../../../../web/js/core/boot-target.js"),
    include_str!("../../../../../web/js/core/bus.js"),
    include_str!("../../../../../web/js/core/store.js"),
    include_str!("../../../../../web/js/core/api.js"),
    include_str!("../../../../../web/js/core/spin.js"),
    include_str!("../../../../../web/js/core/announce.js"),
    include_str!("../../../../../web/js/format/fmt.js"),
    include_str!("../../../../../web/js/ansi/ansi.js"),
    include_str!("../../../../../web/js/log/LineBuffer.js"),
    include_str!("../../../../../web/js/log/RowPool.js"),
    include_str!("../../../../../web/js/log/CapacityController.js"),
    include_str!("../../../../../web/js/log/ChunkRunner.js"),
    include_str!("../../../../../web/js/log/LogView.js"),
    include_str!("../../../../../web/js/host/shell.js"),
    include_str!("../../../../../web/js/host/metricRenderers.js"),
    include_str!("../../../../../web/js/host/canvas.js"),
    include_str!("../../../../../web/js/modals/modal.js"),
    include_str!("../../../../../web/js/modals/log-modal.js"),
    include_str!("../../../../../web/js/modals/detail-modal.js"),
    include_str!("../../../../../web/js/modals/confirm-modal.js"),
    include_str!("../../../../../web/js/shell/network.js"),
    include_str!("../../../../../web/js/shell/stats.js"),
    include_str!("../../../../../web/js/shell/table.js"),
    include_str!("../../../../../web/js/shell/controls.js"),
    include_str!("../../../../../web/js/shell/app.js"),
    include_str!("../../../../../web/js/logpage/logpage.js"),
    include_str!("../../../../../web/dashboard.js"),
);

pub(crate) mod auth;
mod compress;
pub(crate) mod handlers;
pub(crate) mod render;
pub(super) use render::*;
mod routes;
pub(super) use routes::create_router;
mod sse;

// Re-export embedded asset strings needed by handlers and tests
pub const DASHBOARD_CSS: &str = include_str!("../../../../../web/dashboard.css");
pub const DASHBOARD_THEME_JS: &str = include_str!("../../../../../web/theme.js");
pub const DASHBOARD_HTML: &str = include_str!("../../../../../web/index.html");
pub const FAVICON_SVG: &str = include_str!("../../../../../web/favicon.svg");

const ENV_DASHBOARD_INTERVAL_MS: &str = "OXMGR_DASHBOARD_INTERVAL_MS";
const ENV_SEVERITY_STYLING: &str = "OXMGR_SEVERITY_STYLING";
const ENV_DASHBOARD_LABEL: &str = "OXMGR_DASHBOARD_LABEL";
const ENV_DASHBOARD_LABEL_COLOR: &str = "OXMGR_DASHBOARD_LABEL_COLOR";
const ENV_DASHBOARD_LOG_LINES: &str = "OXMGR_DASHBOARD_LOG_LINES";
const ENV_DASHBOARD_LOG_RETAIN: &str = "OXMGR_DASHBOARD_LOG_RETAIN";

// Dashboard auth
const ENV_DASHBOARD_USER: &str = "OXMGR_DASHBOARD_USER";
const ENV_DASHBOARD_PASS: &str = "OXMGR_DASHBOARD_PASS";

const HTML_CONTENT_TYPE: &str = "text/html; charset=utf-8";
const TEXT_PLAIN_CONTENT_TYPE: &str = "text/plain; charset=utf-8";
const SVG_CONTENT_TYPE: &str = "image/svg+xml; charset=utf-8";
const CSS_CONTENT_TYPE: &str = "text/css; charset=utf-8";
const JS_CONTENT_TYPE: &str = "text/javascript; charset=utf-8";
/// Types the wildcard asset route may serve beyond the original three. Binary
/// formats carry no charset — appending one to a font or an image is wrong and
/// some clients reject it.
const JSON_CONTENT_TYPE: &str = "application/json; charset=utf-8";
const WOFF2_CONTENT_TYPE: &str = "font/woff2";
const PNG_CONTENT_TYPE: &str = "image/png";
const WEBP_CONTENT_TYPE: &str = "image/webp";
const ICO_CONTENT_TYPE: &str = "image/x-icon";
/// Fallback for an extension not in the table. Serving opaque bytes is correct
/// and safe: the browser will not execute what it cannot type.
const OCTET_STREAM_CONTENT_TYPE: &str = "application/octet-stream";
/// Strict Content-Security-Policy for the dashboard. No external origins, no
/// inline scripts or styles.
const CSP_HEADER: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; font-src 'self';";

// ─────────────────────────────────────────────────────────────────────────────
// Http Commands (Manager interface)
// ─────────────────────────────────────────────────────────────────────────────

pub(super) enum HttpCommand {
    Stop {
        target: String,
    },
    Restart {
        target: String,
    },
    Reload {
        target: String,
    },
    Dismiss {
        target: String,
        rule: String,
    },
    Restore {
        target: String,
        rule: String,
    },
    Pull {
        target: String,
        secret: Option<String>,
    },
}

pub(super) async fn execute_api_request(
    command: HttpCommand,
    manager: &mut ProcessManager,
) -> Response {
    match command {
        HttpCommand::Stop { target } => {
            let res = if target == "all" {
                manager.stop_all_processes().await.map(|ps| ps.len())
            } else {
                manager.stop_process(&target).await.map(|_| 1)
            };
            handle_action_result("stop", res)
        }
        HttpCommand::Restart { target } => {
            let res = if target == "all" {
                manager.restart_all_processes().await.map(|ps| ps.len())
            } else {
                manager.restart_process(&target).await.map(|_| 1)
            };
            handle_action_result("restart", res)
        }
        HttpCommand::Reload { target } => {
            if target == "all" {
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(json!({"ok":false,"message":"reload all is not supported"})),
                )
                    .into_response();
            }
            let res = manager.reload_process(&target).await.map(|_| 1);
            handle_action_result("reload", res)
        }
        HttpCommand::Dismiss { target, rule } => {
            if rule.is_empty() {
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(json!({"ok":false,"message":"advisory rule id is required"})),
                )
                    .into_response();
            }
            if target == "all" {
                return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"ok":false,"message":"dismissal is per process: name the process explicitly"}))).into_response();
            }
            match manager.dismiss_advisory(&target, &rule) {
                Ok(true) => json_response(
                    200,
                    json!({"ok":true,"message":format!("dismiss {rule} for {target}")}),
                ),
                Ok(false) => json_response(
                    200,
                    json!({"ok":true,"message":format!("{rule} was already dismissed for {target}")}),
                ),
                Err(err) => (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(json!({"ok":false,"message":err.to_string()})),
                )
                    .into_response(),
            }
        }
        HttpCommand::Restore { target, rule } => {
            if rule.is_empty() {
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(json!({"ok":false,"message":"advisory rule id is required"})),
                )
                    .into_response();
            }
            if target == "all" {
                return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"ok":false,"message":"dismissal is per process: name the process explicitly"}))).into_response();
            }
            match manager.restore_advisory(&target, &rule) {
                Ok(true) => json_response(
                    200,
                    json!({"ok":true,"message":format!("restore {rule} for {target}")}),
                ),
                Ok(false) => json_response(
                    200,
                    json!({"ok":true,"message":format!("{rule} was already restored for {target}")}),
                ),
                Err(err) => (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(json!({"ok":false,"message":err.to_string()})),
                )
                    .into_response(),
            }
        }
        HttpCommand::Pull { target, secret } => {
            if manager.get_process(&target).is_err() {
                return (
                    axum::http::StatusCode::NOT_FOUND,
                    axum::Json(json!({"ok":false,"message":"service not found"})),
                )
                    .into_response();
            }
            let Some(secret) = secret else {
                return (
                    axum::http::StatusCode::UNAUTHORIZED,
                    axum::Json(json!({"ok":false,"message":"missing webhook secret"})),
                )
                    .into_response();
            };
            if manager
                .verify_pull_webhook_secret(&target, &secret)
                .is_err()
            {
                return (
                    axum::http::StatusCode::UNAUTHORIZED,
                    axum::Json(json!({"ok":false,"message":"invalid webhook secret"})),
                )
                    .into_response();
            }
            match manager.pull_processes(Some(target.as_str())).await {
                Ok(message) => json_response(200, json!({"ok":true,"message":message})),
                Err(err) => (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(json!({"ok":false,"message":err.to_string()})),
                )
                    .into_response(),
            }
        }
    }
}

fn handle_action_result(action: &str, result: anyhow::Result<usize>) -> Response {
    match result {
        Ok(count) => json_response(
            200,
            json!({"ok":true,"message":format!("{action} {count} process(es)")}),
        ),
        Err(err) => {
            let status = if err
                .downcast_ref::<oxmgr_core::errors::OxmgrError>()
                .is_some()
            {
                404
            } else {
                500
            };
            (
                axum::http::StatusCode::from_u16(status)
                    .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR),
                axum::Json(json!({"ok":false,"message":err.to_string()})),
            )
                .into_response()
        }
    }
}

fn json_response(status: u16, body: serde_json::Value) -> Response {
    (
        axum::http::StatusCode::from_u16(status).unwrap_or(axum::http::StatusCode::OK),
        axum::Json(body),
    )
        .into_response()
}

// ─────────────────────────────────────────────────────────────────────────────
// Dashboard page (static document, no template substitution)
// ─────────────────────────────────────────────────────────────────────────────

/// Resolves `OXMGR_WEB_DIR` once at startup. `Some` when the variable is set,
/// non-empty, and names an existing directory; `None` otherwise. The path is
/// immutable after daemon start — the *bytes* of the files are re-read per
/// request so edits land without a restart. A validated (existing) directory
/// is required so an environment pointing at a typo or an unmounted volume
/// degrades to the embedded fallback instead of a blank dashboard.
pub(crate) fn static_web_dir() -> Option<std::path::PathBuf> {
    static WEB_DIR: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    WEB_DIR
        .get_or_init(|| {
            let dir = std::env::var("OXMGR_WEB_DIR")
                .ok()
                .filter(|s| !s.trim().is_empty())?;
            let p = std::path::PathBuf::from(&dir);
            p.is_dir().then_some(p)
        })
        .clone()
}

/// The dashboard document: a static page whose assets are referenced by URL
/// (`/dashboard.css`, `/dashboard.js`), regardless of whether those routes
/// serve from disk or from the embedded bytes. No template substitution.
pub(super) fn render_dashboard_html() -> &'static str {
    DASHBOARD_HTML
}

pub(super) fn favicon_svg() -> &'static str {
    FAVICON_SVG
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers (kept for compatibility with logs.rs/etc)
// ─────────────────────────────────────────────────────────────────────────────

pub(super) fn resolve_stream(
    query: &HashMap<String, String>,
) -> Result<&'static str, Box<Response>> {
    match query.get("stream").map(String::as_str) {
        None | Some("") | Some("stdout") => Ok("stdout"),
        Some("stderr") => Ok("stderr"),
        Some(other) => Err(Box::new((
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({"ok":false,"message":format!("unknown log stream '{other}', expected stdout or stderr")})),
        ).into_response())),
    }
}

pub(super) fn stream_log_path(process: &ManagedProcess, stream: &str) -> std::path::PathBuf {
    if stream == "stderr" {
        process.stderr_log.clone()
    } else {
        process.stdout_log.clone()
    }
}

pub(super) fn resolve_archive_index(query: &HashMap<String, String>) -> Result<u32, Box<Response>> {
    match query.get("index").map(String::as_str) {
        None | Some("") => Ok(0),
        Some(raw) if raw.bytes().all(|byte| byte.is_ascii_digit()) => {
            raw.parse::<u32>().map_err(|_| {
                Box::new(
                    (
                        axum::http::StatusCode::BAD_REQUEST,
                        axum::Json(json!({"ok":false,"message":"invalid log archive index"})),
                    )
                        .into_response(),
                )
            })
        }
        Some(_) => Err(Box::new(
            (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(json!({"ok":false,"message":"invalid log archive index"})),
            )
                .into_response(),
        )),
    }
}

pub(super) fn resolve_line_offset(query: &HashMap<String, String>) -> Result<usize, Box<Response>> {
    match query.get("before").map(String::as_str) {
        None | Some("") => Ok(0),
        Some(raw) if raw.bytes().all(|byte| byte.is_ascii_digit()) => {
            raw.parse::<usize>().map_err(|_| {
                Box::new(
                    (
                        axum::http::StatusCode::BAD_REQUEST,
                        axum::Json(json!({"ok":false,"message":"invalid line offset"})),
                    )
                        .into_response(),
                )
            })
        }
        Some(_) => Err(Box::new(
            (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(json!({"ok":false,"message":"invalid line offset"})),
            )
                .into_response(),
        )),
    }
}

const LOG_ARCHIVE_PROBE_LIMIT: u32 = 64;

pub(super) fn serve_log_files(process: &ManagedProcess) -> Response {
    let mut files = Vec::new();
    for (stream, base) in [
        ("stdout", &process.stdout_log),
        ("stderr", &process.stderr_log),
    ] {
        if stream == "stderr" && process.stderr_log == process.stdout_log {
            continue;
        }
        files.extend(oxmgr_manager::logging::log_files_for(
            base,
            stream,
            LOG_ARCHIVE_PROBE_LIMIT,
        ));
    }
    json_response(
        200,
        json!({
            "process": process.name,
            "files": serde_json::to_value(&files).unwrap_or(serde_json::Value::Null),
        }),
    )
}

pub(super) async fn serve_log_download(
    process: &ManagedProcess,
    query: &HashMap<String, String>,
) -> Response {
    let stream = match resolve_stream(query) {
        Ok(stream) => stream,
        Err(response) => return *response,
    };
    let index = match resolve_archive_index(query) {
        Ok(index) => index,
        Err(response) => return *response,
    };
    let base = stream_log_path(process, stream);
    let path = oxmgr_manager::logging::log_file_path(&base, index);

    let Ok(contents) = tokio::fs::read(&path).await else {
        return (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(json!({"ok":false,"message":"log file not found"})),
        )
            .into_response();
    };
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("logfile.log");

    let mut response = (
        axum::http::StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, TEXT_PLAIN_CONTENT_TYPE)],
        String::from_utf8_lossy(&contents).into_owned(),
    )
        .into_response();

    // SAFETY: sanitize_filename keeps only visible ASCII + space + tab, which is
    // the byte set HeaderValue::from_str accepts, so the parse cannot fail.
    // The match is defense-in-depth: if logic drifts, return 500 rather than abort.
    let disposition = format!("attachment; filename=\"{}\"", sanitize_filename(filename));
    match HeaderValue::try_from(disposition.as_str()) {
        Ok(value) => {
            response.headers_mut().insert("Content-Disposition", value);
        }
        Err(_) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"ok": false, "message": "invalid log filename"})),
            )
                .into_response();
        }
    }
    response
}

/// Filters a log filename to the byte set `HeaderValue` accepts:
/// visible ASCII, space, and tab. Everything else (control characters, DEL,
/// non-ASCII, quotes, backslashes) is dropped to prevent the daemon from
/// aborting on header parse or escaping confusion.
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .filter(|ch| {
            (ch.is_ascii_graphic() && !matches!(ch, '"' | '\\')) || *ch == ' ' || *ch == '\t'
        })
        .collect()
}

pub(super) fn serve_dashboard_config() -> Response {
    let interval_ms: u64 = std::env::var(ENV_DASHBOARD_INTERVAL_MS)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000)
        .clamp(200, 10000);

    let label = std::env::var(ENV_DASHBOARD_LABEL).ok();
    let label_color = std::env::var(ENV_DASHBOARD_LABEL_COLOR).ok();

    json_response(
        200,
        json!({
            "interval_ms": interval_ms,
            "label": label,
            "label_color": label_color,
            "log_tail_lines": dashboard_tail_lines(),
            "log_tail_warn_above": LOG_TAIL_WARN_ABOVE,
            "log_retain_lines": dashboard_retain_lines(),
            "severity": {
                "warning_percent": oxmgr_core::severity::DEFAULT_WARNING_PERCENT,
                "critical_percent": oxmgr_core::severity::DEFAULT_CRITICAL_PERCENT,
                "hysteresis_percent": oxmgr_core::severity::DEFAULT_BAND_HYSTERESIS_PERCENT,
                "styling_enabled": severity_styling_enabled(),
            },
        }),
    )
}

fn severity_styling_enabled() -> bool {
    match std::env::var(ENV_SEVERITY_STYLING) {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "off" | "false" | "no" | "disabled"
        ),
        Err(_) => true,
    }
}

fn dashboard_retain_lines() -> Option<usize> {
    std::env::var(ENV_DASHBOARD_LOG_RETAIN)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|lines| *lines >= 1)
}

const LOG_TAIL_DEFAULT: usize = 200;
const LOG_TAIL_WARN_ABOVE: usize = 500;
const LOG_TAIL_MAX: usize = 100_000;

fn dashboard_tail_lines() -> usize {
    std::env::var(ENV_DASHBOARD_LOG_LINES)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|lines| *lines >= 1)
        .unwrap_or(LOG_TAIL_DEFAULT)
        .min(LOG_TAIL_MAX)
}

pub(super) fn extract_api_secret(headers: &axum::http::HeaderMap) -> Option<String> {
    if let Some(value) = headers.get("x-oxmgr-secret") {
        return value.to_str().ok().map(|s| s.trim().to_string());
    }
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(|value| value.trim().to_string())
}

pub(super) fn process_json_for_transport(process: &ManagedProcess) -> serde_json::Value {
    let mut value =
        serde_json::to_value(process.redacted_for_transport()).unwrap_or(serde_json::Value::Null);
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "network_io".to_string(),
            json!({
                "status": "unsupported",
                "reason": "Per-process network I/O is not measurable on the supported platforms: the operating system reports network counters per interface, not per process. Attributing traffic to a process needs per-socket accounting (eBPF, or socket-inode correlation), which is not portable. Host-level interface figures are reported separately and are the host's, not any one process's.",
            }),
        );
        object.insert(
            "disk_read_bytes_per_second".to_string(),
            json_rate(process.disk_read_rate_bps()),
        );
        object.insert(
            "disk_write_bytes_per_second".to_string(),
            json_rate(process.disk_write_rate_bps()),
        );
    }
    value
}

/// Attaches observation-derived fields to one process's transport JSON: descendant
/// attribution for every process, and the cluster shape for cluster processes.
///
/// The three absence shapes are distinct on purpose (`managed-process-children`:
/// unavailable is not empty):
///
/// - sampling disabled → `unavailable`, and no totals anywhere — a withheld total is
///   honest, a fallback to the process's own figure would be a lie about what it covers;
/// - enabled but no sample yet → `unavailable` with its own reason;
/// - sample exists but this pid has no entry yet (started mid-cycle, or stopped) →
///   `unavailable`; a stopped process's children are moot, and a fresh one is up to one
///   sampling cycle away from its first observation;
/// - entry present → `ok` with the descendants, their totals over the whole observed
///   set, the observation time, and the truncation marker. An empty `descendants` array
///   here means observed-and-none.
///
/// The cluster shape (`cluster-instance-visibility`) rides the same observation and
/// inherits its availability exactly: requested comes from configuration and is always
/// present (labelled `derived` when the runtime chose the number), observed is the
/// attributed descendant count and is unavailable whenever attribution is — never zero
/// by fallback. A shortfall between the two is reported as the two figures are, with no
/// verdict attached: calling it degraded is the analytics path's job.
pub(super) fn attach_observation_fields(
    mut value: serde_json::Value,
    process: &ManagedProcess,
    consumers: Option<&oxmgr_metrics::host_consumers::HostConsumers>,
    sampling_enabled: bool,
) -> serde_json::Value {
    let process_pid = process.pid;
    // Availability resolved once, shared by both payloads: (entry, sampled_at) when an
    // observation covers this process, else the reason it does not.
    let observation: Result<
        (
            &oxmgr_metrics::host_consumers::ManagedProcessAttribution,
            u64,
        ),
        &str,
    > = match consumers {
        None => Err(if sampling_enabled {
            "no host-wide observation has completed since the daemon started"
        } else {
            "host-wide consumer sampling is disabled (OXMGR_HOST_CONSUMERS)"
        }),
        Some(consumers) => {
            let entry = process_pid
                .and_then(|pid| consumers.attribution.iter().find(|entry| entry.pid == pid));
            match entry {
                Some(entry) => Ok((entry, consumers.sampled_at)),
                None => Err(if process_pid.is_none() {
                    "process is not running"
                } else {
                    "not yet observed by host-wide sampling"
                }),
            }
        }
    };

    if let Some(object) = value.as_object_mut() {
        let descendants = match &observation {
            Err(reason) => json!({ "status": "unavailable", "reason": reason }),
            Ok((entry, sampled_at)) => json!({
                "status": "ok",
                "observed_at": sampled_at,
                "descendants": entry.descendants,
                "descendants_cpu_percent": entry.descendants_cpu_percent,
                "descendants_memory_bytes": entry.descendants_memory_bytes,
                "truncated": entry.truncated,
            }),
        };
        object.insert("descendants".to_string(), descendants);

        if process.cluster_mode {
            // Requested is configuration, not observation: available even when the
            // observation is not. `None` means the operator never chose a number and
            // the bootstrap derived one at startup — reported as derived rather than
            // dressed up as a figure anyone set.
            let requested = match process.cluster_instances {
                Some(count) => json!({ "count": count, "derived": false }),
                None => json!({ "count": serde_json::Value::Null, "derived": true }),
            };
            let observed = match &observation {
                Err(reason) => json!({ "status": "unavailable", "reason": reason }),
                Ok((entry, sampled_at)) => json!({
                    "status": "ok",
                    "workers": entry.descendant_count,
                    "observed_at": sampled_at,
                }),
            };
            object.insert(
                "cluster".to_string(),
                json!({ "requested": requested, "observed": observed }),
            );
        }
    }
    value
}

fn json_rate(rate: Option<f64>) -> serde_json::Value {
    match rate {
        Some(value) if value.is_finite() => json!(value),
        _ => serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each module in `DASHBOARD_JS_MODULE_ORDER` must have a corresponding
    /// embedded asset in `DASHBOARD_JS_ASSETS` with identical bytes.
    /// This replaces the old concat-bundle byte-equality check with a per-file
    /// one: embedded mode and on-disk mode now serve the same set of files.
    #[test]
    fn embedded_assets_match_on_disk_files() {
        let web_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web");
        for module in DASHBOARD_JS_MODULE_ORDER {
            let path = web_dir.join(module);
            assert!(
                path.is_file(),
                "module listed in order but missing on disk: {path:?}"
            );
            let on_disk =
                std::fs::read(&path).unwrap_or_else(|e| panic!("failed to read {path:?}: {e}"));
            let embedded = DASHBOARD_JS_ASSETS
                .iter()
                .find_map(|(p, b)| if *p == *module { Some(*b) } else { None });
            assert!(
                embedded.is_some(),
                "embedded asset missing for module {module}"
            );
            assert_eq!(
                on_disk,
                embedded.unwrap(),
                "embedded asset for {module} drifted from on-disk file"
            );
        }
        // Also verify the sets are exactly equal (no extra embedded assets)
        let embedded_paths: std::collections::HashSet<_> =
            DASHBOARD_JS_ASSETS.iter().map(|(p, _)| *p).collect();
        let module_paths: std::collections::HashSet<_> =
            DASHBOARD_JS_MODULE_ORDER.iter().copied().collect();
        assert_eq!(
            embedded_paths, module_paths,
            "embedded asset set must exactly match module order set"
        );
    }

    /// The module order must have `js/core/const.js` as the first entry
    /// (the dependency root) and `dashboard.js` as the last (the entry point).
    /// `header.js` and `footer.js` no longer exist.
    #[test]
    fn module_order_endpoints_are_root_and_entry() {
        assert_eq!(DASHBOARD_JS_MODULE_ORDER.first(), Some(&"js/core/const.js"));
        assert_eq!(DASHBOARD_JS_MODULE_ORDER.last(), Some(&"dashboard.js"));
        assert!(!DASHBOARD_JS_MODULE_ORDER.contains(&"js/header.js"));
        assert!(!DASHBOARD_JS_MODULE_ORDER.contains(&"js/footer.js"));
    }

    /// Drift guard for the descendant-expansion surface
    /// (managed-process-child-visibility): every module must still carry
    /// the expansion control, its three-shape gate, and the panel styles.
    /// A refactor that drops any of these ships a table that can no longer
    /// reveal attributed children — this test makes that a build failure.
    #[test]
    fn dashboard_ships_the_descendant_expansion_surface() {
        let all_js: String = DASHBOARD_JS_ASSETS
            .iter()
            .map(|(_, b)| std::str::from_utf8(b).unwrap())
            .collect();
        for marker in [
            "expand-btn",                       // the per-row control (§D7)
            "descendantControlWanted",          // ok-with-rows / unavailable / observed-none gate
            "descendant-row",                   // the revealed panel row
            "descendant-freshness",             // §D4 two-clocks marker
            "Subtree total, incl. descendants", // totals never replace own figures
        ] {
            assert!(
                all_js.contains(marker),
                "served dashboard lost the descendant surface marker {marker:?}"
            );
        }
        assert!(
            DASHBOARD_CSS.contains(".descendant-table"),
            "served dashboard.css lost the descendant panel styles"
        );
        assert!(
            DASHBOARD_CSS.contains(".expand-btn"),
            "served dashboard.css lost the expansion control styles"
        );
    }

    /// Drift guard for the cluster-shape surface (`cluster-instance-visibility`):
    /// the served modules must carry the requested-vs-observed pair, its derived
    /// label, and the marker styles. Losing any of these ships a dashboard that
    /// can no longer tell a supervisor from an ordinary row.
    #[test]
    fn dashboard_ships_the_cluster_shape_surface() {
        let all_js: String = DASHBOARD_JS_ASSETS
            .iter()
            .map(|(_, b)| std::str::from_utf8(b).unwrap())
            .collect();
        for marker in [
            "clusterMarker",          // the per-row chip (5.6)
            "detailCluster",          // the detail-panel section (§D5)
            "derived by the runtime", // derived label, no invented number (5.3)
            "Observed workers",       // the observed figure, separate (5.2)
        ] {
            assert!(
                all_js.contains(marker),
                "served dashboard lost the cluster surface marker {marker:?}"
            );
        }
        assert!(
            DASHBOARD_CSS.contains(".cluster-flag"),
            "served dashboard.css lost the cluster marker styles"
        );
    }

    /// `sanitize_filename` keeps only what `HeaderValue` accepts: visible ASCII,
    /// space, tab. Everything else is dropped so a managed process name carrying a
    /// control byte can no longer abort the daemon on header parse.
    #[test]
    fn sanitize_filename_strips_header_rejected_bytes() {
        assert_eq!(sanitize_filename("api.out.log"), "api.out.log");
        assert_eq!(
            sanitize_filename("we\"ird\\name\u{7}with\u{1b}escape"),
            "weirdnamewithescape",
            "quotes, backslashes, BEL and ESC are all dropped"
        );
        assert_eq!(
            sanitize_filename("name\u{7f}with DEL"),
            "namewith DEL",
            "DEL and control bytes dropped, space kept"
        );
        assert_eq!(
            sanitize_filename("caf\u{e9} \u{2028}line"),
            "caf line",
            "non-ASCII dropped, space kept"
        );
    }

    /// The Content-Disposition value stays a valid header for the worst-case
    /// input after sanitisation — this is the assertion that replaces the old
    /// abort-on-parse behaviour (previously `format!(...).parse().unwrap()`).
    #[test]
    fn content_disposition_header_value_always_parses() {
        for name in [
            "api.out.log",
            "we\"ird\\name\u{7}with\u{1b}escape",
            "name\u{7f}with DEL",
            "caf\u{e9} \u{2028}line",
            "",
        ] {
            let disposition = format!("attachment; filename=\"{}\"", sanitize_filename(name));
            let value = HeaderValue::try_from(disposition.as_str()).expect(
                "sanitised filename must always form a valid Content-Disposition \
                 header (visible ASCII + space + tab only)",
            );
            assert!(
                value
                    .to_str()
                    .expect("valid header value round-trips to str")
                    .starts_with("attachment; filename=\""),
                "disposition keeps its shape"
            );
        }
    }
}
