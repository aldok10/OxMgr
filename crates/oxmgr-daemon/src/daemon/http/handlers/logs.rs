use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::collections::HashMap;

use crate::daemon::http::{
    AppState, resolve_archive_index, resolve_line_offset, resolve_stream, serve_log_download,
    serve_log_files, stream_log_path,
};

/// GET /api/processes/:name/logs — log lines tailing/paging.
pub(crate) async fn get_process_logs(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Some(process) = state.snapshot.get_process(&name).await else {
        return (StatusCode::NOT_FOUND, "service not found").into_response();
    };

    let stream = match resolve_stream(&query) {
        Ok(s) => s,
        Err(e) => return *e,
    };
    let lines = query
        .get("lines")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&l| l >= 1)
        .unwrap_or(200) // Default tail lines
        .min(100_000); // Max tail lines

    let before = match resolve_line_offset(&query) {
        Ok(b) => b,
        Err(e) => return *e,
    };
    let index = match resolve_archive_index(&query) {
        Ok(i) => i,
        Err(e) => return *e,
    };

    let base = stream_log_path(&process, stream);
    let log_path = oxmgr_manager::logging::log_file_path(&base, index);

    if index > 0 && !log_path.is_file() {
        return (StatusCode::NOT_FOUND, "log file not found").into_response();
    }

    let range =
        oxmgr_manager::logging::read_line_range(&log_path, before, lines).unwrap_or_else(|_| {
            oxmgr_manager::logging::LineRange {
                lines: Vec::new(),
                reached_start: false,
            }
        });

    let bytes: usize = range.lines.iter().map(String::len).sum();
    axum::Json(json!({
        "path": log_path.display().to_string(),
        "stream": stream,
        "index": index,
        "before": before,
        "lines": range.lines,
        "bytes": bytes,
        "reached_start": range.reached_start,
    }))
    .into_response()
}

/// GET /api/processes/:name/logs/files — list log files.
pub(crate) async fn get_process_log_files(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    match state.snapshot.get_process(&name).await {
        Some(process) => serve_log_files(&process).into_response(),
        None => (StatusCode::NOT_FOUND, "service not found").into_response(),
    }
}

/// GET /api/processes/:name/logs/download — stream file attachment.
pub(crate) async fn get_process_log_download(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    match state.snapshot.get_process(&name).await {
        Some(process) => serve_log_download(&process, &query).await.into_response(),
        None => (StatusCode::NOT_FOUND, "service not found").into_response(),
    }
}
