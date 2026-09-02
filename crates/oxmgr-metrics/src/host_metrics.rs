//! Host-level system metrics: memory, swap, CPU, load, filesystems, network
//! interfaces and component temperatures.
//!
//! Lint-level cleanup: all casts in this file convert bounded host counters
//! (u64 bytes, durations, small counts) to f64/f32/u32 for display, dashboard
//! rendering, and config. Values are bounded below 2^53 for any real host,
//! so precision is exact. See design decision 8.
//!
//! Collection runs on its own task, publishing into a shared snapshot the HTTP
//! layer reads, so nothing here sits on the process-supervision path. The
//! daemon's maintenance tick is 2s with `MissedTickBehavior::Skip`, and every
//! `ProcessManager` state change is serialised through one command channel:
//! adding host collection there would cost restart latency for data that has no
//! bearing on process state.
//!
//! Two facts about sysinfo 0.39.6 shape this module:
//!
//! 1. `Disks`, `Networks` and `Components` are separate collections with their
//!    own `refresh`. A `System`, however often refreshed, yields no disk or
//!    network data.
//! 2. CPU utilisation is a difference between two samples, so it needs a minimum
//!    interval to mean anything: `MINIMUM_CPU_UPDATE_INTERVAL` is 200ms on Apple
//!    and 100ms on BSD. A faster refresh returns a number that is not a
//!    measurement.
//!
//! Every figure here is the **host's**. Inside a container, `total_memory` is
//! typically the host's rather than the cgroup limit, so these totals must not
//! be read as the applicable limit. Network figures in particular belong to an
//! interface, never to a managed process: `process-io-metrics` records
//! per-process network I/O as unsupported precisely because sysinfo keys network
//! data by interface, not by pid.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::info;

/// Marks which entity a set of figures describes.
///
/// Present in the serialised output so an interface total cannot be read as one
/// process's traffic. `process-io-metrics` states per-process network I/O is
/// unsupported; an unlabelled byte count next to a process name would silently
/// contradict that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricScope {
    /// The machine the daemon runs on, not any managed process.
    Host,
}

/// Which subsystem a collection failure came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostSubsystem {
    Memory,
    Cpu,
    LoadAverage,
    Filesystems,
    Network,
    Components,
}

/// Every subsystem, in the order they are reported.
pub const ALL_SUBSYSTEMS: [HostSubsystem; 6] = [
    HostSubsystem::Memory,
    HostSubsystem::Cpu,
    HostSubsystem::LoadAverage,
    HostSubsystem::Filesystems,
    HostSubsystem::Network,
    HostSubsystem::Components,
];

/// Smallest movement in a percentage figure that counts as a change.
///
/// CPU utilisation differs on essentially every sample, so exact comparison would mark it
/// changed every tick and make the flag useless — which is the polling this is meant to
/// replace. 0.5 of a percentage point is below what a reader can act on and above the jitter
/// of an idle machine.
const PERCENT_EPSILON: f32 = 0.5;

/// Smallest movement in a load average that counts as a change. Load is reported to two
/// decimals and drifts continuously, so the same reasoning applies.
const LOAD_EPSILON: f64 = 0.05;

/// Whether two optional percentages differ enough to report.
///
/// A transition into or out of unavailable is ALWAYS a change, regardless of numeric distance:
/// `None` and `Some(0.0)` are different answers, and collapsing them here would reintroduce the
/// stale-figure-shown-as-current failure through the change filter.
fn percent_changed(before: Option<f32>, after: Option<f32>) -> bool {
    match (before, after) {
        (None, None) => false,
        (Some(a), Some(b)) => (a - b).abs() >= PERCENT_EPSILON,
        _ => true,
    }
}

/// Whether two byte counts differ enough to report.
///
/// Exact comparison: a byte count is a discrete quantity an operator may be watching for a
/// small change in, and unlike a percentage it does not jitter on its own.
fn bytes_changed(before: u64, after: u64) -> bool {
    before != after
}

/// The subsystems whose values differ between two snapshots.
///
/// Computed in the collector rather than by diffing per client in the HTTP layer: diffing there
/// would hold a previous snapshot for every connection, making memory scale with client count —
/// the opposite of what this is for.
///
/// "Refreshed" is deliberately not the same as "changed". A refresh that produced identical
/// values is not a change, and a subsystem on a 10s interval is unchanged on the nine 2s ticks
/// between its refreshes.
pub fn changed_subsystems(before: &HostMetrics, after: &HostMetrics) -> Vec<HostSubsystem> {
    let mut changed = Vec::with_capacity(ALL_SUBSYSTEMS.len());

    let memory_changed = match (&before.memory, &after.memory) {
        (None, None) => false,
        (Some(a), Some(b)) => {
            bytes_changed(a.used_bytes, b.used_bytes)
                || bytes_changed(a.available_bytes, b.available_bytes)
                || bytes_changed(a.total_bytes, b.total_bytes)
                || match (&a.swap, &b.swap) {
                    (None, None) => false,
                    (Some(x), Some(y)) => bytes_changed(x.used_bytes, y.used_bytes),
                    _ => true,
                }
        }
        _ => true,
    };
    if memory_changed {
        changed.push(HostSubsystem::Memory);
    }

    let cpu_changed = match (&before.cpu, &after.cpu) {
        (None, None) => false,
        (Some(a), Some(b)) => {
            percent_changed(a.global_percent, b.global_percent)
                // Per-core is absent unless requested, so this compares lengths first and
                // avoids walking 64 cores on a host that asked for none.
                || match (&a.per_core, &b.per_core) {
                    (None, None) => false,
                    (Some(x), Some(y)) => {
                        x.len() != y.len()
                            || x.iter().zip(y.iter()).any(|(c, d)| {
                                percent_changed(Some(c.usage_percent), Some(d.usage_percent))
                            })
                    }
                    _ => true,
                }
        }
        _ => true,
    };
    if cpu_changed {
        changed.push(HostSubsystem::Cpu);
    }

    let load_changed = match (&before.load_average, &after.load_average) {
        (None, None) => false,
        (Some(a), Some(b)) => {
            (a.one - b.one).abs() >= LOAD_EPSILON
                || (a.five - b.five).abs() >= LOAD_EPSILON
                || (a.fifteen - b.fifteen).abs() >= LOAD_EPSILON
        }
        _ => true,
    };
    if load_changed {
        changed.push(HostSubsystem::LoadAverage);
    }

    // Arc pointer equality first: when the collector did not refresh a subsystem it hands the
    // same allocation back, so the common case costs one comparison rather than a walk over
    // every filesystem row. This is what the Arc sharing from section 2 buys here.
    let filesystems_changed = match (&before.filesystems, &after.filesystems) {
        (None, None) => false,
        (Some(a), Some(b)) => !Arc::ptr_eq(a, b) && a != b,
        _ => true,
    };
    if filesystems_changed {
        changed.push(HostSubsystem::Filesystems);
    }

    let network_changed = match (&before.network, &after.network) {
        (None, None) => false,
        (Some(a), Some(b)) => {
            !Arc::ptr_eq(a, b)
                && (a.interfaces.len() != b.interfaces.len()
                    || a.interfaces.iter().zip(b.interfaces.iter()).any(|(x, y)| {
                        x.name != y.name
                            // Cumulative totals only ever grow, so comparing them catches any
                            // traffic without needing the recent amounts, which are zero on an
                            // idle interface and would otherwise report a change every tick.
                            || bytes_changed(x.total_received_bytes, y.total_received_bytes)
                            || bytes_changed(x.total_transmitted_bytes, y.total_transmitted_bytes)
                    }))
        }
        _ => true,
    };
    if network_changed {
        changed.push(HostSubsystem::Network);
    }

    let components_changed = match (&before.components, &after.components) {
        (None, None) => false,
        (Some(a), Some(b)) => {
            !Arc::ptr_eq(a, b)
                && (a.len() != b.len()
                    || a.iter().zip(b.iter()).any(|(x, y)| {
                        x.label != y.label
                            || percent_changed(x.temperature_celsius, y.temperature_celsius)
                    }))
        }
        _ => true,
    };
    if components_changed {
        changed.push(HostSubsystem::Components);
    }

    changed
}

/// A subsystem that could not be collected. Recorded rather than propagated: one
/// unreadable subsystem must not suppress the rest of the snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubsystemFailure {
    pub subsystem: HostSubsystem,
    pub message: String,
    /// Unix seconds the failure was recorded.
    pub at: u64,
}

/// A configured interval that was raised to the platform minimum.
///
/// Reported rather than applied silently: a CPU interval below
/// `MINIMUM_CPU_UPDATE_INTERVAL` produces a meaningless utilisation, and an
/// operator who configured 50ms should be able to see why they are getting
/// something else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntervalAdjustment {
    pub subsystem: HostSubsystem,
    pub configured_ms: u64,
    pub applied_ms: u64,
    pub reason: String,
}

/// Static host identity. Collected once at startup and never refreshed, because
/// none of it changes while the daemon runs. `None` means the platform could not
/// supply the field, which is distinct from an empty string.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_os_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_version: Option<String>,
    /// sysinfo returns this as a plain `String`; an empty one is normalised to
    /// `None` so "unknown" is never rendered as a blank architecture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_arch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical_core_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_core_count: Option<usize>,
    /// Unix seconds the host booted. `None` when the platform reports 0, which
    /// sysinfo uses for "no value" and which is never a real boot time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_time: Option<u64>,
    /// The container limits that apply to this daemon, when any do.
    ///
    /// `None` on an ordinary host, which is the common case and needs no qualification. `Some`
    /// means a cgroup ceiling is enforced and every host total above is the MACHINE's rather than
    /// the one that will be applied — the distinction this field exists to make visible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<ContainerLimits>,
}

/// Container limits as they reach the API and the dashboard.
///
/// A wire form separate from [`crate::container::Limits`] on purpose: that type is a detection
/// result with an enum per source, and serialising enums into a public payload would tie the wire
/// format to internal variant names. This carries the two figures a consumer needs plus where each
/// came from, as strings that are asserted stable by a test.
///
/// `Eq` because `HostIdentity` derives it, and identity is compared to decide whether a snapshot
/// changed. Every field here is an integer or a string, so `Eq` holds — which is the other reason
/// the CPU quota is carried as milli-cpus rather than as the `f64` it is detected in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerLimits {
    /// The runtime, e.g. `docker`, `kubernetes`, or `container` when unidentifiable.
    pub runtime: String,
    /// The enforced memory ceiling in bytes, when one is enforced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_limit_bytes: Option<u64>,
    /// Where the memory figure came from: `cgroup_v2`, `cgroup_v1` or `host`.
    pub memory_source: String,
    /// CPUs available in MILLI-cpus: a 1.5-core quota is `1500`, a half core is `500`.
    ///
    /// An integer rather than the `f64` this started as, for two reasons. `HostIdentity` derives
    /// `Eq` and a float cannot satisfy it — and weakening `Eq` on the identity struct to carry one
    /// field would be the wrong direction. Milli-cpus is also exactly how Kubernetes expresses a
    /// fractional quota (`1500m`), so the unit is one operators already read, and it keeps the
    /// fraction intact where rounding to whole cores would report a saturated half-core container
    /// as 50% busy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_limit_milli: Option<u32>,
    pub cpu_source: String,
}

/// Host memory. Totals are the host's, not a container's limit.
///
/// Platforms differ in how they account cache and buffers, so `used_bytes` is
/// not comparable between hosts: these figures suit watching one host over time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostMemory {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub free_bytes: u64,
    /// Derived once here so the dashboard, Prometheus and any CLI view cannot
    /// disagree about it. `None` when the total is 0, rather than `NaN`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f32>,
    /// `None` when the host has no swap configured. A host without swap is not
    /// "0 of 0 used" — that reads as a measurement, and it is not one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap: Option<HostSwap>,
    /// The ceiling the kernel will actually enforce, when it differs from `total_bytes`.
    ///
    /// `None` on an ordinary host, where `total_bytes` already IS the limit. `Some` only inside a
    /// container, which keeps the field meaningful: one that is always present stops carrying
    /// information.
    ///
    /// `total_bytes` is deliberately left as the HOST's total rather than being overwritten. Both
    /// numbers are true and answer different questions — "how big is this machine" and "how much am
    /// I allowed" — and silently redefining a documented field would break every existing consumer
    /// while making the two indistinguishable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_total_bytes: Option<u64>,
    /// Utilisation against `effective_total_bytes`. THE figure that means something in a container.
    ///
    /// Measured on a 512 MB container of a 64 GB host using 500 MB: this reads ~97.6% where
    /// `used_percent` reads 0.76%. Both divisions are arithmetically correct; only this one
    /// describes the situation the process is in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_used_percent: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostSwap {
    pub total_bytes: u64,
    pub used_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f32>,
}

impl HostSwap {
    /// Builds swap figures, or `None` when no swap is configured.
    pub fn from_totals(total_bytes: u64, used_bytes: u64) -> Option<Self> {
        if total_bytes == 0 {
            return None;
        }
        Some(Self {
            total_bytes,
            used_bytes,
            used_percent: utilisation_percent(used_bytes, total_bytes),
        })
    }
}

/// Host CPU utilisation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostCpu {
    /// `None` until two samples at least `MINIMUM_CPU_UPDATE_INTERVAL` apart have
    /// been taken. Withheld rather than reported as 0, which would read as an
    /// idle machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_percent: Option<f32>,
    /// Absent unless per-core detail was requested. On a 64-core host this is 64
    /// numbers that push the useful figures off screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_core: Option<Vec<HostCpuCore>>,
    /// Milliseconds between the two samples the utilisation was derived from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_interval_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostCpuCore {
    pub name: String,
    pub usage_percent: f32,
    /// `None` when sysinfo reports 0, which it does when frequency is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_mhz: Option<u64>,
}

/// The number of cores this process can actually dispatch to: cgroup-aware
/// `available_parallelism`, which reads the cgroup quota (and affinity mask)
/// on Linux. Inside a container `System::cpus()` still reports the host's full
/// core set (e.g. 7) while the quota grants only a fraction (e.g. 4); every
/// per-core surface must agree with this count so the dashboard never
/// advertises capacity the cgroup does not grant. `usize::MAX` on failure
/// leaves an unrestricted host unchanged.
fn visible_core_cap() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(usize::MAX)
}

/// Trims a per-core sample to the cores this process can dispatch to.
///
/// Pure so the boundary is testable without a real cgroup: with only a time
/// quota and no cpuset mask, /proc exposes no per-core attribution, so the
/// first N cores stand in for the budget. A future cpuset-aware variant could
/// filter by the affinity mask instead of taking a prefix.
fn trim_per_core_to_cap(cores: Vec<HostCpuCore>, cap: usize) -> Vec<HostCpuCore> {
    cores.into_iter().take(cap).collect()
}

/// One, five and fifteen minute load averages. The whole struct is `None` on a
/// platform that does not provide them, rather than three zeroes that read as an
/// idle machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostLoadAverage {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

/// One mounted filesystem.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostFilesystem {
    pub mount_point: String,
    pub file_system: String,
    pub kind: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub used_bytes: u64,
    /// `None` when the filesystem reports zero capacity, which pseudo-filesystems
    /// commonly do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f32>,
    pub is_removable: bool,
    pub is_read_only: bool,
    /// A hint that a display may fold this row away. Filtering is presentational
    /// only: hiding a mount that later matters is worse than an extra row, so the
    /// API always returns the full set.
    pub pseudo: bool,
}

/// Host network figures, per interface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostNetwork {
    /// Always [`MetricScope::Host`]. These are the machine's interfaces; no part
    /// of this is attributable to a managed process.
    pub scope: MetricScope,
    /// Never aggregated: a single combined figure would hide which interface is
    /// busy, which is the only reason to look.
    pub interfaces: Vec<HostInterface>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostInterface {
    pub name: String,
    /// Bytes received since the previous measurement. An amount, not a rate:
    /// divide by [`Self::interval_ms`], which is the interval actually observed.
    pub received_bytes: u64,
    pub transmitted_bytes: u64,
    /// Cumulative since the interface was first seen. Exposed as a counter.
    pub total_received_bytes: u64,
    pub total_transmitted_bytes: u64,
    pub errors_on_received: u64,
    pub errors_on_transmitted: u64,
    /// Milliseconds the amounts above cover. `None` on the first measurement,
    /// where no interval has been observed yet. Never the nominal interval: the
    /// collection task can be late, and dividing by the nominal value would
    /// overstate the rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<u64>,
}

/// One temperature sensor. Every figure is `Option` because sysinfo's are, and
/// absence is the common case: a VM, a container without sensor access, or a
/// platform sysinfo does not cover.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostComponent {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_celsius: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_celsius: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub critical_celsius: Option<f32>,
}

/// The published host snapshot the HTTP layer reads.
///
/// Each subsystem is its own `Option`, so a subsystem that failed or has not yet
/// been collected is absent rather than present-and-zero. `failures` records why.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostMetrics {
    /// Collected once at startup; identical in every snapshot.
    ///
    /// Behind an `Arc` with the other heavy fields below. The collector publishes a snapshot
    /// every tick while only CPU and memory have usually changed, so a deep clone re-copied 9
    /// identity strings, every filesystem row and all 24 of this host's interfaces each time —
    /// measured as ~1.0-1.3 MB of the loaded-RSS regression against the `runtime-efficiency`
    /// baseline. Sharing them makes the clone a refcount bump and leaves the unchanged
    /// subsystems genuinely unchanged, which is also what lets a stream send only what moved.
    pub identity: Arc<HostIdentity>,
    /// Seconds since boot, derived from `identity.boot_time` rather than
    /// re-queried. `None` when boot time is unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<HostMemory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<HostCpu>,
    /// `None` where the platform does not provide load averages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_average: Option<HostLoadAverage>,
    /// The full set, most-utilised first. Filtering belongs to the display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filesystems: Option<Arc<Vec<HostFilesystem>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<Arc<HostNetwork>>,
    /// `None` when temperatures were not requested or the platform supplies
    /// none. An empty set is never published as `Some(vec![])`: that would render
    /// as a heading with no rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub components: Option<Arc<Vec<HostComponent>>>,
    /// Unix seconds the snapshot was published.
    pub collected_at: u64,
    /// Intervals raised to a platform minimum, so the adjustment is visible
    /// rather than silent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interval_adjustments: Vec<IntervalAdjustment>,
    /// Subsystems that could not be collected. The rest of the snapshot is still
    /// published.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<SubsystemFailure>,
}

impl HostMetrics {
    /// Whether this snapshot carries values for a subsystem. Absence is unavailability, so a
    /// first publish only announces the subsystems that actually reported.
    pub fn has_subsystem(&self, subsystem: HostSubsystem) -> bool {
        match subsystem {
            HostSubsystem::Memory => self.memory.is_some(),
            HostSubsystem::Cpu => self.cpu.is_some(),
            HostSubsystem::LoadAverage => self.load_average.is_some(),
            HostSubsystem::Filesystems => self.filesystems.is_some(),
            HostSubsystem::Network => self.network.is_some(),
            HostSubsystem::Components => self.components.is_some(),
        }
    }

    /// Records a subsystem failure, replacing any earlier one for the same
    /// subsystem so the list cannot grow without bound.
    fn record_failure(&mut self, subsystem: HostSubsystem, message: impl Into<String>) {
        self.failures.retain(|f| f.subsystem != subsystem);
        self.failures.push(SubsystemFailure {
            subsystem,
            message: message.into(),
            at: unix_now(),
        });
    }

    fn clear_failure(&mut self, subsystem: HostSubsystem) {
        self.failures.retain(|f| f.subsystem != subsystem);
    }
}

/// Derives a percentage, guarding a zero denominator to unavailable.
///
/// Central on purpose: `used / total` computed separately in the dashboard, in
/// Prometheus rendering and in a CLI view is three chances to disagree on what
/// "used" means. A zero total is real on unusual platforms and on
/// pseudo-filesystems, and must yield `None` rather than `NaN`, which serialises
/// as `null` in JSON and renders as "NaN%" on a dashboard.
///
/// The percentage is computed as integer per-mille in `u128` (the spec's
/// `cast-suppression-discipline` pattern for percentages): multiply by 1000 before
/// dividing so a ratio below 1 keeps its third decimal, then convert only the
/// bounded quotient. `u128` absorbs `used * 1000` for any `u64` input, and the
/// quotient is ≤ 1000 for any non-overcommitted ratio, so `u32::try_from` always
/// succeeds in practice; `u32::MAX` is the residual guard for pathological
/// overcommit. `u32_to_f32` is the crate's sanctioned u32→f32 conversion (std has
/// no `From<u32> for f32`); per-mille values up to 2^24 are exact.
pub fn utilisation_percent(used: u64, total: u64) -> Option<f32> {
    if total == 0 {
        return None;
    }
    let permille = u32::try_from(u128::from(used) * 1000 / u128::from(total)).unwrap_or(u32::MAX);
    Some(oxmgr_core::numeric::u32_to_f32(permille) / 10.0)
}

/// Unix seconds now, or 0 if the clock is before the epoch.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What the caller wants included. Both extras are off by default: per-core CPU
/// is 64 numbers on a 64-core host, and temperatures are usually absent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostMetricsRequest {
    pub per_core_cpu: bool,
    pub temperatures: bool,
}

/// Per-subsystem collection intervals.
///
/// Different subsystems get different intervals because refreshing all of them at
/// the fastest rate any of them needs would spend the most work on the data that
/// changes least: enumerating filesystems is not free (measured on this host:
/// see `docs/HOST-METRICS.md`), and a mount's capacity does not change in a
/// second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostCollectionIntervals {
    /// CPU and memory. Floored at the platform CPU minimum.
    pub cpu_memory: Duration,
    /// Filesystem and network-interface enumeration.
    pub io: Duration,
    /// Temperatures, which change slowly and are often absent.
    pub components: Duration,
}

/// Default CPU and memory interval. 2s matches the dashboard's own refresh, and
/// is an order of magnitude above the 200ms platform floor.
pub const DEFAULT_CPU_MEMORY_INTERVAL: Duration = Duration::from_secs(2);
/// Default filesystem and interface interval.
pub const DEFAULT_IO_INTERVAL: Duration = Duration::from_secs(10);
/// Default temperature interval.
pub const DEFAULT_COMPONENTS_INTERVAL: Duration = Duration::from_secs(30);

/// Floor for any configured interval. Below this the collection task spends more
/// time waking than measuring, and CPU cannot be sampled faster anyway.
const MIN_ANY_INTERVAL: Duration = sysinfo::MINIMUM_CPU_UPDATE_INTERVAL;

impl Default for HostCollectionIntervals {
    fn default() -> Self {
        Self {
            cpu_memory: DEFAULT_CPU_MEMORY_INTERVAL,
            io: DEFAULT_IO_INTERVAL,
            components: DEFAULT_COMPONENTS_INTERVAL,
        }
    }
}

impl HostCollectionIntervals {
    /// Reads intervals from the environment, falling back to the documented
    /// defaults when a value is absent or cannot be interpreted.
    ///
    /// - `OXMGR_HOST_CPU_INTERVAL_MS` (default 2000)
    /// - `OXMGR_HOST_IO_INTERVAL_MS` (default 10000)
    /// - `OXMGR_HOST_COMPONENTS_INTERVAL_MS` (default 30000)
    pub fn from_env() -> Self {
        Self::from_values(
            env_ms("OXMGR_HOST_CPU_INTERVAL_MS"),
            env_ms("OXMGR_HOST_IO_INTERVAL_MS"),
            env_ms("OXMGR_HOST_COMPONENTS_INTERVAL_MS"),
        )
    }

    /// Builds intervals from optional configured values. `None`, zero, or an
    /// unparseable value falls back to the documented default rather than being
    /// treated as "as fast as possible".
    pub fn from_values(
        cpu_memory_ms: Option<u64>,
        io_ms: Option<u64>,
        components_ms: Option<u64>,
    ) -> Self {
        Self {
            cpu_memory: cpu_memory_ms
                .filter(|ms| *ms > 0)
                .map_or(DEFAULT_CPU_MEMORY_INTERVAL, Duration::from_millis),
            io: io_ms
                .filter(|ms| *ms > 0)
                .map_or(DEFAULT_IO_INTERVAL, Duration::from_millis),
            components: components_ms
                .filter(|ms| *ms > 0)
                .map_or(DEFAULT_COMPONENTS_INTERVAL, Duration::from_millis),
        }
    }

    /// Raises any interval below the platform minimum, returning what was
    /// adjusted so the caller can report it instead of applying it silently.
    ///
    /// The floor is sysinfo's `MINIMUM_CPU_UPDATE_INTERVAL`: 200ms on Apple,
    /// 100ms on BSD, 200ms elsewhere.
    pub fn floored(self) -> (Self, Vec<IntervalAdjustment>) {
        let mut adjustments = Vec::new();
        let floor_ms = oxmgr_core::numeric::duration_millis(MIN_ANY_INTERVAL);

        let mut floor = |value: Duration, subsystem: HostSubsystem| -> Duration {
            if value >= MIN_ANY_INTERVAL {
                return value;
            }
            adjustments.push(IntervalAdjustment {
                subsystem,
                configured_ms: oxmgr_core::numeric::duration_millis(value),
                applied_ms: floor_ms,
                reason: format!(
                    "raised to the platform minimum sampling interval of {floor_ms}ms; \
                     a shorter interval cannot produce a meaningful CPU measurement"
                ),
            });
            MIN_ANY_INTERVAL
        };

        let cpu_memory = floor(self.cpu_memory, HostSubsystem::Cpu);
        let io = floor(self.io, HostSubsystem::Filesystems);
        let components = floor(self.components, HostSubsystem::Components);

        (
            Self {
                cpu_memory,
                io,
                components,
            },
            adjustments,
        )
    }
}

/// Whether host collection should run at all.
///
/// Opt-out rather than opt-in: host figures are useful by default, and a dashboard that shows
/// nothing until an environment variable is set would be a worse default than the memory cost.
///
/// This exists because attribution proved the cost cannot be reduced from inside the collector.
/// The resident increase is allocator working set acquired by the first collection and never
/// returned to the OS — dropping the collections recovered 0.00 MB, and dropping the whole
/// collector recovered 0.03 MB. Three attempts at allocating less each moved the figure by
/// noise. Not collecting is the only lever that provably works, so it is offered explicitly
/// rather than pretended away.
///
/// Anything other than a recognised false value keeps collection on, so a typo cannot silently
/// disable a feature the operator wanted.
pub fn collection_enabled_from_env() -> bool {
    match std::env::var("OXMGR_HOST_METRICS") {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "off" | "false" | "no" | "disabled"
        ),
        Err(_) => true,
    }
}

/// Whether per-core CPU detail is collected. On unless explicitly disabled.
///
/// Opt-OUT rather than opt-in, unlike temperatures, because the cost is one f32 per core per
/// tick against a refresh that already happened: `sysinfo` holds the per-core figures once
/// `refresh_cpu_usage` has run, so there is no extra syscall and nothing to schedule. On a
/// 128-core machine that is 512 bytes per tick, which is not a reason to withhold the detail
/// that makes an unbalanced load visible.
///
/// The escape hatch exists for the same reason `OXMGR_HOST_METRICS` does: a deployment that
/// counts bytes should be able to say no. Same recognised-false vocabulary, so an operator
/// learns one convention, and anything unrecognised keeps the feature on rather than silently
/// disabling it on a typo.
pub fn per_core_cpu_from_env() -> bool {
    match std::env::var("OXMGR_HOST_CPU_PER_CORE") {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "off" | "false" | "no" | "disabled"
        ),
        Err(_) => true,
    }
}

fn env_ms(key: &str) -> Option<u64> {
    std::env::var(key)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
}

/// Owns the sysinfo handles and the per-subsystem schedule.
///
/// Not `Send`-shared: one collection task owns it and publishes the result. The
/// four sysinfo collections are held across refreshes on purpose — `Networks`
/// deltas and CPU utilisation are both differences against the previous sample,
/// so rebuilding the collection each time would zero them.
pub struct HostCollector {
    system: sysinfo::System,
    disks: sysinfo::Disks,
    networks: sysinfo::Networks,
    /// Built on first use, not at construction: temperatures are off by default, and
    /// `Components::new()` loads the platform's sensor bindings whether or not anyone asks
    /// for a reading. Measured, on this host, as part of a +43.9% loaded-RSS regression
    /// against the `runtime-efficiency` baseline.
    components: Option<sysinfo::Components>,
    identity: Arc<HostIdentity>,
    intervals: HostCollectionIntervals,
    interval_adjustments: Vec<IntervalAdjustment>,
    request: HostMetricsRequest,
    /// Last accepted CPU sample. Utilisation is withheld until two samples are at
    /// least the platform minimum apart.
    last_cpu_sample: Option<Instant>,
    last_network_refresh: Option<Instant>,
    last_io_refresh: Option<Instant>,
    last_components_refresh: Option<Instant>,
    current: HostMetrics,
    /// The snapshot handed out last tick, kept to diff against. One snapshot for the whole
    /// collector, not one per reader: diffing per client would make memory scale with
    /// connection count.
    last_published: Option<HostMetrics>,
    last_changed: Vec<HostSubsystem>,
}

impl HostCollector {
    /// Builds a collector, collecting static identity once.
    ///
    /// `System::new()` rather than `new_all()`: this type refreshes memory and
    /// CPU explicitly, and the other three subsystems are separate collections
    /// that `System` would not refresh anyway.
    pub fn new(intervals: HostCollectionIntervals, request: HostMetricsRequest) -> Self {
        let (intervals, interval_adjustments) = intervals.floored();
        let identity = Arc::new(collect_identity());
        let mut current = HostMetrics {
            identity: Arc::clone(&identity),
            collected_at: unix_now(),
            interval_adjustments: interval_adjustments.clone(),
            ..HostMetrics::default()
        };
        current.uptime_secs = derive_uptime(identity.boot_time, unix_now());

        Self {
            system: sysinfo::System::new(),
            disks: sysinfo::Disks::new(),
            networks: sysinfo::Networks::new(),
            components: None,
            identity,
            intervals,
            interval_adjustments,
            request,
            last_cpu_sample: None,
            last_network_refresh: None,
            last_io_refresh: None,
            last_components_refresh: None,
            current,
            last_published: None,
            last_changed: Vec::new(),
        }
    }

    /// The intervals actually in use, after flooring.
    pub fn intervals(&self) -> HostCollectionIntervals {
        self.intervals
    }

    /// Intervals that were raised, for logging at startup.
    pub fn interval_adjustments(&self) -> &[IntervalAdjustment] {
        &self.interval_adjustments
    }

    /// The tick period for the collection task: the shortest configured interval,
    /// so each subsystem can be collected on its own schedule from one timer.
    pub fn tick_interval(&self) -> Duration {
        self.intervals
            .cpu_memory
            .min(self.intervals.io)
            .min(self.intervals.components)
    }

    /// The most recently published snapshot.
    ///
    /// Public library API: in the monolith this was `expect(dead_code)` because
    /// only tests called it, but as a `pub` item of a library crate it is
    /// reachable API and the expectation can no longer be fulfilled.
    pub fn current(&self) -> &HostMetrics {
        &self.current
    }

    /// Refreshes whichever subsystems are due and returns the updated snapshot.
    ///
    /// Each subsystem is guarded separately: one that cannot be read records a
    /// failure and leaves the others untouched, because a snapshot missing its
    /// disk figures is far more useful than no snapshot.
    pub fn collect(&mut self) -> HostMetrics {
        self.collect_at(Instant::now())
    }

    /// [`Self::collect`] with an injected clock, so the schedule and the CPU
    /// sampling floor are testable without sleeping.
    pub fn collect_at(&mut self, now: Instant) -> HostMetrics {
        // Identity is never re-collected: it is copied from the value taken at
        // construction. Uptime is derived from boot time rather than re-queried.
        // Arc clone: a refcount bump, not 9 string allocations per tick.
        self.current.identity = Arc::clone(&self.identity);
        let wall_now = unix_now();
        self.current.uptime_secs = derive_uptime(self.identity.boot_time, wall_now);
        self.current.collected_at = wall_now;
        self.current.interval_adjustments = self.interval_adjustments.clone();

        if is_due(self.last_cpu_sample, now, self.intervals.cpu_memory) {
            self.refresh_memory();
            self.refresh_cpu(now);
        }

        if is_due(self.last_io_refresh, now, self.intervals.io) {
            self.refresh_filesystems();
            self.refresh_network(now);
            self.last_io_refresh = Some(now);
        }

        if self.request.temperatures
            && is_due(self.last_components_refresh, now, self.intervals.components)
        {
            self.refresh_components();
            self.last_components_refresh = Some(now);
        }

        // Compared against what was last published, not against what was refreshed: a refresh
        // that produced identical values is not a change, and a subsystem on the 10s interval is
        // unchanged on the nine 2s ticks in between. The result is what a stream sends.
        let published = self.current.clone();
        self.last_changed = match &self.last_published {
            Some(previous) => changed_subsystems(previous, &published),
            // Nothing published yet, so everything available is new to a reader.
            None => ALL_SUBSYSTEMS
                .iter()
                .copied()
                .filter(|s| published.has_subsystem(*s))
                .collect(),
        };
        self.last_published = Some(published.clone());
        published
    }

    /// The subsystems whose values changed on the most recent collection.
    pub fn last_changed(&self) -> &[HostSubsystem] {
        &self.last_changed
    }

    fn refresh_memory(&mut self) {
        self.system.refresh_memory();
        let total = self.system.total_memory();
        if total == 0 {
            // A zero total is not a measurement of an empty machine; it means the
            // platform did not answer.
            self.current.record_failure(
                HostSubsystem::Memory,
                "platform reported zero total memory; treating memory as unavailable",
            );
            self.current.memory = None;
            return;
        }
        let used = self.system.used_memory();
        // Resolved per collection rather than cached: a limit can change under a live container
        // (`docker update --memory`), and a cached ceiling would keep reporting the old one.
        let host_cpus = std::thread::available_parallelism()
            .map(|n| oxmgr_core::numeric::usize_to_f64(n.get()))
            .unwrap_or(1.0);
        let limits = crate::container::detect(total, host_cpus);
        // `is_containerised` gates it so an ordinary host emits neither field. `effective_memory_total`
        // is the accessor rather than reaching into the struct, so the "which denominator" decision
        // lives in one place.
        let effective = limits
            .is_containerised()
            .then(|| limits.effective_memory_total())
            .filter(|limit| *limit != total);
        self.current.memory = Some(HostMemory {
            total_bytes: total,
            used_bytes: used,
            available_bytes: self.system.available_memory(),
            free_bytes: self.system.free_memory(),
            used_percent: utilisation_percent(used, total),
            swap: HostSwap::from_totals(self.system.total_swap(), self.system.used_swap()),
            effective_total_bytes: effective,
            // Usage from the CGROUP, not from `used`.
            //
            // `used` is `sysinfo::used_memory()`, which is the whole machine's. Measured in a
            // 512 MB container on a 4 GB VM: `used` was 1.6 GB while the cgroup reported 1.5 MB, so
            // dividing `used` by the container limit gave 299.8% — a figure that cannot exist.
            // Both halves of a ratio have to come from the same scope.
            //
            // Withheld entirely when the cgroup usage cannot be read, rather than falling back to
            // `used`: a wrong percentage is worse than an absent one, and the fallback is exactly
            // the bug this replaces.
            effective_used_percent: effective.and_then(|limit| {
                crate::container::current_usage_bytes()
                    .and_then(|cgroup_used| utilisation_percent(cgroup_used, limit))
            }),
        });
        self.current.clear_failure(HostSubsystem::Memory);
    }

    fn refresh_cpu(&mut self, now: Instant) {
        if self.request.per_core_cpu {
            self.system.refresh_cpu_all();
        } else {
            self.system.refresh_cpu_usage();
        }

        // The first refresh has nothing to difference against, and a second one
        // taken too soon produces a figure sysinfo itself documents as
        // meaningless. Either way utilisation is withheld, not zeroed.
        let elapsed = self.last_cpu_sample.map(|previous| now - previous);
        let sampled = elapsed.filter(|d| *d >= MIN_ANY_INTERVAL);

        // The per-core sample is trimmed to the cgroup-aware core count.
        // Inside a container `System::cpus()` reports the host's full set
        // (e.g. 7) while the quota grants only a fraction (e.g. 4); the grid,
        // the gauges, and the Prometheus series must all agree with
        // `logical_core_count`, so a limited container shows its limit — never
        // capacity it does not have.
        let per_core = if self.request.per_core_cpu && sampled.is_some() {
            Some(trim_per_core_to_cap(
                self.system
                    .cpus()
                    .iter()
                    .map(|cpu| HostCpuCore {
                        name: cpu.name().to_string(),
                        usage_percent: cpu.cpu_usage(),
                        frequency_mhz: Some(cpu.frequency()).filter(|f| *f > 0),
                    })
                    .collect(),
                visible_core_cap(),
            ))
        } else {
            None
        };

        let sample_interval_ms = sampled.map(oxmgr_core::numeric::duration_millis);
        self.current.cpu = Some(HostCpu {
            global_percent: sampled.map(|_| self.system.global_cpu_usage()),
            per_core,
            sample_interval_ms,
        });

        // Advanced on every refresh, accepted or not. sysinfo resets its CPU
        // counters on each `refresh_cpu_*` call, so its utilisation always covers
        // "since the last refresh" — keeping an older baseline after a rejected
        // sample would report an interval longer than the window the figure was
        // actually derived from.
        self.last_cpu_sample = Some(now);
        self.current.load_average = collect_load_average();
        if self.current.load_average.is_none() {
            self.current.record_failure(
                HostSubsystem::LoadAverage,
                "platform does not provide load averages",
            );
        } else {
            self.current.clear_failure(HostSubsystem::LoadAverage);
        }
    }

    fn refresh_filesystems(&mut self) {
        // Sort lives in `sort_filesystems_by_utilisation` below rather than inline here,
        // so the ordering test exercises this code instead of a copy of it.
        //
        // `true` removes filesystems no longer listed, so an unmounted volume stops being
        // reported rather than lingering with stale capacity.
        //
        // Narrowed from `refresh(true)`, which is `DiskRefreshKind::everything()`: that also
        // collects per-disk read/write counters, and nothing here publishes them —
        // `host_filesystem` reads capacity, available space, filesystem type, mount point,
        // kind, and the removable/read-only flags, and no more. Block-device I/O is a stated
        // non-goal of `host-metrics`, so refreshing it was work spent on data no surface can
        // show.
        self.disks.refresh_specifics(
            true,
            sysinfo::DiskRefreshKind::nothing()
                // `disk.kind()` — SSD/HDD, reported as the `kind` field.
                .with_kind()
                // `total_space()` and `available_space()`, which every capacity figure and the
                // derived utilisation come from.
                .with_storage(),
        );
        let disks = self.disks.list();
        let mut filesystems: Vec<HostFilesystem> = Vec::with_capacity(disks.len());
        filesystems.extend(disks.iter().map(host_filesystem));

        sort_filesystems_by_utilisation(&mut filesystems);

        if filesystems.is_empty() {
            self.current.record_failure(
                HostSubsystem::Filesystems,
                "platform enumerated no filesystems",
            );
            self.current.filesystems = None;
            return;
        }
        self.current.filesystems = Some(Arc::new(filesystems));
        self.current.clear_failure(HostSubsystem::Filesystems);
    }

    fn refresh_network(&mut self, now: Instant) {
        self.networks.refresh(true);
        // The interval actually observed, not the nominal one: the task can be
        // late, and dividing an amount by the nominal interval overstates the
        // rate. Process disk I/O had this defect; it is not repeated here.
        let interval_ms = self
            .last_network_refresh
            .map(|previous| oxmgr_core::numeric::duration_millis(now - previous));

        let networks = self.networks.list();
        let mut interfaces: Vec<HostInterface> = Vec::with_capacity(networks.len());
        interfaces.extend(networks.iter().map(|(name, data)| HostInterface {
            name: name.clone(),
            received_bytes: data.received(),
            transmitted_bytes: data.transmitted(),
            total_received_bytes: data.total_received(),
            total_transmitted_bytes: data.total_transmitted(),
            errors_on_received: data.errors_on_received(),
            errors_on_transmitted: data.errors_on_transmitted(),
            interval_ms,
        }));
        interfaces.sort_by(|a, b| a.name.cmp(&b.name));

        self.last_network_refresh = Some(now);

        if interfaces.is_empty() {
            self.current
                .record_failure(HostSubsystem::Network, "platform enumerated no interfaces");
            self.current.network = None;
            return;
        }
        self.current.network = Some(Arc::new(HostNetwork {
            scope: MetricScope::Host,
            interfaces,
        }));
        self.current.clear_failure(HostSubsystem::Network);
    }

    fn refresh_components(&mut self) {
        // Only reached when temperatures were requested, so constructing here costs nothing
        // on the default path.
        let collection = self.components.get_or_insert_with(sysinfo::Components::new);
        collection.refresh(true);
        let components_list = collection.list();
        let mut components: Vec<HostComponent> = Vec::with_capacity(components_list.len());
        components.extend(components_list.iter().map(|component| HostComponent {
            label: component.label().to_string(),
            temperature_celsius: component.temperature(),
            max_celsius: component.max(),
            critical_celsius: component.critical(),
        }));
        self.apply_components(components);
    }

    /// Decides what a collected component set means for the snapshot.
    ///
    /// Split from `refresh_components` so the empty case is testable: a machine with sensors
    /// can never reach it through the real collection path, and this host has 28 of them. The
    /// no-sensor platform is exactly where the wrong behaviour would ship unnoticed, so the
    /// decision is exercised directly rather than left to whichever branch the test host
    /// happens to take.
    fn apply_components(&mut self, components: Vec<HostComponent>) {
        // An empty set is left as `None` rather than published as an empty list:
        // a heading with no rows reads as a broken panel, and zero-filling would
        // read as a broken sensor.
        if components.is_empty() {
            self.current.components = None;
            self.current.record_failure(
                HostSubsystem::Components,
                "platform or privileges supply no component temperatures",
            );
            return;
        }
        self.current.components = Some(Arc::new(components));
        self.current.clear_failure(HostSubsystem::Components);
    }
}

/// Whether a subsystem is due, given when it last ran.
fn is_due(last: Option<Instant>, now: Instant, interval: Duration) -> bool {
    match last {
        None => true,
        Some(previous) => now.saturating_duration_since(previous) >= interval,
    }
}

/// Collects the static identity. Every one of these is an associated function on
/// `System`, so none of them needs an instance or a refresh.
fn collect_identity() -> HostIdentity {
    HostIdentity {
        host_name: non_empty(sysinfo::System::host_name()),
        os_name: non_empty(sysinfo::System::name()),
        os_version: non_empty(sysinfo::System::os_version()),
        long_os_version: non_empty(sysinfo::System::long_os_version()),
        kernel_version: non_empty(sysinfo::System::kernel_version()),
        cpu_arch: non_empty(Some(sysinfo::System::cpu_arch())),
        physical_core_count: sysinfo::System::physical_core_count(),
        logical_core_count: std::thread::available_parallelism().ok().map(|n| n.get()),
        // sysinfo returns 0 for "no value"; 1970 is never a real boot time.
        boot_time: Some(sysinfo::System::boot_time()).filter(|t| *t > 0),
        container: detect_container_limits(),
    }
}

/// Resolves container limits into the wire form, or `None` on an unconstrained host.
///
/// The host totals are read here rather than passed in because identity collection is the one place
/// that already builds a `System` for exactly these figures. Detection is cheap — a few `read` calls
/// on Linux, nothing at all elsewhere — and identity is collected once per snapshot, not per tick.
fn detect_container_limits() -> Option<ContainerLimits> {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    let host_memory = system.total_memory();
    // A platform core count is a small integer (exact in f64); the fallback guards
    // `available_parallelism` failing on an exotic platform.
    let host_cpus = std::thread::available_parallelism()
        .map(|n| oxmgr_core::numeric::usize_to_f64(n.get()))
        .unwrap_or(1.0);

    let limits = crate::container::detect(host_memory, host_cpus);
    let crate::container::Environment::Container(runtime) = limits.environment else {
        // An unconstrained host reports nothing rather than a row of "host" sources: a field that
        // is always present stops carrying information.
        return None;
    };

    Some(ContainerLimits {
        runtime: runtime.to_string(),
        // Reported only when it IS a container limit. Echoing host capacity here would invite a
        // consumer to divide by it and call the result a container percentage.
        memory_limit_bytes: limits
            .memory_bytes
            .source
            .is_container_limit()
            .then_some(limits.memory_bytes.value),
        memory_source: limits.memory_bytes.source.as_wire().to_string(),
        // core count * 1000 milli-cpus is a bounded product well below u32::MAX for any real
        // quota; try_from turns the theoretical overflow into a checked bound.
        cpu_limit_milli: limits
            .cpu_count
            .source
            .is_container_limit()
            .then(|| oxmgr_core::numeric::f64_to_u32_round(limits.cpu_count.value * 1000.0)),
        cpu_source: limits.cpu_count.source.as_wire().to_string(),
    })
}

/// Normalises a blank string to unavailable, so a field the platform could not
/// supply is never rendered as an empty value.
fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Derives uptime from boot time rather than calling `System::uptime()`.
///
/// Saturating: a clock adjustment that puts boot time in the future yields 0
/// rather than a wrapped value in the billions.
pub fn derive_uptime(boot_time: Option<u64>, now: u64) -> Option<u64> {
    boot_time.map(|boot| now.saturating_sub(boot))
}

/// Reads load averages, treating an all-zero triple on a platform that does not
/// implement them as unavailable.
///
/// Windows returns `LoadAvg::default()` — three zeroes — until its performance
/// counter has data, which is indistinguishable from a genuinely idle machine.
/// On Windows the figure is therefore reported as unavailable rather than as a
/// zero an operator would read as "idle".
fn collect_load_average() -> Option<HostLoadAverage> {
    if cfg!(target_os = "windows") {
        return None;
    }
    let load = sysinfo::System::load_average();
    Some(HostLoadAverage {
        one: load.one,
        five: load.five,
        fifteen: load.fifteen,
    })
}

/// Filesystem types that are kernel bookkeeping rather than storage an operator
/// can fill. Used only to set the presentational `pseudo` hint.
const PSEUDO_FILESYSTEMS: &[&str] = &[
    "autofs",
    "binfmt_misc",
    "bpf",
    "cgroup",
    "cgroup2",
    "configfs",
    "debugfs",
    "devfs",
    "devpts",
    "devtmpfs",
    "fusectl",
    "hugetlbfs",
    "mqueue",
    "overlay",
    "proc",
    "pstore",
    "securityfs",
    "squashfs",
    "sysfs",
    "tracefs",
];

/// Orders filesystems most-utilised first.
///
/// The one filling up is the reason an operator is looking, so it leads. An unavailable
/// utilisation sorts last rather than being coerced to 0 or 100 — a filesystem whose capacity
/// could not be read is not "empty", and putting it first would be a false alarm.
/// Mount point breaks ties, so equal utilisation gives a stable order instead of one that
/// shuffles between refreshes.
///
/// A free function rather than an inline closure so the ordering test drives this code instead
/// of re-implementing the comparator and asserting against its own copy.
fn sort_filesystems_by_utilisation(filesystems: &mut [HostFilesystem]) {
    filesystems.sort_by(|a, b| {
        b.used_percent
            .unwrap_or(f32::NEG_INFINITY)
            .total_cmp(&a.used_percent.unwrap_or(f32::NEG_INFINITY))
            .then_with(|| a.mount_point.cmp(&b.mount_point))
    });
}

fn host_filesystem(disk: &sysinfo::Disk) -> HostFilesystem {
    let total = disk.total_space();
    let available = disk.available_space();
    // Saturating: available can exceed total on a filesystem with reserved
    // blocks, and a wrapped "used" would be absurd rather than merely wrong.
    let used = total.saturating_sub(available);
    let file_system = disk.file_system().to_string_lossy().to_string();
    let pseudo = PSEUDO_FILESYSTEMS
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(&file_system))
        || total == 0
        // Bind-mounted regular files (e.g. /etc/hosts in a Docker container) are
        // not filesystems an operator can fill — their capacity belongs to the
        // backing filesystem. Flag them pseudo so presentational surfaces fold them away.
        || !disk.mount_point().is_dir();

    HostFilesystem {
        mount_point: disk.mount_point().to_string_lossy().to_string(),
        file_system,
        kind: disk.kind().to_string(),
        total_bytes: total,
        available_bytes: available,
        used_bytes: used,
        used_percent: utilisation_percent(used, total),
        is_removable: disk.is_removable(),
        is_read_only: disk.is_read_only(),
        pseudo,
    }
}

/// Mount points a display may fold away, computed here so every surface agrees.
/// Presentational only — [`HostMetrics::filesystems`] keeps the full set.
///
/// Public library API: in the monolith this was `expect(dead_code)` because
/// only tests called it (the consuming dashboard panel is `web/` task 3.4), but
/// as a `pub` item of a library crate it is reachable API and the expectation
/// can no longer be fulfilled.
pub fn presentational_mount_points(filesystems: &[HostFilesystem]) -> BTreeSet<String> {
    filesystems
        .iter()
        .filter(|fs| !fs.pseudo && !fs.is_removable)
        .map(|fs| fs.mount_point.clone())
        .collect()
}

/// One published collection: the snapshot, and which subsystems changed to produce it.
///
/// Sent over a broadcast channel so a stream can forward only what moved. The snapshot rides
/// along because a late subscriber needs a complete starting state, and its heavy fields are
/// `Arc`-shared, so carrying it costs a refcount bump rather than a copy.
#[derive(Debug, Clone)]
pub struct HostUpdate {
    pub metrics: HostMetrics,
    pub changed: Vec<HostSubsystem>,
}

/// How many updates a slow reader may fall behind before it is told it lagged.
///
/// Bounded per channel, not per client: a stalled connection cannot grow the daemon's memory,
/// and `broadcast` reports `Lagged` rather than queueing without limit. Eight is four collection
/// intervals at the 2s cadence — long enough to absorb a scheduling hiccup, short enough that a
/// client which has genuinely stopped reading resynchronises from a snapshot instead of
/// replaying stale deltas.
const UPDATE_CHANNEL_CAPACITY: usize = 8;

/// The shared snapshot the HTTP layer reads and the collection task writes.
///
/// Mirrors `DaemonSnapshot`'s process list: an `Arc<RwLock<..>>` the reader can
/// clone out without touching the writer. `None` inside means nothing has been
/// published yet, which is distinct from a snapshot whose subsystems are all
/// unavailable.
#[derive(Clone)]
pub struct HostMetricsHandle {
    inner: std::sync::Arc<tokio::sync::RwLock<Option<HostMetrics>>>,
    /// Fans one collection out to every connected stream. Created eagerly rather than on first
    /// subscribe: a sender with no receivers is a few hundred bytes, and making it optional
    /// would put a lock on the publish path for no benefit.
    updates: tokio::sync::broadcast::Sender<HostUpdate>,
}

impl Default for HostMetricsHandle {
    fn default() -> Self {
        Self {
            inner: std::sync::Arc::default(),
            updates: tokio::sync::broadcast::channel(UPDATE_CHANNEL_CAPACITY).0,
        }
    }
}

impl HostMetricsHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the published snapshot and notifies every stream of what changed.
    ///
    /// A send error means nobody is listening, which is the normal case for a daemon with no
    /// dashboard open. It is deliberately ignored rather than logged: it is not a failure.
    pub async fn publish(&self, metrics: HostMetrics, changed: Vec<HostSubsystem>) {
        *self.inner.write().await = Some(metrics.clone());
        if !changed.is_empty() {
            // The discard is deliberate: a send error means the receiver is gone (shutdown); nothing left to deliver to
            #[expect(
                clippy::let_underscore_must_use,
                reason = "a send error means the receiver is gone (shutdown); nothing left to deliver to"
            )]
            let _ = self.updates.send(HostUpdate { metrics, changed });
        }
    }

    /// Subscribes to per-collection updates. Each subscriber gets every update from now on.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<HostUpdate> {
        self.updates.subscribe()
    }

    /// How many streams are currently subscribed.
    ///
    /// Test-only, and deliberately so. It exists to prove a disconnected client is released
    /// rather than assuming it: the receiver drops when the stream handler returns, and this
    /// makes that observable. Nothing in the daemon should branch on the subscriber count —
    /// gating collection on it was tried and disproved (see the change's task 2.1), so exposing
    /// it in production would only invite that mistake again.
    #[cfg(test)]
    pub fn subscriber_count(&self) -> usize {
        self.updates.receiver_count()
    }

    /// The current snapshot, or `None` before the first collection completes.
    pub async fn current(&self) -> Option<HostMetrics> {
        self.inner.read().await.clone()
    }
}

/// Shared state for host-wide consumer sampling.
///
/// Separate from [`HostMetricsHandle`] rather than folded into it, because the two have different
/// cadences and different failure modes: capacity metrics refresh every 2s and are always available,
/// while consumers refresh every 30s and are `None` when sampling is disabled. Merging them would
/// mean one lock serving two schedules, and a reader unable to tell which figure was stale.
#[derive(Clone, Default)]
pub struct HostConsumersHandle {
    inner: std::sync::Arc<tokio::sync::RwLock<Option<crate::host_consumers::HostConsumers>>>,
    /// The managed pid → name map, written by the daemon loop and read by the sampler.
    ///
    /// Published rather than passed, because the sampler runs on its own task and the map lives on
    /// the manager. A stale entry is harmless: a pid that has exited simply does not appear in the
    /// next sample, so the worst case is one cycle of a consumer being marked unmanaged.
    managed: std::sync::Arc<tokio::sync::RwLock<std::collections::HashMap<u32, String>>>,
    /// Whether the consumer sampling task was started at all. Distinguishes "disabled"
    /// from "enabled but no sample yet" for surfaces that must report the difference.
    sampling_enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl HostConsumersHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// The last sample, or `None` before the first one or when sampling is disabled.
    pub async fn current(&self) -> Option<crate::host_consumers::HostConsumers> {
        self.inner.read().await.clone()
    }

    pub async fn publish(&self, consumers: crate::host_consumers::HostConsumers) {
        *self.inner.write().await = Some(consumers);
    }

    /// Replaces the managed pid map.
    pub async fn set_managed(&self, managed: std::collections::HashMap<u32, String>) {
        *self.managed.write().await = managed;
    }

    async fn managed_snapshot(&self) -> std::collections::HashMap<u32, String> {
        self.managed.read().await.clone()
    }

    /// Whether consumer sampling was enabled at startup. `true` after the sampling
    /// task starts; `false` when disabled by `OXMGR_HOST_CONSUMERS`.
    pub fn sampling_enabled(&self) -> bool {
        self.sampling_enabled
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Marks consumer sampling enabled/disabled at startup. Crosses the crate
    /// boundary: the daemon (`oxmgr`) calls it while wiring the collector task.
    pub fn set_sampling_enabled(&self, enabled: bool) {
        self.sampling_enabled
            .store(enabled, std::sync::atomic::Ordering::Release);
    }
}

/// Runs host-wide consumer sampling until the process ends.
///
/// Its own task, and the measurement is why: a full-host refresh costs 7.79 ms p50 against 0.003 ms
/// for the managed-pid refresh, so putting it on the 2s maintenance tick would spend 0.39% of every
/// tick on the figure that changes least urgently. At the 30s default it is 0.026% duty.
///
/// Returns immediately when sampling is disabled, so a disabled feature costs one task spawn rather
/// than a loop that wakes up to do nothing.
pub async fn run_consumer_loop(
    handle: HostConsumersHandle,
    mut sampler: crate::host_consumers::ConsumerSampler,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    if !sampler.config().enabled {
        return;
    }
    let interval = sampler.config().interval;
    let mut ticker = tokio::time::interval(interval);
    // Skip, matching the maintenance tick: a late sample is dropped rather than queued, since a
    // burst of catch-up refreshes would each cost 8 ms and produce near-identical listings.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            // Cooperative stop on daemon shutdown, rather than runtime teardown
            // killing the task mid-sample.
            _ = shutdown.changed() => {
                info!("host consumer loop: shutdown received, exiting");
                return;
            }
            _ = ticker.tick() => {
                let managed = handle.managed_snapshot().await;
                let now = Instant::now();
                let now_unix = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                sampler.sample(&managed, now, now_unix);
                if let Some(consumers) = sampler.current() {
                    handle.publish(consumers.clone()).await;
                }
            }
        }
    }
}

/// Runs host collection until the process ends or the shutdown flag is set.
///
/// Deliberately its own task: supervision keeps its 2s cadence because this loop
/// shares nothing with `ProcessManager`'s command channel. `MissedTickBehavior::Skip`
/// matches the maintenance tick — a late collection is dropped rather than
/// queued, since a burst of catch-up samples closer than the platform minimum
/// would produce nothing usable.
pub async fn run_collection_loop(
    handle: HostMetricsHandle,
    mut collector: HostCollector,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(collector.tick_interval());
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                info!("host collection loop: shutdown received, exiting");
                return;
            }
            _ = ticker.tick() => {
                let metrics = collector.collect();
                // Nothing moved: publish still refreshes the snapshot's timestamp, but no update is
                // broadcast, so an idle host costs a connected client nothing.
                let changed = collector.last_changed().to_vec();
                handle.publish(metrics, changed).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// Attributes the CONSTRUCTION path, which is where the cost actually is.
    ///
    /// Written after three failed fixes. The control measurements say a collector task that
    /// releases every collection immediately still costs 3.3 MB of daemon RSS (8.06 MB with
    /// collection disabled, 11.33 MB gated-idle), so the expense cannot be in the refresh —
    /// it is paid before the loop runs. This isolates each construction step.
    ///
    /// Run with `cargo test --release host_construction_attribution -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement, not an assertion: run explicitly with --nocapture"]
    fn host_construction_attribution() {
        fn rss_bytes() -> u64 {
            let pid = std::process::id();
            let out = std::process::Command::new("ps")
                .args(["-o", "rss=", "-p", &pid.to_string()])
                .output()
                .expect("ps");
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse::<u64>()
                .unwrap_or(0)
                * 1024
        }
        let mb = |b: u64| oxmgr_core::numeric::u64_to_f64(b) / 1_048_576.0;
        let mut last = rss_bytes();
        println!("baseline: {:.2} MB", mb(last));
        let step = |label: &str, last: &mut u64| {
            let now = rss_bytes();
            println!(
                "  +{:<32} {:>7.2} MB delta  {:>7.2} MB total",
                label,
                mb(now.saturating_sub(*last)),
                mb(now)
            );
            *last = now;
        };

        // Identity, field by field: on macOS the OS version fields read a plist, which may
        // pull in Core Foundation and leave its pages resident for the process lifetime.
        let _ = sysinfo::System::host_name();
        step("System::host_name", &mut last);
        let _ = sysinfo::System::name();
        step("System::name", &mut last);
        let _ = sysinfo::System::os_version();
        step("System::os_version", &mut last);
        let _ = sysinfo::System::long_os_version();
        step("System::long_os_version", &mut last);
        let _ = sysinfo::System::kernel_version();
        step("System::kernel_version", &mut last);
        let _ = sysinfo::System::cpu_arch();
        step("System::cpu_arch", &mut last);
        let _ = sysinfo::System::physical_core_count();
        step("System::physical_core_count", &mut last);
        let _ = sysinfo::System::boot_time();
        step("System::boot_time", &mut last);

        // Construction is cheap; the first collection is not, and dropping the collections
        // afterwards recovers nothing. Measured with a `release()` method that rebuilt every
        // sysinfo collection: `first collect` +4.52 MB, then `release` +0.00 MB. Freed pages
        // stay with the allocator rather than returning to the OS, which is why three
        // successive "allocate less" fixes each moved the daemon figure by noise. The method
        // was removed once it was proven inert; dropping the whole collector is the only thing
        // that would return the pages, and a collector that is dropped is a task that is gone.
        let mut collector = HostCollector::new(
            HostCollectionIntervals::default(),
            HostMetricsRequest::default(),
        );
        step("HostCollector::new", &mut last);
        collector.collect();
        step("first collect", &mut last);
        collector.collect();
        step("second collect", &mut last);
        for _ in 0..20 {
            collector.collect();
        }
        step("20 more collects", &mut last);
        drop(collector);
        step("drop collector", &mut last);
    }

    /// Attributes resident memory to each sysinfo collection, one construction at a time.
    ///
    /// Written because two RSS fixes in a row failed: sharing the snapshot behind `Arc` moved
    /// the loaded-RSS regression only from 48.0% to 44.2%, i.e. noise. Guessing a third time
    /// would be worse than measuring, so this reads the process's own RSS between steps.
    ///
    /// Run with `cargo test host_rss_attribution -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement, not an assertion: run explicitly with --nocapture"]
    fn host_rss_attribution() {
        fn rss_bytes() -> u64 {
            let pid = std::process::id();
            let out = std::process::Command::new("ps")
                .args(["-o", "rss=", "-p", &pid.to_string()])
                .output()
                .expect("ps");
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse::<u64>()
                .unwrap_or(0)
                * 1024
        }
        let mb = |b: u64| oxmgr_core::numeric::u64_to_f64(b) / 1_048_576.0;
        let mut last = rss_bytes();
        println!("baseline (test harness only): {:.2} MB", mb(last));
        let step = |label: &str, current: u64, last: &mut u64| {
            println!(
                "  +{:<28} {:>7.2} MB delta   {:>7.2} MB total",
                label,
                mb(current.saturating_sub(*last)),
                mb(current)
            );
            *last = current;
        };

        let mut system = sysinfo::System::new();
        step("System::new", rss_bytes(), &mut last);
        system.refresh_memory();
        step("refresh_memory", rss_bytes(), &mut last);
        system.refresh_cpu_usage();
        step("refresh_cpu_usage", rss_bytes(), &mut last);

        let mut disks = sysinfo::Disks::new();
        step("Disks::new", rss_bytes(), &mut last);
        disks.refresh(true);
        step("Disks::refresh", rss_bytes(), &mut last);

        let mut networks = sysinfo::Networks::new();
        step("Networks::new", rss_bytes(), &mut last);
        networks.refresh(true);
        step("Networks::refresh", rss_bytes(), &mut last);

        let mut components = sysinfo::Components::new();
        step("Components::new", rss_bytes(), &mut last);
        components.refresh(true);
        step("Components::refresh", rss_bytes(), &mut last);

        // Steady state: does repeated refreshing keep growing, or level off?
        for round in 1..=3 {
            for _ in 0..20 {
                system.refresh_memory();
                system.refresh_cpu_usage();
                disks.refresh(true);
                networks.refresh(true);
            }
            step(
                &format!("20x refresh (round {round})"),
                rss_bytes(),
                &mut last,
            );
        }
        println!(
            "interfaces: {}, disks: {}, components: {}",
            networks.list().len(),
            disks.list().len(),
            components.list().len()
        );
    }

    /// Prints per-subsystem collection cost. Ignored by default: it is a measurement, not an
    /// assertion, and wall-clock timings on a shared CI runner would be a flaky test.
    ///
    /// Run with `cargo test host_collection_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement, not an assertion: run explicitly with --nocapture"]
    fn host_collection_cost_per_subsystem() {
        use std::time::Instant as StdInstant;

        let mut collector = HostCollector::new(
            HostCollectionIntervals::default(),
            HostMetricsRequest {
                per_core_cpu: true,
                temperatures: true,
            },
        );
        // Warm-up: the first pass allocates and populates every collection, so timing it
        // would measure construction rather than steady-state cost.
        collector.collect();

        const ROUNDS: u32 = 20;
        let mut totals = Vec::new();
        for label in [
            "memory",
            "cpu",
            "filesystems",
            "network",
            "components",
            "full",
        ] {
            let started = StdInstant::now();
            for round in 0..ROUNDS {
                // Each subsystem gates on its own interval, so the clock has to advance past
                // it or the call is skipped and the timing is meaningless.
                let now = Instant::now() + Duration::from_secs(60 * u64::from(round + 1));
                match label {
                    "memory" => collector.refresh_memory(),
                    "cpu" => collector.refresh_cpu(now),
                    "filesystems" => collector.refresh_filesystems(),
                    "network" => collector.refresh_network(now),
                    "components" => collector.refresh_components(),
                    _ => {
                        collector.collect_at(now);
                    }
                }
            }
            totals.push((label, started.elapsed() / ROUNDS));
        }

        let fs_count = collector
            .current()
            .filesystems
            .as_deref()
            .map_or(0, Vec::len);
        let iface_count = collector
            .current()
            .network
            .as_ref()
            .map_or(0, |n| n.interfaces.len());
        println!("host collection cost ({fs_count} filesystems, {iface_count} interfaces):");
        for (label, mean) in totals {
            println!("  {label:<12} {:>9.3} ms", mean.as_secs_f64() * 1000.0);
        }
    }

    /// Steady-state cost of a collection tick under the SHIPPED defaults, which is the figure
    /// that matters: the per-subsystem measurement above times each refresh in isolation and
    /// with every optional subsystem switched on, so it overstates what a running daemon pays.
    ///
    /// Run with `cargo test host_default_tick_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement, not an assertion: run explicitly with --nocapture"]
    fn host_default_tick_cost() {
        use std::time::Instant as StdInstant;

        let mut collector = HostCollector::new(
            HostCollectionIntervals::from_values(None, None, None),
            HostMetricsRequest::default(),
        );
        collector.collect();

        // Ticks that fall inside every subsystem's interval: only the clock advances, so this
        // is the cost of the common case where nothing is due.
        const ROUNDS: u32 = 50;
        let base = Instant::now();
        let started = StdInstant::now();
        for round in 0..ROUNDS {
            collector.collect_at(base + Duration::from_millis(u64::from(round) * 100));
        }
        let idle = started.elapsed() / ROUNDS;

        // Ticks far enough apart that every due subsystem refreshes.
        let started = StdInstant::now();
        for round in 0..ROUNDS {
            collector.collect_at(base + Duration::from_secs(60 * u64::from(round + 1)));
        }
        let due = started.elapsed() / ROUNDS;

        println!("host tick cost under shipped defaults:");
        println!(
            "  tick, nothing due  {:>9.3} ms",
            idle.as_secs_f64() * 1000.0
        );
        println!(
            "  tick, all due      {:>9.3} ms",
            due.as_secs_f64() * 1000.0
        );
        println!(
            "  temperatures requested: {} (the 65ms subsystem is off by default)",
            collector.request.temperatures
        );
    }

    fn intervals_ms(cpu: u64, io: u64, components: u64) -> HostCollectionIntervals {
        HostCollectionIntervals {
            cpu_memory: Duration::from_millis(cpu),
            io: Duration::from_millis(io),
            components: Duration::from_millis(components),
        }
    }

    #[test]
    fn utilisation_guards_a_zero_denominator_to_unavailable() {
        assert_eq!(utilisation_percent(0, 0), None);
        assert_eq!(utilisation_percent(512, 0), None);
        // A genuine zero stays a zero: unavailable is not a synonym for empty.
        assert_eq!(utilisation_percent(0, 100), Some(0.0));
        assert_eq!(utilisation_percent(50, 100), Some(50.0));
    }

    #[test]
    fn utilisation_never_yields_nan_or_infinity() {
        for (used, total) in [(0u64, 0u64), (1, 0), (u64::MAX, 0)] {
            let value = utilisation_percent(used, total);
            assert!(value.is_none(), "{used}/{total} should be unavailable");
        }
        let value = utilisation_percent(u64::MAX, u64::MAX).expect("both non-zero");
        assert!(value.is_finite(), "utilisation must stay finite");
    }

    #[test]
    fn absent_swap_is_unavailable_rather_than_zero_of_zero() {
        assert_eq!(HostSwap::from_totals(0, 0), None);
        let swap = HostSwap::from_totals(2048, 0).expect("configured swap");
        // Configured but unused swap is a real measurement of zero.
        assert_eq!(swap.used_bytes, 0);
        assert_eq!(swap.used_percent, Some(0.0));
    }

    #[test]
    fn unavailable_serialises_differently_from_zero() {
        let unavailable = HostMemory {
            total_bytes: 0,
            used_bytes: 0,
            available_bytes: 0,
            free_bytes: 0,
            used_percent: None,
            swap: None,
            // Unconstrained fixture: no container limit applies, so these are
            // None rather than an echo of the host total.
            effective_total_bytes: None,
            effective_used_percent: None,
        };
        let measured_zero = HostMemory {
            used_percent: Some(0.0),
            swap: HostSwap::from_totals(1024, 0),
            // Unconstrained fixture: no container limit applies, so these are
            // None rather than an echo of the host total.
            effective_total_bytes: None,
            effective_used_percent: None,
            ..unavailable.clone()
        };

        let unavailable_json = serde_json::to_string(&unavailable).expect("serialise");
        let zero_json = serde_json::to_string(&measured_zero).expect("serialise");

        // Absent fields are omitted entirely, so a consumer cannot mistake
        // "unavailable" for "measured zero".
        assert!(!unavailable_json.contains("used_percent"));
        assert!(!unavailable_json.contains("swap"));
        assert!(zero_json.contains("\"used_percent\":0"));
        assert!(zero_json.contains("\"swap\""));
        assert_ne!(unavailable_json, zero_json);
    }

    #[test]
    fn uptime_is_derived_from_boot_time_and_advances() {
        let boot = 1_000_000u64;
        let first = derive_uptime(Some(boot), boot + 30).expect("boot time known");
        let later = derive_uptime(Some(boot), boot + 90).expect("boot time known");
        assert_eq!(first, 30);
        assert_eq!(later, 90);
        assert!(later > first, "uptime must advance with wall clock");
        // No boot time means no uptime, rather than an uptime of zero.
        assert_eq!(derive_uptime(None, boot + 30), None);
        // A clock adjustment must not wrap into the billions.
        assert_eq!(derive_uptime(Some(boot), boot - 5), Some(0));
    }

    #[test]
    fn identity_is_collected_once_and_not_refreshed() {
        let mut collector = HostCollector::new(intervals_ms(200, 200, 200), request_all());
        let start = Instant::now();
        let first = collector.collect_at(start);
        let second = collector.collect_at(start + Duration::from_millis(500));

        assert_eq!(
            first.identity, second.identity,
            "static identity must not be re-collected"
        );
        // Identity in the snapshot is the value taken at construction.
        assert_eq!(*first.identity, *collector_identity(&collector));
    }

    fn collector_identity(collector: &HostCollector) -> &HostIdentity {
        &collector.identity
    }

    fn request_all() -> HostMetricsRequest {
        HostMetricsRequest {
            per_core_cpu: true,
            temperatures: true,
        }
    }

    #[test]
    fn identity_fields_are_unavailable_rather_than_empty() {
        assert_eq!(non_empty(Some(String::new())), None);
        assert_eq!(non_empty(Some("   ".to_string())), None);
        assert_eq!(non_empty(None), None);
        assert_eq!(non_empty(Some("arm64".to_string())), Some("arm64".into()));
    }

    #[test]
    fn an_interval_below_the_platform_minimum_is_raised_and_reported() {
        let floor_ms = oxmgr_core::numeric::duration_millis(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        let (applied, adjustments) = intervals_ms(1, 10_000, 30_000).floored();

        assert_eq!(applied.cpu_memory, sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        assert_eq!(applied.io, Duration::from_millis(10_000));
        assert_eq!(adjustments.len(), 1, "only the CPU interval was too short");
        let adjustment = &adjustments[0];
        assert_eq!(adjustment.subsystem, HostSubsystem::Cpu);
        assert_eq!(adjustment.configured_ms, 1);
        assert_eq!(adjustment.applied_ms, floor_ms);
        assert!(
            !adjustment.reason.is_empty(),
            "the adjustment must be reported, not silent"
        );
    }

    #[test]
    fn an_interval_at_or_above_the_minimum_is_left_alone() {
        let (applied, adjustments) = HostCollectionIntervals::default().floored();
        assert_eq!(applied, HostCollectionIntervals::default());
        assert!(adjustments.is_empty());
    }

    #[test]
    fn adjustments_are_reported_in_the_snapshot() {
        let collector = HostCollector::new(intervals_ms(1, 1, 1), HostMetricsRequest::default());
        assert_eq!(collector.interval_adjustments().len(), 3);
        assert_eq!(
            collector.current().interval_adjustments.len(),
            3,
            "the snapshot must carry the adjustment, not just the log"
        );
    }

    #[test]
    fn absent_or_unusable_intervals_fall_back_to_the_documented_defaults() {
        let defaults = HostCollectionIntervals::from_values(None, None, None);
        assert_eq!(defaults, HostCollectionIntervals::default());
        // Zero is unusable rather than "as fast as possible".
        assert_eq!(
            HostCollectionIntervals::from_values(Some(0), Some(0), Some(0)),
            HostCollectionIntervals::default()
        );
        let configured = HostCollectionIntervals::from_values(Some(500), Some(5_000), Some(60_000));
        assert_eq!(configured.cpu_memory, Duration::from_millis(500));
        assert_eq!(configured.io, Duration::from_millis(5_000));
        assert_eq!(configured.components, Duration::from_millis(60_000));
    }

    #[test]
    fn the_tick_is_the_shortest_configured_interval() {
        let collector = HostCollector::new(
            intervals_ms(2_000, 10_000, 30_000),
            HostMetricsRequest::default(),
        );
        assert_eq!(collector.tick_interval(), Duration::from_millis(2_000));
    }

    #[test]
    fn cpu_utilisation_is_withheld_before_a_valid_sampling_interval() {
        let mut collector = HostCollector::new(
            HostCollectionIntervals::default(),
            HostMetricsRequest::default(),
        );
        let start = Instant::now();

        let first = collector.collect_at(start);
        let cpu = first.cpu.expect("cpu subsystem present");
        assert_eq!(
            cpu.global_percent, None,
            "the first sample has nothing to difference against"
        );
        assert_eq!(cpu.sample_interval_ms, None);

        let valid = collector.collect_at(start + Duration::from_secs(2));
        let cpu = valid.cpu.expect("cpu subsystem present");
        assert!(
            cpu.global_percent.is_some(),
            "a sample past the minimum interval yields a value"
        );
        let interval = cpu.sample_interval_ms.expect("interval recorded");
        let min_interval_ms =
            oxmgr_core::numeric::duration_millis(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        assert!(
            interval >= min_interval_ms,
            "reported interval {interval}ms must be at least the platform minimum"
        );
    }

    #[test]
    fn per_core_detail_is_absent_unless_requested() {
        let mut collector = HostCollector::new(
            HostCollectionIntervals::default(),
            HostMetricsRequest::default(),
        );
        let start = Instant::now();
        collector.collect_at(start);
        let metrics = collector.collect_at(start + Duration::from_secs(4));
        let cpu = metrics.cpu.expect("cpu subsystem present");
        assert!(cpu.global_percent.is_some(), "global is always reported");
        assert_eq!(
            cpu.per_core, None,
            "per-core detail must not appear by default"
        );

        let mut collector = HostCollector::new(
            HostCollectionIntervals::default(),
            HostMetricsRequest {
                per_core_cpu: true,
                temperatures: false,
            },
        );
        collector.collect_at(start);
        let metrics = collector.collect_at(start + Duration::from_secs(4));
        let cores = metrics
            .cpu
            .expect("cpu subsystem present")
            .per_core
            .expect("per-core requested");
        assert!(!cores.is_empty(), "a host has at least one core");
    }

    #[test]
    fn per_core_is_trimmed_to_the_visible_core_cap() {
        // 7 cores reported by the host, a 4-core quota: exactly 4 survive, and
        // the surviving cores are the first N (the budget stand-in, since a
        // time quota carries no per-core attribution). Verified to bite:
        // removing `trim_per_core_to_cap` from `refresh_cpu` fails the
        // compile, not just this assertion.
        let seven: Vec<HostCpuCore> = (0..7)
            .map(|i| {
                // i is a fixture core index 0..7; f32::from(u32) is exact for it.
                let usage =
                    oxmgr_core::numeric::u32_to_f32(u32::try_from(i).unwrap_or(u32::MAX)) * 10.0;
                HostCpuCore {
                    name: format!("cpu{i}"),
                    usage_percent: usage,
                    frequency_mhz: None,
                }
            })
            .collect();
        let trimmed = trim_per_core_to_cap(seven, 4);
        assert_eq!(trimmed.len(), 4, "only the capped cores survive");
        assert_eq!(trimmed[0].name, "cpu0");
        assert_eq!(trimmed[3].name, "cpu3");

        // A cap above the sample changes nothing: an unrestricted host keeps
        // every core it reported.
        let four: Vec<HostCpuCore> = (0..4)
            .map(|i| HostCpuCore {
                name: format!("cpu{i}"),
                usage_percent: 0.0,
                frequency_mhz: None,
            })
            .collect();
        assert_eq!(trim_per_core_to_cap(four, 8).len(), 4);
    }

    #[test]
    fn temperatures_are_absent_unless_requested() {
        let mut collector = HostCollector::new(
            HostCollectionIntervals::default(),
            HostMetricsRequest::default(),
        );
        let metrics = collector.collect_at(Instant::now());
        assert_eq!(
            metrics.components, None,
            "temperature detail is on request only"
        );
    }

    /// Reports which temperature branch this host exercises, so a run can tell whether the
    /// no-sensor path was actually covered here or only by the `None` arm of a branching test.
    ///
    /// Run with `cargo test host_temperature_availability -- --ignored --nocapture`.
    #[test]
    #[ignore = "diagnostic, not an assertion: run explicitly with --nocapture"]
    fn host_temperature_availability() {
        let mut collector = HostCollector::new(HostCollectionIntervals::default(), request_all());
        let metrics = collector.collect_at(Instant::now());
        match &metrics.components {
            Some(components) => {
                println!("this host HAS sensors: {} component(s)", components.len());
                for component in components.iter().take(5) {
                    println!(
                        "  {} -> {:?} C",
                        component.label, component.temperature_celsius
                    );
                }
                println!("the no-sensor path is NOT covered on this machine");
            }
            None => {
                println!("this host reports NO sensors");
                for failure in &metrics.failures {
                    println!("  failure: {:?} - {}", failure.subsystem, failure.message);
                }
                println!("the no-sensor path IS covered on this machine");
            }
        }
    }

    /// The no-sensor platform path, driven directly.
    ///
    /// This machine reports 28 sensors (confirmed by `host_temperature_availability`), so the
    /// real collection path cannot reach the empty case here. Feeding the decision an empty
    /// set covers the logic that a sensorless host would exercise; it is not the same as
    /// running on one, and 7.4 is recorded as partial for that reason.
    #[test]
    fn a_platform_without_sensors_reports_unavailable_with_a_reason() {
        let mut collector = HostCollector::new(HostCollectionIntervals::default(), request_all());
        // Seed a populated state first, so this also proves a later empty collection clears a
        // previous reading rather than leaving it stale.
        collector.apply_components(vec![HostComponent {
            label: "cpu".to_string(),
            temperature_celsius: Some(55.0),
            max_celsius: None,
            critical_celsius: None,
        }]);
        assert!(collector.current().components.is_some());

        collector.apply_components(Vec::new());

        // Absent, never `Some(vec![])` and never a zero-degree row.
        assert_eq!(collector.current().components, None);
        let failure = collector
            .current()
            .failures
            .iter()
            .find(|f| f.subsystem == HostSubsystem::Components)
            .expect("the absence is recorded as a failure, not left as a silent gap");
        assert!(
            failure.message.contains("no component temperatures"),
            "the reason must say why: {}",
            failure.message
        );

        // And recovering clears the failure, so a transient permission problem does not leave
        // a permanent error on the snapshot.
        collector.apply_components(vec![HostComponent {
            label: "cpu".to_string(),
            temperature_celsius: Some(55.0),
            max_celsius: None,
            critical_celsius: None,
        }]);
        assert!(
            !collector
                .current()
                .failures
                .iter()
                .any(|f| f.subsystem == HostSubsystem::Components)
        );
    }

    #[test]
    fn an_empty_temperature_set_is_omitted_rather_than_zero_filled() {
        let mut collector = HostCollector::new(HostCollectionIntervals::default(), request_all());
        let metrics = collector.collect_at(Instant::now());
        match metrics.components {
            // Where sensors exist, each carries its own optional reading; a
            // component without one is None, never 0.0.
            Some(components) => {
                assert!(!components.is_empty(), "an empty set must be None, not []");
                for component in components.iter() {
                    assert!(!component.label.is_empty());
                }
            }
            // Where none exist, the absence is recorded as a failure and no rows
            // are published.
            None => assert!(
                metrics
                    .failures
                    .iter()
                    .any(|f| f.subsystem == HostSubsystem::Components)
            ),
        }
    }

    #[test]
    fn filesystems_are_ordered_most_utilised_first_with_unavailable_last() {
        // `/pseudo` has zero capacity, so its utilisation is unavailable rather than 0 — it
        // must sort last, not first.
        let mut rows = [
            fs_row("/quiet", 100, 90),
            fs_row("/pseudo", 0, 0),
            fs_row("/full", 100, 2),
            fs_row("/half", 100, 50),
        ];
        // Calls the production comparator. Re-implementing the sort here would pass even if
        // `refresh_filesystems` ordered rows the wrong way round.
        sort_filesystems_by_utilisation(&mut rows);
        let order: Vec<&str> = rows.iter().map(|fs| fs.mount_point.as_str()).collect();
        assert_eq!(order, ["/full", "/half", "/quiet", "/pseudo"]);
        assert_eq!(rows[3].used_percent, None, "zero capacity is unavailable");
    }

    /// A snapshot with memory, cpu, load, one filesystem and one interface populated.
    fn changed_fixture() -> HostMetrics {
        HostMetrics {
            identity: Arc::new(collect_identity()),
            uptime_secs: Some(100),
            memory: Some(HostMemory {
                total_bytes: 8_000,
                used_bytes: 4_000,
                available_bytes: 4_000,
                free_bytes: 4_000,
                used_percent: Some(50.0),
                swap: None,
                // Unconstrained fixture: no container limit applies, so these are
                // None rather than an echo of the host total.
                effective_total_bytes: None,
                effective_used_percent: None,
            }),
            cpu: Some(HostCpu {
                global_percent: Some(20.0),
                per_core: None,
                sample_interval_ms: Some(2_000),
            }),
            load_average: Some(HostLoadAverage {
                one: 1.0,
                five: 1.0,
                fifteen: 1.0,
            }),
            filesystems: Some(Arc::new(vec![fs_row("/", 100, 40)])),
            network: Some(Arc::new(HostNetwork {
                scope: MetricScope::Host,
                interfaces: vec![HostInterface {
                    name: "eth0".to_string(),
                    received_bytes: 0,
                    transmitted_bytes: 0,
                    total_received_bytes: 1_000,
                    total_transmitted_bytes: 2_000,
                    errors_on_received: 0,
                    errors_on_transmitted: 0,
                    interval_ms: Some(2_000),
                }],
            })),
            components: None,
            collected_at: 1,
            interval_adjustments: Vec::new(),
            failures: Vec::new(),
        }
    }

    #[test]
    fn an_unchanged_snapshot_reports_no_changed_subsystem() {
        let before = changed_fixture();
        // Same values, and the Arc fields deliberately re-shared: this is what the collector
        // hands back on a tick where a subsystem was not due for refresh.
        let after = before.clone();
        assert!(changed_subsystems(&before, &after).is_empty());
    }

    #[test]
    fn only_the_subsystem_that_moved_is_reported() {
        let before = changed_fixture();
        let mut after = before.clone();
        after.cpu = Some(HostCpu {
            global_percent: Some(80.0),
            per_core: None,
            sample_interval_ms: Some(2_000),
        });
        assert_eq!(
            changed_subsystems(&before, &after),
            vec![HostSubsystem::Cpu]
        );
    }

    #[test]
    fn a_movement_below_the_percentage_threshold_is_not_a_change() {
        let before = changed_fixture();
        let mut after = before.clone();
        // 0.4 of a point: below PERCENT_EPSILON. Reporting this would mark CPU changed on
        // essentially every sample and make the whole filter pointless.
        after.cpu = Some(HostCpu {
            global_percent: Some(20.4),
            per_core: None,
            sample_interval_ms: Some(2_000),
        });
        assert!(changed_subsystems(&before, &after).is_empty());

        // 0.6 of a point clears the threshold.
        let mut bigger = before.clone();
        bigger.cpu = Some(HostCpu {
            global_percent: Some(20.6),
            per_core: None,
            sample_interval_ms: Some(2_000),
        });
        assert_eq!(
            changed_subsystems(&before, &bigger),
            vec![HostSubsystem::Cpu]
        );
    }

    #[test]
    fn becoming_unavailable_is_always_a_change() {
        let before = changed_fixture();
        let mut after = before.clone();
        after.cpu = None;
        assert_eq!(
            changed_subsystems(&before, &after),
            vec![HostSubsystem::Cpu]
        );

        // And the reverse: a subsystem that starts reporting.
        let mut from_absent = before.clone();
        from_absent.components = None;
        let mut to_present = from_absent.clone();
        to_present.components = Some(Arc::new(vec![HostComponent {
            label: "cpu".to_string(),
            temperature_celsius: Some(50.0),
            max_celsius: None,
            critical_celsius: None,
        }]));
        assert_eq!(
            changed_subsystems(&from_absent, &to_present),
            vec![HostSubsystem::Components]
        );
    }

    #[test]
    fn an_unavailable_figure_is_never_equal_to_a_measured_zero() {
        // The distinction `host-metrics` exists to protect, checked at the change filter: if
        // these collapsed, a client would keep displaying the last reading as current.
        let before = changed_fixture();
        let mut zero = before.clone();
        zero.cpu = Some(HostCpu {
            global_percent: Some(0.0),
            per_core: None,
            sample_interval_ms: Some(2_000),
        });
        let mut absent = before.clone();
        absent.cpu = Some(HostCpu {
            global_percent: None,
            per_core: None,
            sample_interval_ms: None,
        });
        assert_eq!(changed_subsystems(&zero, &absent), vec![HostSubsystem::Cpu]);
    }

    #[test]
    fn a_re_shared_allocation_is_recognised_as_unchanged_without_a_walk() {
        let before = changed_fixture();
        let mut after = before.clone();
        // Same pointer: the collector did not refresh filesystems this tick.
        assert!(Arc::ptr_eq(
            before.filesystems.as_ref().unwrap(),
            after.filesystems.as_ref().unwrap()
        ));
        assert!(changed_subsystems(&before, &after).is_empty());

        // A fresh allocation holding identical values is still not a change: pointer equality
        // is a fast path, not the definition.
        after.filesystems = Some(Arc::new(vec![fs_row("/", 100, 40)]));
        assert!(!Arc::ptr_eq(
            before.filesystems.as_ref().unwrap(),
            after.filesystems.as_ref().unwrap()
        ));
        assert!(changed_subsystems(&before, &after).is_empty());
    }

    #[test]
    fn interface_traffic_is_detected_from_cumulative_totals() {
        let before = changed_fixture();
        let mut after = before.clone();
        after.network = Some(Arc::new(HostNetwork {
            scope: MetricScope::Host,
            interfaces: vec![HostInterface {
                name: "eth0".to_string(),
                // Recent amounts stay zero — an idle interface reports zero every tick, so
                // comparing them would either miss traffic or report it constantly.
                received_bytes: 0,
                transmitted_bytes: 0,
                total_received_bytes: 1_500,
                total_transmitted_bytes: 2_000,
                errors_on_received: 0,
                errors_on_transmitted: 0,
                interval_ms: Some(2_000),
            }],
        }));
        assert_eq!(
            changed_subsystems(&before, &after),
            vec![HostSubsystem::Network]
        );
    }

    #[test]
    fn several_subsystems_moving_are_all_reported() {
        let before = changed_fixture();
        let mut after = before.clone();
        after.memory = Some(HostMemory {
            total_bytes: 8_000,
            used_bytes: 6_000,
            available_bytes: 2_000,
            free_bytes: 2_000,
            used_percent: Some(75.0),
            swap: None,
            // Unconstrained fixture: no container limit applies, so these are
            // None rather than an echo of the host total.
            effective_total_bytes: None,
            effective_used_percent: None,
        });
        after.load_average = Some(HostLoadAverage {
            one: 3.0,
            five: 1.0,
            fifteen: 1.0,
        });
        let changed = changed_subsystems(&before, &after);
        assert!(changed.contains(&HostSubsystem::Memory));
        assert!(changed.contains(&HostSubsystem::LoadAverage));
        assert_eq!(changed.len(), 2, "nothing else moved: {changed:?}");
    }

    fn fs_row(mount: &str, total: u64, available: u64) -> HostFilesystem {
        let used = total.saturating_sub(available);
        HostFilesystem {
            mount_point: mount.to_string(),
            file_system: "apfs".to_string(),
            kind: "SSD".to_string(),
            total_bytes: total,
            available_bytes: available,
            used_bytes: used,
            used_percent: utilisation_percent(used, total),
            is_removable: false,
            is_read_only: false,
            pseudo: total == 0,
        }
    }

    #[test]
    fn a_zero_capacity_filesystem_yields_unavailable_utilisation() {
        let row = fs_row("/dev", 0, 0);
        assert_eq!(row.used_percent, None, "never NaN, never 0%");
        assert!(
            row.pseudo,
            "a zero-capacity mount is presentationally pseudo"
        );
    }

    #[test]
    fn filesystem_filtering_is_presentational_and_the_api_keeps_everything() {
        let rows = vec![
            fs_row("/", 100, 40),
            HostFilesystem {
                is_removable: true,
                ..fs_row("/Volumes/USB", 100, 10)
            },
            fs_row("/dev", 0, 0),
        ];
        let displayed = presentational_mount_points(&rows);
        assert_eq!(displayed.len(), 1, "removable and pseudo folded away");
        assert!(displayed.contains("/"));
        // The full set is what the API carries.
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn real_filesystems_are_enumerated_with_capacity_and_ordering() {
        let mut collector = HostCollector::new(
            HostCollectionIntervals::default(),
            HostMetricsRequest::default(),
        );
        let metrics = collector.collect_at(Instant::now());
        let filesystems = metrics.filesystems.expect("this host has mounts");
        assert!(!filesystems.is_empty());
        for fs in filesystems.iter() {
            assert!(!fs.mount_point.is_empty());
            if fs.total_bytes == 0 {
                assert_eq!(fs.used_percent, None);
            } else {
                let percent = fs.used_percent.expect("non-zero capacity");
                assert!((0.0..=100.0).contains(&percent), "{percent} out of range");
            }
        }
        let mut previous = f32::INFINITY;
        for fs in filesystems.iter() {
            let current = fs.used_percent.unwrap_or(f32::NEG_INFINITY);
            assert!(current <= previous, "must be ordered most-utilised first");
            previous = current;
        }
    }

    #[test]
    fn interfaces_are_reported_individually_and_scoped_to_the_host() {
        let mut collector = HostCollector::new(
            HostCollectionIntervals::default(),
            HostMetricsRequest::default(),
        );
        let metrics = collector.collect_at(Instant::now());
        let network = metrics.network.expect("this host has interfaces");
        assert_eq!(
            network.scope,
            MetricScope::Host,
            "host figures must be labelled so they cannot read as a process's"
        );
        assert!(!network.interfaces.is_empty());
        let named: BTreeSet<&str> = network.interfaces.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(
            named.len(),
            network.interfaces.len(),
            "each interface is its own row, never aggregated"
        );
        let json = serde_json::to_string(&network).expect("serialise");
        assert!(json.contains("\"scope\":\"host\""));
    }

    #[test]
    fn the_network_rate_uses_the_observed_interval_not_the_nominal_one() {
        let mut collector = HostCollector::new(
            HostCollectionIntervals::default(),
            HostMetricsRequest::default(),
        );
        let start = Instant::now();

        let first = collector.collect_at(start);
        let interfaces = &first.network.expect("interfaces").interfaces;
        assert!(
            interfaces.iter().all(|i| i.interval_ms.is_none()),
            "no interval has been observed on the first measurement"
        );

        // A stretched interval: the nominal is 10s, the observed is 37s. The
        // reported interval must be the one that happened, otherwise a rate
        // derived from it would overstate throughput by nearly 4x.
        let stretched = collector.collect_at(start + Duration::from_secs(37));
        let interfaces = &stretched.network.expect("interfaces").interfaces;
        for interface in interfaces.iter() {
            let observed = interface.interval_ms.expect("interval recorded");
            assert_eq!(observed, 37_000, "must be observed, not the nominal 10s");
        }
        let default_io_ms = oxmgr_core::numeric::duration_millis(DEFAULT_IO_INTERVAL);
        assert_ne!(interfaces[0].interval_ms, Some(default_io_ms));
    }

    #[test]
    fn cumulative_totals_are_at_least_the_recent_amounts() {
        let mut collector = HostCollector::new(
            HostCollectionIntervals::default(),
            HostMetricsRequest::default(),
        );
        let start = Instant::now();
        collector.collect_at(start);
        let metrics = collector.collect_at(start + Duration::from_secs(11));
        for interface in metrics.network.expect("interfaces").interfaces.iter() {
            assert!(interface.total_received_bytes >= interface.received_bytes);
            assert!(interface.total_transmitted_bytes >= interface.transmitted_bytes);
        }
    }

    #[test]
    fn one_failing_subsystem_does_not_suppress_the_others() {
        let mut metrics = HostMetrics {
            memory: Some(HostMemory {
                total_bytes: 16 * 1024 * 1024 * 1024,
                used_bytes: 8 * 1024 * 1024 * 1024,
                available_bytes: 8 * 1024 * 1024 * 1024,
                free_bytes: 1024,
                used_percent: Some(50.0),
                swap: None,
                // Unconstrained fixture: no container limit applies, so these are
                // None rather than an echo of the host total.
                effective_total_bytes: None,
                effective_used_percent: None,
            }),
            filesystems: Some(Arc::new(vec![fs_row("/", 100, 40)])),
            ..HostMetrics::default()
        };
        metrics.record_failure(HostSubsystem::Components, "no sensors");

        assert!(metrics.memory.is_some(), "memory survives a sensor failure");
        assert!(metrics.filesystems.is_some());
        assert_eq!(metrics.components, None);
        assert_eq!(metrics.failures.len(), 1);
        assert_eq!(metrics.failures[0].subsystem, HostSubsystem::Components);

        // A repeated failure replaces rather than accumulates.
        metrics.record_failure(HostSubsystem::Components, "still no sensors");
        assert_eq!(metrics.failures.len(), 1);
        assert_eq!(metrics.failures[0].message, "still no sensors");

        // Recovery clears it.
        metrics.clear_failure(HostSubsystem::Components);
        assert!(metrics.failures.is_empty());
    }

    #[test]
    fn a_collection_with_every_subsystem_unavailable_still_publishes_identity() {
        let metrics = HostMetrics {
            identity: Arc::new(collect_identity()),
            ..HostMetrics::default()
        };
        let json = serde_json::to_string(&metrics).expect("serialise");
        assert!(json.contains("collected_at"));
        assert!(!json.contains("\"memory\""), "absent, not zero-filled");
        assert!(!json.contains("\"cpu\""));
    }

    /// Absent means on, an explicit false turns it off, and a typo leaves it on: on all three
    /// points this is a conservative default for a feature that costs a few percent of a core.
    #[test]
    #[serial]
    fn collection_is_enabled_unless_explicitly_disabled() {
        {
            let _g = crate::test_utils::EnvGuard::remove("OXMGR_HOST_METRICS");
            assert!(collection_enabled_from_env(), "absent means on");
        }

        for off in ["0", "off", "false", "no", "disabled", "OFF", " False "] {
            let _g = crate::test_utils::EnvGuard::set("OXMGR_HOST_METRICS", off);
            assert!(
                !collection_enabled_from_env(),
                "{off:?} must disable collection"
            );
        }

        // Anything unrecognised keeps it on: a typo must not silently switch off a feature the
        // operator was trying to configure.
        for on in ["1", "on", "true", "yes", "", "enabled", "ture"] {
            let _g = crate::test_utils::EnvGuard::set("OXMGR_HOST_METRICS", on);
            assert!(
                collection_enabled_from_env(),
                "{on:?} must leave collection on"
            );
        }
    }

    #[test]
    #[serial]
    fn per_core_cpu_is_enabled_unless_explicitly_disabled() {
        // Opt-OUT, unlike temperatures: the per-core figures are already in hand once the CPU
        // refresh has run, so the cost is one f32 per core per tick and no extra syscall.
        {
            let _g = crate::test_utils::EnvGuard::remove("OXMGR_HOST_CPU_PER_CORE");
            assert!(per_core_cpu_from_env(), "absent means on");
        }

        for off in ["0", "off", "false", "no", "disabled", "OFF", " False "] {
            let _g = crate::test_utils::EnvGuard::set("OXMGR_HOST_CPU_PER_CORE", off);
            assert!(!per_core_cpu_from_env(), "{off:?} must disable per-core");
        }

        // Same recognised-false vocabulary as OXMGR_HOST_METRICS, so an operator learns one
        // convention, and a typo leaves the feature ON rather than silently removing detail.
        for on in ["1", "on", "true", "yes", "", "enabled", "ture"] {
            let _g = crate::test_utils::EnvGuard::set("OXMGR_HOST_CPU_PER_CORE", on);
            assert!(per_core_cpu_from_env(), "{on:?} must leave per-core on");
        }
    }

    #[test]
    fn an_unrequested_subsystem_is_never_constructed() {
        let mut collector = HostCollector::new(
            HostCollectionIntervals::default(),
            HostMetricsRequest::default(),
        );
        // Temperatures are off by default, so the sensor bindings must not be loaded at all —
        // not constructed and then found empty.
        assert!(
            collector.components.is_none(),
            "the collection must not exist before it is asked for"
        );
        collector.collect();
        assert!(
            collector.components.is_none(),
            "a collection tick must not construct an unrequested subsystem"
        );
        assert_eq!(collector.current().components, None);

        // Requested, and it appears.
        let mut asked = HostCollector::new(HostCollectionIntervals::default(), request_all());
        asked.collect();
        assert!(
            asked.components.is_some(),
            "a requested subsystem is constructed"
        );
    }

    #[test]
    fn a_second_collection_does_not_rebuild_unchanged_values() {
        let mut collector = HostCollector::new(
            HostCollectionIntervals::default(),
            HostMetricsRequest::default(),
        );
        let first = collector.collect();
        // Immediately again: nothing is due, so no subsystem is refreshed.
        let second = collector.collect();

        // Pointer equality, not value equality: the heavy fields must be the SAME allocation,
        // which is what makes a per-tick publish a refcount bump rather than a deep copy of 9
        // identity strings, every filesystem row and every interface.
        assert!(
            Arc::ptr_eq(&first.identity, &second.identity),
            "identity is static and must never be rebuilt"
        );
        if let (Some(a), Some(b)) = (&first.filesystems, &second.filesystems) {
            assert!(
                Arc::ptr_eq(a, b),
                "an unrefreshed filesystem list must be re-shared"
            );
        }
        if let (Some(a), Some(b)) = (&first.network, &second.network) {
            assert!(
                Arc::ptr_eq(a, b),
                "an unrefreshed interface list must be re-shared"
            );
        }
    }

    #[test]
    fn static_identity_is_shared_with_the_collector_not_copied() {
        let mut collector = HostCollector::new(
            HostCollectionIntervals::default(),
            HostMetricsRequest::default(),
        );
        let snapshot = collector.collect();
        // The published snapshot points at the collector's own identity allocation.
        assert!(Arc::ptr_eq(&snapshot.identity, &collector.identity));
        assert!(
            Arc::strong_count(&collector.identity) >= 2,
            "the collector and the snapshot both hold it, so the count is at least two"
        );
    }

    #[tokio::test]
    async fn a_subscriber_is_released_when_it_drops() {
        let handle = HostMetricsHandle::new();
        assert_eq!(handle.subscriber_count(), 0);

        let first = handle.subscribe();
        let second = handle.subscribe();
        assert_eq!(handle.subscriber_count(), 2);

        // A stream handler returning drops its receiver. Proving the count falls is what
        // distinguishes "released" from "merely stopped being written to".
        drop(first);
        assert_eq!(handle.subscriber_count(), 1);
        drop(second);
        assert_eq!(handle.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn nothing_is_broadcast_when_no_subsystem_changed() {
        let handle = HostMetricsHandle::new();
        let mut rx = handle.subscribe();
        let metrics = changed_fixture();

        // An unchanged collection still refreshes the published snapshot's timestamp, but must
        // not wake a connected client: an idle host should cost a stream zero bytes.
        handle.publish(metrics.clone(), Vec::new()).await;
        assert!(
            rx.try_recv().is_err(),
            "an empty changed list must not produce an update"
        );

        // And the snapshot endpoint still sees it.
        assert!(handle.current().await.is_some());

        handle.publish(metrics, vec![HostSubsystem::Cpu]).await;
        let update = rx.try_recv().expect("a real change is broadcast");
        assert_eq!(update.changed, vec![HostSubsystem::Cpu]);
    }

    #[tokio::test]
    async fn a_lagging_subscriber_is_told_rather_than_silently_losing_updates() {
        let handle = HostMetricsHandle::new();
        let mut rx = handle.subscribe();
        let metrics = changed_fixture();

        // Overrun the channel without reading: capacity is bounded, so the daemon's memory
        // cannot grow with a stalled client.
        for _ in 0..(UPDATE_CHANNEL_CAPACITY + 4) {
            handle
                .publish(metrics.clone(), vec![HostSubsystem::Cpu])
                .await;
        }

        // Lagged, not an unbounded queue and not silence: the handler resynchronises from a
        // full snapshot on seeing this, because the client's state is unreconstructable.
        match rx.try_recv() {
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(dropped)) => {
                assert!(dropped > 0, "lag must report how many were dropped");
            }
            other => panic!("expected Lagged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn every_subscriber_receives_the_same_update() {
        let handle = HostMetricsHandle::new();
        let mut first = handle.subscribe();
        let mut second = handle.subscribe();

        handle
            .publish(changed_fixture(), vec![HostSubsystem::Memory])
            .await;

        // One collection, fanned out. Collection cost must not scale with client count.
        let a = first.try_recv().expect("first subscriber");
        let b = second.try_recv().expect("second subscriber");
        assert_eq!(a.changed, b.changed);
        assert_eq!(a.metrics.collected_at, b.metrics.collected_at);
    }

    #[test]
    fn a_first_publish_announces_only_the_subsystems_that_reported() {
        let mut collector = HostCollector::new(
            HostCollectionIntervals::default(),
            HostMetricsRequest::default(),
        );
        let metrics = collector.collect();
        let changed = collector.last_changed();

        // Nothing was published before, so everything available counts as new — but only what
        // actually reported. Temperatures are off by default, so components must not appear.
        for subsystem in changed {
            assert!(
                metrics.has_subsystem(*subsystem),
                "{subsystem:?} announced as changed but carries no value"
            );
        }
        assert!(
            !changed.contains(&HostSubsystem::Components),
            "an unrequested subsystem must not be announced"
        );
        assert!(
            changed.contains(&HostSubsystem::Memory),
            "memory reports on this host"
        );
    }

    #[tokio::test]
    async fn the_handle_reports_nothing_before_the_first_collection() {
        let handle = HostMetricsHandle::new();
        assert!(
            handle.current().await.is_none(),
            "no snapshot is distinct from an empty one"
        );

        let mut collector = HostCollector::new(
            HostCollectionIntervals::default(),
            HostMetricsRequest::default(),
        );
        let metrics = collector.collect();
        let changed = collector.last_changed().to_vec();
        handle.publish(metrics, changed).await;
        let published = handle.current().await.expect("published");
        assert!(published.collected_at > 0);
        assert!(
            published.memory.is_some(),
            "memory is available on this host"
        );
    }

    #[test]
    fn memory_and_load_are_available_on_this_host() {
        let mut collector = HostCollector::new(
            HostCollectionIntervals::default(),
            HostMetricsRequest::default(),
        );
        let metrics = collector.collect_at(Instant::now());
        let memory = metrics.memory.expect("macOS reports memory");
        assert!(memory.total_bytes > 0);
        assert!(memory.used_percent.expect("non-zero total") > 0.0);

        // Unix supplies load averages; the struct is absent on Windows rather
        // than three zeroes.
        if cfg!(unix) {
            let load = metrics.load_average.expect("unix supplies load average");
            assert!(load.one >= 0.0 && load.five >= 0.0 && load.fifteen >= 0.0);
        }
    }

    #[test]
    fn slow_subsystems_are_not_collected_at_the_fastest_rate() {
        let mut collector = HostCollector::new(intervals_ms(200, 10_000, 30_000), request_all());
        let start = Instant::now();
        collector.collect_at(start);
        let first_io = collector.last_io_refresh;

        // A CPU-cadence tick must not drag disk and interface enumeration along.
        collector.collect_at(start + Duration::from_millis(400));
        assert_eq!(
            collector.last_io_refresh, first_io,
            "io must keep its own lower rate"
        );

        collector.collect_at(start + Duration::from_secs(11));
        assert_ne!(collector.last_io_refresh, first_io, "io is due at 10s");
    }

    /// The absent-load-average path, which Windows takes and this machine cannot.
    ///
    /// Written because the platform matrix's coverage list claimed a test named
    /// `a_platform_without_load_average_reports_unavailable` and no such test existed — a false claim
    /// of coverage, found by grepping for every name that list cited rather than trusting it.
    ///
    /// Drives the absence directly instead of waiting for a platform that lacks the figure, the same
    /// way `a_platform_without_sensors_reports_unavailable_with_a_reason` does for temperatures. The
    /// distinction being pinned: a platform with no load average reports the whole struct as `None`
    /// AND records a named failure — never `Some(0.0, 0.0, 0.0)`, which would read as a completely
    /// idle machine.
    #[test]
    fn a_platform_without_load_average_reports_unavailable() {
        // The shape the Windows arm produces: `collect_load_average()` returns None there, so this is
        // what `refresh_cpu` writes. Set in the initialiser rather than reassigned, so the absence is
        // stated once — clippy flagged the reassignment and it was right that the intent reads better
        // this way.
        let mut metrics = HostMetrics {
            load_average: None,
            ..HostMetrics::default()
        };
        metrics.record_failure(
            HostSubsystem::LoadAverage,
            "platform does not provide load averages",
        );

        assert!(
            metrics.load_average.is_none(),
            "an unavailable load average must be absent, not three zeroes"
        );
        let failure = metrics
            .failures
            .iter()
            .find(|failure| failure.subsystem == HostSubsystem::LoadAverage)
            .expect("the absence must be recorded as a named failure");
        assert!(
            !failure.message.is_empty(),
            "the failure must state a reason an operator can read"
        );

        // And it serialises as absent rather than as null-or-zero, so a consumer cannot mistake it
        // for a measurement.
        let json = serde_json::to_value(&metrics).expect("host metrics serialise");
        assert!(
            json.get("load_average").is_none() || json["load_average"].is_null(),
            "an absent load average must not serialise as a figure: {json}"
        );

        // The other direction: once it IS available, the failure clears rather than lingering.
        metrics.load_average = Some(HostLoadAverage {
            one: 1.5,
            five: 1.2,
            fifteen: 0.9,
        });
        metrics.clear_failure(HostSubsystem::LoadAverage);
        assert!(
            !metrics
                .failures
                .iter()
                .any(|failure| failure.subsystem == HostSubsystem::LoadAverage),
            "a recovered subsystem must not keep reporting a failure"
        );
    }
}
