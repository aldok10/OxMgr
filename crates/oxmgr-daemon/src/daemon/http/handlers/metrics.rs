use axum::extract::State;
use axum::response::{IntoResponse, Response};

use crate::daemon::PROMETHEUS_CONTENT_TYPE;

use crate::daemon::http::{
    AppState, render_findings_prometheus_metrics, render_host_prometheus_metrics,
    render_prometheus_metrics,
};

/// GET /metrics — Prometheus scrape endpoint. Everything in one response so a
/// single scrape target sees the whole machine:
/// per-process series first, host series after, findings last.
pub(crate) async fn get_metrics(State(state): State<AppState>) -> Response {
    let snapshot = &state.snapshot;
    let processes = snapshot.list_processes().await;

    let mut body = render_prometheus_metrics(&processes);

    // Host series follow the per-process ones in the same scrape: one endpoint, so a
    // scrape config does not need a second target to see the machine its processes
    // are running on. Absent entirely before the first collection completes — an
    // all-zero host block would read as an idle machine.
    if let Some(host) = snapshot.host_metrics().await {
        body.push('\n');
        body.push_str(&render_host_prometheus_metrics(&host));
    }

    // Findings last, in the same scrape. Always rendered rather than gated on there being
    // any: the suppression counters and the warm-up gauge are meaningful at zero, and a
    // series that appears only once something is wrong cannot be alerted on for absence.
    let analysis = snapshot.analysis_snapshot().await;
    body.push('\n');
    body.push_str(&render_findings_prometheus_metrics(&analysis, &processes));

    (
        [(axum::http::header::CONTENT_TYPE, PROMETHEUS_CONTENT_TYPE)],
        body,
    )
        .into_response()
}
