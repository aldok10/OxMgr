//! Detecting whether this daemon runs inside a container, and what its real limits are.
//!
//! Lint-level cleanup: percentage casts divide cgroup byte counts bounded by the
//! host's real memory (≪ 2^53), so u64→f64 is exact for every real value.
//!
//! # Why this module exists
//!
//! Every percentage the daemon reports divides a reading by a total, and inside a container the
//! total that `sysinfo` reports is the **host's**, not the one the kernel will enforce. The
//! codebase already knew this and said so in five places — `advisories.rs` ("inside a container
//! `total_memory` is typically the host's, not the cgroup limit"), `severity.rs`, `docs/HOST-METRICS.md`
//! — and then had nothing that could tell the difference. Every one of those comments was a caveat
//! written for a reader instead of a value the code could use.
//!
//! The consequence is not cosmetic. A 512 MB container on a 64 GB host sitting at 500 MB is at 97%
//! of what it is allowed and reports **0.8%**. Nothing warns, no severity band trips, and the leak
//! forecast projects against a limit two orders of magnitude too high. The process is killed by the
//! OOM killer while every figure on the dashboard reads green.
//!
//! # What is detected, and what is refused
//!
//! `Limits` reports what the kernel will actually enforce, per resource, with the SOURCE of each
//! figure attached. A caller can therefore tell "512 MB, from a cgroup v2 memory.max" from
//! "64 GB, from the host because no limit applies" — and a caller that ignores the distinction
//! still gets a usable number rather than a wrong one.
//!
//! Nothing here guesses. On a host with no cgroup limit the answer is
//! [`LimitSource::HostCapacity`], which is the truth, not a fallback. On a platform with no cgroup
//! concept at all the answer is the same, because a macOS or Windows process genuinely is bounded
//! by the machine.
//!
//! # cgroup v2 first, v1 second
//!
//! v2 (`/sys/fs/cgroup/memory.max`) is the unified hierarchy every current runtime uses. v1
//! (`/sys/fs/cgroup/memory/memory.limit_in_bytes`) is still what older Docker on older kernels
//! mounts, and reading it costs one extra `stat` on a path that does not exist on a v2 host.
//!
//! A v1 limit of `9223372036854771712` (`i64::MAX` rounded to the page size) means "no limit" and
//! is treated as absent rather than as a 8 exabyte quota — a real value that would otherwise sail
//! through every sanity check.

use std::fmt;

/// The environment the daemon believes it is running in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Environment {
    /// A physical or virtual machine with no container limits applying to this process.
    Host,
    /// Inside a container with at least one enforced limit.
    /// Only constructible on Linux, where cgroup limits exist; other platforms keep the variant
    /// for the type's API (`is_containerised`, test fixtures) without ever building one.
    Container(Runtime),
}

/// Which containerisation was detected. Named because the runtime changes what an operator does
/// next: a Kubernetes pod limit is edited in a manifest, a plain Docker one on the command line.
///
/// On non-Linux platforms no variant can ever be constructed (detection is Linux-only), but the
/// variants stay for the wire protocol and test fixtures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Runtime {
    Docker,
    Kubernetes,
    Podman,
    /// LXC, systemd-nspawn, or a cgroup limit with no runtime marker. The limit is real even when
    /// its origin is not identifiable, and reporting "unknown runtime" is honest where guessing
    /// "Docker" would not be.
    Unknown,
}

impl fmt::Display for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Docker => "docker",
            Self::Kubernetes => "kubernetes",
            Self::Podman => "podman",
            Self::Unknown => "container",
        })
    }
}

/// Where a limit figure came from. Carried WITH the figure so a consumer can qualify it rather
/// than having to assume.
///
/// Non-Linux platforms only ever produce [`LimitSource::HostCapacity`]; the cgroup sources exist
/// for the Linux path and the wire protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LimitSource {
    /// A cgroup v2 interface file.
    CgroupV2,
    /// A cgroup v1 interface file.
    CgroupV1,
    /// No limit applies, so the machine's own capacity is the limit. This is a real answer.
    HostCapacity,
}

impl LimitSource {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::CgroupV2 => "cgroup_v2",
            Self::CgroupV1 => "cgroup_v1",
            Self::HostCapacity => "host",
        }
    }

    /// Whether this figure is a container limit rather than the machine's capacity.
    pub fn is_container_limit(self) -> bool {
        matches!(self, Self::CgroupV2 | Self::CgroupV1)
    }
}

/// One resolved limit: the value, and where it came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedLimit<T> {
    pub value: T,
    pub source: LimitSource,
}

impl<T> ResolvedLimit<T> {
    fn host(value: T) -> Self {
        Self {
            value,
            source: LimitSource::HostCapacity,
        }
    }
}

/// What the kernel will actually enforce on this daemon.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub environment: Environment,
    /// The memory ceiling in bytes.
    pub memory_bytes: ResolvedLimit<u64>,
    /// CPUs available, as a fraction — a 1.5-core quota is `1.5`, not `2`.
    ///
    /// Fractional because that is what a cgroup quota actually expresses, and rounding it up is
    /// how a 0.5-CPU container comes to believe it has a whole core: its 50%-of-one-core ceiling
    /// then reads as 50% utilisation when it is in fact saturated.
    pub cpu_count: ResolvedLimit<f64>,
}

impl Limits {
    /// Whether any container limit applies. Cheap discriminator for callers that only need the
    /// boolean, so they do not have to match on two sources.
    pub fn is_containerised(&self) -> bool {
        matches!(self.environment, Environment::Container(_))
    }

    /// The memory total a percentage should divide by.
    pub fn effective_memory_total(&self) -> u64 {
        self.memory_bytes.value
    }
}

/// The memory ceiling the kernel will enforce, when a container limit exists.
///
/// Detected ONCE at startup from figures the caller already has: cgroups are fixed
/// over a container's lifetime in practice, and per-cycle detection would be a
/// second collection pass the daemon committed not to make (efficiency
/// constraint).
///
/// `None` on a host or when no limit was found — and deliberately never the host's
/// own total. "How big is this machine" and "how much am I allowed" are different
/// answers, and a forecast projects toward the ceiling that actually stops growth.
pub fn enforced_memory_ceiling(host_memory_bytes: u64, host_cpu_count: f64) -> Option<u64> {
    let limits = detect(host_memory_bytes, host_cpu_count);
    limits
        .memory_bytes
        .source
        .is_container_limit()
        .then(|| limits.effective_memory_total())
}

/// Resolves the limits that actually apply, reading cgroup interface files on Linux.
///
/// `host_memory_bytes` and `host_cpu_count` are the machine's own figures, passed in rather than
/// read here so this stays testable without a `sysinfo::System` and so the caller — which already
/// has them — does not pay for a second collection.
///
/// Never fails. An unreadable or malformed cgroup file means "no limit detected", because a parse
/// error must not turn into a wrong ceiling: falling back to host capacity is the conservative
/// direction, and it is what a host with no limits reports anyway.
#[cfg(target_os = "linux")]
pub fn detect(host_memory_bytes: u64, host_cpu_count: f64) -> Limits {
    let memory = read_memory_limit(host_memory_bytes);
    let cpu = read_cpu_limit(host_cpu_count);
    // The environment is decided by whether a LIMIT was found, not by whether a marker file
    // exists. A container with no limits set is, for every calculation this daemon performs,
    // indistinguishable from a host — and saying "container" there would qualify figures that
    // need no qualification.
    let environment = if memory.source.is_container_limit() || cpu.source.is_container_limit() {
        Environment::Container(detect_runtime())
    } else {
        Environment::Host
    };
    Limits {
        environment,
        memory_bytes: memory,
        cpu_count: cpu,
    }
}

/// Non-Linux: no cgroup concept, so the machine IS the limit. Not a degraded answer.
#[cfg(not(target_os = "linux"))]
pub fn detect(host_memory_bytes: u64, host_cpu_count: f64) -> Limits {
    Limits {
        environment: Environment::Host,
        memory_bytes: ResolvedLimit::host(host_memory_bytes),
        cpu_count: ResolvedLimit::host(host_cpu_count),
    }
}

/// A cgroup v1 "no limit" sentinel: `i64::MAX` rounded down to a page boundary.
///
/// Treated as absent rather than as a real quota. Without this check an unlimited v1 container
/// reports an 8 exabyte ceiling, which passes every plausibility test and silently makes memory
/// percentages read as zero.
#[cfg(target_os = "linux")]
const V1_UNLIMITED: u64 = 9_223_372_036_854_771_712;

/// The container's OWN memory usage, when running under a cgroup.
///
/// This exists because of a defect measured in Docker, not from reading the docs. `sysinfo`'s
/// `used_memory()` reports the whole machine's usage — inside a 512 MB container on a 4 GB VM it
/// returned **1.6 GB**, while the cgroup's `memory.current` said the container was using **1.5 MB**.
/// Dividing the machine's usage by the container's limit produced **299.8%**, and my first version
/// of the assertion only checked that the container percentage was LARGER than the host one, so a
/// physically impossible figure passed the test.
///
/// A percentage needs both halves from the same scope. The limit comes from `memory.max`, so the
/// usage has to come from `memory.current`.
///
/// `None` when no cgroup usage file is readable, which is the honest answer: better to withhold the
/// container percentage than to publish one built from two different denominators.
#[cfg(target_os = "linux")]
pub fn current_usage_bytes() -> Option<u64> {
    // v2 first, matching `read_memory_limit`.
    if let Some(raw) = read_trimmed("/sys/fs/cgroup/memory.current")
        && let Ok(bytes) = raw.parse::<u64>()
    {
        return Some(bytes);
    }
    if let Some(raw) = read_trimmed("/sys/fs/cgroup/memory/memory.usage_in_bytes")
        && let Ok(bytes) = raw.parse::<u64>()
    {
        return Some(bytes);
    }
    None
}

/// Non-Linux: no cgroup, so there is no container-scoped usage to read.
#[cfg(not(target_os = "linux"))]
pub fn current_usage_bytes() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn read_memory_limit(host_bytes: u64) -> ResolvedLimit<u64> {
    // v2 first: the unified hierarchy is what every current runtime mounts. "max" is the literal
    // string the kernel writes for "no limit".
    if let Some(raw) = read_trimmed("/sys/fs/cgroup/memory.max")
        && raw != "max"
        && let Ok(bytes) = raw.parse::<u64>()
        // A limit at or above host memory is not a limit: some runtimes write the host
        // total rather than "max". Reporting it as a container limit would qualify a
        // figure that needs no qualification.
        && bytes > 0
        && (host_bytes == 0 || bytes < host_bytes)
    {
        return ResolvedLimit {
            value: bytes,
            source: LimitSource::CgroupV2,
        };
    }

    if let Some(raw) = read_trimmed("/sys/fs/cgroup/memory/memory.limit_in_bytes")
        && let Ok(bytes) = raw.parse::<u64>()
        && bytes > 0
        && bytes != V1_UNLIMITED
        && (host_bytes == 0 || bytes < host_bytes)
    {
        return ResolvedLimit {
            value: bytes,
            source: LimitSource::CgroupV1,
        };
    }

    ResolvedLimit::host(host_bytes)
}

#[cfg(target_os = "linux")]
fn read_cpu_limit(host_cpus: f64) -> ResolvedLimit<f64> {
    // v2 `cpu.max` is "QUOTA PERIOD", e.g. "150000 100000" for 1.5 CPUs, or "max 100000".
    if let Some(raw) = read_trimmed("/sys/fs/cgroup/cpu.max") {
        let mut parts = raw.split_whitespace();
        if let (Some(quota), Some(period)) = (parts.next(), parts.next())
            && quota != "max"
            && let (Ok(quota), Ok(period)) = (quota.parse::<f64>(), period.parse::<f64>())
            && quota > 0.0
            && period > 0.0
        {
            let cpus = quota / period;
            if cpus > 0.0 && cpus < host_cpus {
                return ResolvedLimit {
                    value: cpus,
                    source: LimitSource::CgroupV2,
                };
            }
        }
    }

    // v1 splits the same figure across two files.
    let quota =
        read_trimmed("/sys/fs/cgroup/cpu/cpu.cfs_quota_us").and_then(|r| r.parse::<i64>().ok());
    let period =
        read_trimmed("/sys/fs/cgroup/cpu/cpu.cfs_period_us").and_then(|r| r.parse::<i64>().ok());
    if let (Some(quota), Some(period)) = (quota, period) {
        // -1 is v1's "no quota".
        if quota > 0 && period > 0 {
            // Both values are guaranteed positive at this point, so i64→u64 is
            // safe. try_from cannot fail given the guard above; a failure would
            // mean the value changed between the guard and here, so fall back to
            // host capacity rather than emit a wrong verdict.
            let (Ok(quota), Ok(period)) = (u64::try_from(quota), u64::try_from(period)) else {
                return ResolvedLimit::host(host_cpus);
            };
            // u64_to_f64 is the crate's sanctioned conversion for bounded host values.
            let cpus =
                oxmgr_core::numeric::u64_to_f64(quota) / oxmgr_core::numeric::u64_to_f64(period);
            if cpus > 0.0 && cpus < host_cpus {
                return ResolvedLimit {
                    value: cpus,
                    source: LimitSource::CgroupV1,
                };
            }
        }
    }

    ResolvedLimit::host(host_cpus)
}

/// Identifies the runtime from filesystem markers.
///
/// Only reached once a limit has already been found, so this names something that is definitely
/// there rather than being the detection itself. Order matters: a Kubernetes pod is also a Docker
/// (or containerd) container, so the more specific marker is checked first.
#[cfg(target_os = "linux")]
fn detect_runtime() -> Runtime {
    if std::env::var_os("KUBERNETES_SERVICE_HOST").is_some() {
        return Runtime::Kubernetes;
    }
    if std::path::Path::new("/run/.containerenv").exists() {
        return Runtime::Podman;
    }
    if std::path::Path::new("/.dockerenv").exists() {
        return Runtime::Docker;
    }
    // A cgroup limit with no marker: LXC, nspawn, or a runtime that leaves no trace. The limit is
    // real, so this is `Unknown` rather than `Host`.
    Runtime::Unknown
}

#[cfg(target_os = "linux")]
fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxmgr_core::numeric::u64_to_f64;

    #[test]
    fn a_host_with_no_limits_reports_host_capacity() {
        // The common case, and it must not be described as a container: qualifying every figure on
        // an ordinary server would make the qualification meaningless where it matters.
        let limits = detect(64 * 1024 * 1024 * 1024, 8.0);
        // On a developer machine or CI runner without cgroup limits this is Host. Inside a limited
        // container it is Container — so the assertion is on the INVARIANT rather than on one of
        // the two answers, which would fail depending on where the suite runs.
        match limits.environment {
            Environment::Host => {
                assert_eq!(limits.memory_bytes.source, LimitSource::HostCapacity);
                assert_eq!(limits.memory_bytes.value, 64 * 1024 * 1024 * 1024);
                assert_eq!(limits.cpu_count.value, 8.0);
            }
            Environment::Container(_) => {
                // A limit was found, so at least one source must say so — otherwise the
                // environment and the sources disagree, which is the bug this guards.
                assert!(
                    limits.memory_bytes.source.is_container_limit()
                        || limits.cpu_count.source.is_container_limit(),
                    "Container reported with no container-sourced limit"
                );
            }
        }
    }

    #[test]
    fn a_container_limit_never_exceeds_host_capacity() {
        // The plausibility guard. Some runtimes write the host total into `memory.max` rather than
        // "max", and accepting it would report a "container limit" identical to host memory —
        // qualifying a figure that needs no qualification.
        let limits = detect(1024, 1.0);
        assert!(
            limits.memory_bytes.value <= 1024
                || limits.memory_bytes.source == LimitSource::HostCapacity,
            "a container limit must be below host capacity, got {} from {:?}",
            limits.memory_bytes.value,
            limits.memory_bytes.source
        );
    }

    /// Asserts detection against REAL cgroup files, and is therefore gated.
    ///
    /// Gated rather than skipped-by-detection: a test that silently adapts to its environment
    /// cannot fail, and this one exists specifically to prove the Linux cgroup branch reads what
    /// the kernel wrote. On macOS `/sys/fs/cgroup` cannot exist, so the branch is unreachable and a
    /// passing run on a dev machine would prove only that the non-Linux path returns host capacity.
    ///
    /// Run with `OXMGR_CONTAINER_LIMIT_MB` set to the container's configured memory limit:
    ///   docker run --memory 512m -e OXMGR_CONTAINER_LIMIT_MB=512 ... cargo test container
    #[test]
    fn a_real_container_limit_is_read_from_cgroup() {
        let Ok(expected_mb) = std::env::var("OXMGR_CONTAINER_LIMIT_MB") else {
            eprintln!(
                "skipping: set OXMGR_CONTAINER_LIMIT_MB to the container's limit to run this"
            );
            return;
        };
        let expected_bytes: u64 = expected_mb
            .trim()
            .parse::<u64>()
            .expect("OXMGR_CONTAINER_LIMIT_MB must be a whole number of MB")
            * 1024
            * 1024;

        // A host total far above any plausible container limit, so the plausibility guard cannot
        // reject the real figure.
        let limits = detect(1024 * 1024 * 1024 * 1024, 64.0);
        eprintln!("detected: {limits:?}");

        assert!(
            limits.is_containerised(),
            "a configured limit must be detected as a container: {limits:?}"
        );
        assert_eq!(
            limits.memory_bytes.value, expected_bytes,
            "the detected limit must equal the configured one"
        );
        assert!(
            limits.memory_bytes.source.is_container_limit(),
            "the limit must be attributed to a cgroup, got {:?}",
            limits.memory_bytes.source
        );
        // The whole reason the module exists: the denominator a percentage uses.
        assert_eq!(limits.effective_memory_total(), expected_bytes);
    }

    /// The end-to-end claim, asserted through `HostMetrics` rather than through `detect` alone.
    ///
    /// Gated on the same variable as the detection test, and for the same reason: this exercises the
    /// path a running daemon takes, and on macOS the cgroup branch cannot execute at all.
    #[test]
    fn container_memory_percentage_uses_the_enforced_limit() {
        let Ok(expected_mb) = std::env::var("OXMGR_CONTAINER_LIMIT_MB") else {
            eprintln!("skipping: set OXMGR_CONTAINER_LIMIT_MB to run this");
            return;
        };
        let limit_bytes: u64 = expected_mb.trim().parse::<u64>().expect("whole MB") * 1024 * 1024;

        let mut collector = crate::host_metrics::HostCollector::new(
            crate::host_metrics::HostCollectionIntervals::default(),
            crate::host_metrics::HostMetricsRequest::default(),
        );
        let metrics = collector.collect();
        let memory = metrics.memory.as_ref().expect("memory must be collected");

        eprintln!(
            "host total={} effective={:?} used={} used_pct={:?} effective_pct={:?}",
            memory.total_bytes,
            memory.effective_total_bytes,
            memory.used_bytes,
            memory.used_percent,
            memory.effective_used_percent
        );

        let effective = memory
            .effective_total_bytes
            .expect("inside a limited container the effective total must be present");
        assert_eq!(
            effective, limit_bytes,
            "the effective total must be the cgroup limit, not the host's memory"
        );

        let against_limit = memory
            .effective_used_percent
            .expect("effective percentage must be present when a limit applies");

        // PLAUSIBILITY FIRST. This assertion is here because its absence let a real defect pass:
        // the first version only checked that the container percentage exceeded the host one, and
        // 299.8% satisfied that happily. A percentage of an enforced ceiling cannot exceed 100 by
        // any meaningful margin — the kernel kills the process instead.
        assert!(
            against_limit > 0.0 && against_limit <= 100.0,
            "a percentage of the enforced limit must be within 0..=100, got {against_limit} \
             (this is what a host-usage-over-container-limit division produces)"
        );

        // Recomputed from the CGROUP's own usage, which is the figure the percentage must be built
        // from. `memory.used_bytes` is the machine's usage and would reproduce the bug.
        let cgroup_used =
            current_usage_bytes().expect("cgroup usage must be readable inside a container");
        assert!(
            cgroup_used <= effective,
            "cgroup usage {cgroup_used} cannot exceed its own limit {effective}"
        );
        let recomputed = u64_to_f64(cgroup_used) / u64_to_f64(effective) * 100.0;
        assert!(
            (f64::from(against_limit) - recomputed).abs() < 1.0,
            "reported {against_limit} must match the recomputation from cgroup usage {recomputed}"
        );

        // And the scopes must not be mixed: the machine's usage is unrelated to this ceiling.
        assert!(
            memory.used_bytes != cgroup_used || memory.total_bytes == effective,
            "host usage and cgroup usage should differ inside a container; if they match, the \
             wrong source is being read"
        );
    }

    #[test]
    fn effective_total_is_the_limit_not_the_host() {
        // What the whole module exists for: the figure a percentage divides by.
        let limits = Limits {
            environment: Environment::Container(Runtime::Docker),
            memory_bytes: ResolvedLimit {
                value: 512 * 1024 * 1024,
                source: LimitSource::CgroupV2,
            },
            cpu_count: ResolvedLimit::host(8.0),
        };
        assert!(limits.is_containerised());
        assert_eq!(limits.effective_memory_total(), 512 * 1024 * 1024);

        // The arithmetic this prevents: 500 MB inside a 512 MB container is 97%, not 0.8%.
        let used = 500.0 * 1024.0 * 1024.0;
        let against_limit = used / u64_to_f64(limits.effective_memory_total()) * 100.0;
        let against_host = used / (64.0 * 1024.0 * 1024.0 * 1024.0) * 100.0;
        assert!(against_limit > 90.0, "got {against_limit}");
        assert!(against_host < 1.0, "got {against_host}");
    }

    #[test]
    fn host_capacity_is_not_a_container_limit() {
        assert!(!LimitSource::HostCapacity.is_container_limit());
        assert!(LimitSource::CgroupV2.is_container_limit());
        assert!(LimitSource::CgroupV1.is_container_limit());
    }

    #[test]
    fn wire_names_are_stable() {
        // These reach the dashboard, so a rename is a breaking change and should fail here first.
        assert_eq!(LimitSource::CgroupV2.as_wire(), "cgroup_v2");
        assert_eq!(LimitSource::CgroupV1.as_wire(), "cgroup_v1");
        assert_eq!(LimitSource::HostCapacity.as_wire(), "host");
        assert_eq!(Runtime::Kubernetes.to_string(), "kubernetes");
        assert_eq!(Runtime::Unknown.to_string(), "container");
    }

    #[test]
    fn a_fractional_cpu_quota_stays_fractional() {
        // Rounding 0.5 up to 1 is how a half-CPU container comes to believe a saturated core is
        // 50% busy. Asserted on the type rather than on a read, since the file may not exist here.
        let limits = Limits {
            environment: Environment::Container(Runtime::Kubernetes),
            memory_bytes: ResolvedLimit::host(0),
            cpu_count: ResolvedLimit {
                value: 0.5,
                source: LimitSource::CgroupV2,
            },
        };
        assert_eq!(limits.cpu_count.value, 0.5);
    }
}
