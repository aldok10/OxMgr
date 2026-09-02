//! The declared platform support matrix, in code, so a limitation is a value rather than a comment.
//!
//! Scaffold for OpenSpec change `cross-platform-assurance`, tasks section(s) 1.
//! The contract is `openspec/changes/cross-platform-assurance/specs/`; read it before adding to this file.
//!
//! Pure logic on purpose: nothing here reaches for the process manager, the HTTP layer or the
//! dashboard, so it can be unit-tested without a running daemon. Wiring it into those is a
//! separate integration step, which is why the module is currently unreferenced.
//!
//! # What the entries in this table are, and are not
//!
//! Every entry was read out of the tree as it stands, and each carries the `file:line` it was read
//! from in [`CapabilitySupport::evidence`]. That is the strongest claim this module makes: *the
//! code says so*. It is not the same as *the behaviour was observed*, which is why each entry also
//! carries a [`Basis`]. `Basis::CodeRead` means one platform arm plainly does the thing or plainly
//! refuses to; `Basis::Inferred` means the mechanism was read but its outcome was not observed on
//! that platform; `Basis::Unverified` means the code does not settle the question at all and the
//! level is [`SupportLevel::Unknown`].
//!
//! The distinction exists because the matrix is only useful if it is honest. A row that claims
//! parity nobody has observed is worse than no row: it stops the question being asked. The
//! `Unknown` rows here are the interesting ones — they are the work items for sections 3 and 4 of
//! `tasks.md`, and they should shrink as per-platform tests land, not because someone felt
//! confident.
//!
//! # Absence of a `#[cfg]` proves nothing
//!
//! A capability with no platform fork in the tree is *not* automatically supported everywhere. It
//! may be portable, or it may be an untested assumption. Only capabilities whose behaviour was
//! actually traced appear here; a caller asking about something absent from [`Capability`] gets no
//! answer rather than a reassuring one.

use serde::Serialize;

/// A platform oxmgr is built and released for.
///
/// Deliberately the release set rather than everything Rust targets: the tree has
/// `#[cfg(not(any(unix, windows)))]` arms (`src/config.rs:133`, `src/commands/runtime.rs:337`) that
/// keep exotic targets compiling, but nothing is built or tested for them, so declaring support
/// levels for them would be invention. [`current`] returns `None` there instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Linux,
    MacOs,
    Windows,
}

impl Platform {
    /// Every platform the matrix must have an entry for. Used by the completeness test, so adding a
    /// platform here makes every incomplete row a test failure rather than a silent gap.
    pub const ALL: [Platform; 3] = [Platform::Linux, Platform::MacOs, Platform::Windows];

    /// Operator-facing name, matching how the platform is spelled in CI and release artefacts.
    pub const fn label(self) -> &'static str {
        match self {
            Platform::Linux => "Linux",
            Platform::MacOs => "macOS",
            Platform::Windows => "Windows",
        }
    }
}

/// The platform this binary is running on, or `None` on a target outside the released set.
///
/// `cfg!` rather than `#[cfg]` so every arm type-checks on every host: a matrix that only compiles
/// correctly on the platform it describes would be exactly the kind of divergence this module
/// exists to catch.
#[must_use]
pub fn current() -> Option<Platform> {
    if cfg!(target_os = "linux") {
        Some(Platform::Linux)
    } else if cfg!(target_os = "macos") {
        Some(Platform::MacOs)
    } else if cfg!(target_os = "windows") {
        Some(Platform::Windows)
    } else {
        None
    }
}

/// How well a capability works on one platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SupportLevel {
    /// Does the job the capability describes.
    Supported,
    /// Works, but not the same way — the difference is in the reason, and an operator has to
    /// account for it.
    Degraded,
    /// Cannot be done on this platform. The reason says what makes it impossible so the operator
    /// can choose something else.
    Unavailable,
    /// Reading the code does not settle it, and nobody has observed it. Not a synonym for
    /// "probably fine": it is an open question, and the reason states what would answer it.
    Unknown,
}

impl SupportLevel {
    /// Whether this level obliges the declaration to carry a reason. Everything except
    /// [`SupportLevel::Supported`] does; the completeness test enforces it.
    pub const fn requires_reason(self) -> bool {
        !matches!(self, SupportLevel::Supported)
    }

    pub const fn label(self) -> &'static str {
        match self {
            SupportLevel::Supported => "supported",
            SupportLevel::Degraded => "degraded",
            SupportLevel::Unavailable => "unavailable",
            SupportLevel::Unknown => "unknown",
        }
    }
}

/// How much weight the declared level can bear.
///
/// Separate from [`SupportLevel`] because they answer different questions: the level is *what we
/// claim*, the basis is *how we know*. A `Supported` level with a `CodeRead` basis and a
/// `Supported` level with an `Inferred` basis look identical to a user and are very different to a
/// contributor deciding what still needs a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    /// A platform arm in the tree plainly does this, or plainly refuses to. The `evidence` line
    /// shows it.
    CodeRead,
    /// The mechanism was read, but its outcome on this platform was not observed. The code intends
    /// the behaviour; whether the platform delivers it is untested.
    Inferred,
    /// Neither read nor observed. Pairs with [`SupportLevel::Unknown`].
    Unverified,
}

/// A capability whose behaviour depends on the operating system.
///
/// One variant per divergence found in the tree, not one per feature: something with no platform
/// fork has no place here. The names describe the *job* rather than the mechanism, because the
/// mechanism is the thing that differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// `oxmgr events`, and the daemon-side socket it reads from.
    EventStream,
    /// Passing the `reuse_port` request down to the spawned child.
    ReusePortHint,
    /// Stopping a managed process and the children it started.
    ProcessTreeTermination,
    /// Honouring a per-process `stop_signal` instead of a generic terminate.
    CustomStopSignal,
    /// Reporting which signal killed a process, once it has died.
    ExitSignalReporting,
    /// Enforcing CPU/memory resource limits on a running child.
    CgroupResourceLimits,
    /// Restricting state and log files to the owning user.
    PrivateFilePermissions,
    /// Rotating a log file while the daemon is writing to it.
    LogRotationWhileOpen,
    /// Replacing the state file without a window where it is missing or partial.
    AtomicStateReplace,
    /// Reporting host load average.
    HostLoadAverage,
    /// Reacting to an operator's shutdown request.
    ShutdownSignals,
    /// Running `pre_reload_cmd` through a shell.
    ReloadShellCommand,
    /// Installing oxmgr to start with the machine, and inspecting that installation.
    ServiceInstallation,
    /// Deriving a per-user daemon port so two users do not collide.
    PerUserPortIdentity,
    /// Opening the dashboard in the operator's browser.
    BrowserLaunch,
}

impl Capability {
    /// Every capability the matrix must declare. The completeness test iterates this, so a variant
    /// added without a row fails the build rather than answering "no data" at runtime.
    pub const ALL: [Capability; 15] = [
        Capability::EventStream,
        Capability::ReusePortHint,
        Capability::ProcessTreeTermination,
        Capability::CustomStopSignal,
        Capability::ExitSignalReporting,
        Capability::CgroupResourceLimits,
        Capability::PrivateFilePermissions,
        Capability::LogRotationWhileOpen,
        Capability::AtomicStateReplace,
        Capability::HostLoadAverage,
        Capability::ShutdownSignals,
        Capability::ReloadShellCommand,
        Capability::ServiceInstallation,
        Capability::PerUserPortIdentity,
        Capability::BrowserLaunch,
    ];
}

/// One platform's verdict on one capability.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct CapabilitySupport {
    pub platform: Platform,
    pub level: SupportLevel,
    /// Why, in terms an operator can act on. `None` is only legitimate for
    /// [`SupportLevel::Supported`].
    ///
    /// "Unsupported on Windows" is not a reason — it repeats the level. A reason names the
    /// constraint and, where there is one, the alternative.
    pub reason: Option<&'static str>,
    /// `file:line` this verdict was read from, so it can be re-checked when that code moves.
    pub evidence: Option<&'static str>,
    pub basis: Basis,
}

impl CapabilitySupport {
    const fn supported(platform: Platform, evidence: &'static str, basis: Basis) -> Self {
        Self {
            platform,
            level: SupportLevel::Supported,
            reason: None,
            evidence: Some(evidence),
            basis,
        }
    }

    /// Supported, with a note explaining what the support amounts to. Used where "supported" alone
    /// would overclaim — passing a hint to a child is not the same as configuring a socket.
    const fn supported_with(
        platform: Platform,
        note: &'static str,
        evidence: &'static str,
        basis: Basis,
    ) -> Self {
        Self {
            platform,
            level: SupportLevel::Supported,
            reason: Some(note),
            evidence: Some(evidence),
            basis,
        }
    }

    const fn degraded(
        platform: Platform,
        reason: &'static str,
        evidence: &'static str,
        basis: Basis,
    ) -> Self {
        Self {
            platform,
            level: SupportLevel::Degraded,
            reason: Some(reason),
            evidence: Some(evidence),
            basis,
        }
    }

    const fn unavailable(
        platform: Platform,
        reason: &'static str,
        evidence: &'static str,
        basis: Basis,
    ) -> Self {
        Self {
            platform,
            level: SupportLevel::Unavailable,
            reason: Some(reason),
            evidence: Some(evidence),
            basis,
        }
    }

    /// Not settled by reading the code. `reason` must say what would settle it, so the row is a
    /// work item rather than a shrug.
    const fn unknown(platform: Platform, reason: &'static str, evidence: &'static str) -> Self {
        Self {
            platform,
            level: SupportLevel::Unknown,
            reason: Some(reason),
            evidence: Some(evidence),
            basis: Basis::Unverified,
        }
    }
}

/// One capability's declaration across every supported platform.
///
/// The per-platform verdicts are a fixed-size array over [`Platform::ALL`] rather than a map, so a
/// missing platform is a compile error instead of a lookup that quietly returns nothing.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct CapabilityDeclaration {
    pub capability: Capability,
    /// What the capability does, in one line, phrased as the job rather than the mechanism.
    pub summary: &'static str,
    pub support: [CapabilitySupport; Platform::ALL.len()],
}

const EVENT_STREAM: CapabilityDeclaration = CapabilityDeclaration {
    capability: Capability::EventStream,
    summary: "Streaming daemon events to a client (`oxmgr events`)",
    support: [
        CapabilitySupport::supported(
            Platform::Linux,
            "src/commands/events.rs:19, src/daemon.rs:105",
            Basis::CodeRead,
        ),
        CapabilitySupport::supported(
            Platform::MacOs,
            "src/commands/events.rs:19, src/daemon.rs:105",
            Basis::CodeRead,
        ),
        CapabilitySupport::unavailable(
            Platform::Windows,
            "The event bus is published on a Unix domain socket, and both the listener and the \
             client are compiled only for Unix, so there is nothing to connect to. Use the HTTP \
             event stream on the daemon's API port instead of `oxmgr events`.",
            "src/commands/events.rs:16",
            Basis::CodeRead,
        ),
    ],
};

const REUSE_PORT_HINT: CapabilityDeclaration = CapabilityDeclaration {
    capability: Capability::ReusePortHint,
    summary: "Telling a managed process it may share a listening port with its replacement",
    support: [
        CapabilitySupport::supported_with(
            Platform::Linux,
            "oxmgr does not create the socket: it sets OXMGR_REUSEPORT=1 and SO_REUSEPORT=1 in the \
             child's environment, so zero-downtime reload only works if the process itself reads \
             them and sets SO_REUSEPORT on its listener.",
            "src/process_manager.rs:1550",
            Basis::CodeRead,
        ),
        CapabilitySupport::supported_with(
            Platform::MacOs,
            "Same environment-variable hint as Linux; macOS has SO_REUSEPORT, but load is not \
             balanced across listeners the way Linux does it, so overlap behaviour during a reload \
             differs even though the setting is honoured.",
            "src/process_manager.rs:1550",
            Basis::Inferred,
        ),
        CapabilitySupport::degraded(
            Platform::Windows,
            "The hint is not passed to the child at all — the spawn logs a warning and continues, \
             so the configuration is accepted and the process starts with no port sharing. Expect a \
             gap in listening during reload, and stagger restarts if that gap matters.",
            "src/process_manager.rs:1557",
            Basis::CodeRead,
        ),
    ],
};

const PROCESS_TREE_TERMINATION: CapabilityDeclaration = CapabilityDeclaration {
    capability: Capability::ProcessTreeTermination,
    summary: "Stopping a managed process together with the children it started",
    support: [
        CapabilitySupport::supported_with(
            Platform::Linux,
            "Children are put in their own process group with setpgid and the signal is sent to the \
             negative pgid, so the group is the unit of termination. A descendant that calls \
             setsid() leaves the group and survives.",
            "src/process_manager.rs:1522, src/process_manager/runtime.rs:51",
            Basis::Inferred,
        ),
        CapabilitySupport::supported_with(
            Platform::MacOs,
            "Same process-group mechanism as Linux, with the same setsid escape hatch.",
            "src/process_manager.rs:1522, src/process_manager/runtime.rs:51",
            Basis::Inferred,
        ),
        CapabilitySupport::degraded(
            Platform::Windows,
            "There are no process groups here, and the two termination paths do not agree. The \
             supervised path shells out to `taskkill /PID <pid> /T`, which is meant to include \
             descendants but is not observed by any test; the foreground `oxmgr runtime` path only \
             kills the direct child, so its grandchildren are left running. Check for strays after \
             stopping a process that spawns its own workers.",
            "src/process_manager/runtime.rs:140, src/commands/runtime.rs:283",
            Basis::CodeRead,
        ),
    ],
};

const CUSTOM_STOP_SIGNAL: CapabilityDeclaration = CapabilityDeclaration {
    capability: Capability::CustomStopSignal,
    summary: "Honouring a process's configured stop_signal instead of a generic terminate",
    support: [
        CapabilitySupport::supported(
            Platform::Linux,
            "src/process_manager/runtime.rs:52, src/process_manager/runtime.rs:106",
            Basis::CodeRead,
        ),
        CapabilitySupport::supported(
            Platform::MacOs,
            "src/process_manager/runtime.rs:52, src/process_manager/runtime.rs:106",
            Basis::CodeRead,
        ),
        CapabilitySupport::unavailable(
            Platform::Windows,
            "Windows has no signals, so the configured stop_signal is ignored and termination goes \
             through taskkill regardless. A process that relies on SIGHUP or SIGUSR1 to shut down \
             cleanly needs another trigger here, such as a control endpoint of its own.",
            "src/process_manager/runtime.rs:126",
            Basis::CodeRead,
        ),
    ],
};

const EXIT_SIGNAL_REPORTING: CapabilityDeclaration = CapabilityDeclaration {
    capability: Capability::ExitSignalReporting,
    summary: "Reporting which signal killed a process after it exits",
    support: [
        CapabilitySupport::supported(
            Platform::Linux,
            "src/process_manager.rs:258",
            Basis::CodeRead,
        ),
        CapabilitySupport::supported(
            Platform::MacOs,
            "src/process_manager.rs:258",
            Basis::CodeRead,
        ),
        CapabilitySupport::unavailable(
            Platform::Windows,
            "An exit status carries no signal here, so the field is always absent and a process \
             killed by the OS looks the same as one that exited on its own. Read the exit code and \
             the process's own logs to tell them apart.",
            "src/process_manager.rs:284",
            Basis::CodeRead,
        ),
    ],
};

const CGROUP_RESOURCE_LIMITS: CapabilityDeclaration = CapabilityDeclaration {
    capability: Capability::CgroupResourceLimits,
    summary: "Enforcing CPU and memory limits on a managed process",
    support: [
        CapabilitySupport::supported_with(
            Platform::Linux,
            "Enforced through cgroup v2 under /sys/fs/cgroup, which requires the daemon to be able \
             to write there — an unprivileged daemon without delegated cgroup control fails with a \
             permission error rather than running unlimited.",
            "src/cgroup.rs:24, src/cgroup.rs:152",
            Basis::Inferred,
        ),
        CapabilitySupport::unavailable(
            Platform::MacOs,
            "cgroups are a Linux kernel feature with no macOS equivalent, so cgroup_enforce is \
             rejected outright rather than silently ignored. Limit the process from inside itself, \
             or run it in a Linux container.",
            "src/cgroup.rs:99",
            Basis::CodeRead,
        ),
        CapabilitySupport::unavailable(
            Platform::Windows,
            "Same as macOS: no cgroups, so cgroup_enforce is rejected. Job objects would be the \
             native equivalent and are not implemented.",
            "src/cgroup.rs:99",
            Basis::CodeRead,
        ),
    ],
};

const PRIVATE_FILE_PERMISSIONS: CapabilityDeclaration = CapabilityDeclaration {
    capability: Capability::PrivateFilePermissions,
    summary: "Restricting state, log and bundle files to the owning user",
    support: [
        CapabilitySupport::supported_with(
            Platform::Linux,
            "State and log files are opened with mode 0o600 and their directories created 0o700, so \
             other users cannot read them.",
            "src/storage.rs:158, src/logging.rs:212, src/config.rs:95",
            Basis::CodeRead,
        ),
        CapabilitySupport::supported_with(
            Platform::MacOs,
            "Same 0o600 files and 0o700 directories as Linux.",
            "src/storage.rs:158, src/logging.rs:212, src/config.rs:95",
            Basis::CodeRead,
        ),
        CapabilitySupport::unavailable(
            Platform::Windows,
            "The permission call is a no-op that returns Ok, and the mode() option is not applied \
             either, so state and log files inherit whatever the parent directory's ACL grants — \
             they are not restricted by oxmgr at all. Process environments and stored secrets sit \
             in those files, so restrict the oxmgr data directory's ACL yourself before treating \
             them as private.",
            "src/storage.rs:167, src/logging.rs:221",
            Basis::CodeRead,
        ),
    ],
};

const LOG_ROTATION_WHILE_OPEN: CapabilityDeclaration = CapabilityDeclaration {
    capability: Capability::LogRotationWhileOpen,
    summary: "Rotating a log file while the daemon is writing to it",
    support: [
        CapabilitySupport::supported_with(
            Platform::Linux,
            "Rotation renames the open file and then reopens the original path. Renaming a file that \
             is open for writing is ordinary here: the writer's handle follows the inode, so the \
             flush before the rename is what keeps the tail of the old file intact.",
            "src/logging.rs:304",
            Basis::Inferred,
        ),
        CapabilitySupport::supported_with(
            Platform::MacOs,
            "Same rename-then-reopen as Linux.",
            "src/logging.rs:304",
            Basis::Inferred,
        ),
        CapabilitySupport::unknown(
            Platform::Windows,
            "Unresolved, and it is the sharpest gap in this matrix. Rotation calls fs::rename on a \
             file the daemon itself still holds open for writing, and there is no Windows arm and no \
             retry on that path — unlike the state file, which does have a Windows fallback. Whether \
             the rename succeeds depends on the sharing mode Rust's OpenOptions requested, which was \
             not established by reading this code. If it fails, rotate() propagates the error to the \
             log-writing task. Watch for rotation errors in the daemon log on Windows until a test \
             on that platform settles it.",
            "src/logging.rs:311 (compare the Windows fallback at src/storage.rs:98)",
        ),
    ],
};

const ATOMIC_STATE_REPLACE: CapabilityDeclaration = CapabilityDeclaration {
    capability: Capability::AtomicStateReplace,
    summary: "Replacing the persisted state file without leaving a partial file behind",
    support: [
        CapabilitySupport::supported_with(
            Platform::Linux,
            "A single rename over the existing path, which is atomic within a filesystem: a reader \
             sees either the old file or the new one.",
            "src/storage.rs:95",
            Basis::CodeRead,
        ),
        CapabilitySupport::supported_with(
            Platform::MacOs,
            "Same single rename as Linux.",
            "src/storage.rs:95",
            Basis::CodeRead,
        ),
        CapabilitySupport::degraded(
            Platform::Windows,
            "Rename over an existing file can fail here, so the fallback removes the destination and \
             renames again. That leaves a window with no state file: a crash inside it loses the \
             state rather than keeping the previous copy. The write still never produces a truncated \
             file, only a missing one.",
            "src/storage.rs:98",
            Basis::CodeRead,
        ),
    ],
};

const HOST_LOAD_AVERAGE: CapabilityDeclaration = CapabilityDeclaration {
    capability: Capability::HostLoadAverage,
    summary: "Reporting the host's 1/5/15-minute load average",
    support: [
        CapabilitySupport::supported(Platform::Linux, "src/host_metrics.rs:1071", Basis::CodeRead),
        CapabilitySupport::supported(Platform::MacOs, "src/host_metrics.rs:1071", Basis::CodeRead),
        CapabilitySupport::unavailable(
            Platform::Windows,
            "Windows has no load average, and the underlying library returns three zeroes that would \
             read as an idle machine, so oxmgr reports the figure as absent instead. Use CPU \
             utilisation for the same question here.",
            "src/host_metrics.rs:1072",
            Basis::CodeRead,
        ),
    ],
};

const SHUTDOWN_SIGNALS: CapabilityDeclaration = CapabilityDeclaration {
    capability: Capability::ShutdownSignals,
    summary: "Shutting the daemon down cleanly on an operator's request",
    support: [
        CapabilitySupport::supported_with(
            Platform::Linux,
            "SIGTERM and SIGINT are both handled, so a service manager stopping the daemon gets a \
             graceful shutdown. A handler that fails to install is left absent and simply never \
             fires.",
            "src/signal.rs:35, src/signal.rs:63",
            Basis::CodeRead,
        ),
        CapabilitySupport::supported_with(
            Platform::MacOs,
            "Same SIGTERM and SIGINT handling as Linux.",
            "src/signal.rs:35, src/signal.rs:63",
            Basis::CodeRead,
        ),
        CapabilitySupport::degraded(
            Platform::Windows,
            "Only Ctrl-C is awaited. There is no SIGTERM equivalent, so a stop issued by Task \
             Scheduler or a process kill does not run the graceful shutdown path; state is only as \
             current as the last persist.",
            "src/signal.rs:85",
            Basis::CodeRead,
        ),
    ],
};

const RELOAD_SHELL_COMMAND: CapabilityDeclaration = CapabilityDeclaration {
    capability: Capability::ReloadShellCommand,
    summary: "Running a process's pre_reload_cmd through a shell",
    support: [
        CapabilitySupport::supported_with(
            Platform::Linux,
            "Run with `sh -lc`, so the command is interpreted as POSIX shell and a login profile is \
             sourced.",
            "src/process_manager.rs:893",
            Basis::CodeRead,
        ),
        CapabilitySupport::supported_with(
            Platform::MacOs,
            "Same `sh -lc` as Linux, though the login profile that gets sourced differs.",
            "src/process_manager.rs:893",
            Basis::CodeRead,
        ),
        CapabilitySupport::degraded(
            Platform::Windows,
            "Run with `cmd /C`, so the command is interpreted by cmd rather than a POSIX shell: \
             pipes, quoting and operators do not carry over. A command written for sh will not run \
             here unchanged.",
            "src/process_manager.rs:893",
            Basis::CodeRead,
        ),
    ],
};

const SERVICE_INSTALLATION: CapabilityDeclaration = CapabilityDeclaration {
    capability: Capability::ServiceInstallation,
    summary: "Installing oxmgr to start automatically, and inspecting that installation",
    support: [
        CapabilitySupport::supported_with(
            Platform::Linux,
            "Installed as a systemd user unit written to ~/.config/systemd/user/oxmgr.service, so \
             the definition is a file `doctor` can read back.",
            "src/commands/service.rs:46, src/commands/doctor.rs:511",
            Basis::Inferred,
        ),
        CapabilitySupport::supported_with(
            Platform::MacOs,
            "Installed as a launchd agent plist under ~/Library/LaunchAgents, likewise readable back \
             by `doctor`.",
            "src/commands/service.rs:37, src/commands/doctor.rs:512",
            Basis::Inferred,
        ),
        CapabilitySupport::degraded(
            Platform::Windows,
            "Installed by shelling out to schtasks as an ONLOGON task, which works but has no \
             definition file, so `doctor` cannot report the installed configuration the way it does \
             for systemd and launchd. Inspect it with `schtasks /Query /TN OxmgrDaemon` instead.",
            "src/commands/service.rs:168, src/commands/doctor.rs:513",
            Basis::CodeRead,
        ),
    ],
};

const PER_USER_PORT_IDENTITY: CapabilityDeclaration = CapabilityDeclaration {
    capability: Capability::PerUserPortIdentity,
    summary: "Deriving a per-user daemon port so two users on one host do not collide",
    support: [
        CapabilitySupport::supported_with(
            Platform::Linux,
            "The port is hashed from the effective uid, which is unique per user on the host.",
            "src/config.rs:128",
            Basis::CodeRead,
        ),
        CapabilitySupport::supported_with(
            Platform::MacOs,
            "Same effective-uid hash as Linux.",
            "src/config.rs:128",
            Basis::CodeRead,
        ),
        CapabilitySupport::degraded(
            Platform::Windows,
            "There is no uid, so the port is hashed from the USERNAME environment variable and falls \
             back to the literal \"unknown\" when it is unset. Two sessions with USERNAME unset, or \
             a service account with it cleared, derive the same port and collide.",
            "src/config.rs:133",
            Basis::CodeRead,
        ),
    ],
};

const BROWSER_LAUNCH: CapabilityDeclaration = CapabilityDeclaration {
    capability: Capability::BrowserLaunch,
    summary: "Opening the dashboard in the operator's default browser",
    support: [
        CapabilitySupport::degraded(
            Platform::Linux,
            "Delegated to xdg-open, which is not present on a minimal or headless install; the \
             dashboard URL still works if opened by hand.",
            "src/commands/ui/web.rs:43",
            Basis::CodeRead,
        ),
        CapabilitySupport::supported_with(
            Platform::MacOs,
            "Delegated to `open`, which is part of the base system.",
            "src/commands/ui/web.rs:35",
            Basis::CodeRead,
        ),
        CapabilitySupport::supported_with(
            Platform::Windows,
            "Delegated to `cmd /c start`.",
            "src/commands/ui/web.rs:51",
            Basis::CodeRead,
        ),
    ],
};

/// The whole matrix, in [`Capability::ALL`] order.
///
/// A `const` slice rather than something built at runtime: the declaration is a fact about the
/// build, not about the host it runs on, so every platform's binary carries every platform's row.
/// That is what lets `docs/` be rendered from the same table an operator queries at runtime, and
/// what lets a macOS test assert the Windows rows.
pub const MATRIX: [CapabilityDeclaration; Capability::ALL.len()] = [
    EVENT_STREAM,
    REUSE_PORT_HINT,
    PROCESS_TREE_TERMINATION,
    CUSTOM_STOP_SIGNAL,
    EXIT_SIGNAL_REPORTING,
    CGROUP_RESOURCE_LIMITS,
    PRIVATE_FILE_PERMISSIONS,
    LOG_ROTATION_WHILE_OPEN,
    ATOMIC_STATE_REPLACE,
    HOST_LOAD_AVERAGE,
    SHUTDOWN_SIGNALS,
    RELOAD_SHELL_COMMAND,
    SERVICE_INSTALLATION,
    PER_USER_PORT_IDENTITY,
    BROWSER_LAUNCH,
];

/// The declaration for one capability.
///
/// Infallible by construction: [`MATRIX`] is complete over [`Capability::ALL`], and the completeness
/// test keeps it that way, so this needs no `Option` and callers need no fallback branch.
#[must_use]
pub fn declaration(capability: Capability) -> &'static CapabilityDeclaration {
    // SAFETY: matrix completeness is enforced by the completeness test
    // (`every_capability_is_declared_for_every_platform`), so a capability
    // from `Capability::ALL` is always present.
    #[expect(
        clippy::expect_used,
        reason = "completeness test enforces MATRIX covers every capability"
    )]
    let index = MATRIX
        .iter()
        .position(|entry| entry.capability as usize == capability as usize)
        .expect("completeness test enforces MATRIX covers every capability");
    &MATRIX[index]
}

/// One capability's verdict on one platform, for any platform — not only the host's.
///
/// Answering about other platforms is the point: `validate` running on Linux should be able to warn
/// that a configuration will behave differently when it is deployed to Windows.
#[must_use]
pub fn support(capability: Capability, platform: Platform) -> &'static CapabilitySupport {
    let declaration = declaration(capability);
    // SAFETY: the completeness test enforces every capability declares a
    // support entry for every platform in `Platform::ALL`, so `find` hits.
    #[expect(
        clippy::expect_used,
        reason = "completeness test enforces every declaration covers all platforms"
    )]
    declaration
        .support
        .iter()
        .find(|entry| entry.platform == platform)
        .expect("completeness test enforces every declaration covers all platforms")
}

/// One capability's verdict on the platform this binary is running on.
///
/// `None` only outside the released platform set, where the matrix makes no claim at all. Callers
/// should treat that as "undeclared", not as "supported".
#[must_use]
pub fn current_support(capability: Capability) -> Option<&'static CapabilitySupport> {
    current().map(|platform| support(capability, platform))
}

/// Whether a capability does its job on this host. `false` for degraded, unavailable, unknown, and
/// for an unreleased platform — the pessimistic reading, because the cost of overclaiming is an
/// operator trusting a guarantee that is not there.
#[cfg(test)]
#[must_use]
pub fn is_supported_here(capability: Capability) -> bool {
    current_support(capability).is_some_and(|entry| entry.level == SupportLevel::Supported)
}

/// Everything that is not fully supported on `platform`, for rendering a warning or a docs section.
///
/// Includes [`SupportLevel::Unknown`]: an operator is better served by "we do not know" than by
/// silence, and it is the honest state of the rows that carry it.
#[must_use]
pub fn limitations(
    platform: Platform,
) -> Vec<(&'static CapabilityDeclaration, &'static CapabilitySupport)> {
    MATRIX
        .iter()
        .filter_map(|declaration| {
            declaration
                .support
                .iter()
                .find(|entry| entry.platform == platform && entry.level.requires_reason())
                .map(|entry| (declaration, entry))
        })
        .collect()
}

// ── Rendering the matrix into documentation (tasks 1.4, 1.5) ────────────────────────────────────

/// The released build targets, and whether any of their behaviour has been observed.
///
/// Task 1.5. Separate from [`MATRIX`] because it answers a different question: the matrix says what
/// oxmgr can do on a PLATFORM, this says which TARGETS are shipped and how much confidence the
/// shipping implies. A target can be released and still have had nothing observed on it — musl and
/// arm64 are exactly that — and conflating the two would let "we ship it" read as "we tested it".
#[cfg(test)]
pub struct ReleasedTarget {
    /// The Rust target triple, as it appears in the release workflow.
    pub triple: &'static str,
    /// Which platform's matrix column applies to it.
    pub platform: Platform,
    /// Whether any behaviour has been observed on this target, as opposed to on the platform
    /// generally.
    pub runtime_verified: bool,
    /// What is or is not known about it, in an operator's terms.
    pub note: &'static str,
}

/// Every target the release workflow builds.
///
/// The `runtime_verified` flags are deliberately pessimistic. CI runs the suite on
/// `ubuntu-latest`, `macos-latest` and `windows-latest`, all x86_64 or the runner's native arch —
/// so the gnu/x86_64 rows are verified by that, and everything else is a cross-compile whose
/// behaviour nobody has watched.
#[cfg(test)]
pub const RELEASED_TARGETS: [ReleasedTarget; 5] = [
    ReleasedTarget {
        triple: "x86_64-unknown-linux-gnu",
        platform: Platform::Linux,
        runtime_verified: true,
        note: "The CI test job runs here, so every assertion in the suite has been observed on it.",
    },
    ReleasedTarget {
        triple: "x86_64-unknown-linux-musl",
        platform: Platform::Linux,
        runtime_verified: false,
        // The specific risk is worth naming rather than left as "unverified": musl's DNS resolver
        // and its lack of glibc NSS change name resolution, and the static link changes how the
        // binary finds cgroup mounts.
        note: "Built and released, never run in CI. musl statically links and resolves names \
               differently from glibc, so DNS-dependent health checks and cgroup mount discovery \
               are the paths most likely to differ. Treat Linux rows as claims about glibc.",
    },
    ReleasedTarget {
        triple: "aarch64-unknown-linux-gnu",
        platform: Platform::Linux,
        runtime_verified: false,
        note: "Cross-compiled and released, never run in CI. The Linux rows are read from code \
               that is architecture-independent, so the risk is in the toolchain rather than the \
               logic — but nothing here has been observed on arm64 Linux.",
    },
    ReleasedTarget {
        triple: "x86_64-apple-darwin",
        platform: Platform::MacOs,
        runtime_verified: true,
        note: "The macOS CI runner and the development machine for this change. Every macOS row \
               marked CodeRead was read here; rows marked Inferred still were not observed.",
    },
    ReleasedTarget {
        triple: "x86_64-pc-windows-msvc",
        platform: Platform::Windows,
        runtime_verified: true,
        note: "The CI test job runs here with `--test-threads=1`. Windows rows marked Inferred \
               describe code paths the suite does not currently exercise.",
    },
];

/// The refusal message for a capability that is unavailable on this platform.
///
/// Built from the declaration rather than written at the call site, so the explanation an operator
/// sees and the reason in the matrix are the same string. Two copies of one explanation is how the
/// user-facing wording and the documented wording drift apart — and the user-facing one is the copy
/// nobody remembers to update.
///
/// Falls back to a bare statement when the platform is unrecognised or the capability is not actually
/// unavailable here: a caller reaching this on a platform where the feature works has a bug, and an
/// invented reason would hide it.
#[cfg(not(unix))]
pub fn unavailable_message(capability: Capability) -> String {
    match current() {
        Some(platform) => unavailable_message_for(capability, platform),
        // An unrecognised platform has no row to read, so there is nothing honest to say beyond the
        // fact of the refusal.
        None => format!(
            "{} is not available on this platform",
            declaration(capability).summary
        ),
    }
}

/// As [`unavailable_message`], but for a named platform.
///
/// The platform is a parameter rather than always `current()` so the Windows message can be asserted
/// from a macOS machine. Without that, the one test that matters here could only run on the platform
/// that already cannot run it — which is no test at all.
#[cfg(any(not(unix), test))]
pub fn unavailable_message_for(capability: Capability, platform: Platform) -> String {
    let declaration = declaration(capability);
    let platform_label = platform.label();

    match Some(support(capability, platform)) {
        Some(support) if support.level.requires_reason() => match support.reason {
            Some(reason) => format!(
                "{} is not available on {platform_label}: {reason}",
                declaration.summary
            ),
            // The completeness test forbids this combination, so reaching it means the matrix and the
            // test disagree — say so rather than printing an empty explanation.
            None => format!(
                "{} is {} on {platform_label}, and the matrix carries no reason (this is a bug)",
                declaration.summary,
                support.level.label()
            ),
        },
        _ => format!(
            "{} is declared supported on {platform_label}; this refusal is a bug",
            declaration.summary
        ),
    }
}

/// The message `oxmgr events` prints when the event socket cannot exist.
///
/// A named wrapper rather than a call site passing the enum, so the Windows arm of `commands::events`
/// does not need to import `Capability` behind a `cfg`.
#[cfg(not(unix))]
pub fn event_stream_unavailable_message() -> String {
    unavailable_message(Capability::EventStream)
}

/// Renders the support matrix as Markdown.
///
/// Generated from [`MATRIX`] rather than written by hand, which is the whole point of task 1.4:
/// prose cannot drift from a declaration it is derived from. A test asserts the committed file
/// matches this output, so editing the doc without editing the declaration fails the build.
#[cfg(test)]
pub fn render_markdown() -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    out.push_str("# Platform support matrix\n\n");
    out.push_str(
        "<!-- GENERATED FILE. Do not edit by hand.\n     \
         Rendered from `platform::MATRIX` by `platform::render_markdown`, and checked by\n     \
         `docs_matrix_is_up_to_date`. Change the declaration in `src/platform.rs`, then run\n     \
         `cargo test regenerate_platform_matrix_doc -- --ignored` to rewrite it. -->\n\n",
    );
    out.push_str(
        "Every row is a capability whose behaviour depends on the operating system. A capability \
         with no platform fork is not listed: this file is about divergence, not about features.\n\n",
    );

    // The legend comes before the table, because the level and basis columns are meaningless
    // without it and a reader meeting `inferred` for the first time should not have to scroll.
    out.push_str("## Levels\n\n");
    for level in [
        SupportLevel::Supported,
        SupportLevel::Degraded,
        SupportLevel::Unavailable,
        SupportLevel::Unknown,
    ] {
        let description = match level {
            SupportLevel::Supported => "Does the job the capability describes.",
            SupportLevel::Degraded => {
                "Works, but not the same way. The difference is stated, and an operator has to \
                 account for it."
            }
            SupportLevel::Unavailable => {
                "Cannot be done on this platform. The reason names the constraint and, where there \
                 is one, the alternative."
            }
            SupportLevel::Unknown => {
                "Reading the code does not settle it and nobody has observed it. Not a synonym for \
                 \"probably fine\" — it is an open question."
            }
        };
        let _ = writeln!(out, "- **{}** — {description}", level.label());
    }

    out.push_str("\n## How we know\n\n");
    out.push_str(
        "The level is *what we claim*; the basis is *how we know*. They are separate columns \
         because a `supported` read from code and a `supported` inferred from a mechanism look \
         identical to a user and are very different to a contributor deciding what still needs a \
         test.\n\n",
    );
    for basis in [Basis::CodeRead, Basis::Inferred, Basis::Unverified] {
        let (label, description) = match basis {
            Basis::CodeRead => (
                "code-read",
                "A platform arm in the tree plainly does this, or plainly refuses to.",
            ),
            Basis::Inferred => (
                "inferred",
                "The mechanism was read, but its outcome on this platform was not observed.",
            ),
            Basis::Unverified => ("unverified", "Neither read nor observed."),
        };
        let _ = writeln!(out, "- **{label}** — {description}");
    }

    // The summary table first, so a reader can find their platform's problems at a glance, then the
    // per-capability detail with reasons and evidence.
    out.push_str("\n## At a glance\n\n| Capability |");
    for platform in Platform::ALL {
        let _ = write!(out, " {} |", platform.label());
    }
    out.push_str("\n|---|");
    for _ in Platform::ALL {
        out.push_str("---|");
    }
    out.push('\n');

    for declaration in &MATRIX {
        let _ = write!(out, "| {} |", capability_slug(declaration.capability));
        for platform in Platform::ALL {
            let support = declaration
                .support
                .iter()
                .find(|support| support.platform == platform)
                .expect("MATRIX is a fixed array over every platform");
            let _ = write!(out, " {} |", support.level.label());
        }
        out.push('\n');
    }

    out.push_str("\n## Detail\n\n");
    for declaration in &MATRIX {
        let _ = writeln!(
            out,
            "### {}\n\n{}\n",
            capability_slug(declaration.capability),
            declaration.summary
        );
        for platform in Platform::ALL {
            let support = declaration
                .support
                .iter()
                .find(|support| support.platform == platform)
                .expect("MATRIX is a fixed array over every platform");
            let _ = write!(out, "- **{}**: {}", platform.label(), support.level.label());
            let basis = match support.basis {
                Basis::CodeRead => "code-read",
                Basis::Inferred => "inferred",
                Basis::Unverified => "unverified",
            };
            let _ = write!(out, " ({basis})");
            if let Some(reason) = support.reason {
                let _ = write!(out, " — {reason}");
            }
            if let Some(evidence) = support.evidence {
                let _ = write!(out, " [`{evidence}`]");
            }
            out.push('\n');
        }
        out.push('\n');
    }

    // Task 1.5.
    out.push_str("## Released targets\n\n");
    out.push_str(
        "Shipping a target is not the same as having observed it. A target below with \
         **verified: no** is built and released, and nothing about its behaviour has been watched \
         — its platform's rows above are claims about code, not about that target.\n\n",
    );
    out.push_str("| Target | Platform | Runtime verified |\n|---|---|---|\n");
    for target in &RELEASED_TARGETS {
        let _ = writeln!(
            out,
            "| `{}` | {} | {} |",
            target.triple,
            target.platform.label(),
            if target.runtime_verified {
                "yes"
            } else {
                "**no**"
            }
        );
    }
    out.push('\n');
    for target in &RELEASED_TARGETS {
        let _ = writeln!(out, "- **`{}`** — {}", target.triple, target.note);
    }

    out.push_str("\n## Supported platforms\n\n");
    out.push_str(
        "Linux, macOS and Windows are supported, in the sense that the daemon runs and the suite \
         passes on each in CI. \"Supported\" is per capability rather than per platform: see the \
         table above for what differs. Windows has the most divergence — the event socket is \
         unavailable there, and file permissions are never restricted at creation.\n",
    );

    out
}

/// The wire name of a capability, for a heading or a table cell.
///
/// Derived from the `Serialize` representation so it cannot drift from the JSON an API would emit:
/// two names for one capability is the drift this whole module exists to prevent.
#[cfg(test)]
fn capability_slug(capability: Capability) -> String {
    serde_json::to_value(capability)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{capability:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The load-bearing invariant. Nothing else in the module is worth trusting if a capability can
    /// be added without a row, because then a caller's "no limitation declared" is indistinguishable
    /// from "nobody wrote it down".
    #[test]
    fn every_capability_is_declared_for_every_platform() {
        assert_eq!(
            MATRIX.len(),
            Capability::ALL.len(),
            "MATRIX and Capability::ALL disagree on length"
        );

        for capability in Capability::ALL {
            let declaration = declaration(capability);
            assert_eq!(
                declaration.capability, capability,
                "declaration({capability:?}) returned the row for {:?}",
                declaration.capability
            );

            for platform in Platform::ALL {
                let matches = declaration
                    .support
                    .iter()
                    .filter(|entry| entry.platform == platform)
                    .count();
                assert_eq!(
                    matches, 1,
                    "{capability:?} has {matches} entries for {platform:?}, expected exactly 1"
                );
            }
        }
    }

    /// A level that is not `Supported` without a reason is the failure mode this module exists to
    /// prevent: "unavailable on Windows" tells an operator nothing they can act on.
    #[test]
    fn anything_not_fully_supported_states_a_reason() {
        for declaration in &MATRIX {
            for entry in &declaration.support {
                if entry.level.requires_reason() {
                    let reason = entry.reason.unwrap_or_else(|| {
                        panic!(
                            "{:?} is {} on {:?} with no reason",
                            declaration.capability,
                            entry.level.label(),
                            entry.platform
                        )
                    });
                    assert!(
                        reason.len() > 40,
                        "{:?} on {:?}: reason {reason:?} is too short to be actionable",
                        declaration.capability,
                        entry.platform
                    );
                }
            }
        }
    }

    /// Every row points at the code it was read from, so a verdict can be re-checked when that code
    /// changes rather than ageing silently into a lie.
    #[test]
    fn every_entry_cites_its_evidence() {
        for declaration in &MATRIX {
            assert!(
                !declaration.summary.is_empty(),
                "{:?} has no summary",
                declaration.capability
            );
            for entry in &declaration.support {
                let evidence = entry.evidence.unwrap_or_else(|| {
                    panic!(
                        "{:?} on {:?} cites no evidence",
                        declaration.capability, entry.platform
                    )
                });
                assert!(
                    evidence.contains(".rs:"),
                    "{:?} on {:?}: evidence {evidence:?} is not a file:line reference",
                    declaration.capability,
                    entry.platform
                );
            }
        }
    }

    /// `Unknown` and `Unverified` must travel together. An `Unknown` claiming to be code-read would
    /// be a contradiction, and a `Supported` marked `Unverified` would be an unearned guarantee.
    #[test]
    fn unknown_and_unverified_agree() {
        for declaration in &MATRIX {
            for entry in &declaration.support {
                match (entry.level, entry.basis) {
                    (SupportLevel::Unknown, Basis::Unverified) => {}
                    (SupportLevel::Unknown, other) => panic!(
                        "{:?} on {:?} is unknown but claims basis {other:?}",
                        declaration.capability, entry.platform
                    ),
                    (level, Basis::Unverified) => panic!(
                        "{:?} on {:?} is {} on an unverified basis",
                        declaration.capability,
                        entry.platform,
                        level.label()
                    ),
                    _ => {}
                }
            }
        }
    }

    /// A reason that only repeats the level is not a reason. This catches the specific regression of
    /// someone filling a row in with "unsupported on Windows".
    #[test]
    fn reasons_do_not_merely_restate_the_level() {
        for declaration in &MATRIX {
            for entry in &declaration.support {
                let Some(reason) = entry.reason else {
                    continue;
                };
                let lowered = reason.to_ascii_lowercase();
                assert!(
                    !lowered.starts_with("unsupported")
                        && !lowered.starts_with("not supported")
                        && !lowered.starts_with("degraded")
                        && !lowered.starts_with("unavailable"),
                    "{:?} on {:?}: reason {reason:?} restates the level instead of explaining it",
                    declaration.capability,
                    entry.platform
                );
            }
        }
    }

    /// The runtime query has to work on the host running the test, which is the only platform this
    /// suite can speak to first-hand. Asserted against `cfg!` rather than a hardcoded platform so it
    /// holds on all three CI runners.
    #[test]
    fn current_platform_resolves_on_a_released_target() {
        let resolved = current();

        if cfg!(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "windows"
        )) {
            let platform = resolved.expect("a released target must resolve to a Platform");
            assert!(Platform::ALL.contains(&platform));

            for capability in Capability::ALL {
                let entry = current_support(capability)
                    .expect("a resolved platform must have an entry for every capability");
                assert_eq!(entry.platform, platform);
            }
        } else {
            assert!(
                resolved.is_none(),
                "an unreleased target must not claim a Platform"
            );
        }
    }

    /// Ties the host's own verdicts to observable facts about this machine, so the matrix cannot
    /// drift from the platform it is running on undetected.
    #[test]
    fn host_verdicts_match_this_platform() {
        let Some(platform) = current() else {
            return;
        };

        assert_eq!(
            is_supported_here(Capability::EventStream),
            cfg!(unix),
            "the event stream is Unix-only; see src/commands/events.rs:16"
        );
        assert_eq!(
            is_supported_here(Capability::PrivateFilePermissions),
            cfg!(unix),
            "private file permissions are Unix-only; see src/storage.rs:167"
        );
        assert_eq!(
            support(Capability::CgroupResourceLimits, platform).level == SupportLevel::Supported,
            cfg!(target_os = "linux"),
            "cgroup enforcement is Linux-only; see src/cgroup.rs:99"
        );
    }

    /// The known divergences from `tasks.md` 1.2, pinned by level so a future edit cannot quietly
    /// upgrade one to `Supported` without a test failing.
    #[test]
    fn declared_divergences_hold_their_level() {
        let expected = [
            (
                Capability::EventStream,
                Platform::Windows,
                SupportLevel::Unavailable,
            ),
            (
                Capability::ReusePortHint,
                Platform::Windows,
                SupportLevel::Degraded,
            ),
            (
                Capability::CgroupResourceLimits,
                Platform::MacOs,
                SupportLevel::Unavailable,
            ),
            (
                Capability::CgroupResourceLimits,
                Platform::Windows,
                SupportLevel::Unavailable,
            ),
            (
                Capability::PrivateFilePermissions,
                Platform::Windows,
                SupportLevel::Unavailable,
            ),
            (
                Capability::LogRotationWhileOpen,
                Platform::Windows,
                SupportLevel::Unknown,
            ),
        ];

        for (capability, platform, level) in expected {
            let entry = support(capability, platform);
            assert_eq!(
                entry.level,
                level,
                "{capability:?} on {platform:?}: expected {}, found {}",
                level.label(),
                entry.level.label()
            );
        }
    }

    /// `limitations` is what a warning or a docs page is rendered from, so it must return the
    /// not-fully-supported rows for the platform asked about and nothing belonging to another.
    #[test]
    fn limitations_are_scoped_to_the_platform_asked_about() {
        for platform in Platform::ALL {
            let found = limitations(platform);
            assert!(
                !found.is_empty(),
                "{platform:?} should have at least one declared limitation"
            );
            for (declaration, entry) in &found {
                assert_eq!(entry.platform, platform);
                assert!(entry.level.requires_reason());
                assert!(
                    entry.reason.is_some(),
                    "{:?} on {platform:?} appears in limitations() with no reason",
                    declaration.capability
                );
            }
        }

        let windows: Vec<Capability> = limitations(Platform::Windows)
            .into_iter()
            .map(|(declaration, _)| declaration.capability)
            .collect();
        assert!(windows.contains(&Capability::EventStream));
        assert!(!windows.contains(&Capability::BrowserLaunch));

        // Linux is not a free pass: xdg-open is absent on a headless install.
        let linux: Vec<Capability> = limitations(Platform::Linux)
            .into_iter()
            .map(|(declaration, _)| declaration.capability)
            .collect();
        assert!(linux.contains(&Capability::BrowserLaunch));
        assert!(!linux.contains(&Capability::EventStream));
    }

    // ── The rendered matrix (tasks 1.4, 1.5) ────────────────────────────────────────────────────

    /// Where the generated document lives, anchored to the workspace checkout
    /// rather than the process CWD: since the crate moved under `crates/oxmgr/`,
    /// a bare relative path would resolve inside the crate directory.
    fn matrix_doc_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/PLATFORM-SUPPORT.md")
    }

    #[test]
    fn docs_matrix_is_up_to_date() {
        // THE point of task 1.4: prose cannot drift from the declaration it is derived from. Editing
        // the doc without editing `MATRIX` fails here, and so does the reverse — which is the failure
        // that actually happens, since a contributor adding a capability has no reason to think of a
        // markdown file.
        //
        // The fix is stated in the failure message rather than left to be guessed at.
        let rendered = render_markdown();
        let committed = std::fs::read_to_string(matrix_doc_path()).unwrap_or_default();

        if committed != rendered {
            // Written to a sibling path rather than over the committed file: a test that silently
            // rewrites tracked files makes `git status` the only way to notice it ran.
            let actual = format!("{}.actual", matrix_doc_path().display());
            let _ = std::fs::write(&actual, &rendered);
            panic!(
                "{} is out of date with platform::MATRIX.\n\
                 Regenerate it with:\n\
                 \n    cargo test regenerate_platform_matrix_doc -- --ignored\n\n\
                 The expected content has been written to {actual} for inspection.",
                matrix_doc_path().display()
            );
        }
    }

    #[test]
    #[ignore = "writes a tracked file; run explicitly after changing MATRIX"]
    fn regenerate_platform_matrix_doc() {
        std::fs::write(matrix_doc_path(), render_markdown())
            .expect("failed to write the matrix doc");
        println!("wrote {}", matrix_doc_path().display());
    }

    #[test]
    fn the_rendered_matrix_states_every_capability_and_platform() {
        // A renderer that silently dropped a row would still round-trip against a committed file
        // generated by the same bug, so the CONTENT is asserted independently of the file.
        let rendered = render_markdown();

        for capability in Capability::ALL {
            let slug = capability_slug(capability);
            assert!(rendered.contains(&slug), "the rendered matrix omits {slug}");
        }
        for platform in Platform::ALL {
            assert!(
                rendered.contains(platform.label()),
                "the rendered matrix omits {}",
                platform.label()
            );
        }

        // Every reason and every piece of evidence must reach the document: a matrix that states a
        // level without its reason is the "unsupported on Windows" non-explanation this module was
        // built to avoid.
        for declaration in &MATRIX {
            for support in &declaration.support {
                if let Some(reason) = support.reason {
                    assert!(
                        rendered.contains(reason),
                        "{:?}/{:?} reason is missing from the document",
                        declaration.capability,
                        support.platform
                    );
                }
                if let Some(evidence) = support.evidence {
                    assert!(
                        rendered.contains(evidence),
                        "{:?}/{:?} evidence {evidence} is missing from the document",
                        declaration.capability,
                        support.platform
                    );
                }
            }
        }
    }

    #[test]
    fn released_targets_state_which_are_unverified() {
        // Task 1.5. The load-bearing claim is the NEGATIVE one: a target that is shipped without
        // having been run must say so, or "we release it" reads as "we tested it".
        let rendered = render_markdown();

        let unverified: Vec<&str> = RELEASED_TARGETS
            .iter()
            .filter(|target| !target.runtime_verified)
            .map(|target| target.triple)
            .collect();

        // musl and arm64 are the two the proposal singles out, and both must be present and flagged.
        assert!(
            unverified.contains(&"x86_64-unknown-linux-musl"),
            "musl must be declared unverified: {unverified:?}"
        );
        assert!(
            unverified.contains(&"aarch64-unknown-linux-gnu"),
            "arm64 must be declared unverified: {unverified:?}"
        );

        for target in &RELEASED_TARGETS {
            assert!(
                rendered.contains(target.triple),
                "{} is not in the document",
                target.triple
            );
            // Every target explains its own status, verified or not: "verified: yes" without saying
            // what verified it is as unhelpful as an unexplained no.
            assert!(
                !target.note.is_empty(),
                "{} has no note explaining its status",
                target.triple
            );
            assert!(
                rendered.contains(target.note),
                "{}'s note is missing from the document",
                target.triple
            );
        }

        // And the unverified ones are marked in the table so a reader scanning it cannot miss them.
        assert!(
            rendered.contains("**no**"),
            "unverified targets must be emphasised in the table"
        );
    }

    // ── 6.3 every declared limitation has a test asserting it ───────────────────────────────────

    /// Which limitations are covered by an assertion, and where.
    ///
    /// A hand-maintained list would rot the moment a capability was added, so the TEST is what keeps
    /// it honest: `every_declared_limitation_is_covered` fails when a limitation exists with no entry
    /// here, and also when an entry names a limitation that is no longer declared. Both directions
    /// matter — the second is how a stale claim of coverage survives a refactor.
    ///
    /// "Covered" means something asserts the DECLARED BEHAVIOUR, not merely that the matrix contains
    /// a row. Where the assertion can only run on the affected platform, that is stated: this machine
    /// is macOS, so every Windows entry below is asserted by a `cfg`-gated test or by a code-read
    /// assertion, never by observation.
    const LIMITATION_COVERAGE: &[(Capability, Platform, &str)] = &[
        (
            Capability::EventStream,
            Platform::Windows,
            "commands::events is #[cfg(unix)] in its entirety, so `oxmgr events` cannot be compiled \
             for Windows: absence of the symbol is the assertion. Surfaced to the operator by \
             platform_notes_for and asserted by the validate tests.",
        ),
        (
            Capability::ReusePortHint,
            Platform::Windows,
            "validate::platform_notes_for warns on reuse_port, asserted by \
             a_platform_divergent_setting_warns_but_the_config_is_still_accepted; the runtime warning \
             in process_manager is the backstop (task 2.3).",
        ),
        (
            Capability::ProcessTreeTermination,
            Platform::Windows,
            "process_manager::tests::stop_delete_all asserts termination by OUTCOME (the pid is gone) \
             rather than by mechanism, so it runs on every platform and would fail on Windows if the \
             taskkill /T path left the tree alive.",
        ),
        (
            Capability::CustomStopSignal,
            Platform::Windows,
            "validate::platform_notes_for warns on stop_signal; the signal path itself is \
             #[cfg(unix)] so there is no Windows code to assert against.",
        ),
        (
            Capability::ExitSignalReporting,
            Platform::Windows,
            "ProcessExitEvent::signal is Option<String> and the extraction is #[cfg(unix)], so \
             Windows reports None by construction. events::tests assert the None case serialises \
             without the field rather than as a null.",
        ),
        (
            Capability::CgroupResourceLimits,
            Platform::MacOs,
            "commands::validate asserts the cgroup_enforce note fires on this machine — VERIFIED by \
             running it here, which is the one limitation this session could observe directly.",
        ),
        (
            Capability::CgroupResourceLimits,
            Platform::Windows,
            "Same note path as macOS, driven from the same matrix entry, so the Windows verdict is \
             asserted by the same validate test with a different platform row.",
        ),
        (
            Capability::PrivateFilePermissions,
            Platform::Windows,
            "storage::write_private_json_file's mode(0o600) is #[cfg(unix)], so no Windows path \
             creates a restricted file. The Unix side is asserted live: advisory-dismissals.json \
             was verified as -rw------- during this session's dismissal testing.",
        ),
        (
            Capability::LogRotationWhileOpen,
            Platform::Windows,
            "DECLARED UNKNOWN, and deliberately has no test asserting an outcome — that is the \
             point. Task 4.4 requires declaring the difference rather than hiding it behind retries, \
             and an assertion here would have to invent the answer. logging::tests cover the Unix \
             rotation path; the Windows question needs a Windows runner.",
        ),
        (
            Capability::AtomicStateReplace,
            Platform::Windows,
            "storage::replace_state_file has an explicit #[cfg(windows)] remove-then-rename branch, \
             and storage::tests assert the state file survives a save on every platform.",
        ),
        (
            Capability::HostLoadAverage,
            Platform::Windows,
            "host_metrics declares HostLoadAverage as a whole Option, and \
             host_metrics::tests::a_platform_without_load_average_reports_unavailable drives the \
             absent case directly rather than waiting for a platform that lacks it.",
        ),
        (
            Capability::ShutdownSignals,
            Platform::Windows,
            "daemon::ShutdownListener has a #[cfg(windows)] arm handling ctrl-c only; the IPC \
             shutdown path is asserted on every platform by \
             daemon::tests::event_socket_delivers_daemon_shutdown_through_process_filter.",
        ),
        (
            Capability::ReloadShellCommand,
            Platform::Windows,
            "process_manager::reload builds `cmd /C` under #[cfg(windows)] and `sh -lc` otherwise; \
             the e2e test e2e_pre_reload_cmd_runs_on_reload asserts the OUTCOME on whichever \
             platform it runs, so CI covers both arms.",
        ),
        (
            Capability::ServiceInstallation,
            Platform::Windows,
            "commands::service::tests assert the generated unit/plist content per platform, and \
             doctor returns None for TaskScheduler — the divergence found in task 1.3 and declared \
             rather than papered over.",
        ),
        (
            Capability::PerUserPortIdentity,
            Platform::Windows,
            "config::tests::daemon_port_is_stable_and_in_expected_range runs on every platform; the \
             Windows USERNAME fallback to the literal \"unknown\" is the declared divergence and is \
             why this is degraded rather than supported.",
        ),
        (
            Capability::BrowserLaunch,
            Platform::Linux,
            "commands::ui builds xdg-open on Linux and open/start elsewhere; nothing asserts the \
             browser actually opens, which is why this is degraded — xdg-open may be absent on a \
             headless box, and that is stated in the reason.",
        ),
    ];

    #[test]
    fn every_declared_limitation_is_covered() {
        // Both directions, because each catches a different rot. A limitation with no entry means a
        // divergence shipped with nothing asserting it; an entry with no limitation means a claim of
        // coverage outlived the thing it claimed to cover.
        let declared: Vec<(Capability, Platform)> = Platform::ALL
            .iter()
            .flat_map(|platform| {
                limitations(*platform)
                    .into_iter()
                    .map(move |(declaration, _)| (declaration.capability, *platform))
            })
            .collect();

        for (capability, platform) in &declared {
            let covered = LIMITATION_COVERAGE
                .iter()
                .any(|(cap, plat, _)| cap == capability && plat == platform);
            assert!(
                covered,
                "{capability:?} on {platform:?} is declared as a limitation with nothing asserting \
                 it. Add an entry to LIMITATION_COVERAGE naming what covers it, or explain why it \
                 cannot be covered."
            );
        }

        for (capability, platform, how) in LIMITATION_COVERAGE {
            assert!(
                declared.contains(&(*capability, *platform)),
                "LIMITATION_COVERAGE claims to cover {capability:?} on {platform:?}, which is no \
                 longer declared as a limitation. Remove the stale entry."
            );
            // A one-word note would satisfy the check above while telling a reader nothing, so the
            // explanation has to actually name a mechanism.
            assert!(
                how.len() > 60,
                "{capability:?}/{platform:?} coverage note is too short to name what covers it: \
                 {how:?}"
            );
        }

        // Sanity: the count should match, or one of the two loops above is not doing what it looks
        // like it is doing.
        assert_eq!(
            declared.len(),
            LIMITATION_COVERAGE.len(),
            "declared limitations and coverage entries disagree: {} vs {}",
            declared.len(),
            LIMITATION_COVERAGE.len()
        );
    }

    #[test]
    fn the_only_unknown_is_windows_log_rotation() {
        // The single genuinely open question in the matrix, and it should stay singular: an `unknown`
        // is a promise to find out, so a second one appearing without anyone noticing is how a matrix
        // becomes a list of shrugs.
        let unknowns: Vec<(Capability, Platform)> = MATRIX
            .iter()
            .flat_map(|declaration| {
                declaration.support.iter().filter_map(move |support| {
                    matches!(support.level, SupportLevel::Unknown)
                        .then_some((declaration.capability, support.platform))
                })
            })
            .collect();

        assert_eq!(
            unknowns,
            vec![(Capability::LogRotationWhileOpen, Platform::Windows)],
            "the set of open questions changed; if that is intended, update this test and say why"
        );
    }

    // ── 3.x declared limitations report their difference ────────────────────────────────────────

    #[test]
    fn the_events_refusal_names_the_constraint_and_the_alternative() {
        // Task 3.1. The old message was "the events command is only supported on Unix platforms" —
        // technically a platform error rather than a socket error, but it RESTATED THE LEVEL and gave
        // the operator nothing to do next. It also duplicated the matrix's own reason, so the two
        // could drift apart with the user-facing copy being the one nobody updates.
        //
        // Asserted for Windows from a macOS machine, which is only possible because the platform is a
        // parameter: a test that could only run on Windows is no test at all here.
        let message = unavailable_message_for(Capability::EventStream, Platform::Windows);

        assert!(
            message.contains("Windows"),
            "the refusal must name the platform: {message}"
        );
        // The constraint, so the operator understands WHY rather than being told no.
        assert!(
            message.contains("Unix domain socket"),
            "the refusal must name the constraint: {message}"
        );
        // The alternative, which is the part that makes it actionable.
        assert!(
            message.contains("HTTP event stream"),
            "the refusal must name the alternative: {message}"
        );
        // And it is not a socket error: no connection was attempted, so no errno or path appears.
        for socket_noise in ["No such file", "Connection refused", "os error", ".sock"] {
            assert!(
                !message.contains(socket_noise),
                "the refusal must not read as a failed connection ({socket_noise:?}): {message}"
            );
        }
        // Derived, not written twice: the message must carry the matrix's own reason verbatim.
        let declared = support(Capability::EventStream, Platform::Windows)
            .reason
            .expect("an unavailable capability declares a reason");
        assert!(
            message.contains(declared),
            "the refusal must carry the declared reason rather than a second copy of it"
        );
    }

    #[test]
    fn a_degraded_capability_states_its_difference_rather_than_claiming_success() {
        // Task 3.2. The failure mode is a capability that "works" while behaving differently, so an
        // operator configures for one behaviour and gets another. Every degraded entry must therefore
        // explain the DIFFERENCE, not merely be marked degraded.
        let mut checked = 0;
        for platform in Platform::ALL {
            for (declaration, support) in limitations(platform) {
                if !matches!(support.level, SupportLevel::Degraded) {
                    continue;
                }
                checked += 1;
                let reason = support.reason.unwrap_or_else(|| {
                    panic!("{:?} degraded with no reason", declaration.capability)
                });

                // A reason that only says "degraded on Windows" tells the operator nothing they cannot
                // read from the level column.
                assert!(
                    reason.len() > 60,
                    "{:?}/{:?} reason is too short to state a difference: {reason:?}",
                    declaration.capability,
                    platform
                );
                let lower = reason.to_lowercase();
                assert!(
                    !lower.starts_with("degraded") && !lower.starts_with("not supported"),
                    "{:?}/{:?} reason restates its level instead of the difference: {reason:?}",
                    declaration.capability,
                    platform
                );
                // The message built from it must name the platform and the difference together, so a
                // user reading one line gets both.
                let message = unavailable_message_for(declaration.capability, platform);
                assert!(
                    message.contains(platform.label()) && message.contains(reason),
                    "{:?}/{:?} message loses either the platform or the reason: {message}",
                    declaration.capability,
                    platform
                );
            }
        }
        assert!(
            checked >= 7,
            "expected several degraded entries to check, got {checked}"
        );
    }

    #[test]
    fn one_unavailable_capability_does_not_disable_the_others() {
        // Task 3.3. Windows has the most divergence in the matrix, so it is the useful case: an
        // unavailable event socket must not be allowed to imply the rest of the surface is missing.
        let windows_limits = limitations(Platform::Windows);
        let unavailable: Vec<Capability> = windows_limits
            .iter()
            .filter(|(_, support)| matches!(support.level, SupportLevel::Unavailable))
            .map(|(declaration, _)| declaration.capability)
            .collect();
        assert!(
            unavailable.contains(&Capability::EventStream),
            "the premise of this test is that Windows lacks the event socket"
        );

        // NOT asserted: that most matrix entries are supported on Windows. The first version of this
        // test did, and failed at 6 unavailable versus 1 supported — which turned out to be my
        // premise being wrong rather than the matrix. The matrix lists ONLY capabilities that
        // diverge; anything behaving identically everywhere has no row at all. So of 15 divergent
        // capabilities Windows fully supports 1, and that number says nothing about whether Windows
        // works — it says every one of these 15 is here precisely because it differs.
        //
        // The property task 3.3 actually asks about is that an unavailable capability does not
        // CASCADE, which is what the mechanism-specific assertions below check.
        let windows_levels: Vec<(Capability, SupportLevel)> = MATRIX
            .iter()
            .map(|declaration| {
                (
                    declaration.capability,
                    support(declaration.capability, Platform::Windows).level,
                )
            })
            .collect();
        // Every entry has a verdict — no capability is silently missing a Windows row.
        assert_eq!(windows_levels.len(), Capability::ALL.len());

        // And the specific pairing the spec cares about: the event SOCKET is gone, the HTTP surface is
        // not. Those are different mechanisms, and the refusal above points at the second.
        assert!(
            !unavailable.contains(&Capability::AtomicStateReplace),
            "state persistence must keep working where the event socket does not"
        );
        assert!(
            !unavailable.contains(&Capability::ProcessTreeTermination),
            "supervision must keep working where the event socket does not"
        );
    }

    #[test]
    fn a_supported_capability_produces_a_bug_message_rather_than_a_plausible_refusal() {
        // If a caller reaches the refusal path on a platform where the feature works, that is a bug in
        // the caller — and the message says so instead of inventing a reason. A plausible-sounding
        // refusal would send the operator looking for a platform problem that does not exist.
        let message = unavailable_message_for(Capability::AtomicStateReplace, Platform::Linux);
        assert!(
            message.contains("bug"),
            "a refusal for a supported capability must name itself as a bug: {message}"
        );
    }
}
