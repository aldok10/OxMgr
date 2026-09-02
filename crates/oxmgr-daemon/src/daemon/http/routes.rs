use axum::{
    Router,
    http::Request,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};

use crate::daemon::http::handlers::{
    actions, analytics, config, dashboard, host, logs, metrics, processes, pull,
};
use crate::daemon::http::{AppState, auth};

pub(crate) fn create_router(state: AppState) -> Router {
    let router = Router::new()
        .route("/metrics", get(metrics::get_metrics))
        .route("/", get(dashboard::get_dashboard))
        .route("/favicon.ico", get(dashboard::get_favicon))
        .route("/logs/{name}", get(dashboard::get_log_viewer))
        .route("/api/config", get(config::get_config))
        .route("/api/advisories", get(analytics::get_advisories))
        .route("/api/findings", get(analytics::get_findings))
        .route("/api/decisions", get(analytics::get_decisions))
        .route("/api/typical", get(analytics::get_typical))
        .route("/api/host", get(host::get_host))
        .route("/api/host/consumers", get(host::get_host_consumers))
        .route("/api/processes", get(processes::get_processes))
        .route("/api/processes/{name}", get(processes::get_process))
        .route(
            "/api/processes/{name}/findings",
            get(analytics::get_process_findings),
        )
        .route(
            "/api/processes/{name}/decisions",
            get(analytics::get_process_decisions),
        )
        .route("/api/processes/{name}/logs", get(logs::get_process_logs))
        .route(
            "/api/processes/{name}/logs/files",
            get(logs::get_process_log_files),
        )
        .route(
            "/api/processes/{name}/logs/download",
            get(logs::get_process_log_download),
        )
        .route("/api/processes/{name}/stop", post(actions::post_stop))
        .route("/api/processes/{name}/restart", post(actions::post_restart))
        .route("/api/processes/{name}/reload", post(actions::post_reload))
        .route(
            "/api/processes/{name}/dismiss/{rule}",
            post(actions::post_dismiss),
        )
        .route(
            "/api/processes/{name}/restore/{rule}",
            post(actions::post_restore),
        )
        .route(
            "/api/processes/stream",
            get(super::sse::handle_processes_stream),
        )
        .route("/api/events/stream", get(super::sse::handle_events_stream))
        .route("/api/host/stream", get(super::sse::handle_host_stream))
        .route("/api/stream", get(super::sse::handle_unified_stream))
        .route(
            "/api/processes/{name}/logs/stream",
            get(super::sse::handle_log_stream),
        )
        .route("/pull/{target}", post(pull::post_pull))
        .route("/favicon.svg", get(dashboard::get_favicon))
        // Assets are served by the FALLBACK, not by a wildcard route. This replaces the
        // previous hardcoded allowlist of `/dashboard.css`, `/dashboard.js` and
        // `/theme.js`: an asset directory whose servable names are compiled into the
        // binary is not a static file server, and adding a file meant editing this
        // table and recompiling.
        //
        // A `route("/{*path}", get(…))` was tried first and is wrong: it MATCHES every
        // unmatched path while allowing only GET, so `POST /api/processes/x/explode`
        // became 405 Method Not Allowed instead of 404 — measured, it broke
        // `executor_returns_404_for_unknown_process_and_unknown_action`. A fallback runs
        // only when nothing else matched and receives every method, so unknown API paths
        // keep answering 404 whatever the verb.
        //
        // The fallback only ever reaches files under the configured web directory:
        // `get_asset` resolves through `resolve_contained`, which refuses anything
        // escaping it before a read happens, and refuses any non-GET method.
        .fallback(dashboard::get_asset);

    router
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

async fn auth_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let path = request.uri().path();
    if path == "/" || path.starts_with("/api/") || path.starts_with("/logs/") {
        let mut headers = std::collections::HashMap::new();
        for (name, value) in request.headers().iter() {
            headers.insert(
                name.as_str().to_ascii_lowercase(),
                value.to_str().unwrap_or_default().to_string(),
            );
        }
        if !auth::auth_ok(&headers, &state.auth_creds) {
            tracing::error!("Auth failed for path: {}. Headers: {:?}", path, headers);
            return auth::unauthorized_response().into_response();
        }
    }
    next.run(request).await
}
