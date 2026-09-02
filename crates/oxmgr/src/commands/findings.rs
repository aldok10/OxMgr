use anyhow::{Context, Result};

use oxmgr_daemon::config::AppConfig;
use oxmgr_daemon::ipc::{FindingsReport, IpcRequest, send_request};

use super::common::expect_ok;

/// Shows resource findings and the decisions they produced.
///
/// Read-only by construction: the IPC request it sends returns a report and has no side effect, so
/// there is no path from this command to a process. Protection mode is observe-only by default and
/// is not reachable from here at all.
pub(crate) async fn run(
    config: &AppConfig,
    target: Option<String>,
    json: bool,
    all: bool,
) -> Result<()> {
    let response = send_request(&config.daemon_addr, &IpcRequest::Findings { target }).await?;
    let response = expect_ok(response)?;

    let report = response
        .findings
        .context("daemon returned no findings report")?;

    if json {
        // Serialised whole, including cleared findings, regardless of `--all`: a machine consumer
        // filters for itself, and withholding data from a JSON payload makes it unusable for the
        // thing JSON output exists for.
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    print_table(&report, all);
    Ok(())
}

fn print_table(report: &FindingsReport, all: bool) {
    let rows: Vec<_> = report
        .findings
        .iter()
        .filter(|finding| all || finding.status == "active")
        .collect();

    if rows.is_empty() {
        // The empty case is where this command earns its keep. "No findings" alone is ambiguous —
        // it could mean healthy, or disabled, or not yet warmed — so each of those is named.
        if !report.warming.is_empty() {
            println!(
                "no findings yet: {} still building baselines ({})",
                if report.warming.len() == 1 {
                    "1 process is"
                } else {
                    "processes are"
                },
                report.warming.join(", ")
            );
        } else if all {
            println!("no findings recorded");
        } else {
            println!("no active findings");
        }
        print_suppression(report);
        return;
    }

    println!(
        "{:<16} {:<16} {:<14} {:<8} {:>5}  SUMMARY",
        "PROCESS", "DETECTOR", "METRIC", "STATUS", "CONF"
    );
    for finding in rows {
        // A recurrence is marked, because "this is happening again" is a different operational fact
        // from a first sighting and the occurrence count is the only place it shows.
        let recurrence = if finding.occurrence > 1 {
            format!(" (x{})", finding.occurrence)
        } else {
            String::new()
        };
        println!(
            "{:<16} {:<16} {:<14} {:<8} {:>5.2}  {}{}",
            truncate(&finding.process, 16),
            truncate(&finding.detector, 16),
            truncate(&finding.metric, 14),
            finding.status,
            finding.confidence,
            finding.summary.as_deref().unwrap_or("-"),
            recurrence,
        );
        if finding.status == "active"
            && let Some(guidance) = &finding.guidance
        {
            for step in guidance {
                println!("    • {}", step);
            }
        }
    }

    if !report.decisions.is_empty() {
        println!();
        println!("{:<16} {:<24} OUTCOME", "PROCESS", "RULE");
        for decision in &report.decisions {
            // "would" rather than "did": nothing acts in observe-only mode, and a log read later
            // must not be mistakable for a record of actions taken.
            let outcome = match (&decision.action, &decision.withheld) {
                (Some(action), _) => format!("would {action}"),
                (None, Some(reason)) => reason.clone(),
                (None, None) => "no action".to_string(),
            };
            println!(
                "{:<16} {:<24} {}",
                truncate(&decision.process, 16),
                truncate(&decision.rule, 24),
                outcome
            );
        }
    }

    if !report.warming.is_empty() {
        println!();
        println!(
            "note: {} still building baselines, so their metrics are not yet checked ({})",
            report.warming.len(),
            report.warming.join(", ")
        );
    }

    print_suppression(report);
}

/// Reports what tuning withheld, so a quiet daemon is distinguishable from a blind one.
fn print_suppression(report: &FindingsReport) {
    let total = report.suppressed_global + report.suppressed_detector + report.suppressed_process;
    if total == 0 {
        return;
    }
    println!();
    // Broken out by scope rather than given as one total: "you disabled detection entirely" and
    // "you suppressed one detector on one process" need different responses from the reader.
    println!(
        "suppressed: {} global, {} by detector, {} by process",
        report.suppressed_global, report.suppressed_detector, report.suppressed_process
    );
}

/// Truncates with an ellipsis so a long name cannot break column alignment.
fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    // Counted in chars, not bytes: slicing a multi-byte name by bytes would panic, and this runs on
    // operator-supplied process names.
    let kept: String = value.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}
