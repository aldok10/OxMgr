use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::daemon::http::AppState;

/// GET /api/host — host-level metrics. 503 before the first collection
/// completes: an empty object would be indistinguishable from a host where every
/// subsystem is unavailable.
pub(crate) async fn get_host(State(state): State<AppState>) -> Response {
    match state.snapshot.host_metrics().await {
        Some(metrics) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            serde_json::to_string(&metrics).unwrap_or_else(|_| "null".to_string()),
        )
            .into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "host metrics not collected yet: the first collection has not completed",
        )
            .into_response(),
    }
}

/// GET /api/host/consumers — host-wide top consumers. 503 rather than an empty
/// list when sampling is disabled or has not yet produced a sample: "sampling is
/// off" and "this host has no processes" are different claims, and the second is
/// never true.
pub(crate) async fn get_host_consumers(State(state): State<AppState>) -> Response {
    match state.snapshot.host_consumers().await {
        Some(consumers) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            serde_json::to_string(&consumers).unwrap_or_else(|_| "null".to_string()),
        )
            .into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "host-wide consumer sampling is disabled or has not yet produced a sample",
        )
            .into_response(),
    }
}
