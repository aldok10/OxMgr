//! Entry point for the `oxmgr` binary.
//!
//! The binary is intentionally thin: it configures tracing, parses CLI input,
//! loads the local application configuration, and then hands control to either
//! the daemon loop or the command dispatcher.

// Test builds are exempt from the panic-freedom lints (§D3 of
// rust-panic-discipline): `unwrap()` is how a test fails, and forbidding it in
// tests would produce noise that gets suppressed wholesale — which is how a
// deny-level lint becomes decoration. Production code keeps the deny.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::let_underscore_must_use,
        reason = "test-only builds; a panicking assertion is the test failing loudly"
    )
)]

// Indexing bounds (clippy::indexing_slicing): measured 120+ sites across 20 files
// (task 2.2). It cannot be enabled at warn in this repo — both the local
// `warnings = "deny"` catch-all and CI's explicit `-D warnings` escalate every
// emitted warning to an error, so warn-level output is unrepresentable — and deny
// would ship an unreviewable diff. Recorded as follow-up debt; production keeps
// today's status quo (no regression from this change).
// Binary-crate surface (workspace-crate-layout phase 7): wiring only — CLI
// parsing, command dispatch, UI rendering, and the two CLI-owned format
// modules (`oxfile` for oxfile.toml import/convert, `bundle` for .oxpkg
// export/import). All domain, collection, analytics, management and daemon
// logic lives in the library crates under crates/.
mod bundle;
mod cli;
mod commands;
pub mod numeric;
mod oxfile;
mod ui;

#[cfg(test)]
pub mod test_utils;

use std::env;
use std::path::PathBuf;

// Environment variable names
const ENV_OXMGR_CONFIG: &str = "OXMGR_CONFIG";
const ENV_OXMGR_HOME: &str = "OXMGR_HOME";

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Commands, DaemonCommand};
use oxmgr_daemon::config::AppConfig;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();

    // For daemon run, load [http_server] config from oxfile BEFORE AppConfig
    // so that port/auth settings are available when config is loaded.
    if matches!(
        cli.command,
        Commands::Daemon {
            command: DaemonCommand::Run
        }
    ) {
        apply_oxfile_http_server_config();
    }

    let config = AppConfig::load()?;

    match cli.command {
        Commands::Daemon {
            command: DaemonCommand::Run,
        } => oxmgr_daemon::daemon::run_foreground(config).await,
        command => commands::run(command, &config).await,
    }
}

/// Loads `[http_server]` from oxfile.toml and applies to env vars before AppConfig.
fn apply_oxfile_http_server_config() {
    if let Some(http) = load_http_server_config() {
        apply_http_config(&http);
    }
}

/// Attempts to load `[http_server]` config from oxfile in standard locations.
/// Search order: OXMGR_CONFIG > OXMGR_HOME/oxfile.toml > ./oxfile.toml
fn load_http_server_config() -> Option<oxfile::OxHttpServer> {
    let candidates: Vec<PathBuf> = [
        env::var(ENV_OXMGR_CONFIG).ok().map(PathBuf::from),
        env::var(ENV_OXMGR_HOME)
            .ok()
            .map(|h| PathBuf::from(h).join("oxfile.toml")),
        env::current_dir().ok().map(|d| d.join("oxfile.toml")),
    ]
    .into_iter()
    .flatten()
    .filter(|p| p.exists())
    .collect();

    for path in candidates {
        let Ok(result) = oxfile::load_full(&path, None) else {
            continue;
        };
        if result.http_server.is_some() {
            return result.http_server;
        }
    }
    None
}

fn apply_http_config(http: &oxfile::OxHttpServer) {
    let set_if_unset = |key: &str, value: &str| {
        if env::var(key).is_err() {
            // SAFETY: `apply_http_config` is called from `main` before the tokio runtime is created,
            // so no other threads are accessing the environment.
            unsafe { env::set_var(key, value) };
        }
    };

    if let Some(ref port) = http.port {
        set_if_unset("OXMGR_API_ADDR", port);
    }
    if let Some(ref user) = http.username {
        set_if_unset("OXMGR_DASHBOARD_USER", user);
    }
    if let Some(ref pass) = http.password {
        set_if_unset("OXMGR_DASHBOARD_PASS", pass);
    }
    if let Some(interval) = http.interval_ms {
        set_if_unset("OXMGR_DASHBOARD_INTERVAL_MS", &interval.to_string());
    }
    if let Some(ref label) = http.label {
        set_if_unset("OXMGR_DASHBOARD_LABEL", label);
    }
    if let Some(ref color) = http.label_color {
        set_if_unset("OXMGR_DASHBOARD_LABEL_COLOR", color);
    }
}

fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(|| oxmgr_daemon::SafeWriter(std::io::stdout()))
        .init();
}
