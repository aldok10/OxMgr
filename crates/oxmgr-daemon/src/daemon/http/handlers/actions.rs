use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::daemon::http::{AppState, HttpCommand};
use crate::daemon::send_api_command;

/// Builds and sends a POST action through the manager loop, keeping every
/// process mutation serialised with the rest of the manager's work.
async fn send_action(state: &AppState, command: HttpCommand) -> Response {
    match send_api_command(&state.command_tx, command).await {
        Ok(response) => response.into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("daemon manager loop is unavailable: {err}"),
        )
            .into_response(),
    }
}

/// POST /api/processes/:name/stop
pub(crate) async fn post_stop(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    send_action(&state, HttpCommand::Stop { target: name }).await
}

/// POST /api/processes/:name/restart
pub(crate) async fn post_restart(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    send_action(&state, HttpCommand::Restart { target: name }).await
}

/// POST /api/processes/:name/reload
pub(crate) async fn post_reload(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    send_action(&state, HttpCommand::Reload { target: name }).await
}

/// POST /api/processes/:name/dismiss/:rule
pub(crate) async fn post_dismiss(
    State(state): State<AppState>,
    Path((name, rule)): Path<(String, String)>,
) -> Response {
    send_action(&state, HttpCommand::Dismiss { target: name, rule }).await
}

/// POST /api/processes/:name/restore/:rule
pub(crate) async fn post_restore(
    State(state): State<AppState>,
    Path((name, rule)): Path<(String, String)>,
) -> Response {
    send_action(&state, HttpCommand::Restore { target: name, rule }).await
}
