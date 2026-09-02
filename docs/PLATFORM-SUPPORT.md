# Platform support matrix

<!-- GENERATED FILE. Do not edit by hand.
     Rendered from `platform::MATRIX` by `platform::render_markdown`, and checked by
     `docs_matrix_is_up_to_date`. Change the declaration in `src/platform.rs`, then run
     `cargo test regenerate_platform_matrix_doc -- --ignored` to rewrite it. -->

Every row is a capability whose behaviour depends on the operating system. A capability with no platform fork is not listed: this file is about divergence, not about features.

## Levels

- **supported** — Does the job the capability describes.
- **degraded** — Works, but not the same way. The difference is stated, and an operator has to account for it.
- **unavailable** — Cannot be done on this platform. The reason names the constraint and, where there is one, the alternative.
- **unknown** — Reading the code does not settle it and nobody has observed it. Not a synonym for "probably fine" — it is an open question.

## How we know

The level is *what we claim*; the basis is *how we know*. They are separate columns because a `supported` read from code and a `supported` inferred from a mechanism look identical to a user and are very different to a contributor deciding what still needs a test.

- **code-read** — A platform arm in the tree plainly does this, or plainly refuses to.
- **inferred** — The mechanism was read, but its outcome on this platform was not observed.
- **unverified** — Neither read nor observed.

## At a glance

| Capability | Linux | macOS | Windows |
|---|---|---|---|
| event_stream | supported | supported | unavailable |
| reuse_port_hint | supported | supported | degraded |
| process_tree_termination | supported | supported | degraded |
| custom_stop_signal | supported | supported | unavailable |
| exit_signal_reporting | supported | supported | unavailable |
| cgroup_resource_limits | supported | unavailable | unavailable |
| private_file_permissions | supported | supported | unavailable |
| log_rotation_while_open | supported | supported | unknown |
| atomic_state_replace | supported | supported | degraded |
| host_load_average | supported | supported | unavailable |
| shutdown_signals | supported | supported | degraded |
| reload_shell_command | supported | supported | degraded |
| service_installation | supported | supported | degraded |
| per_user_port_identity | supported | supported | degraded |
| browser_launch | degraded | supported | supported |

## Detail

### event_stream

Streaming daemon events to a client (`oxmgr events`)

- **Linux**: supported (code-read) [`src/commands/events.rs:19, src/daemon.rs:105`]
- **macOS**: supported (code-read) [`src/commands/events.rs:19, src/daemon.rs:105`]
- **Windows**: unavailable (code-read) — The event bus is published on a Unix domain socket, and both the listener and the client are compiled only for Unix, so there is nothing to connect to. Use the HTTP event stream on the daemon's API port instead of `oxmgr events`. [`src/commands/events.rs:16`]

### reuse_port_hint

Telling a managed process it may share a listening port with its replacement

- **Linux**: supported (code-read) — oxmgr does not create the socket: it sets OXMGR_REUSEPORT=1 and SO_REUSEPORT=1 in the child's environment, so zero-downtime reload only works if the process itself reads them and sets SO_REUSEPORT on its listener. [`src/process_manager.rs:1550`]
- **macOS**: supported (inferred) — Same environment-variable hint as Linux; macOS has SO_REUSEPORT, but load is not balanced across listeners the way Linux does it, so overlap behaviour during a reload differs even though the setting is honoured. [`src/process_manager.rs:1550`]
- **Windows**: degraded (code-read) — The hint is not passed to the child at all — the spawn logs a warning and continues, so the configuration is accepted and the process starts with no port sharing. Expect a gap in listening during reload, and stagger restarts if that gap matters. [`src/process_manager.rs:1557`]

### process_tree_termination

Stopping a managed process together with the children it started

- **Linux**: supported (inferred) — Children are put in their own process group with setpgid and the signal is sent to the negative pgid, so the group is the unit of termination. A descendant that calls setsid() leaves the group and survives. [`src/process_manager.rs:1522, src/process_manager/runtime.rs:51`]
- **macOS**: supported (inferred) — Same process-group mechanism as Linux, with the same setsid escape hatch. [`src/process_manager.rs:1522, src/process_manager/runtime.rs:51`]
- **Windows**: degraded (code-read) — There are no process groups here, and the two termination paths do not agree. The supervised path shells out to `taskkill /PID <pid> /T`, which is meant to include descendants but is not observed by any test; the foreground `oxmgr runtime` path only kills the direct child, so its grandchildren are left running. Check for strays after stopping a process that spawns its own workers. [`src/process_manager/runtime.rs:140, src/commands/runtime.rs:283`]

### custom_stop_signal

Honouring a process's configured stop_signal instead of a generic terminate

- **Linux**: supported (code-read) [`src/process_manager/runtime.rs:52, src/process_manager/runtime.rs:106`]
- **macOS**: supported (code-read) [`src/process_manager/runtime.rs:52, src/process_manager/runtime.rs:106`]
- **Windows**: unavailable (code-read) — Windows has no signals, so the configured stop_signal is ignored and termination goes through taskkill regardless. A process that relies on SIGHUP or SIGUSR1 to shut down cleanly needs another trigger here, such as a control endpoint of its own. [`src/process_manager/runtime.rs:126`]

### exit_signal_reporting

Reporting which signal killed a process after it exits

- **Linux**: supported (code-read) [`src/process_manager.rs:258`]
- **macOS**: supported (code-read) [`src/process_manager.rs:258`]
- **Windows**: unavailable (code-read) — An exit status carries no signal here, so the field is always absent and a process killed by the OS looks the same as one that exited on its own. Read the exit code and the process's own logs to tell them apart. [`src/process_manager.rs:284`]

### cgroup_resource_limits

Enforcing CPU and memory limits on a managed process

- **Linux**: supported (inferred) — Enforced through cgroup v2 under /sys/fs/cgroup, which requires the daemon to be able to write there — an unprivileged daemon without delegated cgroup control fails with a permission error rather than running unlimited. [`src/cgroup.rs:24, src/cgroup.rs:152`]
- **macOS**: unavailable (code-read) — cgroups are a Linux kernel feature with no macOS equivalent, so cgroup_enforce is rejected outright rather than silently ignored. Limit the process from inside itself, or run it in a Linux container. [`src/cgroup.rs:99`]
- **Windows**: unavailable (code-read) — Same as macOS: no cgroups, so cgroup_enforce is rejected. Job objects would be the native equivalent and are not implemented. [`src/cgroup.rs:99`]

### private_file_permissions

Restricting state, log and bundle files to the owning user

- **Linux**: supported (code-read) — State and log files are opened with mode 0o600 and their directories created 0o700, so other users cannot read them. [`src/storage.rs:158, src/logging.rs:212, src/config.rs:95`]
- **macOS**: supported (code-read) — Same 0o600 files and 0o700 directories as Linux. [`src/storage.rs:158, src/logging.rs:212, src/config.rs:95`]
- **Windows**: unavailable (code-read) — The permission call is a no-op that returns Ok, and the mode() option is not applied either, so state and log files inherit whatever the parent directory's ACL grants — they are not restricted by oxmgr at all. Process environments and stored secrets sit in those files, so restrict the oxmgr data directory's ACL yourself before treating them as private. [`src/storage.rs:167, src/logging.rs:221`]

### log_rotation_while_open

Rotating a log file while the daemon is writing to it

- **Linux**: supported (inferred) — Rotation renames the open file and then reopens the original path. Renaming a file that is open for writing is ordinary here: the writer's handle follows the inode, so the flush before the rename is what keeps the tail of the old file intact. [`src/logging.rs:304`]
- **macOS**: supported (inferred) — Same rename-then-reopen as Linux. [`src/logging.rs:304`]
- **Windows**: unknown (unverified) — Unresolved, and it is the sharpest gap in this matrix. Rotation calls fs::rename on a file the daemon itself still holds open for writing, and there is no Windows arm and no retry on that path — unlike the state file, which does have a Windows fallback. Whether the rename succeeds depends on the sharing mode Rust's OpenOptions requested, which was not established by reading this code. If it fails, rotate() propagates the error to the log-writing task. Watch for rotation errors in the daemon log on Windows until a test on that platform settles it. [`src/logging.rs:311 (compare the Windows fallback at src/storage.rs:98)`]

### atomic_state_replace

Replacing the persisted state file without leaving a partial file behind

- **Linux**: supported (code-read) — A single rename over the existing path, which is atomic within a filesystem: a reader sees either the old file or the new one. [`src/storage.rs:95`]
- **macOS**: supported (code-read) — Same single rename as Linux. [`src/storage.rs:95`]
- **Windows**: degraded (code-read) — Rename over an existing file can fail here, so the fallback removes the destination and renames again. That leaves a window with no state file: a crash inside it loses the state rather than keeping the previous copy. The write still never produces a truncated file, only a missing one. [`src/storage.rs:98`]

### host_load_average

Reporting the host's 1/5/15-minute load average

- **Linux**: supported (code-read) [`src/host_metrics.rs:1071`]
- **macOS**: supported (code-read) [`src/host_metrics.rs:1071`]
- **Windows**: unavailable (code-read) — Windows has no load average, and the underlying library returns three zeroes that would read as an idle machine, so oxmgr reports the figure as absent instead. Use CPU utilisation for the same question here. [`src/host_metrics.rs:1072`]

### shutdown_signals

Shutting the daemon down cleanly on an operator's request

- **Linux**: supported (code-read) — SIGTERM and SIGINT are both handled, so a service manager stopping the daemon gets a graceful shutdown. A handler that fails to install is left absent and simply never fires. [`src/signal.rs:35, src/signal.rs:63`]
- **macOS**: supported (code-read) — Same SIGTERM and SIGINT handling as Linux. [`src/signal.rs:35, src/signal.rs:63`]
- **Windows**: degraded (code-read) — Only Ctrl-C is awaited. There is no SIGTERM equivalent, so a stop issued by Task Scheduler or a process kill does not run the graceful shutdown path; state is only as current as the last persist. [`src/signal.rs:85`]

### reload_shell_command

Running a process's pre_reload_cmd through a shell

- **Linux**: supported (code-read) — Run with `sh -lc`, so the command is interpreted as POSIX shell and a login profile is sourced. [`src/process_manager.rs:893`]
- **macOS**: supported (code-read) — Same `sh -lc` as Linux, though the login profile that gets sourced differs. [`src/process_manager.rs:893`]
- **Windows**: degraded (code-read) — Run with `cmd /C`, so the command is interpreted by cmd rather than a POSIX shell: pipes, quoting and operators do not carry over. A command written for sh will not run here unchanged. [`src/process_manager.rs:893`]

### service_installation

Installing oxmgr to start automatically, and inspecting that installation

- **Linux**: supported (inferred) — Installed as a systemd user unit written to ~/.config/systemd/user/oxmgr.service, so the definition is a file `doctor` can read back. [`src/commands/service.rs:46, src/commands/doctor.rs:511`]
- **macOS**: supported (inferred) — Installed as a launchd agent plist under ~/Library/LaunchAgents, likewise readable back by `doctor`. [`src/commands/service.rs:37, src/commands/doctor.rs:512`]
- **Windows**: degraded (code-read) — Installed by shelling out to schtasks as an ONLOGON task, which works but has no definition file, so `doctor` cannot report the installed configuration the way it does for systemd and launchd. Inspect it with `schtasks /Query /TN OxmgrDaemon` instead. [`src/commands/service.rs:168, src/commands/doctor.rs:513`]

### per_user_port_identity

Deriving a per-user daemon port so two users on one host do not collide

- **Linux**: supported (code-read) — The port is hashed from the effective uid, which is unique per user on the host. [`src/config.rs:128`]
- **macOS**: supported (code-read) — Same effective-uid hash as Linux. [`src/config.rs:128`]
- **Windows**: degraded (code-read) — There is no uid, so the port is hashed from the USERNAME environment variable and falls back to the literal "unknown" when it is unset. Two sessions with USERNAME unset, or a service account with it cleared, derive the same port and collide. [`src/config.rs:133`]

### browser_launch

Opening the dashboard in the operator's default browser

- **Linux**: degraded (code-read) — Delegated to xdg-open, which is not present on a minimal or headless install; the dashboard URL still works if opened by hand. [`src/commands/ui/web.rs:43`]
- **macOS**: supported (code-read) — Delegated to `open`, which is part of the base system. [`src/commands/ui/web.rs:35`]
- **Windows**: supported (code-read) — Delegated to `cmd /c start`. [`src/commands/ui/web.rs:51`]

## Released targets

Shipping a target is not the same as having observed it. A target below with **verified: no** is built and released, and nothing about its behaviour has been watched — its platform's rows above are claims about code, not about that target.

| Target | Platform | Runtime verified |
|---|---|---|
| `x86_64-unknown-linux-gnu` | Linux | yes |
| `x86_64-unknown-linux-musl` | Linux | **no** |
| `aarch64-unknown-linux-gnu` | Linux | **no** |
| `x86_64-apple-darwin` | macOS | yes |
| `x86_64-pc-windows-msvc` | Windows | yes |

- **`x86_64-unknown-linux-gnu`** — The CI test job runs here, so every assertion in the suite has been observed on it.
- **`x86_64-unknown-linux-musl`** — Built and released, never run in CI. musl statically links and resolves names differently from glibc, so DNS-dependent health checks and cgroup mount discovery are the paths most likely to differ. Treat Linux rows as claims about glibc.
- **`aarch64-unknown-linux-gnu`** — Cross-compiled and released, never run in CI. The Linux rows are read from code that is architecture-independent, so the risk is in the toolchain rather than the logic — but nothing here has been observed on arm64 Linux.
- **`x86_64-apple-darwin`** — The macOS CI runner and the development machine for this change. Every macOS row marked CodeRead was read here; rows marked Inferred still were not observed.
- **`x86_64-pc-windows-msvc`** — The CI test job runs here with `--test-threads=1`. Windows rows marked Inferred describe code paths the suite does not currently exercise.

## Supported platforms

Linux, macOS and Windows are supported, in the sense that the daemon runs and the suite passes on each in CI. "Supported" is per capability rather than per platform: see the table above for what differs. Windows has the most divergence — the event socket is unavailable there, and file permissions are never restricted at creation.
