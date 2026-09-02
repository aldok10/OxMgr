use axum::response::{IntoResponse, Response};

use crate::daemon::http::serve_dashboard_config;

/// GET /api/config — returns the current dashboard configuration.
pub(crate) async fn get_config() -> Response {
    serve_dashboard_config().into_response()
}
