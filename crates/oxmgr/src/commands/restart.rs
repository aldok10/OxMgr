use anyhow::Result;

use oxmgr_daemon::config::AppConfig;
use oxmgr_daemon::ipc::{IpcRequest, send_request};

use super::common::{config_target_display, expect_ok, maybe_load_named_targets_from_config};

pub(crate) async fn run(config: &AppConfig, target: String) -> Result<()> {
    if target != "all"
        && let Some(targets) = maybe_load_named_targets_from_config(&target)?
    {
        let mut restarted = 0_usize;
        let mut failures = Vec::new();

        for name in targets {
            let response = send_request(
                &config.daemon_addr,
                &IpcRequest::Restart {
                    target: name.clone(),
                },
            )
            .await?;
            if response.ok {
                restarted = restarted.saturating_add(1);
            } else {
                failures.push(format!("restart {}: {}", name, response.message));
            }
        }

        println!(
            "Restarted {} process(es) from {}",
            restarted,
            config_target_display(&target)
        );
        if !failures.is_empty() {
            for failure in failures {
                eprintln!("- {}", failure);
            }
            anyhow::bail!("restart finished with failures");
        }
        return Ok(());
    }

    let response = send_request(&config.daemon_addr, &IpcRequest::Restart { target }).await?;
    let response = expect_ok(response)?;
    println!("{}", response.message);

    Ok(())
}
