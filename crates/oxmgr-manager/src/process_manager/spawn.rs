use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::fs;

use anyhow::{Context, Result};

use oxmgr_core::errors::OxmgrError;
use oxmgr_metrics::process::ManagedProcess;

#[derive(Debug, Clone)]
pub(super) struct SpawnProgram {
    pub(super) program: String,
    pub(super) args: Vec<String>,
    pub(super) extra_env: HashMap<String, String>,
}

pub(super) fn parse_command_line(command_line: &str) -> Result<(String, Vec<String>)> {
    let tokens = shell_words::split(command_line)
        .map_err(|err| OxmgrError::InvalidCommand(err.to_string()))?;

    if tokens.is_empty() {
        return Err(OxmgrError::InvalidCommand("command cannot be empty".to_string()).into());
    }

    let command = tokens[0].clone();
    let args = tokens[1..].to_vec();
    Ok((command, args))
}

pub(super) async fn resolve_spawn_program(
    process: &ManagedProcess,
    base_dir: &Path,
) -> Result<SpawnProgram> {
    if !process.cluster_mode {
        return Ok(SpawnProgram {
            program: process.command.clone(),
            args: process.args.clone(),
            extra_env: HashMap::new(),
        });
    }

    if !is_node_binary(&process.command) {
        anyhow::bail!("cluster mode requires a Node.js command (expected `node <script> ...`)");
    }
    let Some(script) = process.args.first() else {
        anyhow::bail!("cluster mode requires a script argument (expected `node <script> ...`)");
    };
    if script.starts_with('-') {
        anyhow::bail!(
            "cluster mode currently does not support Node runtime flags before script path"
        );
    }

    let bootstrap = ensure_node_cluster_bootstrap(base_dir).await?;
    let mut args = Vec::with_capacity(process.args.len() + 2);
    args.push(bootstrap.display().to_string());
    args.push("--".to_string());
    args.extend(process.args.clone());

    let mut extra_env = HashMap::new();
    extra_env.insert(
        "OXMGR_CLUSTER_INSTANCES".to_string(),
        process
            .cluster_instances
            .map(|value| value.to_string())
            .unwrap_or_else(|| "auto".to_string()),
    );

    Ok(SpawnProgram {
        program: process.command.clone(),
        args,
        extra_env,
    })
}

async fn ensure_node_cluster_bootstrap(base_dir: &Path) -> Result<PathBuf> {
    let runtime_dir = base_dir.join("runtime");
    fs::create_dir_all(&runtime_dir).await.with_context(|| {
        format!(
            "failed to create runtime directory {}",
            runtime_dir.display()
        )
    })?;

    let bootstrap_path = runtime_dir.join("node_cluster_bootstrap.cjs");
    fs::write(&bootstrap_path, NODE_CLUSTER_BOOTSTRAP)
        .await
        .with_context(|| {
            format!(
                "failed to write node cluster bootstrap at {}",
                bootstrap_path.display()
            )
        })?;
    Ok(bootstrap_path)
}

pub(super) fn normalize_cluster_instances(value: Option<u32>) -> Option<u32> {
    value.filter(|instances| *instances > 0)
}

fn is_node_binary(command: &str) -> bool {
    let executable = Path::new(command)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(command)
        .to_ascii_lowercase();
    matches!(
        executable.as_str(),
        "node" | "node.exe" | "nodejs" | "nodejs.exe"
    )
}

pub(super) fn validate_process_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(OxmgrError::InvalidProcessName("name cannot be empty".to_string()).into());
    }

    if name == "all" {
        return Err(OxmgrError::InvalidProcessName("'all' is a reserved name".to_string()).into());
    }

    let valid = name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');

    if !valid {
        return Err(OxmgrError::InvalidProcessName(name.to_string()).into());
    }
    Ok(())
}

pub(super) fn sanitize_name(input: &str) -> String {
    let value: String = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect();

    let trimmed = value.trim_matches('-');
    if trimmed.is_empty() {
        "process".to_string()
    } else {
        trimmed.to_ascii_lowercase()
    }
}

const NODE_CLUSTER_BOOTSTRAP: &str = r#""use strict";
const cluster = require("node:cluster");
const os = require("node:os");
const path = require("node:path");
const process = require("node:process");

function parseDesiredInstances(raw) {
  if (!raw || raw === "auto") return 0;
  const parsed = Number.parseInt(raw, 10);
  if (!Number.isFinite(parsed) || parsed <= 0) return 0;
  return parsed;
}

function cpuCount() {
  if (typeof os.availableParallelism === "function") {
    const value = os.availableParallelism();
    if (Number.isFinite(value) && value > 0) return value;
  }
  const cpus = os.cpus();
  return Array.isArray(cpus) && cpus.length > 0 ? cpus.length : 1;
}

const argv = process.argv.slice(2);
if (argv[0] === "--") argv.shift();
const script = argv.shift();

if (!script) {
  console.error("[oxmgr] cluster mode needs a script argument (expected: node <script> ...)");
  process.exit(2);
}

const desired = parseDesiredInstances(process.env.OXMGR_CLUSTER_INSTANCES || "");
const workerCount = desired > 0 ? desired : cpuCount();

cluster.setupPrimary({
  exec: path.resolve(script),
  args: argv
});

let shuttingDown = false;
let nextInstance = 0;

function forkWorker() {
  const env = { NODE_APP_INSTANCE: String(nextInstance) };
  nextInstance += 1;
  return cluster.fork(env);
}

for (let idx = 0; idx < workerCount; idx += 1) {
  forkWorker();
}

function shutdown(signal) {
  if (shuttingDown) return;
  shuttingDown = true;
  const workers = Object.values(cluster.workers).filter(Boolean);
  for (const worker of workers) {
    worker.process.kill(signal);
  }
  setTimeout(() => process.exit(0), 3000).unref();
}

process.on("SIGTERM", () => shutdown("SIGTERM"));
process.on("SIGINT", () => shutdown("SIGINT"));

cluster.on("exit", (worker) => {
  if (shuttingDown) return;
  if (worker.exitedAfterDisconnect) return;
  forkWorker();
});
"#;

#[cfg(test)]
mod tests {
    use super::{sanitize_name, validate_process_name};

    // ── 4.5 log paths: names platforms treat differently ────────────────────────────────────────

    #[test]
    fn a_name_that_would_produce_an_unusable_path_is_refused_at_creation() {
        // Task 4.5. The names below are the ones platforms disagree about, and every one of them is
        // refused BEFORE a process exists — which is the point: a name accepted at creation and then
        // found unusable when the log file is opened leaves a registered process that cannot log.
        //
        // `validate_process_name` allows only `[A-Za-z0-9_-]`, so this is a whitelist rather than a
        // list of banned characters. That matters because the set of characters Windows rejects,
        // macOS normalises and Linux allows is not knowable from any one platform — a blacklist
        // written on macOS would miss the Windows cases.
        let unusable = [
            // Path separators: would silently create a subdirectory, or escape the log directory.
            (
                "api/v2",
                "a forward slash would nest or escape the log directory",
            ),
            ("api\\v2", "a backslash is a separator on Windows"),
            (
                "../etc/passwd",
                "traversal must not reach outside the log directory",
            ),
            // Windows-reserved characters. Every one of these is legal on Linux and fatal on Windows,
            // so accepting them would make a config portable in name only.
            ("api:v2", "a colon is a stream separator on NTFS"),
            ("api*", "a wildcard cannot be a filename on Windows"),
            ("api?", "a wildcard cannot be a filename on Windows"),
            ("api\"v2", "a quote cannot be a filename on Windows"),
            (
                "api<v2",
                "a redirect character cannot be a filename on Windows",
            ),
            ("api|v2", "a pipe cannot be a filename on Windows"),
            // Trailing dot and space: Windows silently STRIPS both, so two distinct names would
            // collapse onto one log file.
            (
                "api.",
                "Windows strips a trailing dot, collapsing two names onto one file",
            ),
            (
                "api ",
                "Windows strips a trailing space, collapsing two names onto one file",
            ),
            // Control characters and NUL.
            (
                "api\nv2",
                "a newline in a filename breaks every line-oriented tool",
            ),
            ("api\0v2", "NUL cannot appear in a path on any platform"),
            // Non-ASCII: macOS stores NFD and Linux stores what you gave it, so the same name typed
            // on two machines can produce two different files.
            (
                "café",
                "unicode normalisation differs between macOS and Linux",
            ),
            // Empty and reserved.
            ("", "an empty name has no file to write to"),
            (
                "all",
                "'all' is the command-line wildcard and cannot be a process",
            ),
        ];

        for (name, why) in unusable {
            assert!(
                validate_process_name(name).is_err(),
                "{name:?} must be refused at creation: {why}"
            );
        }
    }

    #[test]
    fn a_portable_name_is_accepted() {
        // The other half: the whitelist must not be so strict that ordinary names fail. A validator
        // that refuses everything passes the test above and is useless.
        for name in [
            "api",
            "api-v2",
            "api_v2",
            "API",
            "worker-01",
            "a",
            // 200 characters: long, but every platform allows a 255-byte filename component, and the
            // log file adds only a short suffix.
            "x".repeat(200).leak(),
        ] {
            assert!(
                validate_process_name(name).is_ok(),
                "{name:?} is portable and must be accepted"
            );
        }
    }

    #[test]
    fn the_refusal_names_the_offending_input() {
        // An operator who typed `api/v2` needs to see `api/v2` in the error. "Invalid process name"
        // alone sends them looking through a config file for something they cannot identify.
        let error = validate_process_name("api/v2")
            .expect_err("a slash must be refused")
            .to_string();
        assert!(
            error.contains("api/v2"),
            "the refusal must quote the name it rejected, got {error:?}"
        );
    }

    #[test]
    fn an_auto_derived_name_is_always_usable_as_a_path() {
        // `sanitize_name` runs on a COMMAND rather than an operator's chosen name — it derives a
        // default when `--name` is absent, so its input is an executable path and is expected to
        // contain separators and dots. Whatever it produces must satisfy the same validator, or the
        // daemon could generate a name it would itself refuse.
        for command in [
            "/usr/local/bin/my-app",
            "C:\\Program Files\\App\\server.exe",
            "./node_modules/.bin/next",
            "café-server",
            "...",
            "!!!",
            "",
        ] {
            let derived = sanitize_name(command);
            assert!(
                validate_process_name(&derived).is_ok(),
                "sanitize_name({command:?}) produced {derived:?}, which the validator refuses"
            );
            // And it is never empty, or the process would have no log file name at all.
            assert!(!derived.is_empty());
        }
    }
}
