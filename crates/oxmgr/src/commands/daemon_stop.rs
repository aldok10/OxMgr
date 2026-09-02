use anyhow::Result;

use oxmgr_daemon::config::AppConfig;
use oxmgr_daemon::ipc::{IpcRequest, send_request};

use super::common::expect_ok;

pub(crate) async fn run(config: &AppConfig) -> Result<()> {
    match send_request(&config.daemon_addr, &IpcRequest::Shutdown).await {
        Ok(response) => {
            let response = expect_ok(response)?;
            println!("{}", response.message);
        }
        Err(_) => {
            println!("Daemon is not running.");
        }
    }

    Ok(())
}
