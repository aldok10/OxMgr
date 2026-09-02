use anyhow::Result;

#[cfg(unix)]
use oxmgr_core::events::{BusEvent, EventFilter};
use oxmgr_daemon::config::AppConfig;

pub(crate) async fn run(
    config: &AppConfig,
    process: Option<String>,
    filter: Vec<String>,
    json: bool,
) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = (config, process, filter, json);
        // The refusal text is DERIVED from the platform matrix rather than written here.
        //
        // Task 3.1 asks that this report its platform limitation rather than a socket error — and the
        // previous message ("only supported on Unix platforms") was a platform error, but it restated
        // the level without naming the constraint or the alternative. It also duplicated the matrix's
        // own reason, so the two could drift: someone correcting the explanation in one place would
        // leave the other stale.
        //
        // Reading it from the declaration means the operator gets the constraint AND the workaround
        // (the HTTP event stream), and there is one place to improve the wording.
        anyhow::bail!(
            "{}",
            oxmgr_metrics::platform::event_stream_unavailable_message()
        );
    }

    #[cfg(unix)]
    run_unix(config, process, filter, json).await
}

#[cfg(unix)]
async fn run_unix(
    config: &AppConfig,
    process: Option<String>,
    filter: Vec<String>,
    json: bool,
) -> Result<()> {
    use anyhow::Context;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let socket_path = &config.event_socket_path;
    let mut stream = UnixStream::connect(socket_path).await.with_context(|| {
        format!(
            "failed to connect to event socket at {} (is the daemon running?)",
            socket_path.display()
        )
    })?;

    // Send the filter as the first line.
    let event_filter = EventFilter {
        subscribe: filter,
        process,
    };
    let mut payload = serde_json::to_vec(&event_filter).context("failed to encode filter")?;
    payload.push(b'\n');
    stream
        .write_all(&payload)
        .await
        .context("failed to send filter")?;

    let (read_half, _write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .context("read from event socket failed")?;
        if n == 0 {
            break; // daemon closed the connection
        }

        let trimmed = line.trim();
        if json {
            println!("{trimmed}");
        } else {
            match serde_json::from_str::<BusEvent>(trimmed) {
                Ok(event) => print_event(&event),
                Err(_) => println!("{trimmed}"),
            }
        }
    }

    Ok(())
}

#[cfg(unix)]
fn print_event(event: &BusEvent) {
    use oxmgr_core::events::BusEvent::*;

    let ts = format_timestamp(event_at(event));

    match event {
        ProcessStarted { process, .. } => {
            println!("[{ts}] \x1b[36mprocess:started\x1b[0m   {}", process.name);
        }
        ProcessOnline { process, data, .. } => {
            println!(
                "[{ts}] \x1b[32mprocess:online\x1b[0m    {}  status={}",
                process.name, data.status
            );
        }
        ProcessStopped { process, .. } => {
            println!("[{ts}] \x1b[33mprocess:stopped\x1b[0m   {}", process.name);
        }
        ProcessExited { process, data, .. } => {
            println!(
                "[{ts}] \x1b[33mprocess:exited\x1b[0m    {}  exit_code={} restarts={}",
                process.name,
                fmt_opt(data.exit_code),
                data.restart_count
            );
        }
        ProcessCrashed { process, data, .. } => {
            println!(
                "[{ts}] \x1b[31mprocess:crashed\x1b[0m   {}  exit_code={} restarts={}",
                process.name,
                fmt_opt(data.exit_code),
                data.restart_count
            );
        }
        ProcessRestarting { process, data, .. } => {
            println!(
                "[{ts}] \x1b[33mprocess:restarting\x1b[0m {}  delay={}s restarts={}",
                process.name, data.delay_secs, data.restart_count
            );
        }
        ProcessErrored { process, .. } => {
            println!(
                "[{ts}] \x1b[31mprocess:errored\x1b[0m   {}  (crash loop limit reached)",
                process.name
            );
        }
        LogOut { process, data, .. } => {
            println!(
                "[{ts}] \x1b[2mlog:out\x1b[0m           {}  {}",
                process.name, data.line
            );
        }
        LogErr { process, data, .. } => {
            println!(
                "[{ts}] \x1b[31mlog:err\x1b[0m           {}  {}",
                process.name, data.line
            );
        }
        HealthHealthy { process, .. } => {
            println!("[{ts}] \x1b[32mhealth:healthy\x1b[0m    {}", process.name);
        }
        HealthUnhealthy { process, data, .. } => {
            // The correlated findings are appended rather than replacing the error, because the
            // error is still the primary fact: the findings say what was *also* true.
            let correlated = if data.resource_findings.is_empty() {
                String::new()
            } else {
                format!(" findings={}", data.resource_findings.join(","))
            };
            println!(
                "[{ts}] \x1b[31mhealth:unhealthy\x1b[0m  {}  failures={} error={}{correlated}",
                process.name, data.failures, data.error
            );
        }
        AnomalyDetected { process, data, .. } => {
            println!(
                "[{ts}] \x1b[35manomaly:detected\x1b[0m  {}  {} {} confidence={:.2}{}",
                process.name,
                data.detector,
                data.metric,
                data.confidence,
                // A recurrence is marked, because "this is happening again" is a different
                // operational fact from a first sighting.
                if data.occurrence > 1 {
                    format!(" occurrence={}", data.occurrence)
                } else {
                    String::new()
                }
            );
        }
        AnomalyCleared { process, data, .. } => {
            println!(
                "[{ts}] \x1b[32manomaly:cleared\x1b[0m   {}  {} {}",
                process.name, data.detector, data.metric
            );
        }
        RemediationDecided { process, data, .. } => {
            // "would" rather than "did" while observing, so a log read later cannot be mistaken for
            // a record of actions taken.
            let outcome = match (&data.action, &data.withheld, data.acted) {
                (Some(action), _, true) => format!("took {action}"),
                (Some(action), _, false) => format!("would {action}"),
                (None, Some(reason), _) => reason.clone(),
                (None, None, _) => "no action".to_string(),
            };
            println!(
                "[{ts}] \x1b[36mremediation:decided\x1b[0m {}  rule={} {outcome}",
                process.name, data.rule
            );
        }
        DaemonShutdown { .. } => {
            println!("[{ts}] \x1b[33mdaemon:shutdown\x1b[0m");
        }
    }
}

#[cfg(unix)]
fn event_at(event: &BusEvent) -> u64 {
    match event {
        BusEvent::ProcessStarted { at, .. }
        | BusEvent::ProcessOnline { at, .. }
        | BusEvent::ProcessStopped { at, .. }
        | BusEvent::ProcessExited { at, .. }
        | BusEvent::ProcessCrashed { at, .. }
        | BusEvent::ProcessRestarting { at, .. }
        | BusEvent::ProcessErrored { at, .. }
        | BusEvent::LogOut { at, .. }
        | BusEvent::LogErr { at, .. }
        | BusEvent::HealthHealthy { at, .. }
        | BusEvent::HealthUnhealthy { at, .. }
        | BusEvent::AnomalyDetected { at, .. }
        | BusEvent::AnomalyCleared { at, .. }
        | BusEvent::RemediationDecided { at, .. }
        | BusEvent::DaemonShutdown { at } => *at,
    }
}

#[cfg(unix)]
fn format_timestamp(epoch_secs: u64) -> String {
    use std::time::{Duration, UNIX_EPOCH};

    let dt = UNIX_EPOCH + Duration::from_secs(epoch_secs);
    match dt.elapsed() {
        Ok(_) => {
            let secs = epoch_secs % 86400;
            let h = secs / 3600;
            let m = (secs % 3600) / 60;
            let s = secs % 60;
            format!("{h:02}:{m:02}:{s:02}")
        }
        Err(_) => epoch_secs.to_string(),
    }
}

#[cfg(unix)]
fn fmt_opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "?".into())
}
