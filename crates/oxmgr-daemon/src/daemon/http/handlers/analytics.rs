use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::daemon::http::AppState;

/// GET /api/advisories — configuration risk advisories, derived on demand.
pub(crate) async fn get_advisories(State(state): State<AppState>) -> Response {
    let snapshot = &state.snapshot;
    let processes = snapshot.list_processes().await;

    let capacity = snapshot
        .host_metrics()
        .await
        .and_then(|metrics| metrics.memory.as_ref().map(|memory| memory.total_bytes))
        .and_then(oxmgr_manager::advisories::HostCapacity::from_total_memory_bytes);

    let dismissed = snapshot.dismissals().await;

    let reports: Vec<serde_json::Value> = processes
        .iter()
        .map(|process| {
            let report = oxmgr_manager::advisories::evaluate(
                &oxmgr_manager::advisories::ProcessConfig::from(process),
                capacity,
            );
            let dismissed_rules = dismissed.get(&process.name);
            let is_dismissed = |advisory: &oxmgr_manager::advisories::Advisory| {
                dismissed_rules
                    .map(|rules| rules.contains(advisory.id.as_ref()))
                    .unwrap_or(false)
            };

            let (dismissed_list, active): (Vec<_>, Vec<_>) =
                report.advisories.iter().cloned().partition(is_dismissed);

            let highest = active
                .iter()
                .map(|advisory| advisory.severity)
                .max()
                .map(|severity| severity.label());

            json!({
                "process": process.name,
                "id": process.id,
                "highest_severity": highest,
                "capacity": report.capacity,
                "advisories": active,
                "dismissed": dismissed_list,
            })
        })
        .collect();

    let total: usize = reports
        .iter()
        .map(|entry| entry["advisories"].as_array().map_or(0, Vec::len))
        .sum();
    let dismissed_total: usize = reports
        .iter()
        .map(|entry| entry["dismissed"].as_array().map_or(0, Vec::len))
        .sum();

    axum::Json(json!({
        "processes": reports,
        "total": total,
        "dismissed_total": dismissed_total,
        "capacity_available": capacity.is_some(),
    }))
    .into_response()
}

/// GET /api/findings — findings newest and most severe first, plus suppression
/// and warm-up context.
pub(crate) async fn get_findings(State(state): State<AppState>) -> Response {
    let analysis = state.snapshot.analysis_snapshot().await;
    let findings: Vec<serde_json::Value> = analysis
        .findings
        .iter()
        .map(|finding| {
            let mut value = serde_json::to_value(finding).unwrap_or(serde_json::Value::Null);
            if let serde_json::Value::Object(map) = &mut value {
                let guidance = oxmgr_core::findings::guidance_for(finding)
                    .and_then(|steps| serde_json::to_value(steps).ok());
                map.insert(
                    "guidance".to_string(),
                    guidance.unwrap_or(serde_json::Value::Null),
                );
            }
            value
        })
        .collect();
    let active = analysis.active().count();

    axum::Json(json!({
        "findings": findings,
        "active": active,
        "total": analysis.findings.len(),
        "suppressed": {
            "global": analysis.suppressed.blocked_globally,
            "detector": analysis.suppressed.blocked_by_detector,
            "process": analysis.suppressed.blocked_by_process,
        },
        "warming": analysis.warming,
    }))
    .into_response()
}

/// GET /api/decisions — recorded decision history.
pub(crate) async fn get_decisions(State(state): State<AppState>) -> Response {
    let analysis = state.snapshot.analysis_snapshot().await;
    let decisions: Vec<serde_json::Value> = analysis
        .decisions
        .iter()
        .map(|decision| {
            json!({
                "process": decision.process,
                "at": decision.at_unix,
                "rule": decision.rule.map(|rule| rule.to_string()),
                "action": decision.action.map(|action| action.to_string()),
                "withheld": decision.withheld.as_ref().map(|w| w.reason()),
                "findings": decision
                    .findings
                    .iter()
                    .map(|key| key.as_string())
                    .collect::<Vec<String>>(),
                "summary": decision.summary(),
            })
        })
        .collect();

    axum::Json(json!({
        "decisions": decisions,
        "total": analysis.decisions.len(),
        "acting_enabled": false,
    }))
    .into_response()
}

/// GET /api/processes/:name/findings — findings for one process.
///
/// The existence check comes first and returns 404, so an unknown process is
/// refused rather than answered with an empty list — "this process has no
/// findings" and "this process does not exist" are different facts, and
/// collapsing them would let a typo read as a clean bill of health.
pub(crate) async fn get_process_findings(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    let Some(_) = state.snapshot.get_process(&name).await else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"ok": false, "message": "service not found"})),
        )
            .into_response();
    };
    let analysis = state.snapshot.analysis_snapshot().await;
    let findings: Vec<serde_json::Value> = analysis
        .for_process(&name)
        .into_iter()
        .map(|finding| serde_json::to_value(finding).unwrap_or(serde_json::Value::Null))
        .collect();
    let active = analysis
        .for_process(&name)
        .into_iter()
        .filter(|finding| finding.is_active())
        .count();

    axum::Json(json!({
        "process": name,
        "findings": findings,
        "active": active,
        "total": analysis.for_process(&name).len(),
        // Per process, because that is the scope in which the question is asked: "why
        // does this process have no findings" is answered by its own warm-up state.
        "warming": analysis.warming.iter().any(|warming| warming == &name),
    }))
    .into_response()
}

/// GET /api/processes/:name/decisions — decision history for one process, same
/// refusal rule as findings.
pub(crate) async fn get_process_decisions(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    if state.snapshot.get_process(&name).await.is_none() {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"ok": false, "message": "service not found"})),
        )
            .into_response();
    }
    let analysis = state.snapshot.analysis_snapshot().await;
    let decisions: Vec<serde_json::Value> = analysis
        .decisions
        .iter()
        .filter(|decision| decision.process == name)
        .map(|decision| {
            json!({
                "at": decision.at_unix,
                "rule": decision.rule.map(|rule| rule.to_string()),
                "action": decision.action.map(|action| action.to_string()),
                "withheld": decision.withheld.as_ref().map(|w| w.reason()),
                "summary": decision.summary(),
            })
        })
        .collect();

    axum::Json(json!({
        "process": name,
        "decisions": decisions,
        "acting_enabled": false,
    }))
    .into_response()
}

/// GET /api/typical — current and typical values together, per process and host.
pub(crate) async fn get_typical(State(state): State<AppState>) -> Response {
    let snapshot = &state.snapshot;
    let processes = snapshot.list_processes().await;
    let typical = snapshot.typical_values().await;

    let entries: Vec<serde_json::Value> = processes
        .iter()
        .map(|process| {
            let report = typical.get(&process.name);
            let running = process.status == oxmgr_metrics::process::ProcessStatus::Running
                && process.pid.is_some();
            let render = |value: Option<&oxmgr_core::severity::Typical>| match value {
                Some(typical) => match typical.value() {
                    Some(t) => json!({
                        "state": "available",
                        "median": t.median,
                        "window_secs": t.window_secs,
                        "sample_count": t.sample_count,
                    }),
                    None => match typical {
                        oxmgr_core::severity::Typical::Unavailable { reason } => json!({
                            "state": "unavailable",
                            "reason": reason,
                        }),
                        oxmgr_core::severity::Typical::Available(_) => json!({
                            "state": "unavailable",
                            "reason": "internal inconsistency: Available typical returned no value",
                        }),
                    },
                },
                None => json!({ "state": "unavailable", "reason": "no_history" }),
            };

            json!({
                "process": process.name,
                "id": process.id,
                "running": running,
                "current": {
                    "cpu_percent": running.then_some(process.cpu_percent),
                    "memory_bytes": running.then_some(process.memory_bytes),
                },
                "typical": {
                    "cpu_percent": render(report.and_then(|r| r.cpu.as_ref())),
                    "memory_bytes": render(report.and_then(|r| r.memory.as_ref())),
                },
            })
        })
        .collect();

    let host = snapshot.host_metrics().await;
    axum::Json(json!({
        "processes": entries,
        "host": {
            "current": {
                "cpu_percent": host
                    .as_ref()
                    .and_then(|metrics| metrics.cpu.as_ref())
                    .and_then(|cpu| cpu.global_percent),
                "memory_used_percent": host
                    .as_ref()
                    .and_then(|metrics| metrics.memory.as_ref())
                    .and_then(|memory| memory.used_percent),
                "memory_effective_used_percent": host
                    .as_ref()
                    .and_then(|metrics| metrics.memory.as_ref())
                    .and_then(|memory| memory.effective_used_percent),
                "memory_effective_total_bytes": host
                    .as_ref()
                    .and_then(|metrics| metrics.memory.as_ref())
                    .and_then(|memory| memory.effective_total_bytes),
            },
            "typical": {
                "state": "unavailable",
                "reason": "no_history",
            },
        },
    }))
    .into_response()
}
