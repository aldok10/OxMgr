//! Lint-level cleanup: display-path casts in API rendering.

use oxmgr_core::numeric::u64_to_f64;
use std::fmt::Write as _;

use oxmgr_metrics::host_metrics::HostMetrics;
use oxmgr_metrics::process::{MIN_RATE_INTERVAL_MS, ManagedProcess, ProcessStatus};

/// Renders the host series.
///
/// Every metric here is omitted when the underlying figure is unavailable, never zero-filled.
/// A zero-filled gauge is indistinguishable from a real measurement of zero, and for the host
/// that difference matters most where it is easiest to get wrong: a load average of 0 reads as
/// an idle machine, and a temperature of 0 reads as a cold one, when both actually mean "this
/// platform does not report it".
///
/// Cumulative interface figures are counters; utilisation and capacity are gauges.
///
/// Every `writeln!` below discards its `Result`: `fmt::Write for String` is infallible
/// (the `Result` exists only to satisfy the trait), so the discard cannot hide a failure.
#[expect(
    clippy::let_underscore_must_use,
    reason = "writeln! into an in-memory String is infallible"
)]
pub(in crate::daemon) fn render_host_prometheus_metrics(metrics: &HostMetrics) -> String {
    let mut body = String::new();

    // Identity as labels on a constant gauge, the usual `_info` pattern: these are strings,
    // and a string cannot be a metric value.
    let mut info_labels = vec![
        (
            "host",
            metrics.identity.host_name.clone().unwrap_or_default(),
        ),
        ("os", metrics.identity.os_name.clone().unwrap_or_default()),
        (
            "os_version",
            metrics.identity.os_version.clone().unwrap_or_default(),
        ),
        (
            "kernel",
            metrics.identity.kernel_version.clone().unwrap_or_default(),
        ),
        (
            "arch",
            metrics.identity.cpu_arch.clone().unwrap_or_default(),
        ),
    ];
    info_labels.retain(|(_, value)| !value.is_empty());
    let rendered_labels = info_labels
        .iter()
        .map(|(key, value)| format!("{key}=\"{}\"", escape_prometheus_label_value(value)))
        .collect::<Vec<_>>()
        .join(",");
    body.push_str("# HELP oxmgr_host_info Static identity of the host oxmgr is running on.\n");
    body.push_str("# TYPE oxmgr_host_info gauge\n");
    let _ = writeln!(body, "oxmgr_host_info{{{rendered_labels}}} 1");
    body.push('\n');

    if let Some(uptime) = metrics.uptime_secs {
        body.push_str("# HELP oxmgr_host_uptime_seconds Seconds since the host booted.\n");
        body.push_str("# TYPE oxmgr_host_uptime_seconds gauge\n");
        let _ = writeln!(body, "oxmgr_host_uptime_seconds {uptime}");
        body.push('\n');
    }

    if let Some(memory) = &metrics.memory {
        body.push_str("# HELP oxmgr_host_memory_bytes Host memory in bytes by state.\n");
        body.push_str("# TYPE oxmgr_host_memory_bytes gauge\n");
        let _ = writeln!(
            body,
            "oxmgr_host_memory_bytes{{state=\"total\"}} {}",
            memory.total_bytes
        );
        let _ = writeln!(
            body,
            "oxmgr_host_memory_bytes{{state=\"used\"}} {}",
            memory.used_bytes
        );
        let _ = writeln!(
            body,
            "oxmgr_host_memory_bytes{{state=\"available\"}} {}",
            memory.available_bytes
        );
        let _ = writeln!(
            body,
            "oxmgr_host_memory_bytes{{state=\"free\"}} {}",
            memory.free_bytes
        );
        body.push('\n');

        if let Some(percent) = memory.used_percent {
            body.push_str("# HELP oxmgr_host_memory_used_percent Share of host memory in use.\n");
            body.push_str("# TYPE oxmgr_host_memory_used_percent gauge\n");
            let _ = writeln!(
                body,
                "oxmgr_host_memory_used_percent {}",
                sanitize_prometheus_f32(percent)
            );
            body.push('\n');
        }

        // Omitted entirely on a host with no swap. "0 of 0 used" is not a measurement.
        if let Some(swap) = &memory.swap {
            body.push_str("# HELP oxmgr_host_swap_bytes Host swap in bytes by state.\n");
            body.push_str("# TYPE oxmgr_host_swap_bytes gauge\n");
            let _ = writeln!(
                body,
                "oxmgr_host_swap_bytes{{state=\"total\"}} {}",
                swap.total_bytes
            );
            let _ = writeln!(
                body,
                "oxmgr_host_swap_bytes{{state=\"used\"}} {}",
                swap.used_bytes
            );
            body.push('\n');
        }
    }

    if let Some(cpu) = &metrics.cpu {
        // Withheld until sampled over a valid interval, so a starting daemon does not report
        // an idle machine.
        if let Some(global) = cpu.global_percent {
            body.push_str("# HELP oxmgr_host_cpu_used_percent Host CPU utilisation.\n");
            body.push_str("# TYPE oxmgr_host_cpu_used_percent gauge\n");
            let _ = writeln!(
                body,
                "oxmgr_host_cpu_used_percent {}",
                sanitize_prometheus_f32(global)
            );
            body.push('\n');
        }
        if let Some(cores) = &cpu.per_core {
            body.push_str("# HELP oxmgr_host_cpu_core_used_percent Per-core CPU utilisation.\n");
            body.push_str("# TYPE oxmgr_host_cpu_core_used_percent gauge\n");
            for core in cores {
                let _ = writeln!(
                    body,
                    "oxmgr_host_cpu_core_used_percent{{core=\"{}\"}} {}",
                    escape_prometheus_label_value(&core.name),
                    sanitize_prometheus_f32(core.usage_percent)
                );
            }
            body.push('\n');
        }
    }

    // Absent on a platform that does not provide it, rather than three zeroes.
    if let Some(load) = &metrics.load_average {
        body.push_str("# HELP oxmgr_host_load_average Host load average.\n");
        body.push_str("# TYPE oxmgr_host_load_average gauge\n");
        let _ = writeln!(
            body,
            "oxmgr_host_load_average{{window=\"1m\"}} {}",
            load.one
        );
        let _ = writeln!(
            body,
            "oxmgr_host_load_average{{window=\"5m\"}} {}",
            load.five
        );
        let _ = writeln!(
            body,
            "oxmgr_host_load_average{{window=\"15m\"}} {}",
            load.fifteen
        );
        body.push('\n');
    }

    if let Some(filesystems) = metrics.filesystems.as_deref() {
        body.push_str(
            "# HELP oxmgr_host_filesystem_bytes Filesystem capacity in bytes by state.\n",
        );
        body.push_str("# TYPE oxmgr_host_filesystem_bytes gauge\n");
        for fs in filesystems {
            let mount = escape_prometheus_label_value(&fs.mount_point);
            let kind = escape_prometheus_label_value(&fs.file_system);
            let _ = writeln!(
                body,
                "oxmgr_host_filesystem_bytes{{mount=\"{mount}\",fstype=\"{kind}\",state=\"total\"}} {}",
                fs.total_bytes
            );
            let _ = writeln!(
                body,
                "oxmgr_host_filesystem_bytes{{mount=\"{mount}\",fstype=\"{kind}\",state=\"available\"}} {}",
                fs.available_bytes
            );
            let _ = writeln!(
                body,
                "oxmgr_host_filesystem_bytes{{mount=\"{mount}\",fstype=\"{kind}\",state=\"used\"}} {}",
                fs.used_bytes
            );
        }
        body.push('\n');

        body.push_str("# HELP oxmgr_host_filesystem_used_percent Share of a filesystem in use.\n");
        body.push_str("# TYPE oxmgr_host_filesystem_used_percent gauge\n");
        for fs in filesystems {
            // A pseudo-filesystem reports zero capacity, so its utilisation is unavailable.
            // Emitting 0 would put a permanently-empty series next to the real ones; emitting
            // 100 would page someone.
            if let Some(percent) = fs.used_percent {
                let _ = writeln!(
                    body,
                    "oxmgr_host_filesystem_used_percent{{mount=\"{}\",fstype=\"{}\"}} {}",
                    escape_prometheus_label_value(&fs.mount_point),
                    escape_prometheus_label_value(&fs.file_system),
                    sanitize_prometheus_f32(percent)
                );
            }
        }
        body.push('\n');
    }

    if let Some(network) = &metrics.network {
        // `scope="host"` on every series: these are the machine's interfaces. Per-process
        // network I/O is not measurable (see docs/PROCESS-IO-METRICS.md), and the label is
        // what stops a dashboard query from quietly attributing this to one service.
        body.push_str(
            "# HELP oxmgr_host_network_received_bytes_total Cumulative bytes received per interface.\n",
        );
        body.push_str("# TYPE oxmgr_host_network_received_bytes_total counter\n");
        for iface in &network.interfaces {
            let _ = writeln!(
                body,
                "oxmgr_host_network_received_bytes_total{{interface=\"{}\",scope=\"host\"}} {}",
                escape_prometheus_label_value(&iface.name),
                iface.total_received_bytes
            );
        }
        body.push('\n');

        body.push_str(
            "# HELP oxmgr_host_network_transmitted_bytes_total Cumulative bytes transmitted per interface.\n",
        );
        body.push_str("# TYPE oxmgr_host_network_transmitted_bytes_total counter\n");
        for iface in &network.interfaces {
            let _ = writeln!(
                body,
                "oxmgr_host_network_transmitted_bytes_total{{interface=\"{}\",scope=\"host\"}} {}",
                escape_prometheus_label_value(&iface.name),
                iface.total_transmitted_bytes
            );
        }
        body.push('\n');

        body.push_str(
            "# HELP oxmgr_host_network_errors_total Cumulative interface errors by direction.\n",
        );
        body.push_str("# TYPE oxmgr_host_network_errors_total counter\n");
        for iface in &network.interfaces {
            let name = escape_prometheus_label_value(&iface.name);
            let _ = writeln!(
                body,
                "oxmgr_host_network_errors_total{{interface=\"{name}\",scope=\"host\",direction=\"received\"}} {}",
                iface.errors_on_received
            );
            let _ = writeln!(
                body,
                "oxmgr_host_network_errors_total{{interface=\"{name}\",scope=\"host\",direction=\"transmitted\"}} {}",
                iface.errors_on_transmitted
            );
        }
        body.push('\n');

        // Rates come from the observed interval, and are omitted on the first measurement
        // where no interval exists yet.
        body.push_str(
            "# HELP oxmgr_host_network_received_bytes_per_second Bytes received per second over the observed interval.\n",
        );
        body.push_str("# TYPE oxmgr_host_network_received_bytes_per_second gauge\n");
        for iface in &network.interfaces {
            if let Some(rate) = interface_rate(iface.received_bytes, iface.interval_ms) {
                let _ = writeln!(
                    body,
                    "oxmgr_host_network_received_bytes_per_second{{interface=\"{}\",scope=\"host\"}} {}",
                    escape_prometheus_label_value(&iface.name),
                    sanitize_prometheus_f64(rate)
                );
            }
        }
        body.push('\n');

        body.push_str(
            "# HELP oxmgr_host_network_transmitted_bytes_per_second Bytes transmitted per second over the observed interval.\n",
        );
        body.push_str("# TYPE oxmgr_host_network_transmitted_bytes_per_second gauge\n");
        for iface in &network.interfaces {
            if let Some(rate) = interface_rate(iface.transmitted_bytes, iface.interval_ms) {
                let _ = writeln!(
                    body,
                    "oxmgr_host_network_transmitted_bytes_per_second{{interface=\"{}\",scope=\"host\"}} {}",
                    escape_prometheus_label_value(&iface.name),
                    sanitize_prometheus_f64(rate)
                );
            }
        }
        body.push('\n');
    }

    // Omitted wholesale where the platform has no sensors, rather than a zero-degree series.
    if let Some(components) = &metrics.components {
        let readable: Vec<_> = components
            .iter()
            .filter(|c| c.temperature_celsius.is_some())
            .collect();
        if !readable.is_empty() {
            body.push_str("# HELP oxmgr_host_temperature_celsius Component temperature.\n");
            body.push_str("# TYPE oxmgr_host_temperature_celsius gauge\n");
            for component in readable {
                if let Some(temp) = component.temperature_celsius {
                    let _ = writeln!(
                        body,
                        "oxmgr_host_temperature_celsius{{component=\"{}\"}} {}",
                        escape_prometheus_label_value(&component.label),
                        sanitize_prometheus_f32(temp)
                    );
                }
            }
            body.push('\n');
        }
    }

    // A failing subsystem is reported rather than left to be inferred from a gap: a missing
    // series could be a collection failure or a platform that never had it, and those need
    // different responses.
    if !metrics.failures.is_empty() {
        body.push_str(
            "# HELP oxmgr_host_subsystem_failed Whether a host metrics subsystem failed to collect.\n",
        );
        body.push_str("# TYPE oxmgr_host_subsystem_failed gauge\n");
        for failure in &metrics.failures {
            let _ = writeln!(
                body,
                "oxmgr_host_subsystem_failed{{subsystem=\"{:?}\"}} 1",
                failure.subsystem
            );
        }
        body.push('\n');
    }

    body
}

/// Bytes per second over an observed interval, or `None` when there is no usable interval.
///
/// Mirrors the per-process rule: the first measurement has no interval, and dividing by the
/// nominal one would overstate the rate exactly when collection was late.
fn interface_rate(amount: u64, interval_ms: Option<u64>) -> Option<f64> {
    let interval_ms = interval_ms?;
    if interval_ms < MIN_RATE_INTERVAL_MS {
        return None;
    }
    Some(u64_to_f64(amount) * 1000.0 / u64_to_f64(interval_ms))
}

/// Same discard rule as `render_host_prometheus_metrics`: `writeln!` into a `String`
/// cannot fail, so the `Result` carries no information.
#[expect(
    clippy::let_underscore_must_use,
    reason = "writeln! into an in-memory String is infallible"
)]
pub(in crate::daemon) fn render_prometheus_metrics(processes: &[ManagedProcess]) -> String {
    let mut body = String::new();

    body.push_str(
        "# HELP oxmgr_managed_processes Total number of processes currently managed by oxmgr.\n",
    );
    body.push_str("# TYPE oxmgr_managed_processes gauge\n");
    let _ = writeln!(body, "oxmgr_managed_processes {}", processes.len());
    body.push('\n');

    body.push_str("# HELP oxmgr_process_info Static metadata about each managed process.\n");
    body.push_str("# TYPE oxmgr_process_info gauge\n");
    for process in processes {
        let labels = process_metric_labels(
            process,
            &[
                ("desired_state", desired_state_label(process)),
                ("restart_policy", process.restart_policy.to_string()),
                ("status", process.status.to_string()),
            ],
        );
        let _ = writeln!(body, "oxmgr_process_info{labels} 1");
    }
    body.push('\n');

    body.push_str("# HELP oxmgr_process_up Whether the managed process is currently running.\n");
    body.push_str("# TYPE oxmgr_process_up gauge\n");
    for process in processes {
        let value =
            u8::from(matches!(process.status, ProcessStatus::Running) && process.pid.is_some());
        let _ = writeln!(
            body,
            "oxmgr_process_up{} {}",
            process_metric_labels(process, &[]),
            value
        );
    }
    body.push('\n');

    body.push_str(
        "# HELP oxmgr_process_restart_count Number of restarts recorded for the managed process.\n",
    );
    body.push_str("# TYPE oxmgr_process_restart_count counter\n");
    for process in processes {
        let _ = writeln!(
            body,
            "oxmgr_process_restart_count{} {}",
            process_metric_labels(process, &[]),
            process.restart_count
        );
    }
    body.push('\n');

    body.push_str(
        "# HELP oxmgr_process_cpu_percent Latest CPU usage percentage reported by oxmgr.\n",
    );
    body.push_str("# TYPE oxmgr_process_cpu_percent gauge\n");
    for process in processes {
        let _ = writeln!(
            body,
            "oxmgr_process_cpu_percent{} {}",
            process_metric_labels(process, &[]),
            sanitize_prometheus_f32(process.cpu_percent)
        );
    }
    body.push('\n');

    body.push_str(
        "# HELP oxmgr_process_memory_bytes Latest memory usage in bytes reported by oxmgr.\n",
    );
    body.push_str("# TYPE oxmgr_process_memory_bytes gauge\n");
    for process in processes {
        let _ = writeln!(
            body,
            "oxmgr_process_memory_bytes{} {}",
            process_metric_labels(process, &[]),
            process.memory_bytes
        );
    }
    body.push('\n');

    body.push_str("# HELP oxmgr_process_pid Current operating-system PID for the process, or 0 when unavailable.\n");
    body.push_str("# TYPE oxmgr_process_pid gauge\n");
    for process in processes {
        let _ = writeln!(
            body,
            "oxmgr_process_pid{} {}",
            process_metric_labels(process, &[]),
            process.pid.unwrap_or_default()
        );
    }
    body.push('\n');

    body.push_str("# HELP oxmgr_process_status Current lifecycle status of the managed process.\n");
    body.push_str("# TYPE oxmgr_process_status gauge\n");
    for process in processes {
        let labels = process_metric_labels(process, &[("status", process.status.to_string())]);
        let _ = writeln!(body, "oxmgr_process_status{labels} 1");
    }
    body.push('\n');

    body.push_str(
        "# HELP oxmgr_process_health_status Current health-check status of the managed process.\n",
    );
    body.push_str("# TYPE oxmgr_process_health_status gauge\n");
    for process in processes {
        let labels = process_metric_labels(
            process,
            &[("health_status", process.health_status.to_string())],
        );
        let _ = writeln!(body, "oxmgr_process_health_status{labels} 1");
    }
    body.push('\n');

    body.push_str("# HELP oxmgr_process_last_started_at_seconds Unix timestamp of the last successful start, or 0 when unknown.\n");
    body.push_str("# TYPE oxmgr_process_last_started_at_seconds gauge\n");
    for process in processes {
        let _ = writeln!(
            body,
            "oxmgr_process_last_started_at_seconds{} {}",
            process_metric_labels(process, &[]),
            process.last_started_at.unwrap_or_default()
        );
    }
    body.push('\n');

    body.push_str("# HELP oxmgr_process_last_metrics_at_seconds Unix timestamp of the last resource metrics refresh, or 0 when unknown.\n");
    body.push_str("# TYPE oxmgr_process_last_metrics_at_seconds gauge\n");
    for process in processes {
        let _ = writeln!(
            body,
            "oxmgr_process_last_metrics_at_seconds{} {}",
            process_metric_labels(process, &[]),
            process.last_metrics_at.unwrap_or_default()
        );
    }
    body.push('\n');

    // Disk I/O.
    //
    // The lifetime accumulators are the only cumulative disk figures exposed, and they are
    // the only ones that may be typed as counters: they are scoped to the managed process
    // and never decrease, whereas sysinfo's per-PID totals fall backwards on every restart
    // (measured: pid 53658 write_total 33353728 -> pid 4032 write_total 1814528). Prometheus
    // reads a decrease as a counter reset, so exposing a per-PID total here would turn each
    // restart into a phantom discontinuity.
    body.push_str(
        "# HELP oxmgr_process_disk_read_bytes_total Cumulative bytes read by the managed process across restarts.\n",
    );
    body.push_str("# TYPE oxmgr_process_disk_read_bytes_total counter\n");
    for process in processes {
        let _ = writeln!(
            body,
            "oxmgr_process_disk_read_bytes_total{} {}",
            process_metric_labels(process, &[]),
            process.disk_read_total
        );
    }
    body.push('\n');

    body.push_str(
        "# HELP oxmgr_process_disk_written_bytes_total Cumulative bytes written by the managed process across restarts.\n",
    );
    body.push_str("# TYPE oxmgr_process_disk_written_bytes_total counter\n");
    for process in processes {
        let _ = writeln!(
            body,
            "oxmgr_process_disk_written_bytes_total{} {}",
            process_metric_labels(process, &[]),
            process.disk_write_total
        );
    }
    body.push('\n');

    // Rates are omitted, not zero-filled, when the daemon has no usable measurement — a
    // stopped process, the first sample after a PID appears, or an interval too short to
    // divide by. A zero-filled sample would be indistinguishable from a process that ran
    // and performed no I/O, and would drag any average over the series towards zero.
    body.push_str(
        "# HELP oxmgr_process_disk_read_bytes_per_second Bytes read per second over the interval the last sample covered.\n",
    );
    body.push_str("# TYPE oxmgr_process_disk_read_bytes_per_second gauge\n");
    for process in processes {
        if let Some(rate) = process.disk_read_rate_bps() {
            let _ = writeln!(
                body,
                "oxmgr_process_disk_read_bytes_per_second{} {}",
                process_metric_labels(process, &[]),
                sanitize_prometheus_f64(rate)
            );
        }
    }
    body.push('\n');

    body.push_str(
        "# HELP oxmgr_process_disk_written_bytes_per_second Bytes written per second over the interval the last sample covered.\n",
    );
    body.push_str("# TYPE oxmgr_process_disk_written_bytes_per_second gauge\n");
    for process in processes {
        if let Some(rate) = process.disk_write_rate_bps() {
            let _ = writeln!(
                body,
                "oxmgr_process_disk_written_bytes_per_second{} {}",
                process_metric_labels(process, &[]),
                sanitize_prometheus_f64(rate)
            );
        }
    }

    body
}

/// Renders the findings and decision series.
///
/// Kept separate from `render_prometheus_metrics` rather than appended inside it, because the
/// process series come from the process list and these come from the analysis snapshot: joining
/// them would mean threading a second argument through a function that 30-odd series already share.
///
/// Series names follow the existing `oxmgr_*` convention. Labels reuse `name` and `namespace` so a
/// scrape config that already groups by process needs no change, and add the finding's own
/// dimensions — a finding is identified by detector and metric, so those are labels rather than
/// separate series.
///
/// Same discard rule as the other render functions: `writeln!` into a `String` cannot fail.
#[expect(
    clippy::let_underscore_must_use,
    reason = "writeln! into an in-memory String is infallible"
)]
pub(in crate::daemon) fn render_findings_prometheus_metrics(
    analysis: &oxmgr_analytics::analysis::AnalysisSnapshot,
    processes: &[ManagedProcess],
) -> String {
    let mut body = String::new();

    // Namespace is looked up per finding rather than carried in the snapshot, so the label matches
    // whatever the process record says now.
    let namespace_of = |process: &str| -> String {
        processes
            .iter()
            .find(|candidate| candidate.name == process)
            .and_then(|candidate| candidate.namespace.clone())
            .unwrap_or_default()
    };

    let finding_labels = |finding: &oxmgr_core::findings::Finding| -> String {
        let mut rendered = String::from("{");
        for (index, (key, value)) in [
            ("name", finding.key.process.clone()),
            ("namespace", namespace_of(&finding.key.process)),
            ("detector", finding.key.detector.to_string()),
            ("metric", finding.key.metric.to_string()),
            ("variant", finding.key.variant.clone().unwrap_or_default()),
        ]
        .iter()
        .enumerate()
        {
            if index > 0 {
                rendered.push(',');
            }
            rendered.push_str(key);
            rendered.push_str("=\"");
            rendered.push_str(&escape_prometheus_label_value(value));
            rendered.push('"');
        }
        rendered.push('}');
        rendered
    };

    // A gauge at 1 per active finding, which is how Prometheus expresses a set. Cleared findings are
    // deliberately absent rather than exposed at 0: a series that disappears is how a resolved
    // condition ends an alert, whereas a 0 sample keeps the series alive for ever.
    body.push_str(
        "# HELP oxmgr_finding_active A finding whose condition currently holds, labelled by detector and metric.\n",
    );
    body.push_str("# TYPE oxmgr_finding_active gauge\n");
    for finding in analysis.active() {
        let _ = writeln!(body, "oxmgr_finding_active{} 1", finding_labels(finding));
    }
    body.push('\n');

    body.push_str("# HELP oxmgr_finding_confidence Confidence of an active finding, 0 to 1.\n");
    body.push_str("# TYPE oxmgr_finding_confidence gauge\n");
    for finding in analysis.active() {
        let _ = writeln!(
            body,
            "oxmgr_finding_confidence{} {}",
            finding_labels(finding),
            sanitize_prometheus_f64(finding.confidence.score)
        );
    }
    body.push('\n');

    // The episode counter. Typed as a gauge, not a counter, because it is per finding key and resets
    // when the key is forgotten on delete — a counter that can go backwards is a counter that lies.
    body.push_str(
        "# HELP oxmgr_finding_occurrence Which episode of this condition, from 1. Above 1 is a recurrence.\n",
    );
    body.push_str("# TYPE oxmgr_finding_occurrence gauge\n");
    for finding in analysis.active() {
        let _ = writeln!(
            body,
            "oxmgr_finding_occurrence{} {}",
            finding_labels(finding),
            finding.occurrence
        );
    }
    body.push('\n');

    // Suppression, so a quiet daemon can be told apart from a blind one. Three separate scopes
    // rather than one total: "you disabled detection" and "you suppressed this one detector" need
    // different responses.
    body.push_str(
        "# HELP oxmgr_findings_suppressed_total Findings withheld by tuning, by the scope that withheld them.\n",
    );
    body.push_str("# TYPE oxmgr_findings_suppressed_total counter\n");
    for (scope, value) in [
        ("global", analysis.suppressed.blocked_globally),
        ("detector", analysis.suppressed.blocked_by_detector),
        ("process", analysis.suppressed.blocked_by_process),
    ] {
        let _ = writeln!(
            body,
            "oxmgr_findings_suppressed_total{{scope=\"{scope}\"}} {value}"
        );
    }
    body.push('\n');

    // Baseline warm-up, which is what makes an absence of findings explainable: a warming process
    // has not been checked and found healthy.
    body.push_str(
        "# HELP oxmgr_process_baseline_warming Whether any of the process's baselines is still warming.\n",
    );
    body.push_str("# TYPE oxmgr_process_baseline_warming gauge\n");
    for process in processes {
        let warming = analysis.warming.iter().any(|name| name == &process.name);
        let _ = writeln!(
            body,
            "oxmgr_process_baseline_warming{} {}",
            process_metric_labels(process, &[]),
            u8::from(warming)
        );
    }
    body.push('\n');

    // Decisions, by rule and by whether they proposed an action. Observe-only is the default, so
    // `acted` is the label that tells an operator whether protection mode is live.
    body.push_str(
        "# HELP oxmgr_remediation_decisions Recent decisions retained, by rule and outcome.\n",
    );
    body.push_str("# TYPE oxmgr_remediation_decisions gauge\n");
    let mut by_rule: std::collections::BTreeMap<(String, String), u32> =
        std::collections::BTreeMap::new();
    for decision in &analysis.decisions {
        let Some(rule) = decision.rule else { continue };
        let outcome = if decision.action.is_some() {
            "proposed"
        } else {
            "withheld"
        };
        *by_rule
            .entry((rule.to_string(), outcome.to_string()))
            .or_default() += 1;
    }
    for ((rule, outcome), count) in by_rule {
        let _ = writeln!(
            body,
            "oxmgr_remediation_decisions{{rule=\"{}\",outcome=\"{}\"}} {count}",
            escape_prometheus_label_value(&rule),
            escape_prometheus_label_value(&outcome)
        );
    }

    body
}

pub(in crate::daemon) fn escape_prometheus_label_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn process_metric_labels(process: &ManagedProcess, extra: &[(&str, String)]) -> String {
    let mut labels = vec![
        ("id", process.id.to_string()),
        ("name", process.name.clone()),
        ("namespace", process.namespace.clone().unwrap_or_default()),
    ];
    labels.extend(extra.iter().map(|(key, value)| (*key, value.clone())));

    let mut rendered = String::from("{");
    for (index, (key, value)) in labels.iter().enumerate() {
        if index > 0 {
            rendered.push(',');
        }
        rendered.push_str(key);
        rendered.push_str("=\"");
        rendered.push_str(&escape_prometheus_label_value(value));
        rendered.push('"');
    }
    rendered.push('}');
    rendered
}

fn desired_state_label(process: &ManagedProcess) -> String {
    match process.desired_state {
        oxmgr_metrics::process::DesiredState::Running => "running".to_string(),
        oxmgr_metrics::process::DesiredState::Stopped => "stopped".to_string(),
    }
}

fn sanitize_prometheus_f32(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}

/// Guards a rate against `NaN` and the infinities, which Prometheus text format cannot carry.
///
/// Rates are already withheld when the interval is unusable, so a non-finite value here would
/// mean an arithmetic fault upstream rather than a missing measurement. Zero is the safe
/// rendering: the alternative is emitting a token that fails a scrape and takes the whole
/// series with it.
fn sanitize_prometheus_f64(value: f64) -> f64 {
    if value.is_finite() { value } else { 0.0 }
}
