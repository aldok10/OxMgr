# Architecture Overview

This document explains how Oxmgr is structured internally and how the main runtime pieces work together.

## Design Goals

Oxmgr is designed as a lightweight, cross-platform process manager with a clear separation between:

- CLI input and command dispatch
- long-lived daemon state
- process lifecycle orchestration
- persistence, logging, and import/export formats

The implementation favours simple local primitives such as a localhost TCP IPC channel, JSON/TOML files, and explicit state transitions over a deeply layered service architecture.

## High-Level Flow

1. The `oxmgr` binary starts in `crates/oxmgr/src/main.rs`.
2. CLI arguments are parsed in `crates/oxmgr/src/cli.rs`.
3. `crates/oxmgr-daemon/src/config.rs` resolves runtime paths, daemon addresses, and log policy.
4. Most commands are dispatched through `crates/oxmgr/src/commands/mod.rs`.
5. If the command needs the daemon, `crates/oxmgr-daemon/src/daemon.rs` ensures that it is running and then communicates through `crates/oxmgr-daemon/src/ipc.rs`.
6. The daemon owns a single `ProcessManager` instance from `crates/oxmgr-manager/src/process_manager.rs`.
7. `ProcessManager` persists state through `crates/oxmgr-manager/src/storage.rs`, writes logs through `crates/oxmgr-manager/src/logging.rs`, and manages child processes described by types in `crates/oxmgr-metrics/src/process.rs`.

## Core Modules

### `crates/oxmgr/src/cli.rs`

Defines the user-facing command-line interface. This module is intentionally thin: it maps parsed flags into helper types such as `HealthCheck` and `ResourceLimits`, then leaves execution to the command layer.

### `crates/oxmgr/src/commands/`

Contains one implementation module per top-level command. These modules translate CLI intent into daemon requests or local-only operations such as validation, conversion, and service installation.

### `crates/oxmgr-daemon/src/daemon.rs`

Runs the foreground daemon event loop. The daemon listens on:

- a localhost TCP IPC endpoint for CLI requests
- a localhost HTTP endpoint for authenticated pull webhooks and Prometheus scraping

The daemon serialises state changes through a single manager command channel, which keeps lifecycle transitions predictable.

### `crates/oxmgr-manager/src/process_manager.rs`

This is the operational core of Oxmgr. It is responsible for:

- starting and stopping processes
- tracking desired state versus observed runtime state
- handling reloads, delayed restarts, and crash-loop protection
- running health checks
- collecting CPU and memory metrics
- applying optional resource controls
- persisting state after mutations

If you need to understand runtime behaviour, this is the first file to read.

### `crates/oxmgr-metrics/src/process.rs`

Defines the shared domain model:

- requested process configuration (`StartProcessSpec`)
- persisted/runtime process record (`ManagedProcess`)
- restart, health, and desired-state enums

These types are shared by the CLI, daemon, storage layer, importers, and IPC protocol.

### `crates/oxmgr-manager/src/storage.rs`

Persists daemon state to a JSON file under the Oxmgr home directory. Writes are performed through a temporary file and replace step so state updates are resilient to partial writes.

### `crates/oxmgr-manager/src/logging.rs`

Calculates per-process stdout/stderr log paths, rotates oversized logs, cleans up expired rotations, and reads recent log tails for status views and CLI commands.

## Configuration Inputs

Oxmgr accepts several ways to define managed services:

- direct CLI input through `oxmgr start`
- native `oxfile.toml` files parsed by `crates/oxmgr/src/oxfile.rs`
- PM2-compatible `ecosystem.config.json` files parsed by `crates/oxmgr-manager/src/ecosystem.rs`
- portable `.oxpkg` bundles handled by `crates/oxmgr/src/bundle.rs`

Import layers normalise external formats into a common process-spec representation before the daemon starts or updates services. This keeps the runtime logic independent of the source format.

## Process Lifecycle

The normal process lifecycle looks like this:

1. A `StartProcessSpec` is created from CLI input or imported configuration.
2. `ProcessManager` validates and normalises the process name and command line.
3. Log files are prepared and the child process is spawned.
4. Runtime metadata is stored in `ManagedProcess` and written to disk.
5. The daemon periodically:
   - refreshes metrics
   - evaluates file-watch fingerprints
   - runs health checks
   - executes scheduled restarts
6. When a child exits, an exit event is sent back to `ProcessManager`.
7. Restart policy, crash-loop limits, and desired state determine whether Oxmgr restarts the process or marks it as stopped, crashed, or errored.

## Persistence and Recovery

The daemon writes process state to disk so that a restart of Oxmgr does not lose service definitions. On daemon startup, `recover_processes()`:

- clears stale runtime-only fields such as PIDs and transient metrics
- attempts to clean up stale processes that were left behind
- restarts services whose desired state is still `running`

This means Oxmgr treats persisted configuration as authoritative, while live operating-system process IDs are always revalidated.

## IPC and Transport Safety

The CLI and daemon communicate using newline-delimited JSON messages over localhost TCP. The transport is intentionally simple and private to the local machine.

Before process data is returned to the CLI, Oxmgr redacts sensitive values such as:

- environment variables
- stored pull-webhook secret hashes

This keeps status responses useful without leaking secrets through normal tooling.

## Platform-Specific Concerns

- Linux can optionally enforce resource limits through cgroup v2 in `crates/oxmgr-metrics/src/cgroup.rs`.
- macOS and Windows use the same high-level lifecycle code but skip Linux-specific cgroup enforcement.
- Service installation is delegated to platform-specific command implementations rather than being mixed into the daemon core.

## Where To Extend the Code

For common kinds of changes, start in these places:

The workspace is split into layered crates (downward-only dependencies,
enforced by `scripts/check-crate-layering.sh`): `oxmgr-core` (domain types),
`oxmgr-store` (retention/hashing), `oxmgr-metrics` (collection),
`oxmgr-analytics` (detectors/baselines), `oxmgr-manager` (lifecycle),
`oxmgr-daemon` (HTTP/SSE + IPC + config), and the thin `oxmgr` binary.

For common kinds of changes, start in these places:

- new CLI flag or subcommand: `crates/oxmgr/src/cli.rs` and `crates/oxmgr/src/commands/`
- new runtime lifecycle behaviour: `crates/oxmgr-manager/src/process_manager.rs`
- new persisted process field: `crates/oxmgr-metrics/src/process.rs` and `crates/oxmgr-manager/src/storage.rs`
- new configuration format capability: `crates/oxmgr/src/oxfile.rs` or `crates/oxmgr-manager/src/ecosystem.rs`
- log handling changes: `crates/oxmgr-manager/src/logging.rs`
- daemon protocol change: `crates/oxmgr-daemon/src/ipc.rs` and the relevant command handler

Keep source-level rustdoc and user-facing docs in sync when changing behaviour, flags, or configuration semantics.
