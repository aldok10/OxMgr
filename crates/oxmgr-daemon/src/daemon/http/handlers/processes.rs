use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::daemon::http::AppState;
use crate::daemon::http::process_json_for_transport;
/// GET /api/processes — full list of managed processes (redacted).
pub(crate) async fn get_processes(State(state): State<AppState>) -> Response {
    let processes = state.snapshot.list_processes().await;
    let consumers = state.snapshot.host_consumers().await;
    let sampling_enabled = state.snapshot.consumers.sampling_enabled();
    let redacted: Vec<serde_json::Value> = processes
        .into_iter()
        .map(|process| {
            let value = process_json_for_transport(&process);
            crate::daemon::http::attach_observation_fields(
                value,
                &process,
                consumers.as_ref(),
                sampling_enabled,
            )
        })
        .collect();
    axum::Json(serde_json::Value::Array(redacted)).into_response()
}

/// GET /api/processes/:name — single process detail.
pub(crate) async fn get_process(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    let consumers = state.snapshot.host_consumers().await;
    let sampling_enabled = state.snapshot.consumers.sampling_enabled();
    match state.snapshot.get_process(&name).await {
        Some(process) => {
            let value = process_json_for_transport(&process);
            let value = crate::daemon::http::attach_observation_fields(
                value,
                &process,
                consumers.as_ref(),
                sampling_enabled,
            );
            axum::Json(value).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"ok": false, "message": "service not found"})),
        )
            .into_response(),
    }
}
