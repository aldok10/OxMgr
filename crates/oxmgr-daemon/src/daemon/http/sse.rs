use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, Stream, StreamExt, select_all};
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};

use super::{AppState, attach_observation_fields, process_json_for_transport};
use oxmgr_metrics::host_metrics::{HostMetrics, HostSubsystem};

/// A boxed SSE event stream, the common shape every multiplexed branch yields.
type BoxedEventStream = std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>;

pub(super) fn host_event_name(subsystem: HostSubsystem) -> &'static str {
    match subsystem {
        HostSubsystem::Memory => "memory",
        HostSubsystem::Cpu => "cpu",
        HostSubsystem::LoadAverage => "load_average",
        HostSubsystem::Filesystems => "filesystems",
        HostSubsystem::Network => "network",
        HostSubsystem::Components => "components",
    }
}

pub(super) fn host_subsystem_payload(
    metrics: &HostMetrics,
    subsystem: HostSubsystem,
) -> serde_json::Value {
    match subsystem {
        HostSubsystem::Memory => serde_json::to_value(&metrics.memory).unwrap_or_default(),
        HostSubsystem::Cpu => serde_json::to_value(&metrics.cpu).unwrap_or_default(),
        HostSubsystem::LoadAverage => {
            serde_json::to_value(&metrics.load_average).unwrap_or_default()
        }
        HostSubsystem::Filesystems => {
            serde_json::to_value(&metrics.filesystems).unwrap_or_default()
        }
        HostSubsystem::Network => serde_json::to_value(&metrics.network).unwrap_or_default(),
        HostSubsystem::Components => serde_json::to_value(&metrics.components).unwrap_or_default(),
    }
}

/// Streams the full (redacted) process list over Server-Sent Events. Pushes a
/// snapshot at a configurable interval so the dashboard updates live over one connection.
///
/// Query parameters:
/// - `interval_ms`: refresh interval in milliseconds (default 2000, clamped to 200..10000)
pub(super) async fn handle_processes_stream(
    State(state): State<AppState>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let interval_ms = query
        .get("interval_ms")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(2000)
        .clamp(200, 10000);

    let stream = stream::unfold(
        (state, interval_ms, true), // first iteration sends immediately, like the legacy loop
        |(state, interval, first)| async move {
            if !first {
                tokio::time::sleep(Duration::from_millis(interval)).await;
            }
            let payload = processes_payload_with_attribution(&state).await;
            Some((
                Ok(Event::default().event(EVENT_PROCESSES).data(payload)),
                (state, interval, false),
            ))
        },
    );

    sse_response(Sse::new(stream))
}

/// One frame of the process stream: the redacted list with descendant attribution
/// attached. Shared by both stream handlers so the SSE payload and the REST
/// endpoint cannot drift into different shapes — the dashboard renders whichever
/// arrives first, and a field present in one but not the other would flicker.
async fn processes_payload_with_attribution(state: &AppState) -> String {
    let processes = state.snapshot.list_processes().await;
    let consumers = state.snapshot.host_consumers().await;
    let sampling_enabled = state.snapshot.consumers.sampling_enabled();
    let values: Vec<serde_json::Value> = processes
        .into_iter()
        .map(|process| {
            let value = process_json_for_transport(&process);
            attach_observation_fields(value, &process, consumers.as_ref(), sampling_enabled)
        })
        .collect();
    serde_json::to_string(&values).unwrap_or_else(|_| "[]".to_string())
}

/// Streams BusEvent events over Server-Sent Events. Clients receive real-time
/// process lifecycle events, log lines, and health updates.
///
/// Query parameters:
/// - `subscribe`: comma-separated event patterns (e.g., "process:*,log:*"). Default: all events.
/// - `process`: filter to events from this process name only. Default: all processes.
pub(super) async fn handle_events_stream(
    State(state): State<AppState>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    use oxmgr_core::events::EventFilter;

    let subscribe: Vec<String> = query
        .get("subscribe")
        .map(|s| s.split(',').map(|p| p.trim().to_string()).collect())
        .unwrap_or_default();
    let process = query.get("process").cloned();
    let filter = EventFilter { subscribe, process };

    let rx = state.snapshot.event_tx.subscribe();

    let stream = stream::unfold((rx, filter), |(mut rx, filter)| async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if !filter.matches(&event) {
                        continue;
                    }
                    let payload = serde_json::to_string(&*event).unwrap_or_default();
                    return Some((Ok(Event::default().data(payload)), (rx, filter)));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("event stream client lagged, dropped {n} events");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });

    sse_response(Sse::new(stream))
}

/// Streams host metrics as per-subsystem Server-Sent Events.
///
/// The first event is a `snapshot` carrying everything available (when a collection has
/// completed), so a client starts from a complete state rather than accumulating one as each
/// subsystem happens to move. After that, one event per changed subsystem, named for it — a
/// tick where only processor utilisation moved sends `cpu` and nothing else. A client that
/// provably missed deltas is resynced with a fresh `snapshot` event instead of being dropped.
pub(super) async fn handle_host_stream(State(state): State<AppState>) -> Response {
    // Subscribed before the first read, so an update published while the opening snapshot is
    // being prepared is queued rather than missed.
    let updates = state.snapshot.host.subscribe();
    let opening = state.snapshot.host.current().await;

    // `pending` holds already-built (event name, body) pairs waiting to be emitted: the opening
    // snapshot first, then per-subsystem events from whichever update produced them.
    let mut pending: VecDeque<(&'static str, String)> = VecDeque::new();
    if let Some(metrics) = &opening {
        pending.push_back((
            "snapshot",
            serde_json::to_string(metrics).unwrap_or_else(|_| "null".to_string()),
        ));
    }

    let stream = stream::unfold(
        (updates, state.snapshot, pending),
        |(mut updates, snapshot, mut pending)| async move {
            if let Some((name, body)) = pending.pop_front() {
                return Some((
                    Ok(Event::default().event(name).data(body)),
                    (updates, snapshot, pending),
                ));
            }
            loop {
                match updates.recv().await {
                    Ok(update) => {
                        // Build one event per changed subsystem from the update's own metrics,
                        // then emit the first and keep the rest queued.
                        let mut queued: VecDeque<(&'static str, String)> = update
                            .changed
                            .iter()
                            .map(|subsystem| {
                                let payload = host_subsystem_payload(&update.metrics, *subsystem);
                                (host_event_name(*subsystem), payload.to_string())
                            })
                            .collect();
                        if let Some((name, body)) = queued.pop_front() {
                            return Some((
                                Ok(Event::default().event(name).data(body)),
                                (updates, snapshot, queued),
                            ));
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // The client provably missed deltas, so its state is unreconstructable.
                        // Resend the whole snapshot rather than dropping the connection.
                        tracing::debug!("host stream client lagged; resyncing with snapshot");
                        if let Some(metrics) = snapshot.host.current().await {
                            let body = serde_json::to_string(&metrics)
                                .unwrap_or_else(|_| "null".to_string());
                            return Some((
                                Ok(Event::default().event("snapshot").data(body)),
                                (updates, snapshot, VecDeque::new()),
                            ));
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );

    sse_response(Sse::new(stream))
}

/// Streams one process's log file over Server-Sent Events: the last 200 lines
/// immediately, then the tail as it grows, handling truncation (log rotation)
/// and read errors by reopening the file. A line arrives as one unnamed `data:`
/// event, exactly as the legacy framer produced.
///
/// Query parameters:
/// - `stream`: `stderr` or `error` selects the stderr log; anything else uses stdout.
pub(super) async fn handle_log_stream(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    // axum's `Path` percent-decodes the segment, matching the legacy
    // `decode_segment` on the raw path.
    if name.is_empty() {
        return http_error(404, "not found");
    }
    let Some(process) = state.snapshot.get_process(&name).await else {
        return http_error(404, "service not found");
    };

    let log_path = match query.get("stream").map(String::as_str) {
        Some("stderr" | "error") => process.stderr_log,
        _ => process.stdout_log,
    };

    let initial_lines = oxmgr_manager::logging::read_last_lines(&log_path, 200).unwrap_or_default();
    let stream = stream::iter(
        initial_lines
            .into_iter()
            .map(|line| Ok(Event::default().data(line))),
    )
    .chain(follow_log(&log_path));

    sse_response(Sse::new(stream))
}

/// The named event for the full process list snapshot.
pub(super) const EVENT_PROCESSES: &str = "processes";

/// Host event names that a client may subscribe to individually.
const HOST_EVENT_NAMES: &[&str] = &[
    "snapshot",
    "memory",
    "cpu",
    "load_average",
    "filesystems",
    "network",
    "components",
];

/// Lifecycle event names from `BusEvent::event_name()`.
const LIFECYCLE_EVENT_NAMES: &[&str] = &[
    "process:started",
    "process:online",
    "process:stopped",
    "process:exited",
    "process:crashed",
    "process:restarting",
    "process:errored",
    "log:out",
    "log:err",
    "health:healthy",
    "health:unhealthy",
    "anomaly:detected",
    "anomaly:cleared",
    "remediation:decided",
    "daemon:shutdown",
];

/// Lifecycle prefixes for wildcard patterns like `process:*`.
const LIFECYCLE_PREFIXES: &[&str] = &["process", "log", "health", "anomaly", "remediation"];

/// Returns `true` if `name` is a recognised subscription value (exact name or
/// wildcard prefix like `process:*` or `*`).
pub(super) fn is_known_event_type(name: &str) -> bool {
    name == "*"
        || name == EVENT_PROCESSES
        || HOST_EVENT_NAMES.contains(&name)
        || LIFECYCLE_EVENT_NAMES.contains(&name)
        || LIFECYCLE_PREFIXES.iter().any(|p| format!("{p}:*") == name)
}

/// Returns `true` if a subscription pattern targets lifecycle events
/// (used to decide whether to subscribe to the event bus).
fn is_lifecycle_pattern(p: &str) -> bool {
    p == "*"
        || LIFECYCLE_EVENT_NAMES.contains(&p)
        || LIFECYCLE_PREFIXES
            .iter()
            .any(|prefix| format!("{prefix}:*") == p)
}

/// A unified endpoint that multiplexes process snapshots, process lifecycle
/// events, and host metrics into a single SSE connection, reducing the number
/// of per-client TCP connections from three to one.
///
/// Query parameters:
/// - `subscribe`: comma-separated event types or wildcard patterns (e.g.
///   `"processes,memory,process:crashed"`). Default: all.
/// - `process`: restrict lifecycle events to this process name. Default: all.
/// - `interval_ms`: process snapshot cadence in ms (default 2000, clamped 200..10000).
pub(super) async fn handle_unified_stream(
    State(state): State<AppState>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let subscribe: Vec<String> = query
        .get("subscribe")
        .map(|s| {
            s.split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect()
        })
        .unwrap_or_default();

    if let Some(bad) = subscribe.iter().find(|s| !is_known_event_type(s)) {
        return http_error(400, &format!("unknown event type '{bad}'"));
    }

    let interval_ms = query
        .get("interval_ms")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(2000)
        .clamp(200, 10000);

    let process_filter = query.get("process").cloned();

    // Decide which of the three multiplexed streams are active.
    let sub_empty = subscribe.is_empty();
    let want_processes = sub_empty || subscribe.iter().any(|p| p == EVENT_PROCESSES || p == "*");
    let want_host = sub_empty
        || subscribe
            .iter()
            .any(|p| HOST_EVENT_NAMES.contains(&p.as_str()) || p == "*");
    let want_lifecycle = sub_empty || subscribe.iter().any(|p| is_lifecycle_pattern(p));

    let host_filter: Arc<dyn Fn(&str) -> bool + Send + Sync> = Arc::new({
        let subscribe = subscribe.clone();
        move |name: &str| sub_empty || subscribe.iter().any(|p| p == name || p == "*")
    });

    // Build per-kind streams, each returning named SSE events.
    let mut streams: Vec<BoxedEventStream> = Vec::with_capacity(3);

    if want_processes {
        streams.push(Box::pin(unified_processes_stream(
            state.clone(),
            interval_ms,
        )));
    }
    if want_lifecycle {
        let filter = oxmgr_core::events::EventFilter {
            subscribe: subscribe
                .iter()
                .filter(|p| is_lifecycle_pattern(p))
                .cloned()
                .collect(),
            process: process_filter.clone(),
        };
        streams.push(Box::pin(unified_lifecycle_stream(state.clone(), filter)));
    }
    if want_host {
        let filter = host_filter.clone();
        streams.push(Box::pin(unified_host_stream(state.clone(), filter).await));
    }

    if streams.is_empty() {
        return http_error(400, "no matching event types");
    }

    let stream = select_all(streams);
    sse_response(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

/// Process snapshot stream: emits `event: processes` with a full process list
/// JSON payload at a configurable interval (first frame immediately).
fn unified_processes_stream(
    state: AppState,
    interval_ms: u64,
) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static {
    stream::unfold(
        (state, interval_ms, true),
        |(state, interval, first)| async move {
            if !first {
                tokio::time::sleep(Duration::from_millis(interval)).await;
            }
            let payload = processes_payload_with_attribution(&state).await;
            Some((
                Ok(Event::default().event(EVENT_PROCESSES).data(payload)),
                (state, interval, false),
            ))
        },
    )
}

/// Lifecycle event stream: subscribes to the event bus, applies process and
/// pattern filters, and emits one named SSE event per `BusEvent` using the
/// event's own `event_name()` as the SSE event field.
fn unified_lifecycle_stream(
    state: AppState,
    filter: oxmgr_core::events::EventFilter,
) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static {
    let rx = state.snapshot.event_tx.subscribe();
    stream::unfold((rx, filter), |(mut rx, filter)| async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if !filter.matches(&event) {
                        continue;
                    }
                    let name = event.event_name();
                    let payload = serde_json::to_string(&*event).unwrap_or_default();
                    return Some((Ok(Event::default().event(name).data(payload)), (rx, filter)));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("unified stream client lagged, dropped {n} events");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
}

/// Host metrics stream: first frame is a full `snapshot`, then one event per
/// changed subsystem. On missed deltas the client is resynced with a fresh
/// snapshot rather than dropped.
async fn unified_host_stream(
    state: AppState,
    want_subsystem: Arc<dyn Fn(&str) -> bool + Send + Sync>,
) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static {
    let updates = state.snapshot.host.subscribe();
    let opening = state.snapshot.host.current().await;

    let mut pending: VecDeque<(&'static str, String)> = VecDeque::new();
    if let Some(metrics) = &opening {
        pending.push_back((
            "snapshot",
            serde_json::to_string(metrics).unwrap_or_else(|_| "null".to_string()),
        ));
    }

    stream::unfold(
        (
            updates,
            state.snapshot.host.clone(),
            pending,
            want_subsystem,
        ),
        |(mut updates, host_handle, mut pending, want)| async move {
            if let Some((name, body)) = pending.pop_front() {
                return Some((
                    Ok(Event::default().event(name).data(body)),
                    (updates, host_handle, pending, want),
                ));
            }
            loop {
                match updates.recv().await {
                    Ok(update) => {
                        for subsystem in update.changed {
                            let name = host_event_name(subsystem);
                            if want(name) {
                                let body =
                                    host_subsystem_payload(&update.metrics, subsystem).to_string();
                                pending.push_back((name, body));
                            }
                        }
                        if let Some((name, body)) = pending.pop_front() {
                            return Some((
                                Ok(Event::default().event(name).data(body)),
                                (updates, host_handle, pending, want),
                            ));
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        tracing::debug!(
                            "unified host stream client lagged; resyncing with snapshot"
                        );
                        if let Some(metrics) = host_handle.current().await {
                            let body = serde_json::to_string(&metrics)
                                .unwrap_or_else(|_| "null".to_string());
                            return Some((
                                Ok(Event::default().event("snapshot").data(body)),
                                (updates, host_handle, VecDeque::new(), want),
                            ));
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    )
}

/// Tail-follows the log file after the initial 200 lines: read new lines, sleep
/// briefly at EOF, reopen from the start when the file was truncated (rotation),
/// and reopen at the end when a read failed (replaced under us).
fn follow_log(
    log_path: &std::path::Path,
) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static + use<> {
    let log_path = log_path.to_path_buf();

    stream::unfold(
        (log_path, None::<BufReader<tokio::fs::File>>),
        |(log_path, mut reader)| async move {
            loop {
                let open = match reader.as_mut() {
                    Some(r) => r,
                    None => {
                        match tokio::fs::File::open(&log_path).await {
                            Ok(file) => {
                                let mut opened = BufReader::new(file);
                                opened.seek(std::io::SeekFrom::End(0)).await.ok();
                                reader = Some(opened);
                                // SAFETY: set just above in this branch.
                                #[expect(clippy::unwrap_used, reason = "set just above")]
                                reader.as_mut().unwrap()
                            }
                            Err(err) => {
                                tracing::warn!("log stream open error: {err}, retrying");
                                tokio::time::sleep(Duration::from_millis(300)).await;
                                continue;
                            }
                        }
                    }
                };
                let mut line_buf = String::default();
                match open.read_line(&mut line_buf).await {
                    Ok(0) => {
                        // EOF: check for truncation (log rotation). A newer-shorter file means
                        // it was replaced; reading it from its (new) start replays the rotated
                        // file's content, which is what the legacy stream did.
                        let pos = open.stream_position().await.unwrap_or(0);
                        if tokio::fs::metadata(&log_path)
                            .await
                            .is_ok_and(|m| m.len() < pos)
                        {
                            match tokio::fs::File::open(&log_path).await {
                                Ok(file) => reader = Some(BufReader::new(file)),
                                Err(err) => {
                                    tracing::warn!("log stream reopen error: {err}, retrying");
                                    reader = None;
                                }
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    }
                    Ok(_) => {
                        let line = line_buf.trim_end_matches(['\n', '\r']);
                        if !line.is_empty() {
                            return Some((Ok(Event::default().data(line)), (log_path, reader)));
                        }
                    }
                    Err(err) => {
                        tracing::warn!("log stream read error: {err}, reopening at end");
                        // Reopen and seek to the end at the next pass, as the legacy
                        // handler did for a transient read failure.
                        reader = None;
                    }
                }
            }
        },
    )
}

fn sse_response<S>(sse: Sse<S>) -> Response
where
    S: Stream<Item = Result<Event, Infallible>> + Send + 'static,
{
    let mut response = sse.into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-cache"),
    );
    response
}

fn http_error(status: u16, message: &str) -> Response {
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        axum::Json(serde_json::json!({"ok": false, "message": message})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_known_event_type_exact() {
        assert!(is_known_event_type("*"));
        assert!(is_known_event_type("processes"));
        assert!(is_known_event_type("snapshot"));
        assert!(is_known_event_type("memory"));
        assert!(is_known_event_type("cpu"));
        assert!(is_known_event_type("load_average"));
        assert!(is_known_event_type("filesystems"));
        assert!(is_known_event_type("network"));
        assert!(is_known_event_type("components"));
        assert!(is_known_event_type("process:started"));
        assert!(is_known_event_type("process:online"));
        assert!(is_known_event_type("process:stopped"));
        assert!(is_known_event_type("process:exited"));
        assert!(is_known_event_type("process:crashed"));
        assert!(is_known_event_type("process:restarting"));
        assert!(is_known_event_type("process:errored"));
        assert!(is_known_event_type("log:out"));
        assert!(is_known_event_type("log:err"));
        assert!(is_known_event_type("health:healthy"));
        assert!(is_known_event_type("health:unhealthy"));
        assert!(is_known_event_type("anomaly:detected"));
        assert!(is_known_event_type("anomaly:cleared"));
        assert!(is_known_event_type("remediation:decided"));
        assert!(is_known_event_type("daemon:shutdown"));
    }

    #[test]
    fn is_known_event_type_wildcard() {
        assert!(is_known_event_type("process:*"));
        assert!(is_known_event_type("log:*"));
        assert!(is_known_event_type("health:*"));
        assert!(is_known_event_type("anomaly:*"));
        assert!(is_known_event_type("remediation:*"));
    }

    #[test]
    fn is_known_event_type_unknown() {
        assert!(!is_known_event_type(""));
        assert!(!is_known_event_type("host_stream"));
        assert!(!is_known_event_type("process:resumed"));
        assert!(!is_known_event_type("random:event"));
    }

    #[test]
    fn is_lifecycle_pattern_matches() {
        assert!(is_lifecycle_pattern("*"));
        assert!(is_lifecycle_pattern("process:*"));
        assert!(is_lifecycle_pattern("log:out"));
        assert!(is_lifecycle_pattern("anomaly:detected"));
        assert!(is_lifecycle_pattern("daemon:shutdown"));
    }

    #[test]
    fn is_lifecycle_pattern_rejects_non_lifecycle() {
        assert!(!is_lifecycle_pattern("processes"));
        assert!(!is_lifecycle_pattern("snapshot"));
        assert!(!is_lifecycle_pattern("memory"));
        assert!(!is_lifecycle_pattern("cpu"));
    }

    #[test]
    fn event_processes_constant_matches_handler() {
        // The EVENT_PROCESSES constant must be consistent with what the stream emits
        assert_eq!(EVENT_PROCESSES, "processes");
    }
}
