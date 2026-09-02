use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::daemon::http::{AppState, HttpCommand, extract_api_secret};
use crate::daemon::send_api_command;

/// POST /pull/:target — triggers a pull from the registry for the target
/// process. The webhook-secret verification runs inside the manager loop
/// (`execute_api_request`), where the configured secret lives; the secret is
/// extracted here (X-OXMGR-SECRET first, then Authorization: Bearer) and
/// carried on the command, keeping that verification in exactly one place.
pub(crate) async fn post_pull(
    State(state): State<AppState>,
    Path(target): Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    if target.is_empty() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    let command = HttpCommand::Pull {
        target,
        secret: extract_api_secret(&headers),
    };
    match send_api_command(&state.command_tx, command).await {
        Ok(response) => response.into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("daemon manager loop is unavailable: {err}"),
        )
            .into_response(),
    }
}
