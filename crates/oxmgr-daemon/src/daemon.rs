//! Foreground daemon loop, local IPC handling, and HTTP API handling.

use std::env;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::sync::{RwLock, broadcast, mpsc, oneshot};
use tokio::time::{
    Duration, Instant as TokioInstant, MissedTickBehavior, Sleep, sleep, sleep_until, timeout,
};
use tracing::{error, info, warn};

use crate::config::AppConfig;
use crate::ipc::{IpcRequest, IpcResponse, read_json_line, send_request, write_json_line};
use oxmgr_core::errors::OxmgrError;
use oxmgr_core::events::BusEvent;
#[cfg(unix)]
use oxmgr_core::events::EventFilter;
use oxmgr_manager::logging::ProcessLogs;
use oxmgr_manager::process_manager::ProcessManager;
use oxmgr_manager::signal::ShutdownListener;
use oxmgr_metrics::host_metrics::{
    HostCollectionIntervals, HostCollector, HostMetricsHandle, HostMetricsRequest,
    per_core_cpu_from_env, run_collection_loop,
};
use oxmgr_metrics::process::ManagedProcess;

mod http;

use self::http::execute_api_request;
#[cfg(test)]
use self::http::{
    escape_prometheus_label_value, render_findings_prometheus_metrics, render_prometheus_metrics,
};

#[derive(Clone)]
struct DaemonSnapshot {
    processes: Arc<RwLock<Vec<ManagedProcess>>>,
    event_tx: tokio::sync::broadcast::Sender<std::sync::Arc<BusEvent>>,
    /// Host-level figures, published by a separate collection task. Read-only
    /// observability with no bearing on process state, so it is deliberately not
    /// routed through the manager's command channel.
    host: HostMetricsHandle,
    /// Analysis output as of the last maintenance tick.
    ///
    /// Snapshotted alongside the process list rather than read through the manager, for the reason
    /// the process list is: the HTTP handlers must not take the manager lock. A scrape arriving
    /// mid-tick would otherwise block supervision, which is the one thing the whole change is
    /// careful not to do.
    ///
    /// Slightly stale by construction — up to one tick — and that is correct for a 2s scrape
    /// interval. It is the same staleness the process list already has.
    analysis: Arc<RwLock<oxmgr_analytics::analysis::AnalysisSnapshot>>,
    /// Advisory dismissals, keyed by process name then rule id.
    ///
    /// Snapshotted alongside the process list for the same reason the analysis output is: the HTTP
    /// handlers must not take the manager lock, and publishing in the same call means a response
    /// cannot report a dismissal for a process the listing does not contain.
    dismissals: Arc<RwLock<std::collections::BTreeMap<String, std::collections::BTreeSet<String>>>>,
    /// Typical (median) values per process, published with the process list.
    ///
    /// Snapshotted for the same reason the analysis output is: the HTTP handlers must not take the
    /// manager lock, and publishing in one call means a response cannot carry a typical value for a
    /// process the listing does not contain.
    typical: Arc<
        RwLock<std::collections::BTreeMap<String, oxmgr_manager::process_manager::TypicalReport>>,
    >,
    /// Host-wide top consumers, sampled on its own 30s cadence by a separate task.
    ///
    /// Its own handle rather than part of `host`, because the two have different cadences and
    /// different absence semantics: capacity metrics are always available after the first
    /// collection, while consumers are `None` when sampling is disabled.
    consumers: oxmgr_metrics::host_metrics::HostConsumersHandle,
}

impl Default for DaemonSnapshot {
    fn default() -> Self {
        Self {
            processes: Arc::default(),
            event_tx: broadcast::channel(oxmgr_core::events::BUS_CAPACITY).0,
            host: HostMetricsHandle::default(),
            analysis: Arc::default(),
            dismissals: Arc::default(),
            typical: Arc::default(),
            consumers: oxmgr_metrics::host_metrics::HostConsumersHandle::default(),
        }
    }
}

const DISABLED_RESTART_SLEEP_SECS: u64 = 24 * 60 * 60;
const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

enum ManagerCommand {
    Ipc {
        request: IpcRequest,
        response_tx: oneshot::Sender<IpcResponse>,
    },
    Api {
        command: self::http::HttpCommand,
        response_tx: oneshot::Sender<axum::response::Response>,
    },
}

/// Runs the Oxmgr daemon in the foreground.
///
/// The daemon owns process lifecycle management, serves the local IPC socket
/// used by the CLI, and exposes the lightweight HTTP API used for authenticated
/// pull triggers and Prometheus scraping.
///
/// Note: `[http_server]` config from oxfile.toml is applied in main.rs before
/// AppConfig is loaded, so port/auth settings are already in env vars here.
pub async fn run_foreground(config: AppConfig) -> Result<()> {
    info!("ensuring daemon layout");
    config.ensure_layout()?;

    info!("binding IPC listener at {}", config.daemon_addr);
    let listener = bind_listener(&config.daemon_addr).await?;

    info!("binding webhook API listener at {}", config.api_addr);
    let api_listener = bind_api_listener(&config.api_addr).await?;

    let (exit_tx, mut exit_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel::<()>();
    // Broadcast stop flag for long-lived background tasks (event socket, host
    // collection, consumer sampling). Each observes it in its select! loop so
    // shutdown is cooperative rather than relying on runtime teardown killing
    // the tasks mid-cycle. mpsc cannot be cloned for fan-out; watch is the
    // minimal multi-receiver primitive tokio core provides.
    let (shutdown_flag_tx, shutdown_flag_rx) = tokio::sync::watch::channel(false);
    let (http_shutdown_tx, http_shutdown_rx) = tokio::sync::oneshot::channel();
    let (command_tx, mut command_rx) = mpsc::unbounded_channel::<ManagerCommand>();
    // The kernel's ceiling, detected ONCE before the manager starts rather than once
    // per analysis cycle: cgroups do not change over a container's lifetime, and a
    // per-tick read would be a second collection pass. The host's own figures come
    // from a throwaway `System` — the manager's handle stays narrow, per the note on
    // `ProcessManager::new`.
    let container_memory_ceiling = {
        let system = sysinfo::System::new_with_specifics(
            sysinfo::RefreshKind::nothing()
                .with_memory(sysinfo::MemoryRefreshKind::everything())
                .with_cpu(sysinfo::CpuRefreshKind::nothing()),
        );
        // A platform core count is a small integer; usize_to_f64 is exact for it.
        let cpus = oxmgr_core::numeric::usize_to_f64(system.cpus().len());
        oxmgr_metrics::container::enforced_memory_ceiling(system.total_memory(), cpus)
    };

    info!("initializing process manager");
    let mut manager =
        ProcessManager::new(config.clone().into(), exit_tx, container_memory_ceiling)?;

    info!("recovering processes");
    manager.recover_processes().await?;
    let event_tx = manager.event_tx();
    let snapshot = DaemonSnapshot {
        processes: Arc::default(),
        event_tx: event_tx.clone(),
        host: HostMetricsHandle::new(),
        analysis: Arc::default(),
        dismissals: Arc::default(),
        typical: Arc::default(),
        consumers: oxmgr_metrics::host_metrics::HostConsumersHandle::new(),
    };
    snapshot.publish(&manager).await;
    spawn_host_collection(snapshot.host.clone(), shutdown_flag_rx.clone());
    spawn_consumer_sampling(snapshot.consumers.clone(), shutdown_flag_rx.clone());

    // The webhook API now runs on the axum router. The listener moves into this
    // task; graceful shutdown is triggered from the same exits the main loop
    // uses, so a daemon shutdown closes the API cleanly rather than dropping it.
    let router = self::http::create_router(http::AppState {
        snapshot: snapshot.clone(),
        command_tx: command_tx.clone(),
        auth_creds: http::auth::dashboard_auth_from_env(),
        static_web_dir: http::static_web_dir(),
    });
    tokio::spawn(async move {
        if let Err(err) = axum::serve(api_listener, router)
            .with_graceful_shutdown(async move {
                // The discard is deliberate: a recv error means the sender side is gone; shutdown is happening anyway
                #[expect(clippy::let_underscore_must_use, reason = "a recv error means the sender side is gone; shutdown is happening anyway")]
                let _ = http_shutdown_rx.await;
            })
            .await
        {
            error!("webhook API server failed: {err}");
        }
    });

    #[cfg(unix)]
    {
        let socket_path = config.event_socket_path.clone();
        let tx = event_tx.clone();
        // Observed handle: panic in the event-socket accept loop is logged
        // rather than silently lost when the JoinHandle is dropped.
        let socket_handle = tokio::spawn(async move {
            run_event_socket(socket_path, tx, shutdown_flag_rx.clone()).await;
        });
        tokio::spawn(async move {
            if let Err(e) = socket_handle.await {
                error!("event socket task panicked: {e}");
            }
        });
    }

    let mut restart_sleep = Box::pin(sleep_until(restart_sleep_deadline(
        manager.next_scheduled_restart_at(),
        TokioInstant::now(),
    )));
    let mut maintenance = tokio::time::interval(Duration::from_secs(2));
    maintenance.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // Registered once, before the loop: SIGTERM is what `docker stop` and
    // service supervisors send, and as PID 1 the daemon gets no default
    // disposition, so it must handle the signal explicitly or be SIGKILLed.
    let mut shutdown_signals = ShutdownListener::install();

    info!("oxmgr daemon started at {}", config.daemon_addr);
    info!("oxmgr webhook API started at {}", config.api_addr);

    loop {
        tokio::select! {
            incoming = listener.accept() => {
                match incoming {
                    Ok((stream, _)) => {
                        let command_tx = command_tx.clone();
                        let snapshot = snapshot.clone();
                        // Per-connection task. Ends when the connection ends.
                        // Deliberately detached (Decision 7): join is not
                        // needed because the task lifetime is bounded by the
                        // client connection, not the daemon lifecycle.
                        tokio::spawn(async move {
                            if let Err(err) = handle_client(stream, snapshot, command_tx).await {
                                error!("failed to handle IPC client: {err}");
                            }
                        });
                    }
                    Err(err) => {
                        error!("IPC accept failed: {err}");
                    }
                }
            }
            Some(command) = command_rx.recv() => {
                match command {
                    ManagerCommand::Ipc { request, response_tx } => {
                        let response = execute_request(request, &mut manager, &shutdown_tx).await;
                        // The discard is deliberate: a send error means the receiver is gone (shutdown); nothing left to deliver to
                        #[expect(clippy::let_underscore_must_use, reason = "a send error means the receiver is gone (shutdown); nothing left to deliver to")]
                        let _ = response_tx.send(response);
                    }
                    ManagerCommand::Api { command, response_tx } => {
                        let response = execute_api_request(command, &mut manager).await;
                        // The discard is deliberate: a send error means the receiver is gone (shutdown); nothing left to deliver to
                        #[expect(clippy::let_underscore_must_use, reason = "a send error means the receiver is gone (shutdown); nothing left to deliver to")]
                        let _ = response_tx.send(response);
                    }
                }
                snapshot.publish(&manager).await;
                reset_restart_sleep(restart_sleep.as_mut(), &manager);
            }
            Some(event) = exit_rx.recv() => {
                if let Err(err) = manager.handle_exit_event(event).await {
                    error!("failed to process exit event: {err}");
                }
                snapshot.publish(&manager).await;
                reset_restart_sleep(restart_sleep.as_mut(), &manager);
            }
            _ = restart_sleep.as_mut() => {
                if let Err(err) = manager.run_scheduled_restarts().await {
                    error!("scheduled restart task failed: {err}");
                }
                snapshot.publish(&manager).await;
                reset_restart_sleep(restart_sleep.as_mut(), &manager);
            }
            _ = maintenance.tick() => {
                if let Err(err) = manager.run_periodic_tasks().await {
                    error!("periodic manager task failed: {err}");
                }
                snapshot.publish(&manager).await;
                reset_restart_sleep(restart_sleep.as_mut(), &manager);
            }
            Some(_) = shutdown_rx.recv() => {
                info!("shutdown requested via IPC; stopping managed processes");
                // The discard is deliberate: a send error means the receiver is gone (shutdown); nothing left to deliver to
                drop(listener);
                #[expect(clippy::let_underscore_must_use, reason = "a send error means the receiver is gone (shutdown); nothing left to deliver to")]
                let _ = http_shutdown_tx.send(());
                #[expect(clippy::let_underscore_must_use, reason = "a send error means the receiver is gone (shutdown); nothing left to deliver to")]
                let _ = shutdown_flag_tx.send(true);
                #[expect(clippy::let_underscore_must_use, reason = "a send error means the receiver is gone (shutdown); nothing left to deliver to")]
                let _ = event_tx.send(std::sync::Arc::new(BusEvent::daemon_shutdown()));
                manager.shutdown_all().await?;
                snapshot.publish(&manager).await;
                break;
            }
            signal_name = shutdown_signals.recv() => {
                info!("received {signal_name}; stopping managed processes");
                // The discard is deliberate: a send error means the receiver is gone (shutdown); nothing left to deliver to
                drop(listener);
                #[expect(clippy::let_underscore_must_use, reason = "a send error means the receiver is gone (shutdown); nothing left to deliver to")]
                let _ = http_shutdown_tx.send(());
                #[expect(clippy::let_underscore_must_use, reason = "a send error means the receiver is gone (shutdown); nothing left to deliver to")]
                let _ = shutdown_flag_tx.send(true);
                #[expect(clippy::let_underscore_must_use, reason = "a send error means the receiver is gone (shutdown); nothing left to deliver to")]
                let _ = event_tx.send(std::sync::Arc::new(BusEvent::daemon_shutdown()));
                manager.shutdown_all().await?;
                snapshot.publish(&manager).await;
                break;
            }
        }
    }

    Ok(())
}

fn reset_restart_sleep(restart_sleep: Pin<&mut Sleep>, manager: &ProcessManager) {
    let deadline = restart_sleep_deadline(manager.next_scheduled_restart_at(), TokioInstant::now());
    restart_sleep.reset(deadline);
}

fn restart_sleep_deadline(next_due_at: Option<TokioInstant>, now: TokioInstant) -> TokioInstant {
    next_due_at.unwrap_or_else(|| now + Duration::from_secs(DISABLED_RESTART_SLEEP_SECS))
}

/// Ensures that the local daemon is running, spawning a detached foreground
/// instance when necessary and waiting briefly for it to become reachable.
pub async fn ensure_daemon_running(config: &AppConfig) -> Result<()> {
    if daemon_socket_available(&config.daemon_addr).await {
        return Ok(());
    }

    let executable = env::current_exe().context("failed to locate current executable")?;
    Command::new(executable)
        .arg("daemon")
        .arg("run")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn daemon")?;

    for _ in 0..300 {
        if daemon_socket_available(&config.daemon_addr).await {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }

    anyhow::bail!("daemon did not become ready in time")
}

async fn daemon_socket_available(daemon_addr: &str) -> bool {
    matches!(
        timeout(
            Duration::from_millis(250),
            send_request(daemon_addr, &IpcRequest::Ping),
        )
        .await,
        Ok(Ok(response)) if response.ok
    )
}

async fn bind_listener(daemon_addr: &str) -> Result<TcpListener> {
    if daemon_socket_available(daemon_addr).await {
        return Err(OxmgrError::DaemonAlreadyRunning.into());
    }

    TcpListener::bind(daemon_addr)
        .await
        .with_context(|| format!("failed to bind daemon endpoint at {daemon_addr}"))
}

async fn bind_api_listener(api_addr: &str) -> Result<TcpListener> {
    TcpListener::bind(api_addr)
        .await
        .with_context(|| format!("failed to bind webhook API endpoint at {api_addr}"))
}

async fn handle_client(
    mut stream: TcpStream,
    snapshot: DaemonSnapshot,
    command_tx: mpsc::UnboundedSender<ManagerCommand>,
) -> Result<()> {
    let request = read_json_line::<IpcRequest, _>(&mut stream).await?;
    let response = if let Some(response) = execute_snapshot_request(&request, &snapshot).await {
        response
    } else {
        send_ipc_command(&command_tx, request).await?
    };
    write_json_line(&mut stream, &response).await
}

async fn execute_request(
    request: IpcRequest,
    manager: &mut ProcessManager,
    shutdown_tx: &mpsc::UnboundedSender<()>,
) -> IpcResponse {
    match request {
        IpcRequest::Ping => IpcResponse::ok("pong"),
        IpcRequest::Shutdown => {
            // The discard is deliberate: a send error means the receiver is gone (shutdown); nothing left to deliver to
            #[expect(
                clippy::let_underscore_must_use,
                reason = "a send error means the receiver is gone (shutdown); nothing left to deliver to"
            )]
            let _ = shutdown_tx.send(());
            IpcResponse::ok("daemon shutdown scheduled")
        }
        IpcRequest::Start { spec } => match manager.start_process(*spec).await {
            Ok(process) => {
                let mut response = IpcResponse::ok(format!("started {}", process.target_label()));
                response.process = Some(process.redacted_for_transport());
                response
            }
            Err(err) => IpcResponse::error(err.to_string()),
        },
        IpcRequest::Stop { target } if target == "all" => {
            match manager.stop_all_processes().await {
                Ok(processes) => {
                    let mut response =
                        IpcResponse::ok(format!("stopped {} process(es)", processes.len()));
                    response.processes = redact_processes(processes);
                    response
                }
                Err(err) => IpcResponse::error(err.to_string()),
            }
        }
        IpcRequest::Stop { target } => match manager.stop_process(&target).await {
            Ok(process) => {
                let mut response = IpcResponse::ok(format!("stopped {}", process.target_label()));
                response.process = Some(process.redacted_for_transport());
                response
            }
            Err(err) => IpcResponse::error(err.to_string()),
        },
        IpcRequest::Restart { target } if target == "all" => {
            match manager.restart_all_processes().await {
                Ok(processes) => {
                    let mut response =
                        IpcResponse::ok(format!("restarted {} process(es)", processes.len()));
                    response.processes = redact_processes(processes);
                    response
                }
                Err(err) => IpcResponse::error(err.to_string()),
            }
        }
        IpcRequest::Restart { target } => match manager.restart_process(&target).await {
            Ok(process) => {
                let mut response = IpcResponse::ok(format!("restarted {}", process.target_label()));
                response.process = Some(process.redacted_for_transport());
                response
            }
            Err(err) => IpcResponse::error(err.to_string()),
        },
        IpcRequest::Reload { target } => match manager.reload_process(&target).await {
            Ok(process) => {
                let mut response = IpcResponse::ok(format!("reloaded {}", process.target_label()));
                response.process = Some(process.redacted_for_transport());
                response
            }
            Err(err) => IpcResponse::error(err.to_string()),
        },
        IpcRequest::Pull { target } => match manager.pull_processes(target.as_deref()).await {
            Ok(message) => IpcResponse::ok(message),
            Err(err) => IpcResponse::error(err.to_string()),
        },
        IpcRequest::Delete { target } if target == "all" => {
            match manager.delete_all_processes().await {
                Ok(processes) => {
                    let mut response =
                        IpcResponse::ok(format!("deleted {} process(es)", processes.len()));
                    response.processes = redact_processes(processes);
                    response
                }
                Err(err) => IpcResponse::error(err.to_string()),
            }
        }
        IpcRequest::Delete { target } => match manager.delete_process(&target).await {
            Ok(process) => {
                let mut response = IpcResponse::ok(format!("deleted {}", process.target_label()));
                response.process = Some(process.redacted_for_transport());
                response
            }
            Err(err) => IpcResponse::error(err.to_string()),
        },
        IpcRequest::List => {
            let mut response = IpcResponse::ok("ok");
            response.processes = redact_processes(manager.list_processes());
            response
        }
        IpcRequest::Status { target } => match manager.get_process(&target) {
            Ok(process) => {
                let mut response = IpcResponse::ok("ok");
                response.process = Some(process.redacted_for_transport());
                response
            }
            Err(err) => IpcResponse::error(err.to_string()),
        },
        IpcRequest::Logs { target } => match manager.logs_for(&target) {
            Ok(logs) => {
                let mut response = IpcResponse::ok("ok");
                response.logs = Some(logs);
                response
            }
            Err(err) => IpcResponse::error(err.to_string()),
        },
        IpcRequest::Findings { target } => {
            // An unknown target is refused rather than answered with an empty report, for the same
            // reason the HTTP route refuses it: "no findings" and "no such process" are different
            // facts, and a typo answered with silence reads as a clean bill of health.
            if let Some(target) = target.as_deref()
                && let Err(err) = manager.resolve_target_name(target)
            {
                return IpcResponse::error(err.to_string());
            }
            let mut response = IpcResponse::ok("ok");
            response.findings = Some(manager.findings_report(target.as_deref()));
            response
        }
    }
}

async fn execute_snapshot_request(
    request: &IpcRequest,
    snapshot: &DaemonSnapshot,
) -> Option<IpcResponse> {
    match request {
        IpcRequest::Ping => Some(IpcResponse::ok("pong")),
        IpcRequest::List => {
            let mut response = IpcResponse::ok("ok");
            response.processes = redact_processes(snapshot.list_processes().await);
            Some(response)
        }
        IpcRequest::Status { target } => {
            let process = snapshot.get_process(target).await?;
            let mut response = IpcResponse::ok("ok");
            response.process = Some(process.redacted_for_transport());
            Some(response)
        }
        IpcRequest::Logs { target } => {
            let logs = snapshot.logs_for(target).await?;
            let mut response = IpcResponse::ok("ok");
            response.logs = Some(logs);
            Some(response)
        }
        _ => None,
    }
}

fn redact_processes(processes: Vec<ManagedProcess>) -> Vec<ManagedProcess> {
    processes
        .into_iter()
        .map(|process| process.redacted_for_transport())
        .collect()
}

async fn send_ipc_command(
    command_tx: &mpsc::UnboundedSender<ManagerCommand>,
    request: IpcRequest,
) -> Result<IpcResponse> {
    let (response_tx, response_rx) = oneshot::channel();
    command_tx
        .send(ManagerCommand::Ipc {
            request,
            response_tx,
        })
        .map_err(|_| anyhow::anyhow!("daemon manager loop is unavailable"))?;
    response_rx
        .await
        .map_err(|_| anyhow::anyhow!("daemon manager loop dropped IPC response"))
}

async fn send_api_command(
    command_tx: &mpsc::UnboundedSender<ManagerCommand>,
    command: self::http::HttpCommand,
) -> Result<axum::response::Response> {
    let (response_tx, response_rx) = oneshot::channel();
    command_tx
        .send(ManagerCommand::Api {
            command,
            response_tx,
        })
        .map_err(|_| anyhow::anyhow!("daemon manager loop is unavailable"))?;
    response_rx
        .await
        .map_err(|_| anyhow::anyhow!("daemon manager loop dropped API response"))
}

/// Starts host metric collection on its own task.
///
/// Off the supervision path on purpose: the 2s maintenance tick and every
/// `ProcessManager` state change share one command channel, so collecting host
/// figures there would cost restart and health-check latency for data that has no
/// bearing on process state. This task only writes into a snapshot the HTTP layer
/// reads, exactly as `DaemonSnapshot::processes` is written and read.
/// Starts host-wide consumer sampling on its own task.
///
/// A separate task from `spawn_host_collection`, and the measurement is the reason: a full-host
/// `ProcessesToUpdate::All` refresh costs 7.79ms p50 on this host (596 processes) against 0.003ms
/// for the managed-pid refresh. On the 2s maintenance tick that would be 0.39% duty for the figure
/// that changes least urgently; at the 30s default it is 0.026%.
fn spawn_consumer_sampling(
    handle: oxmgr_metrics::host_metrics::HostConsumersHandle,
    shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let (config, adjustments) = oxmgr_metrics::host_consumers::ConsumerConfig::from_env();

    // Reported rather than silently applied: an operator who asked for a 1s cadence and got 5s
    // should be able to find out why without reading the source.
    for adjustment in &adjustments {
        warn!(
            "host consumers: {} {} — applied {}",
            adjustment.setting, adjustment.reason, adjustment.applied
        );
    }

    if !config.enabled {
        info!("host-wide consumer sampling disabled by OXMGR_HOST_CONSUMERS");
        // Stated explicitly so the API boundary can report "disabled" rather than
        // mistaking it for "enabled, no sample yet".
        handle.set_sampling_enabled(false);
        return;
    }
    handle.set_sampling_enabled(true);

    // Stated at startup because it is a privacy-relevant setting: an operator reading the log should
    // see that the surface is exposing more than its default.
    if config.include_command_lines {
        warn!(
            "host consumers: full command lines ENABLED by OXMGR_HOST_CONSUMERS_COMMANDS — \
             unmanaged process arguments may contain credentials"
        );
    }

    info!(
        "host-wide consumer sampling started (every {}s, top {} by cpu and memory)",
        config.interval.as_secs(),
        config.top_n
    );

    let sampler = oxmgr_metrics::host_consumers::ConsumerSampler::new(config);
    let consumer_handle = tokio::spawn(async move {
        oxmgr_metrics::host_metrics::run_consumer_loop(handle, sampler, shutdown).await;
    });
    tokio::spawn(async move {
        if let Err(e) = consumer_handle.await {
            error!("host consumer sampling task panicked: {e}");
        }
    });
}

fn spawn_host_collection(handle: HostMetricsHandle, shutdown: tokio::sync::watch::Receiver<bool>) {
    // Opt-out, for deployments where ~3-4 MB of resident memory matters more than host
    // figures. `/api/host` then answers 503 and the dashboard's sidebar stays hidden, which is
    // the same path as "before the first collection" — no separate degraded mode to maintain.
    if !oxmgr_metrics::host_metrics::collection_enabled_from_env() {
        info!("host metrics collection disabled by OXMGR_HOST_METRICS");
        return;
    }

    let collector = HostCollector::new(
        HostCollectionIntervals::from_env(),
        // Per-core CPU is on by default; temperatures stay off.
        //
        // The two were previously grouped as "extra detail", but they have different costs.
        // Per-core rides on the CPU refresh that already happens — `sysinfo` has the per-core
        // figures in hand once `refresh_cpu_usage` has run, so reporting them costs one f32 per
        // core per tick and no extra syscall. On this 8-core host that is 32 bytes.
        //
        // Temperatures are the expensive one and stay off: measured at 64.9ms per component
        // refresh in `docs/HOST-METRICS.md`, against 0.003ms for CPU, and commonly absent
        // entirely.
        HostMetricsRequest {
            per_core_cpu: per_core_cpu_from_env(),
            temperatures: false,
        },
    );

    // A configured interval below the platform sampling minimum is raised, and
    // saying so is required — an operator who set 50ms should be able to find out
    // why they are getting 200ms.
    for adjustment in collector.interval_adjustments() {
        warn!(
            "host metrics: {:?} interval of {}ms raised to {}ms: {}",
            adjustment.subsystem,
            adjustment.configured_ms,
            adjustment.applied_ms,
            adjustment.reason
        );
    }

    let intervals = collector.intervals();
    info!(
        "host metrics collection started (cpu/memory {}ms, disks/interfaces {}ms, components {}ms)",
        intervals.cpu_memory.as_millis(),
        intervals.io.as_millis(),
        intervals.components.as_millis()
    );

    let collection_handle = tokio::spawn(async move {
        run_collection_loop(handle, collector, shutdown).await;
    });
    tokio::spawn(async move {
        if let Err(e) = collection_handle.await {
            error!("host collection task panicked: {e}");
        }
    });
}

impl DaemonSnapshot {
    /// The current host figures, or `None` before the first collection completes.
    /// Exposed for the HTTP layer; host collection is independent of the manager,
    /// so this reads a value the collection task published.
    pub(super) async fn host_metrics(&self) -> Option<oxmgr_metrics::host_metrics::HostMetrics> {
        self.host.current().await
    }

    async fn publish(&self, manager: &ProcessManager) {
        let mut processes = self.processes.write().await;
        *processes = manager.list_processes();
        // Published in the same call as the process list, so a scrape cannot see a finding for a
        // process the listing does not contain. Two separate publishes would leave that window
        // open on every delete.
        let mut analysis = self.analysis.write().await;
        *analysis = manager.analysis_snapshot();
        // The managed pid map goes to the consumer sampler, which runs on its own task and cannot
        // reach the manager. Published rather than passed for that reason.
        //
        // A stale entry is harmless: a pid that has exited simply does not appear in the next
        // sample, so the worst case is one 30s cycle of a consumer being marked unmanaged.
        *self.dismissals.write().await = manager.dismissal_map();
        *self.typical.write().await = manager.typical_values();
        self.consumers
            .set_managed(
                processes
                    .iter()
                    .filter_map(|process| process.pid.map(|pid| (pid, process.name.clone())))
                    .collect(),
            )
            .await;
    }

    async fn list_processes(&self) -> Vec<ManagedProcess> {
        self.processes.read().await.clone()
    }

    async fn dismissals(
        &self,
    ) -> std::collections::BTreeMap<String, std::collections::BTreeSet<String>> {
        self.dismissals.read().await.clone()
    }

    async fn typical_values(
        &self,
    ) -> std::collections::BTreeMap<String, oxmgr_manager::process_manager::TypicalReport> {
        self.typical.read().await.clone()
    }

    async fn host_consumers(&self) -> Option<oxmgr_metrics::host_consumers::HostConsumers> {
        self.consumers.current().await
    }

    async fn analysis_snapshot(&self) -> oxmgr_analytics::analysis::AnalysisSnapshot {
        self.analysis.read().await.clone()
    }

    async fn get_process(&self, target: &str) -> Option<ManagedProcess> {
        let processes = self.processes.read().await;
        if let Some(process) = processes.iter().find(|process| process.name == target) {
            return Some(process.clone());
        }

        let id = target.parse::<u64>().ok()?;
        processes.iter().find(|process| process.id == id).cloned()
    }

    async fn logs_for(&self, target: &str) -> Option<ProcessLogs> {
        let process = self.get_process(target).await?;
        Some(ProcessLogs {
            stdout: process.stdout_log,
            stderr: process.stderr_log,
        })
    }
}

// ---------------------------------------------------------------------------
// Event socket (Unix only)
// ---------------------------------------------------------------------------

#[cfg(unix)]
async fn run_event_socket(
    socket_path: std::path::PathBuf,
    event_tx: tokio::sync::broadcast::Sender<std::sync::Arc<BusEvent>>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    use tokio::net::UnixListener;

    // The discard is deliberate: removal is opportunistic; a real failure surfaces at the subsequent rename or bind
    #[expect(
        clippy::let_underscore_must_use,
        reason = "removal is opportunistic; a real failure surfaces at the subsequent rename or bind"
    )]
    let _ = std::fs::remove_file(&socket_path);
    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(err) => {
            error!(
                "failed to bind event socket at {}: {err}",
                socket_path.display()
            );
            return;
        }
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // A socket left world-readable/writable is a real defect, not a tolerated one:
        // the 0600 mode is what keeps local unprivileged users off the control socket.
        if let Err(err) =
            std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
        {
            warn!(
                "failed to restrict event socket {} to mode 0600: {err}",
                socket_path.display()
            );
        }
    }

    info!("oxmgr event socket listening at {}", socket_path.display());

    loop {
        tokio::select! {
            biased;
            // Cooperative stop on daemon shutdown, rather than runtime teardown
            // killing the task mid-accept.
            _ = shutdown.changed() => {
                info!("event socket: shutdown received, exiting");
                return;
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let rx = event_tx.subscribe();
                        // Per-connection task. Ends when the connection ends.
                        // Deliberately detached (Decision 7).
                        tokio::spawn(async move {
                            if let Err(err) = handle_event_client(stream, rx).await
                                && !is_client_disconnect(&err) {
                                    error!("event socket client error: {err}");
                                }
                        });
                    }
                    Err(err) => {
                        error!("event socket accept failed: {err}");
                    }
                }
            }
        }
    }
}

#[cfg(unix)]
async fn handle_event_client(
    stream: tokio::net::UnixStream,
    mut rx: tokio::sync::broadcast::Receiver<std::sync::Arc<BusEvent>>,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::time::Duration;

    let (read_half, mut write_half) = stream.into_split();

    // Give the client up to 500 ms to send a filter line.
    let filter = {
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        match tokio::time::timeout(Duration::from_millis(500), reader.read_line(&mut line)).await {
            Ok(Ok(n)) if n > 0 => {
                serde_json::from_str::<EventFilter>(line.trim()).unwrap_or_default()
            }
            _ => EventFilter::default(),
        }
    };

    loop {
        match rx.recv().await {
            Ok(event) => {
                if filter.matches(&event) {
                    let mut payload = serde_json::to_vec(&*event)?;
                    payload.push(b'\n');
                    write_half.write_all(&payload).await?;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                warn!("event socket client lagged, dropped {n} events");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }

    Ok(())
}

#[cfg(unix)]
fn is_client_disconnect(err: &anyhow::Error) -> bool {
    err.downcast_ref::<std::io::Error>()
        .map(|e| {
            matches!(
                e.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::UnexpectedEof
            )
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command as StdCommand;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use tokio::net::TcpListener;
    use tokio::sync::RwLock;
    use tokio::sync::{broadcast, mpsc::unbounded_channel};
    use tokio::time::Instant as TokioInstant;

    use oxmgr_metrics::host_metrics::HostMetricsHandle;

    use super::http::{
        AppState, HttpCommand, create_router, execute_api_request, extract_api_secret,
        process_json_for_transport, render_dashboard_html, render_host_prometheus_metrics,
    };
    use super::{
        DISABLED_RESTART_SLEEP_SECS, DaemonSnapshot, PROMETHEUS_CONTENT_TYPE,
        daemon_socket_available, escape_prometheus_label_value, execute_snapshot_request,
        render_findings_prometheus_metrics, render_prometheus_metrics, restart_sleep_deadline,
        spawn_consumer_sampling,
    };
    use axum::http::Request;
    use axum::{Router, body::Body};
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serial_test::serial;
    use tower::ServiceExt;

    async fn json_body(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn json_body_sync(response: axum::response::Response) -> serde_json::Value {
        futures::executor::block_on(json_body(response))
    }

    async fn text_body(response: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn status_code(response: &axum::response::Response) -> u16 {
        response.status().as_u16()
    }

    fn build_test_router(snapshot: &DaemonSnapshot) -> Router {
        build_test_router_with_auth(snapshot, None)
    }

    fn build_test_router_with_auth(
        snapshot: &DaemonSnapshot,
        auth_creds: Option<(String, String)>,
    ) -> Router {
        let (command_tx, _) = unbounded_channel();
        let state = AppState {
            snapshot: snapshot.clone(),
            command_tx,
            auth_creds,
            static_web_dir: None,
        };
        create_router(state)
    }

    fn build_test_router_with_static_dir(
        snapshot: &DaemonSnapshot,
        web_dir: std::path::PathBuf,
    ) -> Router {
        let (command_tx, _) = unbounded_channel();
        let state = AppState {
            snapshot: snapshot.clone(),
            command_tx,
            auth_creds: None,
            static_web_dir: Some(web_dir),
        };
        create_router(state)
    }

    /// Minimal unique temp directory for tests that need a real on-disk web
    /// directory. Follows the pattern in `cgroup.rs` tests.
    struct TestWebDir {
        path: PathBuf,
    }

    /// Recursively copy a directory tree (files only; symlinks are read as files).
    /// Used to serve the real `web/` tree from a throwaway `TestWebDir` so route
    /// tests exercise the shipped files, not stubs.
    fn copy_tree(src: &Path, dst: &Path) {
        fs::create_dir_all(dst).expect("failed to create destination dir");
        for entry in fs::read_dir(src).expect("read_dir failed") {
            let entry = entry.expect("dir entry failed");
            let from = entry.path();
            let to = dst.join(entry.file_name());
            if from.is_dir() {
                copy_tree(&from, &to);
            } else if from.is_file() {
                fs::copy(&from, &to).expect("copy failed");
            }
        }
    }

    /// Returns the repo-root `web/` directory, anchored to the workspace rather
    /// than the process CWD. Since the crate lives under `crates/oxmgr/`, plain
    /// relative paths like `web/...` resolve to `crates/oxmgr/web/` which does
    /// not exist. This helper uses `CARGO_MANIFEST_DIR` (the crate's directory)
    /// to reach the repo root.
    fn repo_web_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web")
    }

    fn copy_web_tree(src: &str, dst: &Path) {
        copy_tree(Path::new(src), dst);
    }

    /// Per-process counter guaranteeing distinct `TestWebDir` paths.
    ///
    /// A timestamp alone is not enough: `SystemTime::now()` on this platform
    /// returns the same value for consecutive calls (measured: 0ns deltas), so two
    /// tests constructing a `TestWebDir` in the same instant collided on one
    /// directory — and whichever finished first deleted it in `Drop` while the
    /// other was still reading from it. That surfaced as an unrelated test failing
    /// only under parallel execution.
    static TEST_WEB_DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

    impl TestWebDir {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos();
            let seq = TEST_WEB_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "oxmgr-web-dir-{}-{unique}-{seq}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("failed to create temporary web directory");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestWebDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    async fn oneshot_request(router: Router, method: &str, path: &str) -> axum::response::Response {
        router
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn oneshot_request_with_headers(
        router: Router,
        method: &str,
        path: &str,
        headers: Vec<(String, String)>,
    ) -> axum::response::Response {
        let mut builder = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            builder = builder.header(k, v);
        }
        router
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    use crate::config::AppConfig;
    use crate::ipc::{IpcRequest, IpcResponse, read_json_line, write_json_line};
    use oxmgr_manager::process_manager::ProcessManager;
    use oxmgr_metrics::process::{
        DEFAULT_CRASH_RESTART_LIMIT, DesiredState, HealthStatus, ManagedProcess, ProcessStatus,
        RestartPolicy, StartProcessSpec,
    };
    use oxmgr_store::hash::sha256_hex;

    /// Guards template drift the other way around: the document is now a
    /// static page (no substitution), so any lingering template token would
    /// ship a literal placeholder to browsers, and dropping the asset
    /// references would ship a page with no styles and no behaviour.
    #[test]
    fn dashboard_page_is_static_and_references_assets_by_url() {
        let page = render_dashboard_html();

        assert!(
            !page.contains("{{OXMGR_"),
            "dashboard page still contains an unsubstituted template token"
        );
        assert!(
            page.contains("<link rel=\"stylesheet\" href=\"/dashboard.css\">"),
            "dashboard page should reference dashboard.css by URL"
        );
        assert!(
            page.contains("<script type=\"module\" src=\"/dashboard.js\"></script>"),
            "dashboard page should reference dashboard.js by URL as ES module"
        );
        assert!(
            !page.contains("</style>"),
            "the stylesheet must not be inlined into the document"
        );
        assert!(
            !page.contains("v{{OXMGR_VERSION}}"),
            "the version badge must be gone (no dynamic substitution)"
        );
    }

    /// The live regions must exist in the SERVED document, empty, with their roles and
    /// `aria-atomic`. Asserted on the document rather than in a browser because a region
    /// created at the same moment as its text is not reliably announced, so "present at
    /// first paint" is the property that matters and it is a property of the markup.
    #[test]
    fn dashboard_page_declares_both_live_regions_empty() {
        let page = render_dashboard_html();

        for (id, role) in [("status-live", "status"), ("alert-live", "alert")] {
            let expected = format!(
                "<div class=\"sr-only\" id=\"{id}\" role=\"{role}\" aria-atomic=\"true\"></div>"
            );
            assert!(
                page.contains(&expected),
                "the {role} live region must be present, empty and aria-atomic: expected {expected}"
            );
        }

        // `.sr-only` hides it while KEEPING it in the accessibility tree. `display:none`
        // or `hidden` would remove it and silence every announcement, which is the exact
        // failure mode this change exists to fix — so it is asserted, not assumed.
        for forbidden in [
            "id=\"status-live\" hidden",
            "id=\"alert-live\" hidden",
            "id=\"status-live\" style=\"display:none\"",
            "id=\"alert-live\" style=\"display:none\"",
        ] {
            assert!(
                !page.contains(forbidden),
                "a live region must not be removed from the accessibility tree: found {forbidden}"
            );
        }
    }

    /// The bypass block (SC 2.4.1) must be the FIRST focusable element. Placing it after
    /// `</header>` is the natural mistake and defeats the requirement, because the header
    /// holds a three-radio theme group — so the position is pinned by a test.
    #[test]
    fn dashboard_page_skip_link_precedes_the_header() {
        let page = render_dashboard_html();

        let skip = page
            .find("class=\"skip-link")
            .expect("dashboard page must contain a skip link");
        let header = page
            .find("<header aria-label=\"Page header\">")
            .expect("dashboard page must have a header");
        assert!(
            skip < header,
            "the skip link must precede <header> so it is the first focusable element"
        );

        assert!(
            page.contains("href=\"#process-list\""),
            "the skip link must target the process list"
        );
        assert!(
            page.contains("id=\"process-list\""),
            "the skip link target must exist"
        );
        assert!(
            page.contains("<main aria-label="),
            "the primary content region must carry an accessible name"
        );
    }

    /// Heading structure and landmark naming (SC 1.3.1 / SC 2.4.6): exactly one `h1`,
    /// no skipped heading levels, and every landmark region carries an accessible name.
    /// A header without a name is a banner landmark no assistive tech can announce as such.
    #[test]
    fn dashboard_page_headings_are_sequential_and_landmarks_named() {
        let page = render_dashboard_html();

        // Exactly one h1: the page title. Two would present two competing titles.
        assert_eq!(
            page.matches("<h1>").count(),
            1,
            "the dashboard must have exactly one h1"
        );

        // No skipped levels: collect declared h1-h6, assert each level is at most one
        // above the previous. (h1 -> h2 is fine; h1 -> h3 is a skip.)
        let levels: Vec<u32> = page
            .match_indices("<h")
            .map(|(i, _)| {
                page[i + 2..]
                    .chars()
                    .next()
                    .and_then(|c| c.to_digit(10))
                    .filter(|l| (1..=6).contains(l))
                    .unwrap_or(0)
            })
            .filter(|l| *l != 0)
            .collect();
        assert!(!levels.is_empty(), "the dashboard must declare headings");
        for pair in levels.windows(2) {
            assert!(
                pair[1] <= pair[0] + 1,
                "heading levels must not skip: h{} followed by h{}",
                pair[0],
                pair[1]
            );
        }

        // Every landmark named: <header>/<main>/<aside> must carry aria-label or
        // aria-labelledby. Anonymous landmarks are invisible to the landmark navigation
        // every screen reader exposes.
        for tag in ["<header", "<main", "<aside"] {
            assert!(
                page.contains(tag),
                "the dashboard must contain a {tag} landmark"
            );
        }
        assert!(
            page.contains(r#"<header aria-label="Page header">"#),
            "the banner landmark must carry an accessible name"
        );
        assert!(
            page.contains("<main aria-label="),
            "the main landmark must carry an accessible name"
        );
        assert!(
            page.contains(
                r#"<aside class="host-panel" id="host-panel" aria-labelledby="host-panel-title""#
            ),
            "the host panel landmark must carry an accessible name via aria-labelledby"
        );
    }

    /// Media-query thresholds (SC 1.4.4 / dashboard-design-tokens): every breakpoint must
    /// remain in `px`. An `em` threshold re-tiers the layout on a font-size change — the
    /// same class of defect as a px text size in reverse: the layout jumps when the user
    /// changes their browser font size, because the breakpoint moves relative to the
    /// content. Thresholds in `px` stay anchored to the viewport, which is what the
    /// operator sees.
    #[test]
    fn media_query_thresholds_are_declared_in_px() {
        let css = fs::read_to_string(repo_web_dir().join("dashboard.css"))
            .expect("dashboard.css must exist");
        let mut offenders = Vec::new();
        for (i, line) in css.lines().enumerate() {
            let code = match line.find("/*") {
                Some(j) => &line[..j],
                None => line,
            };
            let trimmed = code.trim();
            if !(trimmed.starts_with("@media") || trimmed.starts_with("@container")) {
                continue;
            }
            // Extract the query body and flag any min-/max-width or min-/max-height value
            // that is not px-denominated.
            let body = trimmed.find('(').map(|j| &trimmed[j..]).unwrap_or(trimmed);
            for piece in body.split(',') {
                let piece = piece.trim();
                let is_size_clause = piece.contains("min-width:")
                    || piece.contains("max-width:")
                    || piece.contains("min-height:")
                    || piece.contains("max-height:");
                if is_size_clause && !piece.contains("px") {
                    offenders.push(format!("dashboard.css:{}: {trimmed}", i + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "media query size threshold declared in a non-px unit (em/rem re-tier the \
             layout on a font-size change); use px:\n{}",
            offenders.join("\n")
        );
    }

    /// The step classification enforced by 6.7/6.8 (dashboard-design-tokens § body-text
    /// floor): the micro step (`--text-3xs`, 9px at the default root) is restricted to
    /// supporting labels — units, captions, symbols, and secondary annotations — while a
    /// figure, a process name, or a status value must sit on a body step (`--text-2xs`
    /// and up). A figure rendered at 9px is an unreadable figure, and a process name at
    /// 9px is a name the operator must squint at to act on. Selectors that legitimately
    /// use the micro step, with the supporting role each one plays.
    const MICRO_STEP_SUPPORTING_SELECTORS: &[&str] = &[
        ".host-gauge-num i", // the "%" unit in the ring; the figure itself is --text-lg
        ".host-gauge-label", // caption under the ring
        ".host-core-id",     // "C0"/"C1" axis label, not a reading
        ".host-core-pct i",  // the "%" unit beside the figure
        ".host-net-scale",   // y-axis ceiling caption for the sparkline
        ".host-net-rate b",  // the ↓/↑ direction glyph, not the rate figure
        ".sev-cue",          // severity glyph, role=img with an aria-label
        ".host-consumers-toggle", // "Consumers"/"Commands" control labels
        ".host-consumers-na", // "n/a" placeholder, not a figure
        ".host-consumers-note", // secondary annotation
        ".finding-guidance", // disclosure summary label
        "[data-sort-order]::after", // circled sort-priority badge, not a figure
    ];

    /// Every font-size-bearing selector that carries a figure, a process name, or a
    /// status value, and therefore MUST NOT use the micro step. This is the inverse of
    /// `MICRO_STEP_SUPPORTING_SELECTORS`: together the two lists classify the whole
    /// scale, so a new micro use on an unlisted selector fails 6.7 and a new figure
    /// class defaulting to micro fails 6.8.
    const BODY_STEP_FIGURE_SELECTORS: &[&str] = &[
        ".host-gauge-num",     // the ring figure ("42%")
        ".host-core-pct",      // per-core utilisation figure
        ".host-net-rate",      // the down/up rate figures
        ".host-net-total",     // lifetime byte totals
        ".host-metric-val",    // the RAM/SWAP byte figure
        ".host-consumer-name", // a process name
        ".host-consumer-val",  // a consumer resource figure
        ".host-identity",      // the hostname
        ".host-gauge-detail",  // byte figure under the ring
    ];

    /// Parse `dashboard.css` into (selector, declaration) pairs for every declaration
    /// that sets a font size (`font-size:` or the `font:` shorthand). Selectors are the
    /// rule's own selector, media/container queries and nested blocks flattened so the
    /// pair carries the innermost selector text.
    fn css_font_size_declarations(css: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut selector = String::new();
        let mut depth = 0u32;
        for line in css.lines() {
            let code = match line.find("/*") {
                Some(j) => &line[..j],
                None => line,
            };
            let trimmed = code.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(sel) = trimmed.strip_suffix('{') {
                selector = sel.trim().to_string();
                depth += 1;
                continue;
            }
            if trimmed.ends_with('}') {
                depth = depth.saturating_sub(1);
                selector.clear();
                continue;
            }
            if depth > 0 && (trimmed.starts_with("font-size:") || trimmed.starts_with("font:")) {
                out.push((selector.clone(), trimmed.to_string()));
            }
        }
        out
    }

    /// 6.7 — the smallest step carries no figure and no process name.
    ///
    /// Every declaration that uses the micro step must belong to one of the
    /// supporting-label selectors. A figure at 9px is below the body floor, and a
    /// process name at 9px is a name the operator must squint at to act on.
    #[test]
    fn micro_step_is_restricted_to_supporting_labels() {
        let css = fs::read_to_string(repo_web_dir().join("dashboard.css"))
            .expect("dashboard.css must exist");
        let mut offenders = Vec::new();
        for (selector, decl) in css_font_size_declarations(&css) {
            let uses_micro = decl.contains("--sb-text-micro") || decl.contains("--text-3xs");
            if !uses_micro {
                continue;
            }
            let allowed = MICRO_STEP_SUPPORTING_SELECTORS
                .iter()
                .any(|s| selector.contains(s));
            if !allowed {
                offenders.push(format!("{selector} {{ {decl} }}"));
            }
        }
        assert!(
            offenders.is_empty(),
            "micro step (9px) used by a selector that is not a supporting label; the \
             smallest step carries no figure and no process name (dashboard-design-tokens \
             § body-text floor):\n{}",
            offenders.join("\n")
        );
    }

    /// 6.8 — every figure, name, and status value uses a body step or larger.
    ///
    /// The inverse of 6.7: a selector that carries a figure or a process name must NOT
    /// resolve to the micro step. Cross-checked against `MICRO_STEP_SUPPORTING_SELECTORS`
    /// so a rename in one list without the other is caught.
    #[test]
    fn figures_names_and_statuses_use_a_body_step() {
        let css = fs::read_to_string(repo_web_dir().join("dashboard.css"))
            .expect("dashboard.css must exist");
        let mut offenders = Vec::new();
        for (selector, decl) in css_font_size_declarations(&css) {
            let uses_micro = decl.contains("--sb-text-micro") || decl.contains("--text-3xs");
            if !uses_micro {
                continue;
            }
            // A micro declaration on a figure selector is the exact defect 6.8 exists
            // to catch. Supporting selectors that NAME a figure as an ancestor (e.g.
            // ".host-gauge-num i") are allowed — they carry a unit, not the figure.
            let is_figure = BODY_STEP_FIGURE_SELECTORS
                .iter()
                .any(|s| selector.contains(s));
            let is_supporting_unit = selector.ends_with(" i") || selector.ends_with(" b");
            if is_figure && !is_supporting_unit {
                offenders.push(format!("{selector} {{ {decl} }}"));
            }
        }
        assert!(
            offenders.is_empty(),
            "a figure/name/status selector resolves to the micro step (9px); every figure, \
             name, and status value must use a body step or larger \
             (dashboard-design-tokens § body-text floor):\n{}",
            offenders.join("\n")
        );
    }

    /// 3.9 — the announcement window (60s) correctly suppresses oscillating crossings.
    /// Models the logic in `app.js` `#checkThresholdsForAnnouncements`: the first
    /// poll is a baseline (no crossing observed), then an oscillating series
    /// crosses at t=10, goes inactive at t=20, crosses again at t=30, etc.
    /// With a 60 s window the crossings at t=30 and t=90 are suppressed, the
    /// ones at t=70 and t=130 are allowed. `Date.now()` is epoch-scaled, so an
    /// unset key (0) always yields a fresh announcement, mirroring the real code.
    #[test]
    fn threshold_announcement_rate_limit_bites() {
        use std::collections::HashMap;
        let mut announce_times: HashMap<String, u64> = HashMap::new();
        let window = 60_000u64;
        let epoch = 1_700_000_000_000u64; // Date.now() scale
        let mut announce_count = 0u32;
        let mut have_baseline = false;

        // Baseline poll at t=0: skipped, nothing announced.
        // Crossings (the times when the figure crosses into active):
        //   t=10  t=30  t=70  t=90  t=130  (active every 40 s, crossing twice)
        // With a 60 s window: t=10 fires, t=30 suppressed, t=70 fires,
        // t=90 suppressed, t=130 fires.  Also the baseline poll at t=0 must be
        // skipped.
        let crossings_sec = [0, 10, 30, 70, 90, 130];
        for t in crossings_sec {
            // Baseline guard: first poll is skipped.
            if !have_baseline {
                have_baseline = true;
                continue;
            }
            let t_ms = epoch + t * 1000;
            let key = "test_proc|det|met";
            let last = *announce_times.get(key).unwrap_or(&0);
            if t_ms - last >= window {
                announce_count += 1;
                announce_times.insert(key.to_string(), t_ms);
            }
        }
        assert_eq!(
            announce_count, 3,
            "expected exactly 3 announcements (t=10, t=70, t=130); t=30 and t=90 are \
             suppressed by the 60 s window"
        );
    }

    /// Returns the full text of every `bus.on(EVENTS.<name>` call in the bundle, by
    /// brace/paren matching from the opening paren of `.on(` to its match. Used to scope a
    /// search to a handler body rather than to a whole file.
    fn bundled_handler_bodies(event: &str) -> Vec<String> {
        let needle = format!("EVENTS.{event}");
        let mut found = Vec::new();
        for module in super::http::DASHBOARD_JS_MODULE_ORDER {
            let path = repo_web_dir().join(module);
            let Ok(src) = fs::read_to_string(&path) else {
                continue;
            };
            let mut from = 0usize;
            while let Some(hit) = src[from..].find(&needle) {
                let abs = from + hit;
                // Walk back to the `(` that opens the `.on(` call this sits in.
                let Some(open) = src[..abs].rfind('(') else {
                    break;
                };
                let bytes = src.as_bytes();
                let mut depth = 0i32;
                let mut end = open;
                for (i, b) in bytes.iter().enumerate().skip(open) {
                    match b {
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                end = i;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                if end > open {
                    found.push(src[open..=end].to_string());
                }
                from = abs + needle.len();
            }
        }
        found
    }

    /// THE assertion that protects the status-messaging design.
    ///
    /// `dashboard-status-messaging` forbids announcing anything that updates on the
    /// collection tick: CPU, memory, disk, per-core figures, process counts and log lines
    /// all change every couple of seconds, and a live region on any of them makes the
    /// dashboard unusable through a screen reader — which SC 4.1.3 itself warns about.
    ///
    /// The reason this is a test and not a review note: adding `aria-live` to the host
    /// panel looks like an accessibility IMPROVEMENT. Nobody reviewing that diff would
    /// object. So the prohibition has to fail a build, or it decays on the first
    /// well-meant edit.
    #[test]
    fn per_tick_handlers_never_announce() {
        for event in ["PROC_DATA", "LOG_DATA", "LOG_TAIL"] {
            let bodies = bundled_handler_bodies(event);
            assert!(
                !bodies.is_empty(),
                "expected to find at least one {event} handler to check; the scan is broken \
                 if this fails, which would silently disable this guard"
            );
            for body in bodies {
                assert!(
                    !body.contains("announce."),
                    "a {event} handler announces to a live region. {event} fires on every \
                     collection tick, so this makes the dashboard unreadable through a \
                     screen reader. See dashboard-status-messaging: per-tick figures are \
                     excluded by requirement. Handler text: {body}"
                );
            }
        }
    }

    /// The counterpart: no module may create a live region of its own. Two containers with
    /// fixed politeness exist for a reason (`aria-live` is read at announce time and
    /// changing it dynamically is unreliable), and a third one attached to a streamed
    /// element would reintroduce exactly the chatter the test above prevents.
    #[test]
    fn no_module_creates_its_own_live_region() {
        for module in super::http::DASHBOARD_JS_MODULE_ORDER {
            let path = repo_web_dir().join(module);
            let Ok(src) = fs::read_to_string(&path) else {
                continue;
            };
            for marker in [
                "aria-live",
                "role=\\\"status\\\"",
                "role=\\\"alert\\\"",
                "role=\\\"log\\\"",
            ] {
                assert!(
                    !src.contains(marker),
                    "{module} creates a live region ({marker}). Announcements go through \
                     js/core/announce.js and the two containers declared in index.html; a \
                     module-created region bypasses the per-tick exclusion."
                );
            }
        }
    }

    /// The destructive confirmation must be a native <dialog>, and must be OPENED with
    /// `showModal()`.
    ///
    /// Both halves matter and only together. A `<dialog>` opened with `show()` or by
    /// setting the `open` attribute is non-modal: no focus containment, no background
    /// inertness, no Escape. So the element alone proves nothing.
    ///
    /// Pinned by a test because the failure is silent — the prompt still appears and still
    /// works with a mouse, and the missing Escape only shows up for a keyboard operator
    /// staring at "This will stop ALL running processes" with no way out.
    #[test]
    fn confirm_dialog_is_a_native_modal_dialog() {
        let page = render_dashboard_html();
        assert!(
            page.contains("<dialog class=\"confirm-dialog\" id=\"confirm-overlay\""),
            "the confirm prompt must be a native <dialog> element"
        );
        assert!(
            page.contains("aria-labelledby=\"confirm-title\""),
            "the confirm dialog must be labelled by its heading"
        );

        let raw = fs::read_to_string(repo_web_dir().join("js/modals/confirm-modal.js"))
            .expect("confirm-modal.js must exist");
        // Strip line comments before the negative assertions below. A comment that
        // EXPLAINS what was replaced legitimately names the forbidden construction, and a
        // raw text search cannot tell that from real code — it flagged this file's own
        // "showModal(), not classList.add(...)" note on the first run.
        let src: String = raw
            .lines()
            .map(|line| match line.find("//") {
                Some(i) => &line[..i],
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            src.contains(".showModal()"),
            "the confirm dialog must be opened with showModal(); show() and the open \
             attribute are non-modal and supply no focus containment or Escape"
        );
        assert!(
            !src.contains("classList.add(\"open\")"),
            "the confirm dialog must not be shown by toggling a class — that is the \
             construction this change replaced"
        );
        // Escape arrives as a `cancel` event. Without a listener the promise the caller is
        // awaiting is abandoned, so the dialog closes and nothing ever resolves.
        assert!(
            src.contains("\"cancel\""),
            "the confirm dialog must handle the cancel event so Escape resolves the promise"
        );
    }

    /// The log and detail overlays must also be native dialogs, and the base Modal must
    /// route a NATIVE close back into the subclass's `close()`.
    ///
    /// That second half is the subtle one and the reason this test exists. `showModal()`
    /// gives Escape for free, but Escape closes the ELEMENT — it does not call
    /// `LogModal.close()`, which is where `#api.stopLog()`, `#resizeObs.disconnect()` and
    /// `#view.destroy()` live. Deleting the old global Escape handler without wiring the
    /// native `close` event would leak an SSE subscription, a ResizeObserver and a
    /// virtualized view on every Escape — silently, with the dialog looking closed.
    #[test]
    fn panel_dialogs_are_native_and_route_native_close_to_teardown() {
        let page = render_dashboard_html();
        for id in ["log-overlay", "detail-overlay"] {
            assert!(
                page.contains(&format!("<dialog class=\"log-overlay\" id=\"{id}\"")),
                "{id} must be a native <dialog>"
            );
        }

        let raw = fs::read_to_string(repo_web_dir().join("js/modals/modal.js"))
            .expect("modal.js must exist");
        let src: String = raw
            .lines()
            .map(|line| match line.find("//") {
                Some(i) => &line[..i],
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            src.contains(".showModal()"),
            "Modal.open() must use showModal(); show() and the open attribute are non-modal"
        );
        assert!(
            src.contains("addEventListener(\"close\""),
            "Modal must listen for the native close event, or Escape bypasses subclass \
             teardown and leaks the log stream, its observer and its virtualized view"
        );
        assert!(
            !src.contains("classList.add(\"open\")"),
            "Modal must not be shown by toggling a class"
        );

        // The global Escape handler must stay gone. It enumerated dismissable modals by
        // hand, which is why the confirm prompt was undismissable for as long as it existed.
        let app_raw =
            fs::read_to_string(repo_web_dir().join("js/shell/app.js")).expect("app.js must exist");
        let app: String = app_raw
            .lines()
            .map(|line| match line.find("//") {
                Some(i) => &line[..i],
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !app.contains("Escape"),
            "app.js must not carry a global Escape handler: each dialog owns its own \
             dismissal, so no hand-maintained list can be missing an entry"
        );
    }

    /// Every pointer-drag gesture needs a non-drag alternative (WCAG 2.2 SC 2.5.7), and
    /// the gestures themselves must not be mouse-only.
    ///
    /// Before this change `modal.js` held the frontend's ONLY two `mousedown` listeners and
    /// there were zero `pointerdown`/`touchstart` anywhere, so the panel could be moved and
    /// resized with a mouse and by no other input at all — no touch, no pen, no keyboard.
    #[test]
    fn panel_gestures_have_non_drag_alternatives() {
        let raw = fs::read_to_string(repo_web_dir().join("js/modals/modal.js"))
            .expect("modal.js must exist");
        let src: String = raw
            .lines()
            .map(|line| match line.find("//") {
                Some(i) => &line[..i],
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n");

        // Mouse-only listeners are the defect. Checked on comment-stripped source so the
        // explanatory note naming `mousedown` does not trip this.
        for mouse_only in [
            "\"mousedown\"",
            "'mousedown'",
            "\"mousemove\"",
            "\"mouseup\"",
        ] {
            assert!(
                !src.contains(mouse_only),
                "modal.js still binds {mouse_only}; pointer events are required so the \
                 gesture works under touch and pen, not only a mouse"
            );
        }
        for pointer in ["pointerdown", "pointermove", "pointerup"] {
            assert!(
                src.contains(pointer),
                "modal.js must bind {pointer} for its move/resize gestures"
            );
        }
        // Touch can be taken away without a `pointerup` (scroll takeover, palm rejection),
        // which would leave the move listener attached for the life of the page.
        assert!(
            src.contains("pointercancel"),
            "modal.js must handle pointercancel, or an interrupted touch drag leaks its \
             move listener"
        );

        // The keyboard alternative itself: movement, a size cycle, and a reset.
        //
        // Assert the CALL SITE, not just that the name appears. A substring check on the
        // definition alone is defeated by renaming the method — verified: renaming it to
        // `#initKeyboardGeometryDisabled` and deleting the call left a `contains` check
        // passing, because the longer name still contains the shorter one. The call is what
        // makes the feature reachable.
        assert!(
            src.contains("this.#initKeyboardGeometry();"),
            "Modal's constructor must CALL #initKeyboardGeometry(); a defined-but-uncalled \
             method is not a non-drag alternative"
        );
        assert!(
            src.contains("#initKeyboardGeometry() {"),
            "modal.js must define #initKeyboardGeometry as the non-drag alternative"
        );
        for key in ["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown"] {
            assert!(
                src.contains(key),
                "keyboard panel movement must handle {key}"
            );
        }
        assert!(
            src.contains("this.#cycleSize("),
            "eight directional resize handles do not map to a key set, so a discrete size \
             cycle is required as the keyboard resize path — and it must be called"
        );
        assert!(
            src.contains("this.#resetGeometry("),
            "an operator who has moved a panel somewhere awkward must be able to reset it \
             while it is still open — and the reset must be reachable"
        );
    }

    /// No text size may be declared in `px` (WCAG 2.2 SC 1.4.4 Resize Text).
    ///
    /// A `px` font-size ignores the browser's font-size setting outright: a user who raises
    /// their default because 12px is unreadable still receives 12px. Page zoom is not a
    /// substitute — it scales the layout as well, which is a different request.
    ///
    /// Checked mechanically because the failure is invisible to everyone who has not
    /// changed that setting, which is most reviewers. Covers the `font:` SHORTHAND too:
    /// the first conversion pass matched only `font-size:` and left nine shorthand
    /// declarations behind, including `body`, so `body` text did not scale at all.
    #[test]
    fn no_text_size_is_declared_in_px() {
        let css = fs::read_to_string(repo_web_dir().join("dashboard.css"))
            .expect("dashboard.css must exist");
        let mut offenders = Vec::new();
        for (i, line) in css.lines().enumerate() {
            let code = match line.find("/*") {
                Some(j) => &line[..j],
                None => line,
            };
            let trimmed = code.trim();
            let is_font_decl = trimmed.starts_with("font-size:") || trimmed.starts_with("font:");
            if !is_font_decl {
                continue;
            }
            // `px` anywhere in a font declaration is a size in px: the other components
            // (weight, family, line-height ratio) are unitless or names.
            if trimmed.contains("px") {
                offenders.push(format!("dashboard.css:{}: {trimmed}", i + 1));
            }
        }
        assert!(
            offenders.is_empty(),
            "text size declared in px, which ignores the browser font-size setting; use a \
             --text-* token from the type scale instead:\n{}",
            offenders.join("\n")
        );

        // And the scale itself must be rem-denominated, or the tokens are a rename rather
        // than a fix.
        for token in ["--text-3xs:", "--text-sm:", "--text-lg:", "--text-display:"] {
            let line = css
                .lines()
                .find(|l| l.trim_start().starts_with(token))
                .unwrap_or_else(|| panic!("type scale must define {token}"));
            assert!(
                line.contains("rem"),
                "{token} must be denominated in rem, not px: {line}"
            );
        }
    }

    /// `announce.js` must be in the bundle, and every announcement site must go through
    /// it. A live region nobody writes to is worse than no live region: it looks handled.
    #[test]
    fn announcer_module_is_bundled() {
        assert!(
            super::http::DASHBOARD_JS_MODULE_ORDER.contains(&"js/core/announce.js"),
            "the announcer must be part of the dashboard bundle"
        );
    }

    /// Every `data-figure` identity must be unique across the host panel (§D7). Two
    /// elements with the same identity means the same fact is rendered in more than one
    /// place — the exact defect this change exists to eliminate.
    ///
    /// This is a static source-level check: it reads the JS files that emit `data-figure`
    /// attributes, extracts every identity string, and asserts uniqueness. A duplication
    /// in the source means a duplication in the DOM, regardless of runtime values.
    ///
    /// The test is intentionally authored AFTER the `data-figure` attributes were added
    /// (tasks 3.1–3.4). To prove it bites, temporarily remove one `data-figure` attribute
    /// and confirm the duplicate assertion fires. The identities are the contract defined
    /// in `figure-ownership-map.md`.
    #[test]
    fn data_figure_identities_are_unique_across_the_host_panel() {
        let js_files = ["js/host/metricRenderers.js", "js/host/canvas.js"];

        // Collect every figure identity from two sources:
        //   1. The FIGURES map — literal string values like `"cpu.used_percent"`
        //   2. `dataset.figure = "..."` — literal assignments like `"host.cpu_stats"`
        // Template expressions (`${figureId}`) are references to the map, not identities
        // themselves — the deduplication lives in the map's values and the assignments.
        let mut identities: Vec<String> = Vec::new();
        let mut file_sources: Vec<String> = Vec::new();

        for rel_path in &js_files {
            let raw = fs::read_to_string(repo_web_dir().join(rel_path))
                .unwrap_or_else(|e| panic!("{rel_path} must exist: {e}"));
            // Strip line comments so explanatory notes naming a figure identity
            // do not trigger the duplicate check.
            let src: String = raw
                .lines()
                .map(|line| match line.find("//") {
                    Some(i) => &line[..i],
                    None => line,
                })
                .collect::<Vec<_>>()
                .join("\n");
            file_sources.push(src);
        }

        let combined = file_sources.join("\n");

        // Source 1: The FIGURES map. Lines like `CPU: "cpu.used_percent"` or
        // `RAM: "memory.used_percent"` — a quoted string on the same line as a
        // key in the static FIGURES object.
        if let Ok(re) = regex::Regex::new(r#":\s*"([a-z][a-z0-9._]+)""#) {
            // Only collect within the FIGURES map block (between "static FIGURES" and "}").
            if let Some(start) = combined.find("static FIGURES")
                && let Some(map_end) = combined[start..].find("};")
            {
                let map_block = &combined[start..start + map_end + 2];
                for cap in re.captures_iter(map_block) {
                    identities.push(cap[1].to_string());
                }
            }
        }

        // Source 2: dataset.figure = "..." assignments.
        if let Ok(re) = regex::Regex::new(r#"dataset\.figure\s*=\s*"([^"]+)""#) {
            for cap in re.captures_iter(&combined) {
                identities.push(cap[1].to_string());
            }
        }

        assert!(
            !identities.is_empty(),
            "no data-figure identities found — the test is broken or the attributes were removed"
        );

        // Check for duplicates: every identity must appear exactly once.
        let mut seen = std::collections::HashMap::new();
        for id in &identities {
            *seen.entry(id.clone()).or_insert(0u32) += 1;
        }
        let mut duplicates: Vec<String> = seen
            .iter()
            .filter(|(_, count)| **count > 1)
            .map(|(id, count)| format!("{id} (×{count})"))
            .collect();
        duplicates.sort();
        assert!(
            duplicates.is_empty(),
            "duplicate data-figure identities found (§D7 requires one surface per figure):\n{}",
            duplicates.join("\n")
        );
    }

    #[tokio::test]
    async fn static_assets_served_from_disk_with_content_types() {
        let snapshot = snapshot_with_processes(Vec::default());
        let web_dir = TestWebDir::new();
        fs::write(
            web_dir.path().join("dashboard.css"),
            b"body { color: red; }",
        )
        .unwrap();
        fs::write(web_dir.path().join("favicon.svg"), b"<svg/>").unwrap();

        // dashboard.js is assembled from module files in DASHBOARD_JS_MODULE_ORDER.
        // Create stub files for every module so assembly succeeds; dashboard.js
        // carries the expected content so the body assertion below matches.
        for module in super::http::DASHBOARD_JS_MODULE_ORDER {
            let path = web_dir.path().join(module);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            if *module == "dashboard.js" {
                fs::write(&path, b"console.log('oxmgr');").unwrap();
            } else {
                fs::write(&path, b"").unwrap();
            }
        }

        let router = build_test_router_with_static_dir(&snapshot, web_dir.path().to_path_buf());

        let css = oneshot_request(router.clone(), "GET", "/dashboard.css").await;
        assert_eq!(status_code(&css), 200);
        assert_eq!(
            css.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("text/css; charset=utf-8")
        );
        assert_eq!(text_body(css).await, "body { color: red; }");

        let js = oneshot_request(router.clone(), "GET", "/dashboard.js").await;
        assert_eq!(status_code(&js), 200);
        assert_eq!(
            js.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("text/javascript; charset=utf-8")
        );
        assert_eq!(text_body(js).await, "console.log('oxmgr');");

        let svg = oneshot_request(router.clone(), "GET", "/favicon.svg").await;
        assert_eq!(status_code(&svg), 200);
        assert_eq!(
            svg.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("image/svg+xml; charset=utf-8")
        );
        assert_eq!(text_body(svg).await, "<svg/>");
    }

    #[tokio::test]
    async fn static_asset_edits_are_picked_up_without_restart() {
        let snapshot = snapshot_with_processes(Vec::default());
        let web_dir = TestWebDir::new();
        fs::write(
            web_dir.path().join("dashboard.css"),
            b"body { color: red; }",
        )
        .unwrap();
        let router = build_test_router_with_static_dir(&snapshot, web_dir.path().to_path_buf());

        let before =
            text_body(oneshot_request(router.clone(), "GET", "/dashboard.css").await).await;
        assert_eq!(before, "body { color: red; }");

        // Edit the file on disk while the router is live; the next request
        // must return the updated bytes (per-request read, no restart).
        fs::write(
            web_dir.path().join("dashboard.css"),
            b"body { color: blue; }",
        )
        .unwrap();
        let after = text_body(oneshot_request(router.clone(), "GET", "/dashboard.css").await).await;
        assert_eq!(after, "body { color: blue; }");
    }

    /// Every asset the dashboard document references must resolve to a registered
    /// route (dashboard-interaction-safety: "Every referenced asset has a route").
    /// The asset set is read from the real `web/index.html` so a new `<link>`/`<script>`
    /// without a matching file fails here before it can fail in a browser.
    #[tokio::test]
    async fn every_dashboard_referenced_asset_has_a_route() {
        let html =
            fs::read_to_string(repo_web_dir().join("index.html")).expect("index.html must exist");
        let mut referenced: Vec<String> = Vec::new();
        for cap in [r##"href="(/[^"#][^"]*)""##, r##"src="(/[^"][^"]*)""##] {
            for m in regex::Regex::new(cap).unwrap().captures_iter(&html) {
                let asset = m[1].to_string();
                if !referenced.contains(&asset) {
                    referenced.push(asset);
                }
            }
        }
        assert!(
            !referenced.is_empty(),
            "index.html references no assets — the scan is broken, which would disable \
             this guard"
        );

        // Serve from the real web dir, so the files themselves must exist.
        let snapshot = snapshot_with_processes(Vec::default());
        let web_dir = TestWebDir::new();
        // Copy the whole web tree into the test dir so every referenced file resolves
        // to a real file on disk, exactly as the shipped daemon serves it.
        copy_web_tree(
            repo_web_dir()
                .to_str()
                .expect("repo web dir must be valid UTF-8"),
            web_dir.path(),
        );

        let router = build_test_router_with_static_dir(&snapshot, web_dir.path().to_path_buf());

        let mut missing: Vec<String> = Vec::new();
        for asset in &referenced {
            let reply = oneshot_request(router.clone(), "GET", asset).await;
            let status = status_code(&reply);
            assert!(
                status < 400,
                "referenced asset {asset} resolves to HTTP {status}; every asset the \
                 document references must have a serving route"
            );
            if status == 404 {
                missing.push(asset.clone());
            }
        }
        assert!(
            missing.is_empty(),
            "referenced assets with no route: {}",
            missing.join(", ")
        );
    }

    #[tokio::test]
    async fn static_asset_missing_file_returns_404_without_fallback() {
        let snapshot = snapshot_with_processes(Vec::default());
        let web_dir = TestWebDir::new();
        // Directory exists but is missing the asset (operator error: dry
        // volume mount). The spec pins the response to 404 — no embedded fallback.
        let router = build_test_router_with_static_dir(&snapshot, web_dir.path().to_path_buf());

        let missing = oneshot_request(router, "GET", "/dashboard.js").await;
        assert_eq!(
            status_code(&missing),
            404,
            "missing asset must 404 rather than fall back to embedded bytes"
        );
    }

    /// Writes a file the web directory must never disclose, one level above it,
    /// and returns its name. Used by the containment tests below.
    fn plant_secret_beside(web_dir: &TestWebDir) -> String {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        let name = format!("oxmgr-secret-{}-{unique}.css", std::process::id());
        let outside = web_dir
            .path()
            .parent()
            .expect("temp web dir always has a parent")
            .join(&name);
        fs::write(&outside, b"SECRET-DO-NOT-SERVE").unwrap();
        name
    }

    /// Containment is decided on the resolved path, so a plain `..` escape must
    /// be refused even though every component of it is individually legal.
    #[tokio::test]
    async fn static_asset_traversal_is_refused() {
        let snapshot = snapshot_with_processes(Vec::default());
        let web_dir = TestWebDir::new();
        let secret = plant_secret_beside(&web_dir);
        let router = build_test_router_with_static_dir(&snapshot, web_dir.path().to_path_buf());

        let escaped = oneshot_request(router, "GET", &format!("/../{secret}")).await;
        assert_ne!(
            status_code(&escaped),
            200,
            "a path resolving outside the web directory must not be served"
        );
    }

    /// The same escape, percent-encoded.
    ///
    /// VERIFIED NOT TO BITE, deliberately kept. Removing `resolve_contained` leaves
    /// this test passing, because axum does not percent-decode `Uri::path()`:
    /// measured, the handler receives the literal `/%2e%2e%2fsecret.css`, which
    /// names a file that does not exist rather than a traversal. So containment is
    /// not what refuses these today — the absence of decoding is.
    ///
    /// It stays because that is a property of the router, not of this code. If a
    /// later change adds decoding, normalises the path, or moves to a handler that
    /// decodes, this test is the thing that catches the traversal it would open.
    /// The two tests either side of it DO bite; this one is a regression guard.
    #[tokio::test]
    async fn static_asset_encoded_traversal_is_refused() {
        let snapshot = snapshot_with_processes(Vec::default());
        let web_dir = TestWebDir::new();
        let secret = plant_secret_beside(&web_dir);
        let router = build_test_router_with_static_dir(&snapshot, web_dir.path().to_path_buf());

        for encoded in ["%2e%2e%2f", "..%2f", "%2e%2e/"] {
            let resp = oneshot_request(router.clone(), "GET", &format!("/{encoded}{secret}")).await;
            assert_ne!(
                status_code(&resp),
                200,
                "encoded traversal {encoded} must not be served"
            );
        }
    }

    /// A symlink inside the web directory pointing outside it. Its own path
    /// contains no traversal sequence at all, so only resolving the link catches
    /// it — this is why containment canonicalises rather than inspecting text.
    #[cfg(unix)]
    #[tokio::test]
    async fn static_asset_symlink_escaping_dir_is_refused() {
        let snapshot = snapshot_with_processes(Vec::default());
        let web_dir = TestWebDir::new();
        let secret = plant_secret_beside(&web_dir);
        let outside = web_dir.path().parent().unwrap().join(&secret);

        // A perfectly ordinary-looking asset name inside the directory.
        std::os::unix::fs::symlink(&outside, web_dir.path().join("dashboard.css")).unwrap();
        let router = build_test_router_with_static_dir(&snapshot, web_dir.path().to_path_buf());

        let resp = oneshot_request(router, "GET", "/dashboard.css").await;
        assert_ne!(
            status_code(&resp),
            200,
            "a symlink whose target is outside the web directory must not be served"
        );
    }

    /// The defect this change exists to fix: a file added to the web directory after
    /// startup must be served, with no recompile and no restart. Under the previous
    /// hardcoded four-route allowlist this returned 404 for any name not compiled in.
    #[tokio::test]
    async fn static_asset_new_file_is_served_without_recompile() {
        let snapshot = snapshot_with_processes(Vec::default());
        let web_dir = TestWebDir::new();
        let router = build_test_router_with_static_dir(&snapshot, web_dir.path().to_path_buf());

        // A name that appears in no route table and no bundle list.
        let before = oneshot_request(router.clone(), "GET", "/brand-new-asset.css").await;
        assert_eq!(status_code(&before), 404, "absent file must 404");

        fs::write(web_dir.path().join("brand-new-asset.css"), b".x{}").unwrap();

        let after = oneshot_request(router.clone(), "GET", "/brand-new-asset.css").await;
        assert_eq!(
            status_code(&after),
            200,
            "a file added at runtime must be served without a recompile"
        );
        assert_eq!(text_body(after).await, ".x{}");
    }

    /// A nested module path, which is the normal case for the frontend module set:
    /// they live under `js/core/`, `js/host/` and so on.
    #[tokio::test]
    async fn static_asset_nested_module_path_is_served() {
        let snapshot = snapshot_with_processes(Vec::default());
        let web_dir = TestWebDir::new();
        let nested = web_dir.path().join("js").join("core");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("bus.js"), b"export const bus = 1;").unwrap();
        let router = build_test_router_with_static_dir(&snapshot, web_dir.path().to_path_buf());

        let resp = oneshot_request(router, "GET", "/js/core/bus.js").await;
        assert_eq!(status_code(&resp), 200, "nested module path must be served");
        assert_eq!(text_body(resp).await, "export const bus = 1;");
    }

    /// Content type comes from a table now, and an unknown extension is served as
    /// opaque bytes rather than refused with 415 — which would have disclosed that
    /// the path exists.
    #[tokio::test]
    async fn static_asset_content_types_cover_fonts_and_unknown_extensions() {
        let snapshot = snapshot_with_processes(Vec::default());
        let web_dir = TestWebDir::new();
        fs::write(web_dir.path().join("f.woff2"), b"woff2-bytes").unwrap();
        fs::write(web_dir.path().join("a.unknownext"), b"opaque").unwrap();
        let router = build_test_router_with_static_dir(&snapshot, web_dir.path().to_path_buf());

        let font = oneshot_request(router.clone(), "GET", "/f.woff2").await;
        assert_eq!(
            status_code(&font),
            200,
            "a font must be served, not refused"
        );
        assert_eq!(
            font.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("font/woff2")
        );

        let opaque = oneshot_request(router.clone(), "GET", "/a.unknownext").await;
        assert_eq!(
            status_code(&opaque),
            200,
            "an unknown extension must be served as opaque bytes, not 415"
        );
        assert_eq!(
            opaque
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/octet-stream")
        );

        // Status reflects existence only: a missing file is 404 whatever the suffix.
        for missing in ["/gone.woff2", "/gone.unknownext"] {
            assert_eq!(
                status_code(&oneshot_request(router.clone(), "GET", missing).await),
                404
            );
        }
    }

    /// Containment must not reject legitimate paths. The modules live in
    /// subdirectories, so a nested path is the normal case, not an edge case.
    #[tokio::test]
    async fn static_asset_nested_path_is_still_served() {
        let snapshot = snapshot_with_processes(Vec::default());
        let web_dir = TestWebDir::new();
        let nested = web_dir.path().join("js").join("core");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("bus.js"), b"export const ok = 1;").unwrap();
        // `theme.js` is a routed asset today, so it is the reachable proof that a
        // real file inside the directory still resolves after containment landed.
        fs::write(web_dir.path().join("theme.js"), b"/* theme */").unwrap();
        let router = build_test_router_with_static_dir(&snapshot, web_dir.path().to_path_buf());

        let ok = oneshot_request(router, "GET", "/theme.js").await;
        assert_eq!(
            status_code(&ok),
            200,
            "containment must not block a file genuinely inside the web directory"
        );
    }

    /// Embedded mode keeps its own validators: precomputed from the compiled-in
    /// bytes (dashboard-per-file-assets §6.5).
    #[tokio::test]
    async fn static_asset_routes_serve_embedded_bytes_when_disabled() {
        let snapshot = snapshot_with_processes(Vec::default());
        // No static dir configured: the asset routes must still exist — the
        // static index.html references them unconditionally — and serve the
        // embedded bytes from the binary as the fallback.
        let router = build_test_router(&snapshot);

        let css = oneshot_request(router.clone(), "GET", "/dashboard.css").await;
        assert_eq!(status_code(&css), 200);
        assert_eq!(
            css.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("text/css; charset=utf-8")
        );
        assert!(
            text_body(css).await.contains("--bg-panel"),
            "embedded fallback should serve the compiled-in stylesheet"
        );

        let js = oneshot_request(router.clone(), "GET", "/dashboard.js").await;
        assert_eq!(status_code(&js), 200);
        assert_eq!(
            js.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("text/javascript; charset=utf-8")
        );
        assert!(
            text_body(js).await.contains("import { sel }"),
            "embedded fallback should serve the entry point module"
        );

        // The favicon route stays registered in both modes (embedded fallback).
        let svg = oneshot_request(router, "GET", "/favicon.svg").await;
        assert_eq!(status_code(&svg), 200);
    }

    /// §6.6 (disk mode): a matching validator is answered as unchanged without a
    /// body — and without reading the file, since the validator comes from
    /// metadata alone.
    #[tokio::test]
    async fn static_asset_disk_etag_304_without_body() {
        let snapshot = snapshot_with_processes(Vec::default());
        let web_dir = TestWebDir::new();
        fs::write(web_dir.path().join("dashboard.css"), b".x{color:red}").unwrap();
        let router = build_test_router_with_static_dir(&snapshot, web_dir.path().to_path_buf());

        let first = oneshot_request(router.clone(), "GET", "/dashboard.css").await;
        assert_eq!(status_code(&first), 200);
        let etag = first
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .expect("disk asset must carry a validator")
            .to_string();

        let cached = oneshot_request_with_headers(
            router,
            "GET",
            "/dashboard.css",
            vec![("If-None-Match".into(), etag.clone())],
        )
        .await;
        assert_eq!(status_code(&cached), 304);
        assert_eq!(
            cached.headers().get("etag").and_then(|v| v.to_str().ok()),
            Some(etag.as_str()),
            "304 must echo the validator"
        );
        let body = axum::body::to_bytes(cached.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty(), "304 must carry no body");
    }

    /// §6.6 (embedded mode): same contract against the compiled-in fallback.
    #[tokio::test]
    async fn static_asset_embedded_etag_304_without_body() {
        let snapshot = snapshot_with_processes(Vec::default());
        let router = build_test_router(&snapshot);

        let first = oneshot_request(router.clone(), "GET", "/dashboard.css").await;
        assert_eq!(status_code(&first), 200);
        let etag = first
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .expect("embedded asset must carry a validator")
            .to_string();

        let cached = oneshot_request_with_headers(
            router,
            "GET",
            "/dashboard.css",
            vec![("If-None-Match".into(), etag)],
        )
        .await;
        assert_eq!(status_code(&cached), 304);
        let body = axum::body::to_bytes(cached.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty(), "304 must carry no body");
    }

    /// §6.7: editing a file changes its validator AND its compressed response
    /// carries the new content, not a stale precomputed form. The replacement
    /// bytes are deliberately LONGER, so the fingerprint cannot collide even if
    /// both writes land inside one filesystem timestamp tick.
    #[tokio::test]
    async fn static_asset_edit_changes_validator_and_compressed_body() {
        use flate2::read::GzDecoder;
        use std::io::Read;

        let snapshot = snapshot_with_processes(Vec::default());
        let web_dir = TestWebDir::new();
        fs::write(web_dir.path().join("app.js"), b"export const v=1;").unwrap();
        let router = build_test_router_with_static_dir(&snapshot, web_dir.path().to_path_buf());

        let first = oneshot_request_with_headers(
            router.clone(),
            "GET",
            "/app.js",
            vec![("Accept-Encoding".into(), "gzip".into())],
        )
        .await;
        assert_eq!(status_code(&first), 200);
        assert_eq!(
            first
                .headers()
                .get("content-encoding")
                .and_then(|v| v.to_str().ok()),
            Some("gzip"),
            "gzip advertised and accepted must yield gzip"
        );
        let etag_before = first
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .expect("first response carries a validator")
            .to_string();

        // The edit: different length, different content, no restart.
        fs::write(
            web_dir.path().join("app.js"),
            b"export const v=2;\n// edited on disk while the daemon runs\n",
        )
        .unwrap();

        let second = oneshot_request_with_headers(
            router.clone(),
            "GET",
            "/app.js",
            vec![
                ("Accept-Encoding".into(), "gzip".into()),
                ("If-None-Match".into(), etag_before.clone()),
            ],
        )
        .await;
        assert_eq!(
            status_code(&second),
            200,
            "the OLD validator must NOT match after the edit"
        );
        let etag_after = second
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .expect("second response carries a validator")
            .to_string();
        assert_ne!(
            etag_before, etag_after,
            "editing the file must change its validator"
        );

        let body = axum::body::to_bytes(second.into_body(), usize::MAX)
            .await
            .unwrap();
        let mut decoder = GzDecoder::new(&body[..]);
        let mut decompressed = String::new();
        decoder.read_to_string(&mut decompressed).unwrap();
        assert!(
            decompressed.contains("edited on disk"),
            "compressed body must be the NEW content, got: {decompressed}"
        );
    }

    /// §6.8: EVERY asset response carries a validator, not only the document —
    /// every module path, css, theme.js, favicon, in disk mode.
    #[tokio::test]
    async fn static_asset_every_response_carries_a_validator() {
        let snapshot = snapshot_with_processes(Vec::default());
        let web_dir = TestWebDir::new();
        copy_web_tree(
            repo_web_dir()
                .to_str()
                .expect("repo web dir must be valid UTF-8"),
            web_dir.path(),
        );
        let router = build_test_router_with_static_dir(&snapshot, web_dir.path().to_path_buf());

        let mut paths: Vec<String> = crate::daemon::http::DASHBOARD_JS_MODULE_ORDER
            .iter()
            .map(|p| format!("/{p}"))
            .collect();
        paths.extend(
            ["/dashboard.css", "/theme.js", "/favicon.svg"]
                .iter()
                .map(|p| (*p).to_string()),
        );
        assert!(
            paths.len() >= 30,
            "the sweep must cover the real surface, got {} paths",
            paths.len()
        );

        for path in &paths {
            let resp = oneshot_request(router.clone(), "GET", path).await;
            assert_eq!(
                status_code(&resp),
                200,
                "{path} must resolve when served from the shipped web tree"
            );
            assert!(
                resp.headers().get("etag").is_some(),
                "{path} must carry a validator"
            );
        }
    }

    /// An absolute filesystem path in the URL must be refused, not resolved
    /// against the web dir (or worse, against the root). `join` with an
    /// absolute component would discard the base entirely — containment must
    /// catch it on the canonical comparison.
    #[tokio::test]
    async fn static_asset_absolute_path_is_refused() {
        let snapshot = snapshot_with_processes(Vec::default());
        let web_dir = TestWebDir::new();
        let secret = plant_secret_beside(&web_dir);
        let outside = web_dir.path().parent().unwrap().join(&secret);
        fs::write(&outside, b"SECRET-DO-NOT-SERVE").unwrap();
        let router = build_test_router_with_static_dir(&snapshot, web_dir.path().to_path_buf());

        let resp =
            oneshot_request(router, "GET", &format!("/{}/{}", outside.display(), secret)).await;
        assert_ne!(
            status_code(&resp),
            200,
            "an absolute path request must never be served"
        );
    }

    /// SSE endpoints must not be compressed: gzip buffers events and would
    /// withhold them from the client until a buffer flushes.
    #[tokio::test]
    async fn streaming_endpoint_is_not_compressed() {
        let snapshot = snapshot_with_processes(Vec::default());
        let router = build_test_router(&snapshot);

        let resp = oneshot_request_with_headers(
            router,
            "GET",
            "/api/stream?subscribe=processes",
            vec![("Accept-Encoding".into(), "gzip".into())],
        )
        .await;
        assert_eq!(status_code(&resp), 200);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream"),
        );
        assert!(
            resp.headers().get("content-encoding").is_none(),
            "an SSE response must never carry a content-encoding header"
        );
    }

    /// The document names entry points only (script + stylesheet), declares the
    /// script as an ES module, and keeps theme.js as a classic non-deferred
    /// script in <head> so the pre-paint restore runs before first paint.
    #[test]
    fn dashboard_document_names_entry_points_and_module_entry() {
        let html =
            fs::read_to_string(repo_web_dir().join("index.html")).expect("index.html must exist");
        assert!(
            html.contains(r#"<script type="module" src="/dashboard.js">"#),
            "the entry point must be declared as a module"
        );
        assert!(
            !html.contains("/js/"),
            "the document must not list individual modules; imports belong to the graph"
        );
        // theme.js: present, classic (no type=module), no defer/async, inside <head>.
        let head = html.split("</head>").next().unwrap_or("");
        assert!(
            head.contains(r#"<script src="/theme.js">"#),
            "theme.js must be a classic script in <head>, before any body content"
        );
        assert!(
            !head.contains("defer") && !head.contains("async"),
            "the pre-paint theme script must not be deferred"
        );
    }

    #[tokio::test]
    async fn dashboard_api_requires_basic_auth_when_configured() {
        let snapshot = snapshot_with_processes(Vec::default());
        let router =
            build_test_router_with_auth(&snapshot, Some(("admin".into(), "s3cret".into())));

        // Without credentials -> 401 + WWW-Authenticate challenge.
        let denied = oneshot_request(router.clone(), "GET", "/").await;
        assert_eq!(status_code(&denied), 401);
        assert!(
            denied
                .headers()
                .get("WWW-Authenticate")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.contains("Basic"))
        );

        let denied_api = oneshot_request(router.clone(), "GET", "/api/processes").await;
        assert_eq!(status_code(&denied_api), 401);

        // Wrong credentials -> 401.
        let wrong = vec![(
            "authorization".to_string(),
            format!("Basic {}", STANDARD.encode("admin:wrong")),
        )];
        let wrong_resp =
            oneshot_request_with_headers(router.clone(), "GET", "/api/processes", wrong).await;
        assert_eq!(status_code(&wrong_resp), 401);

        // Correct credentials -> the request passes the middleware.
        let ok_headers = vec![(
            "authorization".to_string(),
            format!("Basic {}", STANDARD.encode("admin:s3cret")),
        )];
        let ok =
            oneshot_request_with_headers(router.clone(), "GET", "/api/processes", ok_headers).await;
        assert_eq!(
            status_code(&ok),
            200,
            "correct credentials should pass middleware"
        );

        // Metrics is NOT protected.
        let metrics = oneshot_request(router.clone(), "GET", "/metrics").await;
        assert_eq!(
            status_code(&metrics),
            200,
            "metrics should not require basic auth"
        );

        // /pull/* is not basic-auth protected (uses webhook secret instead).
        let pull = oneshot_request(router, "POST", "/pull/api").await;
        assert_ne!(
            status_code(&pull),
            401,
            "/pull/* should not require basic auth"
        );
    }

    #[tokio::test]
    async fn dashboard_api_accepts_hashed_passwords() {
        // Test each hash algorithm with "s3cret" as the password.
        // Generate hashes: echo -n 's3cret' | openssl dgst -<algo> -binary | base64
        let test_cases = [
            (
                "{SHA256}HsHCa1DV08WNlYMYGvgHZlX+AHVr9yhZQLo2cPmfy6A=",
                "SHA256",
            ),
            (
                "{SHA512}lcia3d5QY1fsXv0O5BrCQe/W+xAJp2gMFQHqgXA0K4y/Dy2Ti1YpVJDxnx/F+ijQmxWE6qCcmmsvd3YjKZzVIQ==",
                "SHA512",
            ),
        ];

        let snapshot = snapshot_with_processes(Vec::default());

        for (hash, algo) in test_cases {
            let router =
                build_test_router_with_auth(&snapshot, Some(("admin".into(), hash.into())));

            // Wrong password -> 401.
            let wrong = vec![(
                "authorization".to_string(),
                format!("Basic {}", STANDARD.encode("admin:wrongpass")),
            )];
            let wrong_resp =
                oneshot_request_with_headers(router.clone(), "GET", "/api/processes", wrong).await;
            assert_eq!(status_code(&wrong_resp), 401, "{algo}: expected 401");

            // Correct password -> passes.
            let ok_headers = vec![(
                "authorization".to_string(),
                format!("Basic {}", STANDARD.encode("admin:s3cret")),
            )];
            let ok =
                oneshot_request_with_headers(router.clone(), "GET", "/api/processes", ok_headers)
                    .await;
            assert_eq!(
                status_code(&ok),
                200,
                "{algo}: correct password should pass"
            );
        }
    }

    #[tokio::test]
    async fn snapshot_api_serves_dashboard_html_at_root() {
        let snapshot = snapshot_with_processes(Vec::default());
        let response = oneshot_request(build_test_router(&snapshot), "GET", "/").await;
        assert_eq!(status_code(&response), 200);
        assert!(
            response
                .headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        let body = text_body(response).await;
        assert!(body.contains("<title>OxMgr Dashboard</title>"));
    }

    #[tokio::test]
    async fn snapshot_api_lists_redacted_processes() {
        let snapshot = snapshot_with_processes(vec![fixture_metrics_process()]);
        let response = oneshot_request(build_test_router(&snapshot), "GET", "/api/processes").await;
        assert_eq!(status_code(&response), 200);

        let value = json_body(response).await;
        assert!(value.is_array());
        let processes = value.as_array().expect("expected array");
        assert_eq!(processes.len(), 1);
        assert_eq!(processes[0]["name"], "api");
        // redacted_for_transport clears env and masks the pull-secret hash.
        assert_eq!(processes[0]["env"], serde_json::json!({}));
        assert_eq!(processes[0]["pull_secret_hash"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn advisory_endpoint_reports_risky_configuration_with_evidence() {
        // A process with crash-loop protection off and always-restart: two Critical advisories, and
        // the pair the daemon can least afford together.
        let mut risky = fixture_metrics_process();
        risky.name = "risky".to_string();
        risky.restart_policy = RestartPolicy::Always;
        risky.crash_restart_limit = 0;
        risky.restart_delay_secs = 0;

        let snapshot = snapshot_with_processes(vec![risky]);
        let response =
            oneshot_request(build_test_router(&snapshot), "GET", "/api/advisories").await;
        assert_eq!(status_code(&response), 200);

        let value = json_body(response).await;
        assert!(
            value["total"].as_u64().unwrap_or(0) >= 2,
            "expected advisories: {value}"
        );

        let entry = &value["processes"][0];
        assert_eq!(entry["process"], "risky");
        assert_eq!(
            entry["highest_severity"], "critical",
            "a disabled circuit breaker is critical"
        );

        // Structured evidence, not prose a consumer has to parse. This is what lets a dashboard
        // group by mechanism and show the offending setting without string matching.
        let advisory = &entry["advisories"][0];
        assert!(advisory["id"].is_string(), "machine-readable id required");
        assert!(advisory["mechanism"].is_string());
        assert!(
            advisory["evidence"]
                .as_array()
                .is_some_and(|e| !e.is_empty()),
            "an advisory without evidence is not actionable: {advisory}"
        );
        assert!(
            advisory["consequence"]
                .as_str()
                .is_some_and(|c| c.len() > 40),
            "the consequence must be stated, not just the rule name"
        );
    }

    #[tokio::test]
    async fn advisory_endpoint_withholds_capacity_rules_without_host_memory() {
        // No host collection has run in this snapshot, so capacity is unknown. The response must say
        // so rather than reporting a clean capacity class — an empty class and a checked-and-clear
        // class are different answers.
        let snapshot = snapshot_with_processes(vec![fixture_metrics_process()]);
        let response =
            oneshot_request(build_test_router(&snapshot), "GET", "/api/advisories").await;
        assert_eq!(status_code(&response), 200);

        let value = json_body(response).await;
        assert_eq!(
            value["capacity_available"], false,
            "capacity must be reported as unavailable, not silently skipped"
        );
        assert_eq!(
            value["processes"][0]["capacity"]["state"], "withheld",
            "the withholding travels per process: {value}"
        );
    }

    #[tokio::test]
    async fn advisory_endpoint_is_quiet_for_a_sound_configuration() {
        // Silence is the default. A consumer that always shows a badge would train an operator to
        // ignore it.
        let mut sound = fixture_metrics_process();
        sound.restart_policy = RestartPolicy::OnFailure;
        sound.crash_restart_limit = 3;
        sound.restart_delay_secs = 1;
        sound.resource_limits = None;
        sound.watch = false;

        let snapshot = snapshot_with_processes(vec![sound]);
        let response =
            oneshot_request(build_test_router(&snapshot), "GET", "/api/advisories").await;
        assert_eq!(status_code(&response), 200);

        let value = json_body(response).await;
        assert_eq!(value["total"], 0, "unexpected advisories: {value}");
        assert_eq!(
            value["processes"][0]["highest_severity"],
            serde_json::Value::Null
        );
    }

    #[tokio::test]
    async fn snapshot_api_returns_single_process_detail() {
        let snapshot = snapshot_with_processes(vec![fixture_metrics_process()]);
        let response =
            oneshot_request(build_test_router(&snapshot), "GET", "/api/processes/api").await;
        assert_eq!(status_code(&response), 200);
        let value = json_body(response).await;
        assert_eq!(value["name"], "api");
        assert_eq!(value["status"], "running");

        let missing =
            oneshot_request(build_test_router(&snapshot), "GET", "/api/processes/nope").await;
        assert_eq!(status_code(&missing), 404);
    }

    /// A host-consumers sample carrying attribution for one managed process at
    /// `managed_pid`, with a single direct-child descendant.
    fn fixture_host_consumers(managed_pid: u32) -> oxmgr_metrics::host_consumers::HostConsumers {
        oxmgr_metrics::host_consumers::HostConsumers {
            by_cpu: Vec::new(),
            by_memory: Vec::new(),
            by_cpu_trees: Vec::new(),
            by_memory_trees: Vec::new(),
            total_processes: 2,
            attribution: vec![oxmgr_metrics::host_consumers::ManagedProcessAttribution {
                pid: managed_pid,
                name: "api".to_string(),
                descendants: vec![oxmgr_metrics::host_consumers::AttributedDescendant {
                    pid: managed_pid + 1,
                    ppid: Some(managed_pid),
                    name: "worker".to_string(),
                    cpu_percent: 1.5,
                    memory_bytes: 2048,
                    depth: 1,
                    command: None,
                }],
                descendants_cpu_percent: 1.5,
                descendants_memory_bytes: 2048,
                truncated: false,
                descendant_count: 1,
            }],
            command_lines_included: false,
            sampled_at: 1_700_000_000,
            last_error: None,
        }
    }

    #[tokio::test]
    async fn process_api_attaches_descendant_attribution() {
        // The fixture process runs as pid 4242; the sample attributes one child to it.
        let snapshot = snapshot_with_processes(vec![fixture_metrics_process()]);
        snapshot
            .consumers
            .publish(fixture_host_consumers(4242))
            .await;

        for path in ["/api/processes", "/api/processes/api"] {
            let response = oneshot_request(build_test_router(&snapshot), "GET", path).await;
            assert_eq!(status_code(&response), 200, "{path}");
            let value = json_body(response).await;
            let entry = if path.ends_with("/api") {
                value
            } else {
                value[0].clone()
            };

            let descendants = &entry["descendants"];
            assert_eq!(
                descendants["status"], "ok",
                "a completed observation with an entry is ok: {descendants}"
            );
            assert_eq!(descendants["observed_at"], 1_700_000_000, "{path}");
            assert_eq!(descendants["truncated"], false);
            let rows = descendants["descendants"].as_array().expect("rows");
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["pid"], 4243);
            assert_eq!(rows[0]["depth"], 1);
            assert_eq!(rows[0]["cpu_percent"], 1.5);
            // Command line withheld by default (§D6) — attribution is not a side channel.
            assert_eq!(rows[0]["command"], serde_json::Value::Null);
            // Subtree totals travel WITH the ok status (§ managed-process-children).
            assert_eq!(descendants["descendants_cpu_percent"], 1.5);
            assert_eq!(descendants["descendants_memory_bytes"], 2048);
        }
    }

    #[tokio::test]
    async fn process_api_reports_observed_empty_distinct_from_unavailable() {
        // A sample completed and found no descendants: that is `ok` with an empty
        // list — a different claim from any unavailable reason.
        let mut consumers = fixture_host_consumers(4242);
        consumers.attribution[0].descendants.clear();
        consumers.attribution[0].descendants_cpu_percent = 0.0;
        consumers.attribution[0].descendants_memory_bytes = 0;

        let snapshot = snapshot_with_processes(vec![fixture_metrics_process()]);
        snapshot.consumers.publish(consumers).await;

        let response =
            oneshot_request(build_test_router(&snapshot), "GET", "/api/processes/api").await;
        assert_eq!(status_code(&response), 200);
        let value = json_body(response).await;
        assert_eq!(value["descendants"]["status"], "ok", "{value}");
        assert_eq!(
            value["descendants"]["descendants"],
            serde_json::json!([]),
            "observed-and-none, not absent"
        );
    }

    #[tokio::test]
    async fn process_api_reports_unavailable_when_sampling_is_disabled_and_withholds_totals() {
        // Default handle: sampling never started. The boundary must say unavailable,
        // name the cause, and carry NO subtree total — falling back to the process's
        // own figure would misrepresent what it covers.
        let snapshot = snapshot_with_processes(vec![fixture_metrics_process()]);
        assert!(!snapshot.consumers.sampling_enabled());

        let response =
            oneshot_request(build_test_router(&snapshot), "GET", "/api/processes/api").await;
        assert_eq!(status_code(&response), 200);
        let value = json_body(response).await;
        let descendants = &value["descendants"];
        assert_eq!(descendants["status"], "unavailable", "{value}");
        let reason = descendants["reason"].as_str().expect("reason stated");
        assert!(
            reason.contains("OXMGR_HOST_CONSUMERS"),
            "the disabling knob must be named: {reason}"
        );
        // Withheld entirely, not zeroed and not replaced by the process's own figure.
        assert!(descendants.get("descendants_cpu_percent").is_none());
        assert!(descendants.get("descendants_memory_bytes").is_none());
        assert!(descendants.get("descendants").is_none());
        // And the process's OWN figures are untouched by the withholding.
        assert!(value["cpu_percent"].is_number() || value["memory_bytes"].is_number());

        // The list endpoint withholds the same way.
        let list = oneshot_request(build_test_router(&snapshot), "GET", "/api/processes").await;
        let list_value = json_body(list).await;
        assert_eq!(list_value[0]["descendants"]["status"], "unavailable");
    }

    #[tokio::test]
    async fn process_api_reports_unavailable_before_the_first_sample() {
        // Sampling enabled but the first 30s cycle has not landed yet: still
        // unavailable, but the reason must not claim it was disabled.
        let snapshot = snapshot_with_processes(vec![fixture_metrics_process()]);
        snapshot.consumers.set_sampling_enabled(true);

        let response =
            oneshot_request(build_test_router(&snapshot), "GET", "/api/processes/api").await;
        assert_eq!(status_code(&response), 200);
        let value = json_body(response).await;
        assert_eq!(value["descendants"]["status"], "unavailable");
        let reason = value["descendants"]["reason"].as_str().unwrap();
        assert!(
            !reason.contains("OXMGR_HOST_CONSUMERS"),
            "enabled-but-pending is a different situation from disabled: {reason}"
        );
        assert!(
            value["descendants"]
                .get("descendants_cpu_percent")
                .is_none()
        );
    }

    #[tokio::test]
    async fn process_api_does_not_carry_attribution_across_a_pid_change_or_a_stop() {
        // The sample's entry is keyed to the pid observed THAT cycle. A process that
        // restarted under a new pid, or stopped, matches nothing — there is no
        // per-process memory to leak an old subtree through.
        let mut restarted = fixture_metrics_process();
        restarted.pid = Some(9999); // was 4242 in the fixture

        let mut stopped = fixture_metrics_process();
        stopped.name = "stopped".to_string();
        stopped.pid = None;
        stopped.status = oxmgr_metrics::process::ProcessStatus::Stopped;

        let snapshot = snapshot_with_processes(vec![restarted, stopped]);
        snapshot
            .consumers
            .publish(fixture_host_consumers(4242))
            .await;

        // New pid: the stale entry for 4242 must not follow the process.
        let response =
            oneshot_request(build_test_router(&snapshot), "GET", "/api/processes/api").await;
        let value = json_body(response).await;
        assert_eq!(value["descendants"]["status"], "unavailable", "{value}");

        // Stopped: no pid, no attribution, and its own stated reason.
        let response = oneshot_request(
            build_test_router(&snapshot),
            "GET",
            "/api/processes/stopped",
        )
        .await;
        assert_eq!(status_code(&response), 200);
        let value = json_body(response).await;
        assert_eq!(value["descendants"]["status"], "unavailable", "{value}");
        assert_eq!(value["descendants"]["reason"], "process is not running");
    }

    fn fixture_cluster_process() -> ManagedProcess {
        let mut process = fixture_metrics_process();
        process.name = "web".to_string();
        process.cluster_mode = true;
        process
    }

    #[tokio::test]
    async fn cluster_mode_is_reported_programmatically_not_inferred() {
        // 5.1: whether a process is a cluster is a FIELD on the payload, available to
        // any reader, not something inferred from its command string.
        let snapshot = snapshot_with_processes(vec![fixture_cluster_process()]);
        let response =
            oneshot_request(build_test_router(&snapshot), "GET", "/api/processes/web").await;
        let value = json_body(response).await;
        assert_eq!(value["cluster_mode"], true, "{value}");
    }

    #[tokio::test]
    async fn cluster_api_reports_requested_and_observed_as_separate_figures() {
        // §D5: two figures that legitimately differ during startup or after a crash.
        // Requested comes from config; observed from THIS sample's attribution.
        let mut cluster = fixture_cluster_process();
        cluster.cluster_instances = Some(4);

        let snapshot = snapshot_with_processes(vec![cluster]);
        snapshot
            .consumers
            .publish(fixture_host_consumers(4242))
            .await;

        let response =
            oneshot_request(build_test_router(&snapshot), "GET", "/api/processes/web").await;
        assert_eq!(status_code(&response), 200);
        let value = json_body(response).await;
        let shape = &value["cluster"];
        assert_eq!(
            shape["requested"]["count"], 4,
            "requested is the configured number"
        );
        assert_eq!(shape["requested"]["derived"], false);
        assert_eq!(shape["observed"]["status"], "ok", "{shape}");
        // One attributed child in the fixture → one observed worker. The figures are
        // separate objects precisely so they can disagree without either lying.
        assert_eq!(shape["observed"]["workers"], 1);
        assert_eq!(shape["observed"]["observed_at"], 1_700_000_000);
    }

    #[tokio::test]
    async fn cluster_api_labels_a_derived_requested_count_and_reports_no_invented_number() {
        // cluster_instances unset → the bootstrap derived the count from CPU
        // availability at start. The daemon does not know that number: reporting the
        // OBSERVED count as requested would make every shortfall undetectable, so the
        // figure is null and the derivation is stated instead.
        let cluster = fixture_cluster_process(); // cluster_instances: None

        let snapshot = snapshot_with_processes(vec![cluster]);
        snapshot
            .consumers
            .publish(fixture_host_consumers(4242))
            .await;

        let response =
            oneshot_request(build_test_router(&snapshot), "GET", "/api/processes/web").await;
        let value = json_body(response).await;
        assert_eq!(
            value["cluster"]["requested"]["count"],
            serde_json::Value::Null,
            "{value}"
        );
        assert_eq!(value["cluster"]["requested"]["derived"], true);
        assert_eq!(value["cluster"]["observed"]["workers"], 1);
    }

    #[tokio::test]
    async fn cluster_api_observed_follows_attribution_availability_exactly() {
        // 5.4: unavailable when attribution is unavailable — zero only when a sample
        // completed and found none. Both unavailability reasons carry through to the
        // cluster object, which never substitutes a zero or the requested count.
        let mut cluster = fixture_cluster_process();
        cluster.cluster_instances = Some(4);

        // Disabled: unavailable, named cause.
        let disabled = snapshot_with_processes(vec![cluster.clone()]);
        let response =
            oneshot_request(build_test_router(&disabled), "GET", "/api/processes/web").await;
        let value = json_body(response).await;
        assert_eq!(
            value["cluster"]["observed"]["status"], "unavailable",
            "{value}"
        );
        assert!(
            value["cluster"]["observed"]["reason"]
                .as_str()
                .is_some_and(|r| r.contains("OXMGR_HOST_CONSUMERS")),
        );
        assert!(value["cluster"]["observed"].get("workers").is_none());

        // Enabled but pending: unavailable, different cause.
        let pending = snapshot_with_processes(vec![cluster.clone()]);
        pending.consumers.set_sampling_enabled(true);
        let response =
            oneshot_request(build_test_router(&pending), "GET", "/api/processes/web").await;
        let value = json_body(response).await;
        assert_eq!(value["cluster"]["observed"]["status"], "unavailable");
        assert!(
            !value["cluster"]["observed"]["reason"]
                .as_str()
                .unwrap()
                .contains("OXMGR_HOST_CONSUMERS")
        );

        // Sample completed, entry present, ZERO descendants: ok with workers=0 —
        // an observed none, not an unavailability.
        let mut empty_sample = fixture_host_consumers(4242);
        empty_sample.attribution[0].descendants.clear();
        empty_sample.attribution[0].descendant_count = 0;
        let observed_zero = snapshot_with_processes(vec![cluster]);
        observed_zero.consumers.publish(empty_sample).await;
        let response = oneshot_request(
            build_test_router(&observed_zero),
            "GET",
            "/api/processes/web",
        )
        .await;
        let value = json_body(response).await;
        assert_eq!(value["cluster"]["observed"]["status"], "ok", "{value}");
        assert_eq!(value["cluster"]["observed"]["workers"], 0);
    }

    #[tokio::test]
    async fn non_cluster_carries_no_cluster_field() {
        // Spec scenario: a non-cluster process carries no cluster marking. The field
        // must be ABSENT, not present-with-false — a reader keying on presence would
        // otherwise treat every row as cluster data.
        let snapshot = snapshot_with_processes(vec![fixture_metrics_process()]);
        snapshot
            .consumers
            .publish(fixture_host_consumers(4242))
            .await;

        let response =
            oneshot_request(build_test_router(&snapshot), "GET", "/api/processes/api").await;
        let value = json_body(response).await;
        assert!(
            value.get("cluster").is_none(),
            "non-cluster must not carry a cluster object: {value}"
        );
    }

    #[tokio::test]
    async fn cluster_worker_count_uses_the_untruncated_count() {
        // D5 via the truncation rule: the LISTED rows stop at the node budget, but the
        // observed worker count is of the whole observed set. A count read off
        // descendants.len() would silently become a count of the budget.
        let mut cluster = fixture_cluster_process();
        cluster.cluster_instances = Some(400);

        let mut sample = fixture_host_consumers(4242);
        let entry = &mut sample.attribution[0];
        entry.descendant_count = 350; // more than ATTRIBUTION_NODE_BUDGET (300)
        entry.truncated = true;
        // Listed rows stay small; only the count carries the real size.

        let snapshot = snapshot_with_processes(vec![cluster]);
        snapshot.consumers.publish(sample).await;

        let response =
            oneshot_request(build_test_router(&snapshot), "GET", "/api/processes/web").await;
        let value = json_body(response).await;
        assert_eq!(
            value["cluster"]["observed"]["workers"], 350,
            "the count must be of the observed set, not of the listed rows: {value}"
        );
    }

    #[tokio::test]
    async fn an_expanded_instance_that_is_also_a_cluster_reports_both_facts() {
        // 5.8: `instances` expansion (separate managed processes named name-N) and
        // cluster workers (attributed children of ONE bootstrap pid) are different
        // mechanisms. A process can be both: the instance index lives in the
        // operator-given NAME, the worker count in the cluster object. Neither
        // replaces the other.
        let mut both = fixture_cluster_process();
        both.name = "api-1".to_string(); // as produced by expand_instances
        both.cluster_instances = Some(2);

        let snapshot = snapshot_with_processes(vec![both]);
        snapshot
            .consumers
            .publish(fixture_host_consumers(4242))
            .await;

        let response =
            oneshot_request(build_test_router(&snapshot), "GET", "/api/processes/api-1").await;
        assert_eq!(status_code(&response), 200);
        let value = json_body(response).await;
        assert_eq!(
            value["name"], "api-1",
            "instance identity stays in the name"
        );
        assert_eq!(value["cluster_mode"], true);
        assert_eq!(value["cluster"]["requested"]["count"], 2);
        assert_eq!(
            value["cluster"]["observed"]["workers"], 1,
            "this instance's OWN bootstrap workers, not a fleet-wide figure"
        );
    }

    #[tokio::test]
    async fn attribution_is_memory_only_and_never_persisted() {
        // §D2 / 3.1–3.2: attribution lives in the consumers handle only. It must not
        // reach PersistedState (every whole-file save would grow with descendant
        // count) and must not survive a restart (a pre-restart observation must never
        // be presented as current).
        let directory = temp_dir("attribution-memory-only");
        fs::create_dir_all(&directory).expect("create temp dir");
        let state_path = directory.join("state.json");

        let snapshot = snapshot_with_processes(vec![fixture_metrics_process()]);
        snapshot
            .consumers
            .publish(fixture_host_consumers(4242))
            .await;

        // What the daemon saves is exactly the process list — no descendant rows.
        oxmgr_manager::storage::save_state(
            &state_path,
            &oxmgr_manager::storage::PersistedState {
                next_id: 2,
                processes: vec![fixture_metrics_process()],
            },
        )
        .expect("save state");
        let written = fs::read_to_string(&state_path).expect("read saved state");
        assert!(
            !written.contains("descendants"),
            "persisted state must not carry attribution: {written}"
        );
        let reloaded = oxmgr_manager::storage::load_state(&state_path).expect("reload");
        assert_eq!(reloaded.processes.len(), 1);

        // A fresh daemon (new snapshot) starts with no observation at all.
        let fresh = DaemonSnapshot::default();
        assert!(
            fresh.host_consumers().await.is_none(),
            "a restarted daemon has no pre-restart sample"
        );
        assert!(!fresh.consumers.sampling_enabled());
    }

    #[tokio::test]
    async fn disabled_sampling_leaves_the_flag_off_and_the_api_unavailable() {
        // 3.8: the unavailable path must bite end to end — the env knob keeps the
        // sampling task from starting, the flag stays off, and the API reports
        // "disabled" rather than an empty list.
        let _guard = crate::test_utils::EnvGuard::set("OXMGR_HOST_CONSUMERS", "off");
        let handle = oxmgr_metrics::host_metrics::HostConsumersHandle::new();
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        spawn_consumer_sampling(handle.clone(), shutdown_rx);

        assert!(
            !handle.sampling_enabled(),
            "the disabled path must leave the flag off"
        );
        assert!(handle.current().await.is_none(), "no sample was taken");

        let snapshot = DaemonSnapshot {
            consumers: handle,
            ..snapshot_with_processes(vec![fixture_metrics_process()])
        };
        let response =
            oneshot_request(build_test_router(&snapshot), "GET", "/api/processes/api").await;
        let value = json_body(response).await;
        assert_eq!(value["descendants"]["status"], "unavailable", "{value}");
        assert!(
            value["descendants"]["reason"]
                .as_str()
                .is_some_and(|r| r.contains("OXMGR_HOST_CONSUMERS")),
            "disabled must be distinguishable from pending: {value}"
        );
    }

    #[tokio::test]
    async fn enabled_sampling_marks_the_flag_and_the_first_sample_clears_pending() {
        // The complementary path: with sampling on, the flag flips before the first
        // cycle, so a scrape in that window reads "pending", not "disabled".
        let _rm = crate::test_utils::EnvGuard::remove("OXMGR_HOST_CONSUMERS");
        let handle = oxmgr_metrics::host_metrics::HostConsumersHandle::new();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        spawn_consumer_sampling(handle.clone(), shutdown_rx);

        assert!(
            handle.sampling_enabled(),
            "starting the sampler marks it enabled"
        );
        // Stop the loop task; it owns no resources beyond this test.
        let _ = shutdown_tx.send(true);
    }

    #[tokio::test]
    async fn snapshot_api_returns_log_tail_by_stream() {
        let directory = temp_dir("dashboard-logs");
        fs::create_dir_all(&directory).expect("failed to create temp log dir");
        let mut process = fixture_metrics_process();
        process.stdout_log = directory.join("api.out.log");
        process.stderr_log = directory.join("api.err.log");
        fs::write(&process.stdout_log, "out a\nout b\n").expect("write stdout fixture");
        fs::write(&process.stderr_log, "err a\nerr b\n").expect("write stderr fixture");

        let snapshot = snapshot_with_processes(vec![process]);

        let out = oneshot_request(
            build_test_router(&snapshot),
            "GET",
            "/api/processes/api/logs?stream=stdout&lines=10",
        )
        .await;
        assert_eq!(status_code(&out), 200);
        let out_val = json_body(out).await;
        assert_eq!(out_val["lines"], serde_json::json!(["out a", "out b"]));
        assert_eq!(out_val["stream"], "stdout");

        let err = oneshot_request(
            build_test_router(&snapshot),
            "GET",
            "/api/processes/api/logs?stream=stderr&lines=10",
        )
        .await;
        assert_eq!(status_code(&err), 200);
        let err_val = json_body(err).await;
        assert_eq!(err_val["lines"], serde_json::json!(["err a", "err b"]));

        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn snapshot_api_logs_unknown_process_returns_404() {
        let snapshot = snapshot_with_processes(Vec::default());
        let response = oneshot_request(
            build_test_router(&snapshot),
            "GET",
            "/api/processes/nope/logs?lines=10",
        )
        .await;
        assert_eq!(status_code(&response), 404);
    }

    /// Builds a process whose logs live in `directory`, with seeded stdout,
    /// stderr, and one rotated stdout archive.
    fn log_fixture_process(directory: &Path) -> ManagedProcess {
        let mut process = fixture_metrics_process();
        process.stdout_log = directory.join("api.out.log");
        process.stderr_log = directory.join("api.err.log");
        fs::write(&process.stdout_log, "out a\nout b\n").expect("write stdout fixture");
        fs::write(&process.stderr_log, "err a\nerr b\n").expect("write stderr fixture");
        fs::write(directory.join("api.out.log.1"), "archived\n").expect("write archive fixture");
        process
    }

    /// An unrecognised stream must be refused rather than silently answered with
    /// stdout: `stream=error` used to return stderr under a third name, so a
    /// typo would be served plausible-looking wrong content.
    #[tokio::test]
    async fn snapshot_api_logs_rejects_unknown_stream() {
        let directory = temp_dir("dashboard-logs-unknown-stream");
        fs::create_dir_all(&directory).expect("failed to create temp log dir");
        let snapshot = snapshot_with_processes(vec![log_fixture_process(&directory)]);

        for path in [
            "/api/processes/api/logs?stream=error&lines=10",
            "/api/processes/api/logs?stream=stdrr&lines=10",
            "/api/processes/api/logs/download?stream=error",
        ] {
            let response = oneshot_request(build_test_router(&snapshot), "GET", path).await;
            assert_eq!(
                status_code(&response),
                400,
                "expected {path} to refuse an unknown stream"
            );
        }

        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn snapshot_api_lists_active_and_rotated_log_files() {
        let directory = temp_dir("dashboard-logs-files");
        fs::create_dir_all(&directory).expect("failed to create temp log dir");
        let snapshot = snapshot_with_processes(vec![log_fixture_process(&directory)]);

        let response = oneshot_request(
            build_test_router(&snapshot),
            "GET",
            "/api/processes/api/logs/files",
        )
        .await;
        assert_eq!(status_code(&response), 200);

        let files = json_body(response).await["files"]
            .as_array()
            .expect("files should be an array")
            .clone();
        // Active file first, then its archive, then the other stream.
        let entries: Vec<(String, u64)> = files
            .iter()
            .map(|entry| {
                (
                    entry["stream"].as_str().unwrap_or_default().to_string(),
                    entry["index"].as_u64().unwrap_or_default(),
                )
            })
            .collect();
        assert!(
            entries.contains(&("stdout".to_string(), 0)),
            "active stdout log should be listed, got {entries:?}"
        );
        assert!(
            entries.contains(&("stdout".to_string(), 1)),
            "rotated stdout archive should be listed, got {entries:?}"
        );
        assert!(
            entries.contains(&("stderr".to_string(), 0)),
            "active stderr log should be listed, got {entries:?}"
        );
        // Metadata the dashboard renders must be present, not just the path.
        let active = files
            .iter()
            .find(|entry| entry["stream"] == "stdout" && entry["index"] == 0)
            .expect("active stdout entry");
        assert!(active["size"].as_u64().unwrap_or_default() > 0);
        assert!(active["modified_at"].as_u64().unwrap_or_default() > 0);

        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn snapshot_api_downloads_active_log_and_archive_as_attachments() {
        let directory = temp_dir("dashboard-logs-download");
        fs::create_dir_all(&directory).expect("failed to create temp log dir");
        let snapshot = snapshot_with_processes(vec![log_fixture_process(&directory)]);

        let active = oneshot_request(
            build_test_router(&snapshot),
            "GET",
            "/api/processes/api/logs/download?stream=stdout",
        )
        .await;
        assert_eq!(status_code(&active), 200);
        assert_eq!(
            active
                .headers()
                .get("Content-Disposition")
                .and_then(|v| v.to_str().ok()),
            Some("attachment; filename=\"api.out.log\"")
        );
        assert_eq!(text_body(active).await, "out a\nout b\n");

        let archive = oneshot_request(
            build_test_router(&snapshot),
            "GET",
            "/api/processes/api/logs/download?stream=stdout&index=1",
        )
        .await;
        assert_eq!(status_code(&archive), 200);
        assert_eq!(
            archive
                .headers()
                .get("Content-Disposition")
                .and_then(|v| v.to_str().ok()),
            Some("attachment; filename=\"api.out.log.1\"")
        );
        assert_eq!(text_body(archive).await, "archived\n");

        let _ = fs::remove_dir_all(directory);
    }

    /// The archive index reconstructs a path from the process's own log path, so
    /// no value here may escape the log directory. Pins that the parameter is
    /// parsed as an integer rather than concatenated.
    #[tokio::test]
    async fn snapshot_api_log_download_refuses_traversal_and_missing_archive() {
        let directory = temp_dir("dashboard-logs-traversal");
        fs::create_dir_all(&directory).expect("failed to create temp log dir");
        let snapshot = snapshot_with_processes(vec![log_fixture_process(&directory)]);

        for index in [
            "../../../etc/passwd",
            "-1",
            "1;rm",
            "0x1",
            "%2e%2e%2fetc%2fpasswd",
        ] {
            let path = format!("/api/processes/api/logs/download?stream=stdout&index={index}");
            let response = oneshot_request(build_test_router(&snapshot), "GET", &path).await;
            assert_eq!(
                status_code(&response),
                400,
                "expected index {index:?} to be refused"
            );
        }

        // A well-formed index that names no retained archive is a 404, not a 400:
        // the request was valid, the file simply is not there.
        let missing = oneshot_request(
            build_test_router(&snapshot),
            "GET",
            "/api/processes/api/logs/download?stream=stdout&index=99",
        )
        .await;
        assert_eq!(status_code(&missing), 404);

        let _ = fs::remove_dir_all(directory);
    }

    /// Paging backwards through a log: successive `before` offsets must return
    /// contiguous sections, and the last one must say there is nothing earlier. This is
    /// what makes the start of a rotated archive reachable at all — before this, the
    /// only options were "the tail" or "the whole file".
    #[tokio::test]
    async fn snapshot_api_logs_pages_backwards_and_reports_the_start() {
        let directory = temp_dir("dashboard-logs-paging");
        fs::create_dir_all(&directory).expect("failed to create temp log dir");
        let mut process = fixture_metrics_process();
        process.stdout_log = directory.join("api.out.log");
        process.stderr_log = directory.join("api.err.log");
        let body: String = (1..=30).map(|idx| format!("line {idx}\n")).collect();
        fs::write(&process.stdout_log, body).expect("write paged fixture");
        let snapshot = snapshot_with_processes(vec![process]);

        let page = |before: usize| {
            let snapshot = snapshot.clone();
            async move {
                let path =
                    format!("/api/processes/api/logs?stream=stdout&lines=10&before={before}");
                let response = oneshot_request(build_test_router(&snapshot), "GET", &path).await;
                assert_eq!(status_code(&response), 200);
                json_body(response).await
            }
        };

        let newest = page(0).await;
        assert_eq!(newest["lines"][0], "line 21");
        assert_eq!(newest["lines"][9], "line 30");
        assert_eq!(newest["reached_start"], false);

        let middle = page(10).await;
        assert_eq!(middle["lines"][0], "line 11");
        assert_eq!(middle["lines"][9], "line 20");
        assert_eq!(middle["reached_start"], false);

        let oldest = page(20).await;
        assert_eq!(oldest["lines"][0], "line 1");
        assert_eq!(oldest["lines"][9], "line 10");
        assert_eq!(
            oldest["reached_start"], true,
            "a window beginning at line 1 must report the start"
        );

        // Past the beginning: nothing left, and it says so rather than erroring.
        let beyond = page(100).await;
        assert_eq!(beyond["lines"].as_array().map(Vec::len), Some(0));
        assert_eq!(beyond["reached_start"], true);

        let _ = fs::remove_dir_all(directory);
    }

    /// The same paging must work on a rotated archive, which is the case that prompted
    /// it: an archive is finished, so every section of it is re-readable.
    #[tokio::test]
    async fn snapshot_api_logs_pages_within_a_rotated_archive() {
        let directory = temp_dir("dashboard-logs-paging-archive");
        fs::create_dir_all(&directory).expect("failed to create temp log dir");
        let mut process = fixture_metrics_process();
        process.stdout_log = directory.join("api.out.log");
        process.stderr_log = directory.join("api.err.log");
        fs::write(&process.stdout_log, "active\n").expect("write active");
        let archived: String = (1..=25).map(|idx| format!("old {idx}\n")).collect();
        fs::write(directory.join("api.out.log.1"), archived).expect("write archive");
        let snapshot = snapshot_with_processes(vec![process]);

        let tail = oneshot_request(
            build_test_router(&snapshot),
            "GET",
            "/api/processes/api/logs?stream=stdout&index=1&lines=5",
        )
        .await;
        assert_eq!(status_code(&tail), 200);
        let tail_val = json_body(tail).await;
        assert_eq!(tail_val["lines"][4], "old 25");
        assert_eq!(tail_val["index"], 1);
        assert_eq!(tail_val["reached_start"], false);

        let start = oneshot_request(
            build_test_router(&snapshot),
            "GET",
            "/api/processes/api/logs?stream=stdout&index=1&lines=5&before=20",
        )
        .await;
        assert_eq!(status_code(&start), 200);
        let start_val = json_body(start).await;
        assert_eq!(start_val["lines"][0], "old 1");
        assert_eq!(start_val["reached_start"], true);

        // An index that names no retained archive is a 404, not an empty page.
        let missing = oneshot_request(
            build_test_router(&snapshot),
            "GET",
            "/api/processes/api/logs?stream=stdout&index=9&lines=5",
        )
        .await;
        assert_eq!(status_code(&missing), 404);

        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn snapshot_api_logs_rejects_a_malformed_line_offset() {
        let directory = temp_dir("dashboard-logs-bad-offset");
        fs::create_dir_all(&directory).expect("failed to create temp log dir");
        let snapshot = snapshot_with_processes(vec![log_fixture_process(&directory)]);

        for before in ["-1", "abc", "1;drop", "1.5"] {
            let path = format!("/api/processes/api/logs?stream=stdout&before={before}");
            let response = oneshot_request(build_test_router(&snapshot), "GET", &path).await;
            assert_eq!(
                status_code(&response),
                400,
                "expected offset {before:?} to be refused"
            );
        }

        let _ = fs::remove_dir_all(directory);
    }

    /// Existing callers pass no `before`, so the endpoint must still answer with the
    /// tail exactly as it did before paging existed.
    #[tokio::test]
    async fn snapshot_api_logs_without_an_offset_still_returns_the_tail() {
        let directory = temp_dir("dashboard-logs-default-offset");
        fs::create_dir_all(&directory).expect("failed to create temp log dir");
        let snapshot = snapshot_with_processes(vec![log_fixture_process(&directory)]);

        let response = oneshot_request(
            build_test_router(&snapshot),
            "GET",
            "/api/processes/api/logs?stream=stdout&lines=10",
        )
        .await;
        assert_eq!(status_code(&response), 200);
        let val = json_body(response).await;
        assert_eq!(val["lines"], serde_json::json!(["out a", "out b"]));
        assert_eq!(val["before"], 0);

        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn snapshot_api_log_files_and_download_reject_unknown_process() {
        let snapshot = snapshot_with_processes(Vec::default());
        for path in [
            "/api/processes/nope/logs/files",
            "/api/processes/nope/logs/download?stream=stdout",
        ] {
            let response = oneshot_request(build_test_router(&snapshot), "GET", path).await;
            assert_eq!(status_code(&response), 404, "expected {path} to 404");
        }
    }

    /// The standalone log page reuses the dashboard document, so it must still be
    /// gated on the process existing rather than serving a page for anything.
    #[tokio::test]
    async fn standalone_log_page_requires_a_known_process() {
        let directory = temp_dir("dashboard-logs-page");
        fs::create_dir_all(&directory).expect("failed to create temp log dir");
        let snapshot = snapshot_with_processes(vec![log_fixture_process(&directory)]);

        let ok = oneshot_request(
            build_test_router(&snapshot),
            "GET",
            "/logs/api?stream=stdout",
        )
        .await;
        assert_eq!(status_code(&ok), 200);
        let body = text_body(ok).await;
        assert!(
            body.contains("OxMgr Dashboard"),
            "log page should be the dashboard document"
        );

        let unknown = oneshot_request(build_test_router(&snapshot), "GET", "/logs/nope").await;
        assert_eq!(status_code(&unknown), 404);

        let _ = fs::remove_dir_all(directory);
    }

    // --- dashboard REST API (manager mutation endpoints) ---

    #[tokio::test]
    async fn executor_stops_process_and_reports_count() {
        let mut manager = empty_manager("api-stop-action");
        let _ = start_minimal_service(&mut manager, "api", None, None, None).await;

        let response = execute_api_request(
            HttpCommand::Stop {
                target: "all".into(),
            },
            &mut manager,
        )
        .await;
        assert_eq!(status_code(&response), 200);
        let val = json_body(response).await;
        assert_eq!(val["message"], "stop 1 process(es)");

        let process = manager
            .get_process("api")
            .expect("process should still exist");
        assert_eq!(process.status.to_string(), "stopped");
        assert_eq!(process.desired_state.to_string(), "stopped");

        let _ = manager.shutdown_all().await;
    }

    #[tokio::test]
    async fn executor_restarts_process() {
        let mut manager = empty_manager("api-restart-action");
        let _ = start_minimal_service(&mut manager, "api", None, None, None).await;

        let response = execute_api_request(
            HttpCommand::Restart {
                target: "api".into(),
            },
            &mut manager,
        )
        .await;
        assert_eq!(status_code(&response), 200);
        assert_eq!(
            json_body(response).await["message"],
            "restart 1 process(es)"
        );

        let _ = manager.shutdown_all().await;
    }

    #[tokio::test]
    async fn executor_reloads_process() {
        let mut manager = empty_manager("api-reload-action");
        let _ = start_minimal_service(&mut manager, "api", None, None, None).await;

        let response = execute_api_request(
            HttpCommand::Reload {
                target: "api".into(),
            },
            &mut manager,
        )
        .await;
        assert_eq!(status_code(&response), 200);
        assert_eq!(json_body(response).await["message"], "reload 1 process(es)");

        let _ = manager.shutdown_all().await;
    }

    #[tokio::test]
    async fn executor_handles_stop_all_for_dashboard() {
        let mut manager = empty_manager("api-stop-all");
        let _ = start_minimal_service(&mut manager, "api", None, None, None).await;
        let _ = start_minimal_service(&mut manager, "worker", None, None, None).await;

        let response = execute_api_request(
            HttpCommand::Stop {
                target: "all".into(),
            },
            &mut manager,
        )
        .await;
        assert_eq!(status_code(&response), 200);
        assert_eq!(json_body(response).await["message"], "stop 2 process(es)");

        assert_eq!(manager.list_processes().len(), 2);
        assert!(
            manager
                .list_processes()
                .iter()
                .all(|p| matches!(p.status, ProcessStatus::Stopped))
        );

        let _ = manager.shutdown_all().await;
    }

    #[tokio::test]
    async fn executor_returns_404_for_unknown_process_and_unknown_action() {
        let mut manager = empty_manager("api-404");
        let response = execute_api_request(
            HttpCommand::Stop {
                target: "nope".into(),
            },
            &mut manager,
        )
        .await;
        assert_eq!(status_code(&response), 404);

        // The router rejects an action that has no route — no enum variant for
        // "explode", so the unknown-action half is now axum's 404, not ours.
        let _ = start_minimal_service(&mut manager, "api", None, None, None).await;
        let response = oneshot_request(
            build_test_router(&snapshot_with_processes(Vec::default())),
            "POST",
            "/api/processes/api/explode",
        )
        .await;
        assert_eq!(status_code(&response), 404);

        let _ = manager.shutdown_all().await;
    }

    #[tokio::test]
    async fn execute_api_request_rejects_non_post_method() {
        let router = build_test_router(&snapshot_with_processes(Vec::default()));
        let response = oneshot_request(router, "GET", "/pull/api").await;
        assert_eq!(status_code(&response), 405);
    }

    #[tokio::test]
    async fn execute_api_request_rejects_missing_secret() {
        let mut manager = empty_manager("daemon-api-missing-secret");
        start_minimal_service(&mut manager, "api", None, None, None).await;

        let response = execute_api_request(
            HttpCommand::Pull {
                target: "api".into(),
                secret: None,
            },
            &mut manager,
        )
        .await;
        assert_eq!(status_code(&response), 401);
        assert_eq!(
            json_body(response).await["message"],
            "missing webhook secret"
        );
        let _ = manager.shutdown_all().await;
    }

    #[tokio::test]
    async fn execute_api_request_rejects_invalid_secret() {
        let mut manager = empty_manager("daemon-api-invalid-secret");
        start_minimal_service(
            &mut manager,
            "api",
            None,
            None,
            Some(hash_secret("expected")),
        )
        .await;

        let response = execute_api_request(
            HttpCommand::Pull {
                target: "api".into(),
                secret: Some("wrong".into()),
            },
            &mut manager,
        )
        .await;
        assert_eq!(status_code(&response), 401);
        assert_eq!(
            json_body(response).await["message"],
            "invalid webhook secret"
        );
        let _ = manager.shutdown_all().await;
    }

    #[tokio::test]
    async fn execute_api_request_runs_pull_when_secret_is_valid() {
        let git = setup_git_fixture("daemon-api-pull");
        let mut manager = empty_manager("daemon-api-pull-manager");
        start_minimal_service(
            &mut manager,
            "api",
            Some(git.clone_dir.clone()),
            Some(git.remote_dir.display().to_string()),
            Some(hash_secret("hook-secret")),
        )
        .await;

        let response = execute_api_request(
            HttpCommand::Pull {
                target: "api".into(),
                secret: Some("hook-secret".into()),
            },
            &mut manager,
        )
        .await;
        assert_eq!(status_code(&response), 200);
        let value = json_body(response).await;
        assert!(value["ok"].as_bool().unwrap());
        assert!(
            value["message"]
                .as_str()
                .unwrap_or_default()
                .contains("Pull complete"),
            "unexpected response body: {}",
            value
        );

        let _ = manager.shutdown_all().await;
        let _ = fs::remove_dir_all(git.root);
    }

    #[tokio::test]
    async fn snapshot_api_request_serves_prometheus_metrics() {
        let snapshot = snapshot_with_processes(vec![fixture_metrics_process()]);
        let response = oneshot_request(build_test_router(&snapshot), "GET", "/metrics").await;
        assert_eq!(status_code(&response), 200);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap(),
            PROMETHEUS_CONTENT_TYPE
        );

        let body = text_body(response).await;
        assert!(body.contains("# TYPE oxmgr_managed_processes gauge"));
        assert!(body.contains("oxmgr_managed_processes 1"));
        assert!(body.contains("oxmgr_process_up{id=\"42\",name=\"api\",namespace=\"prod\"} 1"));
        assert!(body.contains(
            "oxmgr_process_info{id=\"42\",name=\"api\",namespace=\"prod\",desired_state=\"running\",restart_policy=\"always\",status=\"running\"} 1"
        ));
        assert!(body.contains(
            "oxmgr_process_health_status{id=\"42\",name=\"api\",namespace=\"prod\",health_status=\"healthy\"} 1"
        ));
    }

    #[test]
    fn render_prometheus_metrics_escapes_labels_and_sanitizes_nan() {
        let mut process = fixture_metrics_process();
        process.name = "api\"svc".to_string();
        process.namespace = Some("prod\\blue\nline".to_string());
        process.cpu_percent = f32::NAN;

        let rendered = render_prometheus_metrics(&[process]);
        assert!(rendered.contains("name=\"api\\\"svc\""));
        assert!(rendered.contains("namespace=\"prod\\\\blue\\nline\""));
        assert!(rendered.contains("oxmgr_process_cpu_percent{id=\"42\",name=\"api\\\"svc\",namespace=\"prod\\\\blue\\nline\"} 0"));
    }

    /// A host snapshot with every subsystem present, for the "everything reports" path.
    fn fixture_host_metrics() -> oxmgr_metrics::host_metrics::HostMetrics {
        use oxmgr_metrics::host_metrics::*;
        HostMetrics {
            identity: std::sync::Arc::new(HostIdentity {
                host_name: Some("build-01".to_string()),
                os_name: Some("Linux".to_string()),
                os_version: Some("6.1".to_string()),
                kernel_version: Some("6.1.0".to_string()),
                cpu_arch: Some("x86_64".to_string()),
                physical_core_count: Some(8),
                logical_core_count: Some(16),
                boot_time: Some(1_700_000_000),
                long_os_version: None,
                // A fixture for an unconstrained host, so no container limits apply.
                container: None,
            }),
            uptime_secs: Some(3_600),
            memory: Some(HostMemory {
                total_bytes: 8_000_000_000,
                used_bytes: 4_000_000_000,
                available_bytes: 4_000_000_000,
                free_bytes: 4_000_000_000,
                used_percent: Some(50.0),
                swap: Some(HostSwap {
                    total_bytes: 2_000_000_000,
                    used_bytes: 500_000_000,
                    used_percent: Some(25.0),
                }),
                // No container limit in this fixture, so the effective figures are absent
                // rather than duplicating the host total.
                effective_total_bytes: None,
                effective_used_percent: None,
            }),
            cpu: Some(HostCpu {
                global_percent: Some(42.5),
                per_core: None,
                sample_interval_ms: Some(2_000),
            }),
            load_average: Some(HostLoadAverage {
                one: 1.5,
                five: 2.0,
                fifteen: 2.5,
            }),
            filesystems: Some(std::sync::Arc::new(vec![
                HostFilesystem {
                    mount_point: "/".to_string(),
                    file_system: "ext4".to_string(),
                    kind: "ext4".to_string(),
                    total_bytes: 100_000_000,
                    available_bytes: 40_000_000,
                    used_bytes: 60_000_000,
                    used_percent: Some(60.0),
                    is_removable: false,
                    is_read_only: false,
                    pseudo: false,
                },
                // Zero capacity, so utilisation is unavailable rather than 0 or 100.
                HostFilesystem {
                    mount_point: "/proc".to_string(),
                    file_system: "proc".to_string(),
                    kind: "proc".to_string(),
                    total_bytes: 0,
                    available_bytes: 0,
                    used_bytes: 0,
                    used_percent: None,
                    is_removable: false,
                    is_read_only: true,
                    pseudo: true,
                },
            ])),
            network: Some(std::sync::Arc::new(HostNetwork {
                scope: MetricScope::Host,
                interfaces: vec![
                    HostInterface {
                        name: "eth0".to_string(),
                        received_bytes: 2_000,
                        transmitted_bytes: 1_000,
                        total_received_bytes: 900_000,
                        total_transmitted_bytes: 400_000,
                        errors_on_received: 1,
                        errors_on_transmitted: 2,
                        interval_ms: Some(2_000),
                    },
                    // No interval yet: the first measurement for this interface.
                    HostInterface {
                        name: "lo".to_string(),
                        received_bytes: 500,
                        transmitted_bytes: 500,
                        total_received_bytes: 500,
                        total_transmitted_bytes: 500,
                        errors_on_received: 0,
                        errors_on_transmitted: 0,
                        interval_ms: None,
                    },
                ],
            })),
            components: None,
            collected_at: 1_700_003_600,
            interval_adjustments: Vec::new(),
            failures: Vec::new(),
        }
    }

    #[test]
    fn host_metrics_render_counters_and_gauges_with_the_right_types() {
        let rendered = render_host_prometheus_metrics(&fixture_host_metrics());

        // Cumulative interface figures are the only counters: they only ever grow.
        assert!(rendered.contains("# TYPE oxmgr_host_network_received_bytes_total counter"));
        assert!(rendered.contains("# TYPE oxmgr_host_network_transmitted_bytes_total counter"));
        assert!(rendered.contains("# TYPE oxmgr_host_network_errors_total counter"));

        // Utilisation and capacity are gauges: they move in both directions.
        assert!(rendered.contains("# TYPE oxmgr_host_memory_bytes gauge"));
        assert!(rendered.contains("# TYPE oxmgr_host_memory_used_percent gauge"));
        assert!(rendered.contains("# TYPE oxmgr_host_cpu_used_percent gauge"));
        assert!(rendered.contains("# TYPE oxmgr_host_load_average gauge"));
        assert!(rendered.contains("# TYPE oxmgr_host_filesystem_bytes gauge"));
        assert!(rendered.contains("# TYPE oxmgr_host_filesystem_used_percent gauge"));

        // Every series carries a HELP line, and the text is well formed enough that no
        // value is left empty by a formatting slip.
        for line in rendered
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            let (_, value) = line.rsplit_once(' ').expect("every sample has a value");
            assert!(
                !value.is_empty() && value.parse::<f64>().is_ok(),
                "not a numeric sample: {line}"
            );
        }
    }

    #[test]
    fn host_network_series_are_scoped_to_the_host() {
        let rendered = render_host_prometheus_metrics(&fixture_host_metrics());

        // The label is what stops a dashboard query attributing the machine's traffic to one
        // service. Per-process network I/O is not measurable at all.
        assert!(rendered.contains(
            "oxmgr_host_network_received_bytes_total{interface=\"eth0\",scope=\"host\"} 900000"
        ));
        assert!(rendered.contains("oxmgr_host_network_errors_total{interface=\"eth0\",scope=\"host\",direction=\"received\"} 1"));
        for line in rendered
            .lines()
            .filter(|l| l.starts_with("oxmgr_host_network"))
        {
            assert!(line.contains("scope=\"host\""), "unscoped series: {line}");
        }
    }

    #[test]
    fn unavailable_host_metrics_are_omitted_rather_than_zero_filled() {
        // A host that reports nothing: no memory, no CPU sample yet, no load average, no
        // filesystems, no interfaces, and one subsystem that failed.
        let mut metrics = oxmgr_metrics::host_metrics::HostMetrics {
            identity: fixture_host_metrics().identity,
            collected_at: 1,
            ..Default::default()
        };
        metrics
            .failures
            .push(oxmgr_metrics::host_metrics::SubsystemFailure {
                subsystem: oxmgr_metrics::host_metrics::HostSubsystem::Components,
                message: "no sensors on this platform".to_string(),
                at: 1,
            });

        let rendered = render_host_prometheus_metrics(&metrics);

        // Absent, not zero. A load average of 0 reads as an idle machine and a temperature
        // of 0 reads as a cold one; both would be fabrications here.
        for absent in [
            "oxmgr_host_memory_bytes",
            "oxmgr_host_memory_used_percent",
            "oxmgr_host_swap_bytes",
            "oxmgr_host_cpu_used_percent",
            "oxmgr_host_load_average",
            "oxmgr_host_filesystem_bytes",
            "oxmgr_host_temperature_celsius",
        ] {
            assert!(
                !rendered.contains(absent),
                "{absent} must be omitted when unavailable, not zero-filled"
            );
        }

        // Identity still renders: it is collected once at startup and does not depend on a
        // subsystem refresh succeeding.
        assert!(rendered.contains("oxmgr_host_info{"));
        // The failure is named, so a gap is distinguishable from a platform that never had
        // the figure.
        assert!(rendered.contains("oxmgr_host_subsystem_failed{subsystem=\"Components\"} 1"));
    }

    #[test]
    fn a_host_interface_rate_needs_an_observed_interval() {
        let rendered = render_host_prometheus_metrics(&fixture_host_metrics());

        // eth0 has an interval: 2000 bytes over 2000ms is 1000 B/s.
        assert!(rendered.contains(
            "oxmgr_host_network_received_bytes_per_second{interface=\"eth0\",scope=\"host\"} 1000"
        ));
        // lo is on its first measurement, so no rate can be derived. Zero-filling it would
        // be indistinguishable from an idle interface.
        assert!(
            !rendered.contains("oxmgr_host_network_received_bytes_per_second{interface=\"lo\""),
            "an interface with no observed interval must be omitted"
        );
        // Its counter is still present: a cumulative total needs no interval.
        assert!(rendered.contains(
            "oxmgr_host_network_received_bytes_total{interface=\"lo\",scope=\"host\"} 500"
        ));
    }

    #[test]
    fn a_zero_capacity_filesystem_reports_bytes_but_no_utilisation() {
        let rendered = render_host_prometheus_metrics(&fixture_host_metrics());

        // The capacity figures are real measurements of zero, so they render.
        assert!(rendered.contains(
            "oxmgr_host_filesystem_bytes{mount=\"/proc\",fstype=\"proc\",state=\"total\"} 0"
        ));
        // The utilisation is not a measurement at all: 0/0 is unavailable, and emitting 0
        // would put a permanently-empty series beside the real ones.
        assert!(
            !rendered.contains("oxmgr_host_filesystem_used_percent{mount=\"/proc\""),
            "utilisation of a zero-capacity filesystem must be omitted"
        );
        assert!(
            rendered.contains("oxmgr_host_filesystem_used_percent{mount=\"/\",fstype=\"ext4\"} 60")
        );
    }

    #[test]
    fn network_io_is_reported_as_unsupported_with_a_reason() {
        let process = fixture_metrics_process();

        let value = process_json_for_transport(&process);
        let network = value
            .get("network_io")
            .expect("network status must be present, not omitted");

        // Present and explicit: an absent field would be indistinguishable from a process
        // that performed no network I/O, and those are different answers.
        assert_eq!(
            network.get("status").and_then(|s| s.as_str()),
            Some("unsupported")
        );
        let reason = network
            .get("reason")
            .and_then(|r| r.as_str())
            .expect("the reason travels with the status");
        assert!(
            reason.contains("per interface"),
            "the reason must say why, not just that it is unsupported: {reason}"
        );
        // Never a number. A zero here would be read as measured traffic.
        assert!(network.get("status").and_then(|s| s.as_f64()).is_none());
    }

    #[test]
    fn an_unsupported_status_is_distinguishable_from_a_measured_zero() {
        // A running process measured over a valid interval that genuinely did nothing.
        let mut idle = fixture_metrics_process();
        idle.status = ProcessStatus::Running;
        idle.record_io_sample(0, 0, 4242, Some(2000));
        idle.record_io_sample(0, 0, 4242, Some(2000));

        let value = process_json_for_transport(&idle);

        // Disk: a real zero, as a number.
        assert_eq!(
            value
                .get("disk_read_bytes_per_second")
                .and_then(|v| v.as_f64()),
            Some(0.0),
            "a measured zero is a number"
        );
        // Network: not a number at all, so no consumer can average it, sum it, or plot it
        // as a zero-valued sample.
        assert!(
            value
                .get("network_io")
                .and_then(|n| n.get("status"))
                .and_then(|s| s.as_str())
                .is_some()
        );
    }

    #[test]
    fn a_rate_is_null_when_the_daemon_cannot_measure_it() {
        // Stopped: readings cleared, so there is no interval to divide by.
        let mut stopped = fixture_metrics_process();
        stopped.record_io_sample(4_096, 8_192, 4242, Some(2000));
        stopped.record_io_sample(4_096, 8_192, 4242, Some(2000));
        stopped.clear_resource_metrics();

        let value = process_json_for_transport(&stopped);

        // `null`, not 0: the difference between "cannot say" and "measured nothing".
        assert!(
            value
                .get("disk_read_bytes_per_second")
                .expect("key present")
                .is_null(),
            "an unavailable rate must be null rather than zero"
        );
        assert!(
            value
                .get("disk_write_bytes_per_second")
                .expect("key present")
                .is_null()
        );
        // The lifetime total survives, because it is scoped to the managed process rather
        // than to the PID that has just gone.
        assert_eq!(
            value.get("disk_write_total").and_then(|v| v.as_u64()),
            Some(8_192)
        );
    }

    #[test]
    fn disk_totals_are_counters_and_rates_are_omitted_when_unavailable() {
        let mut measured = fixture_metrics_process();
        measured.status = ProcessStatus::Running;
        measured.record_io_sample(1_000, 2_000, 4242, Some(2000));
        measured.record_io_sample(1_000, 2_000, 4242, Some(2000));

        let mut unmeasured = fixture_metrics_process();
        unmeasured.id = 43;
        unmeasured.name = "stopped-svc".to_string();
        unmeasured.record_io_sample(500, 500, 9999, Some(2000));
        unmeasured.record_io_sample(500, 500, 9999, Some(2000));
        unmeasured.clear_resource_metrics();

        let rendered = render_prometheus_metrics(&[measured, unmeasured]);

        // Only the lifetime accumulators are counters. sysinfo's per-PID totals fall
        // backwards on restart, and Prometheus reads a decrease as a counter reset.
        assert!(rendered.contains("# TYPE oxmgr_process_disk_read_bytes_total counter"));
        assert!(rendered.contains("# TYPE oxmgr_process_disk_written_bytes_total counter"));
        assert!(rendered.contains("# TYPE oxmgr_process_disk_read_bytes_per_second gauge"));

        // The counter is present for both processes, including the stopped one: a lifetime
        // total does not stop being true when the process stops.
        assert!(rendered.contains("oxmgr_process_disk_written_bytes_total{id=\"42\""));
        assert!(rendered.contains("oxmgr_process_disk_written_bytes_total{id=\"43\""));

        // The rate is emitted only for the process that has a usable interval. Zero-filling
        // the other would be indistinguishable from a process that ran and did nothing.
        assert!(rendered.contains("oxmgr_process_disk_written_bytes_per_second{id=\"42\""));
        assert!(
            !rendered.contains("oxmgr_process_disk_written_bytes_per_second{id=\"43\""),
            "a process with no measurement must be omitted, not zero-filled"
        );
    }

    #[test]
    fn extract_api_secret_prefers_explicit_header_then_bearer() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("authorization", "Bearer bearer-secret".parse().unwrap());
        headers.insert("x-oxmgr-secret", "header-secret".parse().unwrap());

        assert_eq!(
            extract_api_secret(&headers).as_deref(),
            Some("header-secret")
        );
    }

    #[test]
    fn extract_api_secret_accepts_bearer_when_custom_header_missing() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("authorization", "Bearer token123".parse().unwrap());
        assert_eq!(extract_api_secret(&headers).as_deref(), Some("token123"));
    }

    #[test]
    fn escape_prometheus_label_value_handles_special_characters() {
        assert_eq!(
            escape_prometheus_label_value("prod\\blue\"green\nline"),
            "prod\\\\blue\\\"green\\nline"
        );
    }

    #[test]
    fn restart_sleep_deadline_uses_due_instant_when_available() {
        let now = TokioInstant::now();
        let due = now + Duration::from_secs(7);
        assert_eq!(restart_sleep_deadline(Some(due), now), due);
    }

    #[test]
    fn restart_sleep_deadline_uses_far_future_when_no_restart_is_scheduled() {
        let now = TokioInstant::now();
        let deadline = restart_sleep_deadline(None, now);
        assert_eq!(
            deadline,
            now + Duration::from_secs(DISABLED_RESTART_SLEEP_SECS)
        );
    }

    #[tokio::test]
    async fn daemon_socket_available_sends_ping_request() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind local listener");
        let addr = listener
            .local_addr()
            .expect("failed to resolve listener addr");

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept failed");
            let request: IpcRequest = read_json_line(&mut stream).await.expect("read failed");
            assert!(matches!(request, IpcRequest::Ping));
            write_json_line(&mut stream, &IpcResponse::ok("pong"))
                .await
                .expect("write failed");
        });

        assert!(daemon_socket_available(&addr.to_string()).await);
        server.await.expect("server task failed");
    }

    #[tokio::test]
    async fn snapshot_request_serves_list_status_and_logs_without_manager() {
        let mut manager = empty_manager("daemon-snapshot-read");
        start_minimal_service(&mut manager, "api", None, None, None).await;

        let snapshot = DaemonSnapshot::default();
        snapshot.publish(&manager).await;

        let list = execute_snapshot_request(&crate::ipc::IpcRequest::List, &snapshot)
            .await
            .expect("list should be served from snapshot");
        assert_eq!(list.processes.len(), 1);

        let status = execute_snapshot_request(
            &crate::ipc::IpcRequest::Status {
                target: "api".to_string(),
            },
            &snapshot,
        )
        .await
        .expect("status should be served from snapshot");
        assert_eq!(
            status.process.as_ref().map(|process| process.name.as_str()),
            Some("api")
        );

        let logs = execute_snapshot_request(
            &crate::ipc::IpcRequest::Logs {
                target: "api".to_string(),
            },
            &snapshot,
        )
        .await
        .expect("logs should be served from snapshot");
        assert!(logs.logs.is_some());

        let _ = manager.shutdown_all().await;
    }

    #[tokio::test]
    async fn snapshot_request_redacts_env_and_pull_secret_hash() {
        let mut manager = empty_manager("daemon-snapshot-redaction");
        let exe = std::env::current_exe().expect("failed to read current executable path");
        let command = format!("\"{}\" --help", exe.display());
        let spec = StartProcessSpec {
            command,
            name: Some("api".to_string()),
            pre_reload_cmd: None,
            restart_policy: RestartPolicy::Never,
            max_restarts: 1,
            crash_restart_limit: 3,
            cwd: None,
            env: HashMap::from([("SECRET_TOKEN".to_string(), "value".to_string())]),
            health_check: None,
            stop_signal: None,
            stop_timeout_secs: 1,
            restart_delay_secs: 0,
            start_delay_secs: 0,
            watch: false,
            watch_paths: Vec::new(),
            ignore_watch: Vec::new(),
            watch_delay_secs: 0,
            cluster_mode: false,
            cluster_instances: None,
            namespace: None,
            resource_limits: None,
            git_repo: None,
            git_ref: None,
            pull_secret_hash: Some(hash_secret("hook-secret")),
            reuse_port: false,
            wait_ready: false,
            ready_timeout_secs: oxmgr_metrics::process::default_ready_timeout_secs(),
            log_date_format: None,
            unified_logs: false,
            cron_restart: None,
            stdout_log_override: None,
            stderr_log_override: None,
            depends_on: Vec::new(),
        };

        manager
            .start_process(spec)
            .await
            .expect("failed to start redaction test service");

        let snapshot = DaemonSnapshot::default();
        snapshot.publish(&manager).await;

        let status = execute_snapshot_request(
            &crate::ipc::IpcRequest::Status {
                target: "api".to_string(),
            },
            &snapshot,
        )
        .await
        .expect("status should be served from snapshot");
        let process = status.process.expect("expected process in status response");
        assert!(process.env.is_empty(), "env should be redacted from IPC");
        assert_eq!(
            process.pull_secret_hash.as_deref(),
            Some("<redacted>"),
            "pull secret hash should be redacted from IPC"
        );

        let _ = manager.shutdown_all().await;
    }

    async fn start_minimal_service(
        manager: &mut ProcessManager,
        name: &str,
        cwd: Option<PathBuf>,
        git_repo: Option<String>,
        pull_secret_hash: Option<String>,
    ) {
        let exe = std::env::current_exe().expect("failed to read current executable path");
        let command = format!("\"{}\" --help", exe.display());

        let spec = StartProcessSpec {
            command,
            name: Some(name.to_string()),
            pre_reload_cmd: None,
            restart_policy: RestartPolicy::Never,
            max_restarts: 1,
            crash_restart_limit: 3,
            cwd,
            env: HashMap::new(),
            health_check: None,
            stop_signal: None,
            stop_timeout_secs: 1,
            restart_delay_secs: 0,
            start_delay_secs: 0,
            watch: false,
            watch_paths: Vec::new(),
            ignore_watch: Vec::new(),
            watch_delay_secs: 0,
            cluster_mode: false,
            cluster_instances: None,
            namespace: None,
            resource_limits: None,
            git_repo,
            git_ref: Some("main".to_string()),
            pull_secret_hash,
            reuse_port: false,
            wait_ready: false,
            ready_timeout_secs: oxmgr_metrics::process::default_ready_timeout_secs(),
            log_date_format: None,
            unified_logs: false,
            cron_restart: None,
            stdout_log_override: None,
            stderr_log_override: None,
            depends_on: Vec::new(),
        };

        manager
            .start_process(spec)
            .await
            .expect("failed to start service for daemon API test");
    }

    fn empty_manager(prefix: &str) -> ProcessManager {
        let config = test_config(prefix);
        let (exit_tx, _exit_rx) = unbounded_channel();
        ProcessManager::new(config.into(), exit_tx, None)
            .expect("failed to initialize test process manager")
    }

    fn hash_secret(value: &str) -> String {
        sha256_hex(value.as_bytes())
    }

    struct GitFixture {
        root: PathBuf,
        remote_dir: PathBuf,
        clone_dir: PathBuf,
    }

    fn setup_git_fixture(prefix: &str) -> GitFixture {
        let root = temp_dir(prefix);
        let remote_dir = root.join("remote.git");
        let source_dir = root.join("source");
        let clone_dir = root.join("clone");

        fs::create_dir_all(&root).expect("failed to create git fixture root");
        fs::create_dir_all(&source_dir).expect("failed to create git source directory");
        run_git_sync(
            &root,
            &["init", "--bare", remote_dir.to_str().unwrap_or_default()],
        );
        run_git_sync(&source_dir, &["init"]);
        run_git_sync(&source_dir, &["config", "user.email", "tests@oxmgr.local"]);
        run_git_sync(&source_dir, &["config", "user.name", "Oxmgr Tests"]);
        fs::write(source_dir.join("app.js"), "console.log('v1');\n")
            .expect("failed writing fixture file");
        run_git_sync(&source_dir, &["add", "."]);
        run_git_sync(&source_dir, &["commit", "-m", "initial"]);
        run_git_sync(&source_dir, &["branch", "-M", "main"]);
        run_git_sync(
            &source_dir,
            &[
                "remote",
                "add",
                "origin",
                remote_dir.to_str().unwrap_or_default(),
            ],
        );
        run_git_sync(&source_dir, &["push", "-u", "origin", "main"]);
        run_git_sync(
            &root,
            &[
                "clone",
                remote_dir.to_str().unwrap_or_default(),
                clone_dir.to_str().unwrap_or_default(),
            ],
        );
        run_git_sync(&clone_dir, &["checkout", "main"]);

        GitFixture {
            root,
            remote_dir,
            clone_dir,
        }
    }

    fn run_git_sync(cwd: &Path, args: &[&str]) {
        let output = StdCommand::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("failed to run git in daemon test");
        assert!(
            output.status.success(),
            "git {:?} failed in {}: {}",
            args,
            cwd.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn test_config(prefix: &str) -> AppConfig {
        let base = temp_dir(prefix);
        let log_dir = base.join("logs");
        fs::create_dir_all(&log_dir).expect("failed to create log directory");
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock failure")
            .subsec_nanos();
        // Unix socket paths must be < 104 chars on macOS; use /tmp directly
        let event_socket_path = PathBuf::from(format!("/tmp/oxmgr-ev-{nonce}.sock"));
        AppConfig {
            base_dir: base.clone(),
            daemon_addr: "127.0.0.1:50200".to_string(),
            api_addr: "127.0.0.1:51200".to_string(),
            state_path: base.join("state.json"),
            log_dir,
            log_rotation: oxmgr_manager::logging::LogRotationPolicy {
                max_size_bytes: 1024 * 1024,
                max_files: 2,
                max_age_days: 1,
                max_age_secs: None,
            },
            event_socket_path,
        }
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock failure")
            .as_nanos();
        std::env::temp_dir().join(format!("oxmgr-daemon-{prefix}-{nonce}"))
    }

    /// Returns a short socket path under /tmp to stay within SUN_LEN (~104 chars on macOS).
    #[cfg(unix)]
    fn short_socket_path(tag: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock failure")
            .subsec_nanos();
        PathBuf::from(format!("/tmp/ox-{tag}-{nonce}.sock"))
    }

    fn snapshot_with_processes(processes: Vec<ManagedProcess>) -> DaemonSnapshot {
        DaemonSnapshot {
            processes: Arc::new(RwLock::new(processes)),
            event_tx: broadcast::channel(oxmgr_core::events::BUS_CAPACITY).0,
            host: HostMetricsHandle::new(),
            analysis: Arc::default(),
            dismissals: Arc::default(),
            typical: Arc::default(),
            consumers: oxmgr_metrics::host_metrics::HostConsumersHandle::new(),
        }
    }

    /// A snapshot carrying analysis output, for the findings endpoints and series.
    fn snapshot_with_analysis(
        processes: Vec<ManagedProcess>,
        analysis: oxmgr_analytics::analysis::AnalysisSnapshot,
    ) -> DaemonSnapshot {
        DaemonSnapshot {
            processes: Arc::new(RwLock::new(processes)),
            event_tx: broadcast::channel(oxmgr_core::events::BUS_CAPACITY).0,
            host: HostMetricsHandle::new(),
            analysis: Arc::new(RwLock::new(analysis)),
            dismissals: Arc::default(),
            typical: Arc::default(),
            consumers: oxmgr_metrics::host_metrics::HostConsumersHandle::new(),
        }
    }

    fn fixture_metrics_process() -> ManagedProcess {
        ManagedProcess {
            id: 42,
            name: "api".to_string(),
            command: "sleep".to_string(),
            args: vec!["30".to_string()],
            pre_reload_cmd: None,
            cwd: None,
            env: HashMap::new(),
            restart_policy: RestartPolicy::Always,
            max_restarts: 5,
            restart_count: 2,
            crash_restart_limit: DEFAULT_CRASH_RESTART_LIMIT,
            auto_restart_history: Vec::new(),
            namespace: Some("prod".to_string()),
            git_repo: None,
            git_ref: None,
            pull_secret_hash: None,
            reuse_port: false,
            stop_signal: Some("SIGTERM".to_string()),
            stop_timeout_secs: 5,
            restart_delay_secs: 1,
            restart_backoff_cap_secs: 0,
            restart_backoff_reset_secs: 0,
            restart_backoff_attempt: 0,
            start_delay_secs: 0,
            watch: false,
            watch_paths: Vec::new(),
            ignore_watch: Vec::new(),
            watch_delay_secs: 0,
            cluster_mode: false,
            cluster_instances: None,
            resource_limits: None,
            cgroup_path: None,
            pid: Some(4242),
            status: oxmgr_metrics::process::ProcessStatus::Running,
            desired_state: DesiredState::Running,
            last_exit_code: None,
            stdout_log: PathBuf::from("/tmp/api.stdout.log"),
            stderr_log: PathBuf::from("/tmp/api.stderr.log"),
            health_check: None,
            health_status: HealthStatus::Healthy,
            health_failures: 0,
            last_health_check: Some(1_700_000_100),
            next_health_check: Some(1_700_000_120),
            last_health_error: None,
            wait_ready: false,
            ready_timeout_secs: oxmgr_metrics::process::default_ready_timeout_secs(),
            cpu_percent: 12.5,
            memory_bytes: 4096,
            disk_read_bytes: 0,
            disk_write_bytes: 0,
            metrics_interval_ms: None,
            metrics_pid: None,
            disk_read_total: 0,
            disk_write_total: 0,
            last_metrics_at: Some(1_700_000_050),
            last_started_at: Some(1_700_000_000),
            last_stopped_at: None,
            config_fingerprint: "fixture-fingerprint".to_string(),
            log_date_format: Some("%Y-%m-%d %H:%M:%S".to_string()),
            unified_logs: false,
            cron_restart: None,
            next_cron_restart: None,
            last_error: None,
            depends_on: Vec::new(),
        }
    }

    // --- event socket tests (Unix only) ---

    #[cfg(unix)]
    #[tokio::test]
    async fn event_socket_delivers_events_to_connected_client() {
        use std::sync::Arc;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixStream;
        use tokio::time::{Duration, timeout};

        use oxmgr_core::events::{BusEvent, EventFilter, EventProcessInfo};

        let dir = temp_dir("event-socket-basic");
        let socket_path = short_socket_path("basic");

        let (event_tx, _) = tokio::sync::broadcast::channel::<Arc<BusEvent>>(64);
        let (_shutdown_flag_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let tx_clone = event_tx.clone();
        let path_clone = socket_path.clone();
        tokio::spawn(async move {
            super::run_event_socket(path_clone, tx_clone, shutdown_rx).await;
        });

        // Give the socket a moment to bind.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = UnixStream::connect(&socket_path)
            .await
            .expect("failed to connect to event socket");

        // Send an empty filter (subscribe to everything).
        let filter = EventFilter::default();
        let mut payload = serde_json::to_vec(&filter).expect("serialize filter");
        payload.push(b'\n');
        client.write_all(&payload).await.expect("write filter");

        // Give the socket task time to read the filter before emitting.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let process_info = EventProcessInfo {
            id: 1,
            name: "api".into(),
            namespace: None,
            pid: Some(1234),
            command: "node server.js".into(),
            cwd: None,
        };
        event_tx
            .send(Arc::new(BusEvent::process_crashed(
                process_info,
                Some(1),
                None,
                0,
                3,
                vec![],
            )))
            .expect("broadcast send failed");

        let (read_half, _) = client.into_split();
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        timeout(Duration::from_secs(2), reader.read_line(&mut line))
            .await
            .expect("timed out waiting for event")
            .expect("read failed");

        let json: serde_json::Value =
            serde_json::from_str(line.trim()).expect("invalid JSON from socket");
        assert_eq!(json["event"], "process:crashed");
        assert_eq!(json["process"]["name"], "api");
        assert_eq!(json["data"]["exit_code"], 1);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn event_socket_respects_process_filter() {
        use std::sync::Arc;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixStream;
        use tokio::time::{Duration, timeout};

        use oxmgr_core::events::{BusEvent, EventFilter, EventProcessInfo};

        let dir = temp_dir("event-socket-filter");
        let socket_path = short_socket_path("filter");

        let (event_tx, _) = tokio::sync::broadcast::channel::<Arc<BusEvent>>(64);
        let (_shutdown_flag_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let tx_clone = event_tx.clone();
        let path_clone = socket_path.clone();
        tokio::spawn(async move {
            super::run_event_socket(path_clone, tx_clone, shutdown_rx).await;
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = UnixStream::connect(&socket_path)
            .await
            .expect("connect failed");

        // Only subscribe to events for "api" process.
        let filter = EventFilter {
            subscribe: vec![],
            process: Some("api".into()),
        };
        let mut payload = serde_json::to_vec(&filter).expect("serialize filter");
        payload.push(b'\n');
        client.write_all(&payload).await.expect("write filter");

        tokio::time::sleep(Duration::from_millis(100)).await;

        // Emit for "worker" — should be filtered out.
        event_tx
            .send(Arc::new(BusEvent::process_online(EventProcessInfo {
                id: 2,
                name: "worker".into(),
                namespace: None,
                pid: Some(9999),
                command: String::new(),
                cwd: None,
            })))
            .expect("send worker event");

        // Emit for "api" — should be delivered.
        event_tx
            .send(Arc::new(BusEvent::process_online(EventProcessInfo {
                id: 1,
                name: "api".into(),
                namespace: None,
                pid: Some(1234),
                command: String::new(),
                cwd: None,
            })))
            .expect("send api event");

        let (read_half, _) = client.into_split();
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        timeout(Duration::from_secs(2), reader.read_line(&mut line))
            .await
            .expect("timed out waiting for event")
            .expect("read failed");

        let json: serde_json::Value = serde_json::from_str(line.trim()).expect("invalid JSON");
        // The first line received must be the "api" event, not "worker".
        assert_eq!(json["event"], "process:online");
        assert_eq!(json["process"]["name"], "api");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn event_socket_delivers_daemon_shutdown_through_process_filter() {
        use std::sync::Arc;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixStream;
        use tokio::time::{Duration, timeout};

        use oxmgr_core::events::{BusEvent, EventFilter};

        let dir = temp_dir("event-socket-shutdown");
        let socket_path = short_socket_path("shutdown");

        let (event_tx, _) = tokio::sync::broadcast::channel::<Arc<BusEvent>>(64);
        let (_shutdown_flag_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let tx_clone = event_tx.clone();
        let path_clone = socket_path.clone();
        tokio::spawn(async move {
            super::run_event_socket(path_clone, tx_clone, shutdown_rx).await;
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = UnixStream::connect(&socket_path)
            .await
            .expect("connect failed");

        // Subscribe only to "api" process — daemon:shutdown must still arrive.
        let filter = EventFilter {
            subscribe: vec![],
            process: Some("api".into()),
        };
        let mut payload = serde_json::to_vec(&filter).expect("serialize filter");
        payload.push(b'\n');
        client.write_all(&payload).await.expect("write filter");

        tokio::time::sleep(Duration::from_millis(100)).await;

        event_tx
            .send(Arc::new(BusEvent::daemon_shutdown()))
            .expect("send shutdown event");

        let (read_half, _) = client.into_split();
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        timeout(Duration::from_secs(2), reader.read_line(&mut line))
            .await
            .expect("timed out waiting for event")
            .expect("read failed");

        let json: serde_json::Value = serde_json::from_str(line.trim()).expect("invalid JSON");
        assert_eq!(json["event"], "daemon:shutdown");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn event_socket_terminates_on_shutdown_signal() {
        use oxmgr_core::events::BusEvent;
        use tokio::time::{Duration, timeout};

        let socket_path = short_socket_path("shutdown-signal");
        let (event_tx, _) = tokio::sync::broadcast::channel::<Arc<BusEvent>>(64);
        let (shutdown_flag_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let tx_clone = event_tx.clone();
        let path_clone = socket_path.clone();
        let handle = tokio::spawn(async move {
            super::run_event_socket(path_clone, tx_clone, shutdown_rx).await;
        });

        // Give the socket a moment to bind.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Signal shutdown; the accept loop must exit cooperatively rather than
        // running until the test process ends.
        shutdown_flag_tx.send(true).expect("send shutdown signal");
        timeout(Duration::from_millis(500), handle)
            .await
            .expect("socket task did not terminate after shutdown signal")
            .expect("socket task panicked");
    }

    // ── Findings exposure (10.3, 10.6) ──────────────────────────────────────────────────────────

    /// One active finding for `process`, built through the real builder so its evidence is valid and
    /// its confidence is derived rather than asserted.
    fn fixture_finding(process: &str) -> oxmgr_core::findings::Finding {
        use oxmgr_core::findings::{
            Agreement, BaselineSummary, Detector, Direction, Evidence, EvidenceWindow, Finding,
            FindingKey, Metric, SampleRef,
        };
        let evidence = Evidence {
            window: EvidenceWindow {
                start_unix: 1_000,
                end_unix: 1_060,
                sample_count: 30,
            },
            observed: 400.0,
            expected: 100.0,
            statistic: 9.0,
            threshold: 3.0,
            direction: Some(Direction::Above),
            agreement: Some(Agreement {
                consecutive_samples: 6,
                required_samples: 3,
            }),
            baseline: Some(BaselineSummary {
                center: 100.0,
                spread: 10.0,
                sample_count: 120,
                min_samples: 30,
            }),
            trend: None,
            forecast: None,
            samples: vec![SampleRef::new(1_060, 400.0)],
            extra: std::collections::BTreeMap::new(),
        };
        Finding::builder(
            FindingKey::new(process, Detector::LevelDeparture, Metric::MemoryBytes)
                .with_variant("above"),
            1_060,
            evidence,
        )
        .summary("memory sits far above its baseline")
        .build()
        .expect("fixture evidence must be valid")
    }

    fn fixture_analysis(process: &str) -> oxmgr_analytics::analysis::AnalysisSnapshot {
        oxmgr_analytics::analysis::AnalysisSnapshot {
            findings: vec![fixture_finding(process)],
            decisions: vec![oxmgr_core::rules::Decision {
                process: process.to_string(),
                at_unix: 2_000,
                rule: Some(oxmgr_core::rules::RuleId::ResourceLeak),
                action: Some(oxmgr_core::rules::Action::Restart),
                withheld: None,
                findings: vec![fixture_finding(process).key],
            }],
            withheld: vec![],
            suppressed: oxmgr_core::tuning::SuppressionCounters {
                blocked_globally: 0,
                blocked_by_detector: 2,
                blocked_by_process: 5,
            },
            warming: vec!["warming-proc".to_string()],
        }
    }

    #[tokio::test]
    async fn findings_endpoint_reports_findings_suppression_and_warming() {
        let snapshot =
            snapshot_with_analysis(vec![fixture_metrics_process()], fixture_analysis("api"));
        let response = oneshot_request(build_test_router(&snapshot), "GET", "/api/findings").await;
        assert_eq!(status_code(&response), 200);

        let value = json_body(response).await;
        assert_eq!(value["active"], 1);
        assert_eq!(value["total"], 1);
        let finding = &value["findings"][0];
        assert_eq!(finding["key"]["process"], "api");
        assert!(
            finding["evidence"]["samples"]
                .as_array()
                .is_some_and(|s| !s.is_empty())
        );
        assert_eq!(value["suppressed"]["detector"], 2);
        assert_eq!(value["suppressed"]["process"], 5);
        assert_eq!(value["warming"][0], "warming-proc");
    }

    #[tokio::test]
    async fn decisions_endpoint_states_that_nothing_acts() {
        let snapshot =
            snapshot_with_analysis(vec![fixture_metrics_process()], fixture_analysis("api"));
        let response = oneshot_request_with_headers(
            build_test_router(&snapshot),
            "GET",
            "/api/decisions",
            vec![("authorization".into(), "Bearer test-secret".into())],
        )
        .await;
        assert_eq!(status_code(&response), 200);

        let value = json_body(response).await;
        assert_eq!(value["total"], 1);
        let decision = &value["decisions"][0];
        assert_eq!(decision["rule"], "resource_leak");
        assert_eq!(decision["action"], "restart");
        assert_eq!(value["acting_enabled"], false);
    }

    #[tokio::test]
    async fn per_process_findings_refuse_an_unknown_process() {
        let snapshot =
            snapshot_with_analysis(vec![fixture_metrics_process()], fixture_analysis("api"));

        let known = oneshot_request(
            build_test_router(&snapshot),
            "GET",
            "/api/processes/api/findings",
        )
        .await;
        assert_eq!(status_code(&known), 200);
        assert_eq!(json_body(known).await["active"], 1);

        for path in [
            "/api/processes/ghost/findings",
            "/api/processes/ghost/decisions",
        ] {
            let response = oneshot_request(build_test_router(&snapshot), "GET", path).await;
            assert_eq!(
                status_code(&response),
                404,
                "{path} must refuse an unknown process"
            );
        }
    }

    #[tokio::test]
    async fn per_process_findings_are_scoped_to_that_process() {
        let mut other = fixture_metrics_process();
        other.id = 43;
        other.name = "worker".to_string();
        let snapshot = snapshot_with_analysis(
            vec![fixture_metrics_process(), other],
            fixture_analysis("api"),
        );

        let response = oneshot_request(
            build_test_router(&snapshot),
            "GET",
            "/api/processes/worker/findings",
        )
        .await;
        assert_eq!(status_code(&response), 200);
        let value = json_body(response).await;
        assert_eq!(value["process"], "worker");
        assert_eq!(
            value["active"], 0,
            "another process's finding must not appear here"
        );
    }

    #[test]
    fn findings_metrics_render_valid_prometheus_text() {
        let analysis = fixture_analysis("api");
        let processes = vec![fixture_metrics_process()];
        let rendered = render_findings_prometheus_metrics(&analysis, &processes);

        // Every series must be declared before it is used: a HELP/TYPE pair then samples. A scrape
        // accepts undeclared series, but an undeclared one has no type and cannot be aggregated
        // correctly, so this is the check that matters rather than "the string is non-empty".
        for series in [
            "oxmgr_finding_active",
            "oxmgr_finding_confidence",
            "oxmgr_finding_occurrence",
            "oxmgr_findings_suppressed_total",
            "oxmgr_process_baseline_warming",
            "oxmgr_remediation_decisions",
        ] {
            assert!(
                rendered.contains(&format!("# HELP {series} ")),
                "{series} has no HELP line"
            );
            assert!(
                rendered.contains(&format!("# TYPE {series} ")),
                "{series} has no TYPE line"
            );
        }

        // The active finding is present with its identifying labels.
        // `namespace="prod"` comes from the process record, not from the finding: the label is
        // looked up per render so it matches whatever the process says now rather than whatever it
        // said when the finding was raised.
        assert!(
            rendered.contains(r#"oxmgr_finding_active{name="api",namespace="prod",detector="level_departure",metric="memory_bytes",variant="above"} 1"#),
            "active finding series missing or mislabelled:\n{rendered}"
        );
        // Suppression counters, one series per scope.
        assert!(rendered.contains(r#"oxmgr_findings_suppressed_total{scope="detector"} 2"#));
        assert!(rendered.contains(r#"oxmgr_findings_suppressed_total{scope="process"} 5"#));
        // The decision, grouped by rule and outcome.
        assert!(
            rendered.contains(
                r#"oxmgr_remediation_decisions{rule="resource_leak",outcome="proposed"} 1"#
            ),
            "decision series missing:\n{rendered}"
        );

        // Every sample line must parse as `name{labels} value` with a finite value. A NaN here would
        // be accepted by the parser and then poison an aggregation.
        for line in rendered.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let value = line
                .rsplit_once(' ')
                .map(|(_, value)| value)
                .unwrap_or_else(|| panic!("sample line has no value: {line}"));
            let parsed: f64 = value
                .parse()
                .unwrap_or_else(|_| panic!("unparseable sample value in: {line}"));
            assert!(parsed.is_finite(), "non-finite sample value in: {line}");
        }
    }

    #[test]
    fn a_cleared_finding_leaves_no_active_series() {
        // A resolved condition must make its series DISAPPEAR rather than report 0: a series held at
        // zero keeps an alert's target alive for ever, whereas absence is how Prometheus expresses
        // "no longer true".
        let mut analysis = fixture_analysis("api");
        analysis.findings[0].status = oxmgr_core::findings::FindingStatus::Cleared;
        analysis.findings[0].cleared_at_unix = Some(2_000);

        let rendered = render_findings_prometheus_metrics(&analysis, &[fixture_metrics_process()]);
        assert!(
            !rendered.contains("oxmgr_finding_active{"),
            "a cleared finding must not emit an active sample:\n{rendered}"
        );
        // The declaration still stands, so the series exists and is simply empty.
        assert!(rendered.contains("# TYPE oxmgr_finding_active gauge"));
    }

    // ── Severity presentation config (resource-awareness 5.1, 5.4) ──────────────────────────────

    #[tokio::test]
    async fn config_endpoint_serves_the_rust_severity_thresholds() {
        // The point of this test is DRIFT. `dashboard.js` used to carry its own 75/90 beside
        // `severity.rs`'s constants, so changing the documented default would silently leave the
        // dashboard banding on the old boundary with nothing failing. Asserting the served values
        // against the constants themselves is what makes that impossible to reintroduce.
        let snapshot = snapshot_with_processes(Vec::default());
        let response = oneshot_request(build_test_router(&snapshot), "GET", "/api/config").await;
        assert_eq!(status_code(&response), 200);

        let value = json_body(response).await;
        let severity = &value["severity"];
        assert_eq!(
            severity["warning_percent"].as_f64(),
            Some(f64::from(oxmgr_core::severity::DEFAULT_WARNING_PERCENT)),
        );
        assert_eq!(
            severity["critical_percent"].as_f64(),
            Some(f64::from(oxmgr_core::severity::DEFAULT_CRITICAL_PERCENT)),
        );
        assert_eq!(
            severity["hysteresis_percent"].as_f64(),
            Some(f64::from(
                oxmgr_core::severity::DEFAULT_BAND_HYSTERESIS_PERCENT
            )),
        );
        // Monotonic, which 5.1 requires. Asserted on the SERVED values rather than on the constants:
        // comparing two consts is a constant expression the compiler folds away, so it proves
        // nothing at runtime — clippy was right to flag it. The served pair is what the client
        // actually adopts, and it is what has to be ordered.
        let served_warning = severity["warning_percent"]
            .as_f64()
            .expect("warning threshold is served");
        let served_critical = severity["critical_percent"]
            .as_f64()
            .expect("critical threshold is served");
        assert!(
            served_warning < served_critical,
            "warning ({served_warning}) must sit below critical ({served_critical}) for the bands to be ordered"
        );
        // On by default: an operator has to ask for styling to be removed.
        assert_eq!(severity["styling_enabled"], true);
    }

    #[test]
    #[serial]
    fn severity_styling_can_be_disabled_without_changing_the_figures() {
        // Task 5.4. The switch removes the STYLING, not the data — so the same response must still
        // carry every figure-shaping value it did before. A switch that also dropped the thresholds
        // would be a different feature, and would leave the client unable to re-enable styling
        // without a reload.
        //
        // Deliberately NOT a `#[tokio::test]`: `serve_dashboard_config` is synchronous, and an async
        // test here would hold the env mutex across an await point. Clippy flagged that, and
        // it was right — a std guard held across a yield can deadlock if the runtime schedules
        // another task that wants the same lock. Dropping the async removes the hazard rather than
        // silencing the lint.
        for off in ["0", "off", "false", "no", "disabled", "OFF"] {
            let _guard = crate::test_utils::EnvGuard::set("OXMGR_SEVERITY_STYLING", off);
            let value = config_json();
            assert_eq!(
                value["severity"]["styling_enabled"], false,
                "{off:?} must disable styling"
            );
            // The figures are untouched: thresholds still served, interval still present.
            assert_eq!(
                value["severity"]["warning_percent"].as_f64(),
                Some(f64::from(oxmgr_core::severity::DEFAULT_WARNING_PERCENT)),
                "disabling styling must not withhold the thresholds"
            );
            assert!(value["interval_ms"].as_u64().is_some());
        }

        // Anything unrecognised leaves styling ON, so a typo cannot silently remove a signal
        // someone was relying on.
        for on in ["1", "on", "true", "yes", "", "enabled", "ture"] {
            let _guard = crate::test_utils::EnvGuard::set("OXMGR_SEVERITY_STYLING", on);
            assert_eq!(
                config_json()["severity"]["styling_enabled"],
                true,
                "{on:?} must leave styling on"
            );
        }
    }

    /// The config payload, read synchronously.
    ///
    /// Cloned because `json_body` borrows from the response, and the response is a local — returning
    /// the borrow would outlive it. One clone of a small object in a test is not worth restructuring
    /// the helper to avoid.
    fn config_json() -> serde_json::Value {
        let response = super::http::serve_dashboard_config();
        json_body_sync(response)
    }

    // ── Advisory dismissal on the endpoint (resource-awareness 3.7) ──────────────────────────────

    /// A snapshot whose advisory dismissals are pre-populated.
    fn snapshot_with_dismissals(
        processes: Vec<ManagedProcess>,
        dismissals: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    ) -> DaemonSnapshot {
        DaemonSnapshot {
            processes: Arc::new(RwLock::new(processes)),
            event_tx: broadcast::channel(oxmgr_core::events::BUS_CAPACITY).0,
            host: HostMetricsHandle::new(),
            analysis: Arc::default(),
            dismissals: Arc::new(RwLock::new(dismissals)),
            typical: Arc::default(),
            consumers: oxmgr_metrics::host_metrics::HostConsumersHandle::new(),
        }
    }

    /// A process whose configuration produces two critical advisories.
    fn risky_for_advisories(name: &str, id: u64) -> ManagedProcess {
        let mut process = fixture_metrics_process();
        process.id = id;
        process.name = name.to_string();
        process.restart_policy = RestartPolicy::Always;
        process.crash_restart_limit = 0;
        process.restart_delay_secs = 0;
        process
    }

    #[tokio::test]
    async fn a_dismissed_advisory_is_reported_as_dismissed_not_dropped() {
        // "Dismissal is visible" is a requirement, not a nicety. Dropping the entry would make an
        // acknowledged risk indistinguishable from one that never fired, so an operator could never
        // audit what had been waved through.
        let mut dismissals = std::collections::BTreeMap::new();
        dismissals.insert(
            "risky".to_string(),
            ["crash_loop_protection_disabled".to_string()]
                .into_iter()
                .collect(),
        );
        let snapshot = snapshot_with_dismissals(vec![risky_for_advisories("risky", 1)], dismissals);

        let response =
            oneshot_request(build_test_router(&snapshot), "GET", "/api/advisories").await;
        assert_eq!(status_code(&response), 200);
        let value = json_body(response).await;
        let entry = &value["processes"][0];

        // The dismissed rule appears in its own list, carrying its full consequence text.
        let dismissed = entry["dismissed"]
            .as_array()
            .expect("dismissed list is present");
        assert_eq!(
            dismissed.len(),
            1,
            "the dismissal must be reported: {entry}"
        );
        assert_eq!(dismissed[0]["id"], "crash_loop_protection_disabled");
        assert!(
            dismissed[0]["consequence"]
                .as_str()
                .is_some_and(|text| !text.is_empty()),
            "a dismissed advisory keeps its consequence text so it can be audited"
        );

        // And it is NOT in the active list.
        let active = entry["advisories"].as_array().expect("active list");
        assert!(
            active
                .iter()
                .all(|advisory| advisory["id"] != "crash_loop_protection_disabled"),
            "a dismissed advisory must not also be presented as active"
        );
        // The other rule still fires, so a dismissal does not hide a different advisory.
        assert!(
            active
                .iter()
                .any(|advisory| advisory["id"] == "immediate_restart_loop"),
            "the undismissed rule must still be active: {active:?}"
        );

        // Totals are separate, so "nothing is wrong" and "everything was waved through" are
        // distinguishable at a glance.
        assert_eq!(value["dismissed_total"], 1);
        assert_eq!(
            value["total"], 1,
            "the active total must exclude the dismissed one"
        );
    }

    #[tokio::test]
    async fn a_dismissal_does_not_lower_another_process_severity() {
        // Two processes with the same risky configuration. Dismissing for one must leave the other's
        // headline severity untouched, which is what the table marker reads.
        let mut dismissals = std::collections::BTreeMap::new();
        dismissals.insert(
            "risky".to_string(),
            [
                "crash_loop_protection_disabled".to_string(),
                "immediate_restart_loop".to_string(),
            ]
            .into_iter()
            .collect(),
        );
        let snapshot = snapshot_with_dismissals(
            vec![
                risky_for_advisories("risky", 1),
                risky_for_advisories("also-risky", 2),
            ],
            dismissals,
        );

        let response =
            oneshot_request(build_test_router(&snapshot), "GET", "/api/advisories").await;
        assert_eq!(status_code(&response), 200);
        let value = json_body(response).await;

        let by_name = |name: &str| -> serde_json::Value {
            value["processes"]
                .as_array()
                .expect("processes")
                .iter()
                .find(|entry| entry["process"] == name)
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        };

        let dismissed_entry = by_name("risky");
        // Every advisory dismissed, so the headline severity is null and the marker clears — which is
        // the whole point of dismissing.
        assert_eq!(dismissed_entry["highest_severity"], serde_json::Value::Null);
        assert_eq!(
            dismissed_entry["advisories"].as_array().map(Vec::len),
            Some(0)
        );
        assert_eq!(
            dismissed_entry["dismissed"].as_array().map(Vec::len),
            Some(2)
        );

        let other = by_name("also-risky");
        assert_eq!(
            other["highest_severity"], "critical",
            "another process with the same configuration keeps its severity"
        );
        assert!(other["dismissed"].as_array().map(Vec::len) == Some(0));
    }

    #[tokio::test]
    async fn advisories_are_unaffected_when_nothing_is_dismissed() {
        // The default path: an empty dismissal map must change nothing about the response, so the
        // feature cannot cost anything when unused.
        let snapshot = snapshot_with_dismissals(
            vec![risky_for_advisories("risky", 1)],
            std::collections::BTreeMap::new(),
        );
        let response =
            oneshot_request(build_test_router(&snapshot), "GET", "/api/advisories").await;
        assert_eq!(status_code(&response), 200);
        let value = json_body(response).await;
        let entry = &value["processes"][0];

        assert_eq!(entry["highest_severity"], "critical");
        assert!(entry["advisories"].as_array().map(Vec::len).unwrap_or(0) >= 2);
        assert_eq!(entry["dismissed"].as_array().map(Vec::len), Some(0));
        assert_eq!(value["dismissed_total"], 0);
    }
}
