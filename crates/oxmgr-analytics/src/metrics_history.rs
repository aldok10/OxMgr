//! Bounded per-process metric history: retention, downsampling, and the memory bound.
//!
//! Lint-level cleanup: sample conversion casts are bounded per-process counters
//! (u64 bytes < 2^53 on any real host), so f64 samples are exact.
//!
//! Scaffold for OpenSpec change `process-intelligence`, tasks section(s) 1.
//! The contract is `openspec/changes/process-intelligence/specs/`; read it before adding to this file.
//!
//! Pure logic on purpose: nothing here reaches for the process manager, the HTTP layer or the
//! dashboard, so it can be unit-tested without a running daemon. Wiring it into those is a
//! separate integration step, which is why the module is currently unreferenced.
//!
//! Two tiers, not three: raw samples at the tick rate plus one-minute aggregates. The per-hour
//! tier the design leaves optional is not built, because nothing yet asks a day-scale question
//! and an unused roll-up path is a path nobody has tested. Adding it is one more `Ring` and one
//! more period index; the shape here does not have to change to accommodate it.
//!
//! A missing sample is never a zero. The daemon's maintenance tick uses
//! `MissedTickBehavior::Skip`, so ticks are dropped under load and the spacing between samples
//! stretches; disk figures additionally carry no measurement at all until there are two samples
//! for one pid to difference (see `ManagedProcess::record_io_sample`). Both cases are carried as
//! `None` here rather than as `0`, matching `src/process.rs` and `src/host_metrics.rs`: a period
//! with no samples produces no aggregate rather than an aggregate of zero.

use oxmgr_core::numeric::u64_to_f64;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt;

/// A metric the history retains. Naming a metric by enum rather than by string is what makes
/// "unknown metric" a refusal at the edge instead of an empty result that reads like "no data".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetricKind {
    /// CPU share as `sysinfo` reports it: percent of one core, so a busy multi-threaded process
    /// legitimately exceeds 100.
    Cpu,
    /// Resident memory in bytes.
    Memory,
    /// Bytes read from disk during the sample's own interval. An amount, not a rate.
    DiskRead,
    /// Bytes written to disk during the sample's own interval. An amount, not a rate.
    DiskWrite,
}

/// Every metric retained, in a fixed order so a caller can iterate tiers without hardcoding the
/// list and silently missing one when a metric is added.
pub const ALL_METRICS: [MetricKind; 4] = [
    MetricKind::Cpu,
    MetricKind::Memory,
    MetricKind::DiskRead,
    MetricKind::DiskWrite,
];

impl MetricKind {
    /// The wire name, matching the `ManagedProcess` field it is sampled from so an operator
    /// reading the process JSON and an operator querying history use the same word.
    pub fn as_str(self) -> &'static str {
        match self {
            MetricKind::Cpu => "cpu_percent",
            MetricKind::Memory => "memory_bytes",
            MetricKind::DiskRead => "disk_read_bytes",
            MetricKind::DiskWrite => "disk_write_bytes",
        }
    }

    /// Parses a metric name, or `None` when it names nothing retained.
    ///
    /// The short aliases exist because `cpu` and `memory` are what anyone types; they are
    /// accepted rather than guessed at, and anything else is refused outright.
    #[cfg(test)]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "cpu_percent" | "cpu" => Some(MetricKind::Cpu),
            "memory_bytes" | "memory" | "mem" => Some(MetricKind::Memory),
            "disk_read_bytes" | "disk_read" => Some(MetricKind::DiskRead),
            "disk_write_bytes" | "disk_write" => Some(MetricKind::DiskWrite),
            _ => None,
        }
    }
}

impl fmt::Display for MetricKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A metric name that names nothing retained. Returned rather than answered with an empty
/// window, because "we do not keep that" and "we keep it and it is empty" are different facts
/// and a caller that cannot tell them apart will report the wrong one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownMetric {
    pub name: String,
}

impl fmt::Display for UnknownMetric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown metric '{}'; retained metrics are ", self.name)?;
        for (index, metric) in ALL_METRICS.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            f.write_str(metric.as_str())?;
        }
        Ok(())
    }
}

impl std::error::Error for UnknownMetric {}

/// One resource reading for one process at one instant.
///
/// The disk fields are `Option` for the same reason they are qualified on `ManagedProcess`: an
/// amount only exists once there are two samples for the same pid to difference, so the first
/// sample after a spawn, an adopt or a restart has no measurement to report. Storing `0` there
/// would make a leak detector read a pid change as a moment of no I/O.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MetricSample {
    /// Unix milliseconds the sample was taken. Milliseconds rather than seconds because the tick
    /// is 2s and roll-up boundaries land inside a second often enough to matter.
    pub at_ms: u64,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    /// Bytes read during [`Self::interval_ms`]. `None` when no usable interval existed.
    pub disk_read_bytes: Option<u64>,
    /// Bytes written during [`Self::interval_ms`]. `None` when no usable interval existed.
    pub disk_write_bytes: Option<u64>,
    /// Milliseconds the disk amounts cover, straight from `ManagedProcess::metrics_interval_ms`.
    /// Retained so a consumer can derive a rate from the interval actually observed instead of
    /// assuming the nominal 2s tick, which overstates the rate whenever a tick was skipped.
    pub interval_ms: Option<u64>,
}

impl MetricSample {
    /// A sample carrying CPU and memory only: the shape of the first reading for a pid, where
    /// the disk amounts do not yet exist.
    pub fn cpu_memory(at_ms: u64, cpu_percent: f32, memory_bytes: u64) -> Self {
        Self {
            at_ms,
            cpu_percent,
            memory_bytes,
            disk_read_bytes: None,
            disk_write_bytes: None,
            interval_ms: None,
        }
    }

    /// Adds the disk amounts and the interval they cover.
    pub fn with_disk(mut self, read_bytes: u64, write_bytes: u64, interval_ms: u64) -> Self {
        self.disk_read_bytes = Some(read_bytes);
        self.disk_write_bytes = Some(write_bytes);
        self.interval_ms = Some(interval_ms);
        self
    }

    /// This sample's value for one metric, or `None` when the sample carries no measurement for
    /// it. CPU and memory are always measured when a sample exists; disk is not.
    pub fn value(&self, metric: MetricKind) -> Option<f64> {
        match metric {
            MetricKind::Cpu => Some(f64::from(self.cpu_percent)),
            MetricKind::Memory => Some(u64_to_f64(self.memory_bytes)),
            MetricKind::DiskRead => self.disk_read_bytes.map(u64_to_f64),
            MetricKind::DiskWrite => self.disk_write_bytes.map(u64_to_f64),
        }
    }
}

/// Min, max, mean and count for one metric over one closed period.
///
/// Deliberately not enough to reconstruct the samples: keeping the range and the centre is what
/// makes an older window cheap, and `count` is what stops a summary from being mistaken for a
/// single reading. A period whose samples all lacked a measurement has no aggregate at all
/// rather than one with `count: 0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MetricAggregate {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    /// How many samples carried a measurement for this metric. Always at least 1.
    pub count: u32,
}

/// One closed period's summary across every metric.
///
/// `start_ms` and `end_ms` are the first and last sample actually seen in the period, not the
/// period's nominal edges: reporting the nominal edges would claim coverage of ticks that were
/// skipped.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AggregateSample {
    pub start_ms: u64,
    pub end_ms: u64,
    /// Samples folded into this period, across all metrics. Distinct from a per-metric `count`,
    /// which can be lower when disk measurements were missing.
    pub samples: u32,
    pub cpu: Option<MetricAggregate>,
    pub memory: Option<MetricAggregate>,
    pub disk_read: Option<MetricAggregate>,
    pub disk_write: Option<MetricAggregate>,
}

impl AggregateSample {
    /// This period's summary for one metric, or `None` when nothing in the period measured it.
    pub fn aggregate(&self, metric: MetricKind) -> Option<MetricAggregate> {
        match metric {
            MetricKind::Cpu => self.cpu,
            MetricKind::Memory => self.memory,
            MetricKind::DiskRead => self.disk_read,
            MetricKind::DiskWrite => self.disk_write,
        }
    }
}

/// Streaming min/max/sum for one metric inside the period currently open.
///
/// Held as running state rather than a buffer of the period's samples, because roll-up must cost
/// the same whatever the period contained: closing a period is four `Option` reads and four
/// divisions, not a walk. That is the constant-cost requirement, and a `Vec` per open period
/// would quietly break it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct MetricAccumulator {
    min: f64,
    max: f64,
    sum: f64,
    count: u32,
}

impl MetricAccumulator {
    fn push(&mut self, value: f64) {
        if self.count == 0 {
            self.min = value;
            self.max = value;
        } else {
            if value < self.min {
                self.min = value;
            }
            if value > self.max {
                self.max = value;
            }
        }
        self.sum += value;
        self.count += 1;
    }

    /// The closed summary, or `None` when nothing measured this metric during the period.
    fn finish(self) -> Option<MetricAggregate> {
        if self.count == 0 {
            return None;
        }
        Some(MetricAggregate {
            min: self.min,
            max: self.max,
            mean: self.sum / f64::from(self.count),
            count: self.count,
        })
    }
}

/// The period currently being accumulated, across every metric.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct PeriodAccumulator {
    /// Which period this is: `at_ms / period_ms`. Compared against the incoming sample's index to
    /// decide whether the open period has closed, which is how roll-up needs no timer of its own.
    index: u64,
    start_ms: u64,
    end_ms: u64,
    samples: u32,
    cpu: MetricAccumulator,
    memory: MetricAccumulator,
    disk_read: MetricAccumulator,
    disk_write: MetricAccumulator,
}

impl PeriodAccumulator {
    fn open(index: u64, sample: &MetricSample) -> Self {
        let mut period = Self {
            index,
            start_ms: sample.at_ms,
            end_ms: sample.at_ms,
            ..Self::default()
        };
        period.push(sample);
        period
    }

    fn push(&mut self, sample: &MetricSample) {
        self.end_ms = sample.at_ms;
        self.samples += 1;
        // A metric with no measurement in this sample is skipped, not pushed as 0. Folding the
        // missing disk amount in as a zero would drag the period's mean toward zero and make its
        // min zero, which reads as "the process briefly stopped doing I/O" — a measurement that
        // was never taken.
        if let Some(value) = sample.value(MetricKind::Cpu) {
            self.cpu.push(value);
        }
        if let Some(value) = sample.value(MetricKind::Memory) {
            self.memory.push(value);
        }
        if let Some(value) = sample.value(MetricKind::DiskRead) {
            self.disk_read.push(value);
        }
        if let Some(value) = sample.value(MetricKind::DiskWrite) {
            self.disk_write.push(value);
        }
    }

    fn finish(self) -> AggregateSample {
        AggregateSample {
            start_ms: self.start_ms,
            end_ms: self.end_ms,
            samples: self.samples,
            cpu: self.cpu.finish(),
            memory: self.memory.finish(),
            disk_read: self.disk_read.finish(),
            disk_write: self.disk_write.finish(),
        }
    }
}

/// A fixed-capacity ring: append is O(1), and at capacity the oldest entry leaves to make room.
///
/// `VecDeque::with_capacity` allocates once at construction and `push_back` after a matching
/// `pop_front` reuses that allocation, so a full ring never reallocates and its memory does not
/// move with uptime. Capacity 0 is not representable — [`Ring::new`] raises it to 1 — because a
/// zero-capacity ring silently discards every sample, which looks identical to sampling being
/// broken.
#[derive(Debug, Clone)]
struct Ring<T> {
    entries: VecDeque<T>,
    capacity: usize,
}

impl<T> Ring<T> {
    fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Appends, evicting the oldest entry first when full. Returns the evicted entry so a caller
    /// that needs to know what was dropped does not have to read it back before pushing.
    fn push(&mut self, entry: T) -> Option<T> {
        let evicted = if self.entries.len() == self.capacity {
            self.entries.pop_front()
        } else {
            None
        };
        self.entries.push_back(entry);
        evicted
    }

    fn iter(&self) -> impl DoubleEndedIterator<Item = &T> {
        self.entries.iter()
    }

    fn oldest(&self) -> Option<&T> {
        self.entries.front()
    }

    fn newest(&self) -> Option<&T> {
        self.entries.back()
    }
}

/// Raw samples kept per process by default: 900.
///
/// At the 2s maintenance tick that is 1800s = 30 minutes of tick-resolution history, which covers
/// the short windows detection asks about (a step, a spike, a change point over adjacent windows)
/// without keeping seconds-resolution data for hours. Skipped ticks stretch the span rather than
/// shorten it, so 30 minutes is the floor of what 900 slots cover, not the ceiling.
pub const DEFAULT_RAW_CAPACITY: usize = 900;

/// One-minute aggregates kept per process by default: 720 — 12 hours.
///
/// The leak window is the thing that sets this. A memory leak worth acting on shows a trend over
/// hours, and 12 hours of minute-resolution history is enough to fit a regression over a leak and
/// still see the level it started from. A day-scale (per-hour) tier is deliberately absent until
/// something asks a day-scale question.
pub const DEFAULT_MINUTE_CAPACITY: usize = 720;

/// The aggregate period, in milliseconds: one minute.
pub const MINUTE_PERIOD_MS: u64 = 60_000;

/// Upper bound on either tier's configured capacity.
///
/// A configured capacity is an operator's number, and a typo of 90000000 would allocate gigabytes
/// at construction — before any sample arrives. Clamping to 100_000 slots caps one tier at
/// 100_000 x 72B = 7.2MB for raw and 100_000 x 184B = 18.4MB for minutes per process, which is
/// large but survivable; the adjustment is reported, not applied silently.
pub const MAX_TIER_CAPACITY: usize = 100_000;

/// Bytes one raw slot occupies. `MetricSample` measures 72: 8 (`at_ms`) + 4 (`cpu_percent`) +
/// 8 (`memory_bytes`) + 3 x 16 for the `Option<u64>` fields = 68, padded to 72 by its 8-byte
/// alignment. Measured with `size_of`, not estimated, and re-asserted in this module's tests so a
/// field added later fails a test instead of quietly invalidating the bound stated below.
#[cfg(test)]
pub const RAW_SLOT_BYTES: usize = 72;

/// Bytes one minute-aggregate slot occupies. `AggregateSample` measures 184: 8 (`start_ms`) +
/// 8 (`end_ms`) + 4 (`samples`) + 4 padding + four `Option<MetricAggregate>` at 40 each
/// (`MetricAggregate` is 32 — three `f64` plus a `u32` padded — and its `f64`/`u32` fields offer no
/// niche, so the discriminant costs a further 8). Also `size_of`-asserted in tests.
#[cfg(test)]
pub const AGGREGATE_SLOT_BYTES: usize = 184;

/// How many raw samples and minute aggregates to retain per process.
///
/// Capacities are slots, not durations, because slots are what the memory bound is expressed in.
/// A duration would have to be converted through an assumed tick interval, and the tick interval
/// is exactly what `MissedTickBehavior::Skip` makes unreliable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionConfig {
    pub raw_capacity: usize,
    pub minute_capacity: usize,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            raw_capacity: DEFAULT_RAW_CAPACITY,
            minute_capacity: DEFAULT_MINUTE_CAPACITY,
        }
    }
}

/// A configured capacity that was replaced. Reported for the same reason
/// `host_metrics::IntervalAdjustment` is: an operator who configured 0 slots should be able to see
/// why they are getting 900, instead of concluding that history is broken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionAdjustment {
    /// Which capacity was adjusted: `"raw_capacity"` or `"minute_capacity"`.
    pub field: &'static str,
    pub configured: usize,
    pub applied: usize,
    pub reason: String,
}

impl RetentionConfig {
    /// Applies the configuration, substituting a documented default or bound for anything
    /// unusable, and reporting every substitution.
    ///
    /// Zero is unusable rather than "retain nothing": a tier that discards every sample is
    /// indistinguishable from sampling never happening, and "retain nothing" already has an
    /// expression — not enabling history at all.
    pub fn sanitised(self) -> (Self, Vec<RetentionAdjustment>) {
        let mut adjustments = Vec::new();
        let raw_capacity = Self::sanitise_one(
            "raw_capacity",
            self.raw_capacity,
            DEFAULT_RAW_CAPACITY,
            &mut adjustments,
        );
        let minute_capacity = Self::sanitise_one(
            "minute_capacity",
            self.minute_capacity,
            DEFAULT_MINUTE_CAPACITY,
            &mut adjustments,
        );
        (
            Self {
                raw_capacity,
                minute_capacity,
            },
            adjustments,
        )
    }

    fn sanitise_one(
        field: &'static str,
        configured: usize,
        default: usize,
        adjustments: &mut Vec<RetentionAdjustment>,
    ) -> usize {
        if configured == 0 {
            adjustments.push(RetentionAdjustment {
                field,
                configured,
                applied: default,
                reason: "capacity 0 retains nothing; using the documented default".to_string(),
            });
            return default;
        }
        if configured > MAX_TIER_CAPACITY {
            adjustments.push(RetentionAdjustment {
                field,
                configured,
                applied: MAX_TIER_CAPACITY,
                reason: format!("capacity above the {MAX_TIER_CAPACITY} slot ceiling"),
            });
            return MAX_TIER_CAPACITY;
        }
        configured
    }

    /// Bytes one process's history occupies once both tiers are full.
    ///
    /// Slot counts times slot sizes, which is the whole point of fixed capacity: at the defaults
    /// this is 900 x 72 + 720 x 184 = 64_800 + 132_480 = 197_280 bytes, about 193KiB per process.
    /// 100 managed processes is therefore ~18.8MiB, and that figure does not move with uptime
    /// because neither tier grows once full. The two `VecDeque`s and the struct itself add a
    /// per-process constant well under 1KiB, which is excluded here so the number stays checkable
    /// arithmetic rather than an estimate.
    #[cfg(test)]
    pub fn full_bytes_per_process(&self) -> usize {
        self.raw_capacity * RAW_SLOT_BYTES + self.minute_capacity * AGGREGATE_SLOT_BYTES
    }
}

/// Which tier answered a query. Part of the answer rather than inferred by the caller, because a
/// summary and a sample are not interchangeable: one is a reading, the other is several readings
/// with the detail removed, and a consumer that cannot tell them apart will present an aggregate
/// as though a process really held that value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Individual samples at the tick rate.
    Raw,
    /// One-minute aggregates. Each entry covers several samples; `count` says how many.
    Minute,
}

impl Resolution {
    #[cfg(test)]
    pub fn as_str(self) -> &'static str {
        match self {
            Resolution::Raw => "raw",
            Resolution::Minute => "minute",
        }
    }
}

/// One entry of a query result: either a sample or a summary, never silently one presented as the
/// other.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SeriesPoint {
    /// A single reading at `at_ms`.
    Sample { at_ms: u64, value: f64 },
    /// An aggregate of `count` readings between `start_ms` and `end_ms`. Carrying min, max and
    /// count alongside the mean is what keeps downsampling honest — a consumer can see the range
    /// that was collapsed and how many readings went into it.
    Summary {
        start_ms: u64,
        end_ms: u64,
        min: f64,
        max: f64,
        mean: f64,
        count: u32,
    },
}

/// The answer to a window query.
///
/// `points` empty means no data covered the window. It is never padded with zeros: a gap in
/// sampling is a gap, and synthesising `0.0` for it would make a skipped tick look like an idle
/// process — the same distinction `ManagedProcess::disk_read_rate_bps` keeps with `None`.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricSeries {
    pub metric: MetricKind,
    pub resolution: Resolution,
    /// The window as asked for, echoed back so a caller can see what was answered.
    pub from_ms: u64,
    pub to_ms: u64,
    pub points: Vec<SeriesPoint>,
}

impl MetricSeries {
    /// Whether the window contained no data. Distinct from "the values were zero".
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }
}

/// Bounded metric history for one process: raw samples plus one-minute aggregates.
///
/// Sampling is driven from outside — the caller pushes on the existing maintenance tick, and does
/// not push while the process is not running. That is why a stopped process keeps its history and
/// gains no new samples without this type knowing anything about process state: the absence of a
/// push is the absence of a sample.
#[derive(Debug, Clone)]
pub struct ProcessMetricHistory {
    raw: Ring<MetricSample>,
    minutes: Ring<AggregateSample>,
    /// The minute currently accumulating. Closed and pushed to `minutes` when a sample arrives
    /// with a later period index, so roll-up needs no clock of its own and no timer.
    open_minute: Option<PeriodAccumulator>,
    /// Samples pushed over this history's life, including those since evicted. Retained because
    /// "how much has been seen" is a different question from "how much is kept", and a baseline's
    /// warm-up gate needs the former.
    total_samples: u64,
    /// Whether the raw tier has ever discarded a sample. Until it has, raw holds the complete
    /// history and can answer any window in full — including one starting before the first sample.
    /// Deciding tier purely on `oldest_raw <= from_ms` got this wrong: a query from 0 against a
    /// process sampled from 3s was answered with summaries even though every sample was still there.
    raw_evicted: bool,
}

impl ProcessMetricHistory {
    /// Builds a history with the documented defaults.
    pub fn new() -> Self {
        Self::with_retention(RetentionConfig::default())
    }

    /// Builds a history with a configured retention, substituting defaults for unusable values.
    /// Substitution is deliberate: a `0` capacity would silently discard every sample, so the
    /// documented default stands in instead. The substitutions themselves are surfaced through
    /// `RetentionConfig::sanitised`, which a caller that cares can call before constructing.
    pub fn with_retention(retention: RetentionConfig) -> Self {
        let (retention, _adjustments) = retention.sanitised();
        Self {
            raw: Ring::new(retention.raw_capacity),
            minutes: Ring::new(retention.minute_capacity),
            open_minute: None,
            total_samples: 0,
            raw_evicted: false,
        }
    }

    /// Records one sample.
    ///
    /// Constant cost regardless of how much history is retained: one ring push (with an eviction
    /// when full, both O(1)), and at a period boundary one accumulator close plus one more ring
    /// push. Nothing here walks the retained data, so a process with 30 minutes of history pays
    /// exactly what a process with one sample pays.
    ///
    /// Out-of-order samples are dropped rather than inserted. A caller sampling from one
    /// maintenance tick cannot produce them, and the alternative — an insertion that keeps the
    /// ring sorted — is O(n), which would break the constant cost above for a case that does not
    /// occur.
    pub fn record(&mut self, sample: MetricSample) -> bool {
        if let Some(newest) = self.raw.newest()
            && sample.at_ms < newest.at_ms
        {
            return false;
        }
        self.total_samples += 1;
        if self.raw.push(sample).is_some() {
            self.raw_evicted = true;
        }
        self.fold_into_minute(&sample);
        true
    }

    fn fold_into_minute(&mut self, sample: &MetricSample) {
        let index = sample.at_ms / MINUTE_PERIOD_MS;
        match self.open_minute.take() {
            Some(mut open) if open.index == index => {
                open.push(sample);
                self.open_minute = Some(open);
            }
            Some(closed) => {
                // The period the sample lands in is later than the open one, so the open one can
                // never receive another sample: close it. Periods with no samples at all are
                // simply absent from the tier — no zero-valued aggregate is written for a minute
                // in which the daemon took no readings, whether because the process was stopped or
                // because ticks were skipped.
                self.minutes.push(closed.finish());
                self.open_minute = Some(PeriodAccumulator::open(index, sample));
            }
            None => self.open_minute = Some(PeriodAccumulator::open(index, sample)),
        }
    }

    /// Raw samples retained, oldest first.
    #[cfg(test)]
    pub fn raw_len(&self) -> usize {
        self.raw.entries.len()
    }

    /// Closed minute aggregates retained. The minute still accumulating is not counted: it is not
    /// a summary until its period ends, and reporting a partial period as closed would present a
    /// mean over 3 samples as a mean over the minute.
    #[cfg(test)]
    pub fn minute_len(&self) -> usize {
        self.minutes.entries.len()
    }

    /// Samples ever recorded, including evicted ones.
    ///
    /// Permanent API (workspace-crate-layout phase 5): test-only visibility does
    /// not cross a crate boundary; manager tests assert on it.
    pub fn total_samples(&self) -> u64 {
        self.total_samples
    }

    /// The most recent sample, or `None` when nothing has been recorded.
    #[cfg(test)]
    pub fn latest(&self) -> Option<&MetricSample> {
        self.raw.newest()
    }

    /// The oldest retained raw sample.
    #[cfg(test)]
    pub fn oldest_raw(&self) -> Option<&MetricSample> {
        self.raw.oldest()
    }

    /// Bytes the retained slots occupy right now. Reaches the bound as the tiers fill and then
    /// stops moving, which is the claim the memory test asserts.
    #[cfg(test)]
    pub fn resident_bytes(&self) -> usize {
        self.raw.entries.capacity() * RAW_SLOT_BYTES
            + self.minutes.entries.capacity() * AGGREGATE_SLOT_BYTES
    }
}

impl Default for ProcessMetricHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessMetricHistory {
    /// Samples or summaries for one metric over `[from_ms, to_ms]`, inclusive at both ends.
    ///
    /// Tier choice is by coverage, not by window length: raw answers when the retained raw samples
    /// actually reach back to `from_ms`, and the minute tier answers when they do not. Choosing on
    /// window length alone would return raw for a 5-minute window that raw only covers the last 40
    /// seconds of, and silently report a partial answer as complete.
    ///
    /// An unknown metric is refused; a window with no data returns an empty series, which is not
    /// the same answer.
    pub fn query(
        &self,
        metric: MetricKind,
        from_ms: u64,
        to_ms: u64,
    ) -> Result<MetricSeries, UnknownMetric> {
        let resolution = self.resolution_for(from_ms);
        let points = match resolution {
            Resolution::Raw => self.raw_points(metric, from_ms, to_ms),
            Resolution::Minute => self.minute_points(metric, from_ms, to_ms),
        };
        Ok(MetricSeries {
            metric,
            resolution,
            from_ms,
            to_ms,
            points,
        })
    }

    /// The same query addressed by metric name, refusing a name that is not retained.
    #[cfg(test)]
    pub fn query_named(
        &self,
        metric: &str,
        from_ms: u64,
        to_ms: u64,
    ) -> Result<MetricSeries, UnknownMetric> {
        let kind = MetricKind::from_name(metric).ok_or_else(|| UnknownMetric {
            name: metric.to_string(),
        })?;
        self.query(kind, from_ms, to_ms)
    }

    fn resolution_for(&self, from_ms: u64) -> Resolution {
        // Nothing has been evicted, so raw holds everything ever recorded and can answer any
        // window in full — including one starting before the first sample.
        //
        // This check is why `raw_evicted` exists. Comparing only `oldest.at_ms <= from_ms` was
        // wrong: a process first sampled at t=3s answered a query from t=0 with minute
        // summaries, because its oldest raw sample starts after the window does. Nothing had
        // been lost, so summarising discarded detail the caller could have had.
        if !self.raw_evicted {
            return Resolution::Raw;
        }
        match self.raw.oldest() {
            // Raw covers the window's start, so it can answer it in full.
            Some(oldest) if oldest.at_ms <= from_ms => Resolution::Raw,
            // Raw has evicted and starts after the window does: part of the window genuinely
            // predates retained raw, so the minute tier is the honest answer even where raw
            // overlaps. Answering from raw would present a partial series as complete.
            Some(_) => Resolution::Minute,
            // Nothing retained at all. Raw is reported, and the series is empty — "no samples"
            // rather than "answered from a tier that also has nothing".
            None => Resolution::Raw,
        }
    }

    fn raw_points(&self, metric: MetricKind, from_ms: u64, to_ms: u64) -> Vec<SeriesPoint> {
        self.raw
            .iter()
            .filter(|sample| sample.at_ms >= from_ms && sample.at_ms <= to_ms)
            // A sample with no measurement for this metric contributes nothing rather than a zero:
            // the first disk reading after a pid change is not 0 bytes of I/O, it is no reading.
            .filter_map(|sample| {
                sample.value(metric).map(|value| SeriesPoint::Sample {
                    at_ms: sample.at_ms,
                    value,
                })
            })
            .collect()
    }

    fn minute_points(&self, metric: MetricKind, from_ms: u64, to_ms: u64) -> Vec<SeriesPoint> {
        // The still-open minute is included so a query covering "now" is not missing the last
        // partial period, but it is reported with its real sample count, so a consumer can see it
        // summarises 3 samples rather than a full minute.
        let open = self
            .open_minute
            .map(|open| open.finish())
            .filter(|closed| closed.samples > 0);
        self.minutes
            .iter()
            .copied()
            .chain(open)
            .filter(|period| period.end_ms >= from_ms && period.start_ms <= to_ms)
            .filter_map(|period| {
                period
                    .aggregate(metric)
                    .map(|aggregate| SeriesPoint::Summary {
                        start_ms: period.start_ms,
                        end_ms: period.end_ms,
                        min: aggregate.min,
                        max: aggregate.max,
                        mean: aggregate.mean,
                        count: aggregate.count,
                    })
            })
            .collect()
    }
}

/// Per-process histories, keyed by the process id the manager already uses.
///
/// A thin owner: it exists so that "history is per process" and "history is released on delete"
/// are properties of one type with tests, rather than a `HashMap` open-coded in the manager where
/// the release path is easy to forget.
#[derive(Debug, Clone, Default)]
pub struct MetricHistoryStore {
    retention: RetentionConfig,
    histories: HashMap<String, ProcessMetricHistory>,
}

impl MetricHistoryStore {
    pub fn new() -> Self {
        Self::with_retention(RetentionConfig::default())
    }

    /// The retention every history in this store is created with. Sanitised once here, so a bad
    /// configured value is substituted a single time instead of per process.
    pub fn with_retention(retention: RetentionConfig) -> Self {
        let (retention, _) = retention.sanitised();
        Self {
            retention,
            histories: HashMap::new(),
        }
    }

    /// Records a sample for one process, creating its history on first sample.
    ///
    /// Returns false when the sample was out of order and dropped. Creating on first sample rather
    /// than on process registration means a process that never runs never costs a tier: history is
    /// allocated by evidence of sampling, not by existence.
    pub fn record(&mut self, process_id: &str, sample: MetricSample) -> bool {
        self.histories
            .entry(process_id.to_string())
            .or_insert_with(|| ProcessMetricHistory::with_retention(self.retention))
            .record(sample)
    }

    /// One process's history, or `None` when nothing has been sampled for it.
    pub fn history(&self, process_id: &str) -> Option<&ProcessMetricHistory> {
        self.histories.get(process_id)
    }

    /// Releases a process's history. Called on delete: the samples are the deleted process's, and
    /// keeping them would both leak memory for processes that no longer exist and let a recreated
    /// process inherit a predecessor's series.
    pub fn release(&mut self, process_id: &str) -> bool {
        self.histories.remove(process_id).is_some()
    }

    /// Processes with retained history.
    ///
    /// Permanent API (workspace-crate-layout phase 5): test-only visibility does
    /// not cross a crate boundary; manager tests assert on it.
    pub fn len(&self) -> usize {
        self.histories.len()
    }

    pub fn is_empty(&self) -> bool {
        self.histories.is_empty()
    }

    /// Queries one process's history. `Ok(None)` means the process has no history at all, which is
    /// distinct from `Ok(Some(series))` with an empty series — no samples ever versus none in this
    /// window — and from `Err`, which means the metric is not retained.
    #[cfg(test)]
    pub fn query(
        &self,
        process_id: &str,
        metric: &str,
        from_ms: u64,
        to_ms: u64,
    ) -> Result<Option<MetricSeries>, UnknownMetric> {
        // The metric name is validated before the process lookup, so an unknown metric is refused
        // for an unknown process too. Reporting "no history" for a misspelt metric would let a typo
        // read as an idle process.
        let kind = MetricKind::from_name(metric).ok_or_else(|| UnknownMetric {
            name: metric.to_string(),
        })?;
        match self.histories.get(process_id) {
            Some(history) => history.query(kind, from_ms, to_ms).map(Some),
            None => Ok(None),
        }
    }

    /// Bytes every retained history occupies at full capacity: the daemon-wide bound. Linear in
    /// process count and independent of uptime, which is the property the design asks for.
    #[cfg(test)]
    pub fn bounded_bytes(&self) -> usize {
        self.histories.len() * self.retention.full_bytes_per_process()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    /// The tick the daemon actually uses. Samples in these tests are spaced by it unless the test
    /// is specifically about a stretched or skipped interval.
    const TICK_MS: u64 = 2_000;

    fn sample_at(at_ms: u64, cpu: f32, memory: u64) -> MetricSample {
        MetricSample::cpu_memory(at_ms, cpu, memory).with_disk(1_024, 2_048, TICK_MS)
    }

    /// Pushes `count` samples starting at `start_ms`, spaced by the tick, with memory climbing by
    /// one MiB per sample so a series is distinguishable from a flat one.
    fn fill(history: &mut ProcessMetricHistory, start_ms: u64, count: usize) {
        let index_u64 = u64::try_from(count).expect("sample count fits in u64");
        for index in 0..index_u64 {
            let at_ms = start_ms + index * TICK_MS;
            let memory = 1_048_576 * (index + 1);
            assert!(history.record(sample_at(at_ms, 10.0, memory)));
        }
    }

    // -- Requirement: metric samples are retained per process ------------------------------------

    #[test]
    fn samples_accumulate_with_their_own_timestamps() {
        let mut history = ProcessMetricHistory::new();
        fill(&mut history, 1_000, 5);

        assert_eq!(history.raw_len(), 5, "every sample retained below capacity");
        let series = history
            .query(MetricKind::Memory, 0, 1_000_000)
            .expect("memory is retained");
        assert_eq!(series.points.len(), 5);
        let timestamps: Vec<u64> = series
            .points
            .iter()
            .map(|point| match point {
                SeriesPoint::Sample { at_ms, .. } => *at_ms,
                other => panic!("expected raw samples, got {other:?}"),
            })
            .collect();
        assert_eq!(timestamps, vec![1_000, 3_000, 5_000, 7_000, 9_000]);
    }

    #[test]
    fn every_retained_metric_is_queryable() {
        let mut history = ProcessMetricHistory::new();
        fill(&mut history, 0, 3);
        for metric in ALL_METRICS {
            let series = history.query(metric, 0, 10_000).expect("retained metric");
            assert_eq!(
                series.points.len(),
                3,
                "{metric} should have three samples retained"
            );
        }
    }

    #[test]
    fn history_is_per_process() {
        let mut store = MetricHistoryStore::new();
        assert!(store.record("alpha", sample_at(1_000, 10.0, 1_000)));
        assert!(store.record("alpha", sample_at(3_000, 10.0, 2_000)));
        assert!(store.record("beta", sample_at(3_000, 90.0, 9_000)));

        assert_eq!(store.len(), 2);
        assert_eq!(store.history("alpha").expect("alpha").raw_len(), 2);
        assert_eq!(store.history("beta").expect("beta").raw_len(), 1);

        let beta = store
            .query("beta", "cpu_percent", 0, 10_000)
            .expect("known metric")
            .expect("beta has history");
        assert_eq!(
            beta.points,
            vec![SeriesPoint::Sample {
                at_ms: 3_000,
                value: 90.0
            }],
            "beta's series must not contain alpha's samples"
        );
    }

    #[test]
    fn stopped_process_keeps_history_and_gains_no_samples() {
        let mut store = MetricHistoryStore::new();
        for index in 0..4u64 {
            assert!(store.record("alpha", sample_at(1_000 + index * TICK_MS, 10.0, 1_000)));
        }
        let before = store.history("alpha").expect("alpha").raw_len();
        assert_eq!(before, 4, "guard: samples exist before the stop");

        // A stopped process is simply not sampled: the caller stops calling record. Nothing else
        // happens, which is exactly what this asserts.
        let after = store.history("alpha").expect("alpha").raw_len();
        assert_eq!(after, before, "history survives the stop unchanged");
        assert_eq!(
            store.history("alpha").expect("alpha").total_samples(),
            4,
            "and no sample was added while not running"
        );
    }

    #[test]
    fn history_is_released_on_delete() {
        let mut store = MetricHistoryStore::new();
        assert!(store.record("alpha", sample_at(1_000, 10.0, 1_000)));
        assert_eq!(store.len(), 1, "guard: history exists before delete");
        assert!(store.bounded_bytes() > 0, "guard: it costs memory");

        assert!(store.release("alpha"), "release reports it removed one");
        assert!(store.history("alpha").is_none());
        assert!(store.is_empty());
        assert_eq!(store.bounded_bytes(), 0, "the bound drops with it");
        assert!(!store.release("alpha"), "releasing twice is not an error");
    }

    #[test]
    fn a_recreated_process_does_not_inherit_the_previous_series() {
        let mut store = MetricHistoryStore::new();
        assert!(store.record("alpha", sample_at(1_000, 99.0, 9_999)));
        assert!(store.release("alpha"));
        assert!(store.record("alpha", sample_at(5_000, 1.0, 1)));

        let history = store.history("alpha").expect("alpha resampled");
        assert_eq!(history.raw_len(), 1);
        assert_eq!(
            history.total_samples(),
            1,
            "counters start fresh, not carried over"
        );
    }

    // -- Requirement: retention is bounded and tiered by resolution ------------------------------

    #[test]
    fn slot_sizes_match_the_documented_bound() {
        // The memory bound in RetentionConfig::full_bytes_per_process is arithmetic over these two
        // numbers. If a field is added to either type the bound silently understates, so the doc
        // comment's constants are asserted against the real layout here.
        assert_eq!(
            size_of::<MetricSample>(),
            RAW_SLOT_BYTES,
            "MetricSample grew; update RAW_SLOT_BYTES and the stated bound"
        );
        assert_eq!(
            size_of::<AggregateSample>(),
            AGGREGATE_SLOT_BYTES,
            "AggregateSample grew; update AGGREGATE_SLOT_BYTES and the stated bound"
        );
    }

    #[test]
    fn default_bound_is_the_stated_arithmetic() {
        let retention = RetentionConfig::default();
        // 900 x 72 + 720 x 184 = 64_800 + 132_480 = 197_280 bytes per process, ~193 KiB.
        // The 56/136 figures this test first carried were a guess at the layout; the sizes are
        // measured in `slot_sizes_match_the_documented_bound`, which reports 72 and 184.
        assert_eq!(retention.full_bytes_per_process(), 197_280);
        assert_eq!(DEFAULT_RAW_CAPACITY * RAW_SLOT_BYTES, 64_800);
        assert_eq!(DEFAULT_MINUTE_CAPACITY * AGGREGATE_SLOT_BYTES, 132_480);
    }

    #[test]
    fn raw_tier_evicts_oldest_first_at_capacity() {
        let retention = RetentionConfig {
            raw_capacity: 4,
            minute_capacity: 8,
        };
        let mut history = ProcessMetricHistory::with_retention(retention);
        fill(&mut history, 0, 4);
        assert_eq!(history.raw_len(), 4, "guard: the tier is full, not empty");
        assert_eq!(history.oldest_raw().expect("oldest").at_ms, 0);

        assert!(history.record(sample_at(8_000, 10.0, 5_242_880)));
        assert_eq!(history.raw_len(), 4, "count stays at capacity");
        assert_eq!(
            history.oldest_raw().expect("oldest").at_ms,
            TICK_MS,
            "the 0ms sample was the one discarded"
        );
        assert_eq!(history.latest().expect("newest").at_ms, 8_000);
        assert_eq!(
            history.total_samples(),
            5,
            "eviction does not un-count a sample that was seen"
        );
    }

    #[test]
    fn minute_tier_evicts_oldest_first_at_capacity() {
        // `raw_capacity: 2` so raw genuinely evicts. At capacity 8 all five samples stayed
        // retained and a query answered from raw was the CORRECT answer — the minute tier is only
        // the honest source once raw has actually lost something. This test asserted `Minute`
        // against the old behaviour, where the tier was chosen without checking whether anything
        // had been evicted.
        let retention = RetentionConfig {
            raw_capacity: 2,
            minute_capacity: 2,
        };
        let mut history = ProcessMetricHistory::with_retention(retention);
        // One sample per minute for five minutes: each new minute closes the previous one.
        for minute in 0..5u64 {
            assert!(history.record(sample_at(minute * MINUTE_PERIOD_MS, 10.0, 1_000)));
        }
        assert_eq!(
            history.minute_len(),
            2,
            "four minutes closed, capacity 2 retained"
        );

        let series = history
            .query(MetricKind::Memory, 0, 10 * MINUTE_PERIOD_MS)
            .expect("retained metric");
        assert_eq!(series.resolution, Resolution::Minute);
        let starts: Vec<u64> = series
            .points
            .iter()
            .map(|point| match point {
                SeriesPoint::Summary { start_ms, .. } => *start_ms,
                other => panic!("expected summaries, got {other:?}"),
            })
            .collect();
        // Minutes 0 and 1 were evicted; 2 and 3 are retained and 4 is still open.
        assert_eq!(
            starts,
            vec![
                2 * MINUTE_PERIOD_MS,
                3 * MINUTE_PERIOD_MS,
                4 * MINUTE_PERIOD_MS
            ]
        );
    }

    #[test]
    fn memory_does_not_grow_with_uptime() {
        let retention = RetentionConfig {
            raw_capacity: 4,
            minute_capacity: 2,
        };
        let mut history = ProcessMetricHistory::with_retention(retention);
        // Enough samples to fill both tiers: 3 minutes of ticks closes 2 minutes and fills raw.
        fill(&mut history, 0, 100);
        let filled = history.resident_bytes();
        assert_eq!(history.raw_len(), 4, "guard: raw is at capacity");
        assert_eq!(history.minute_len(), 2, "guard: minutes are at capacity");

        // Far longer than the longest retention window: 5000 more samples is ~2.8 hours of ticks.
        let start = 100 * TICK_MS;
        for index in 0..5_000u64 {
            assert!(history.record(sample_at(start + index * TICK_MS, 10.0, 1_000)));
        }
        assert_eq!(
            history.resident_bytes(),
            filled,
            "retained slots do not grow once the tiers are full"
        );
        assert_eq!(history.raw_len(), 4);
        assert_eq!(history.minute_len(), 2);
        assert_eq!(
            history.resident_bytes(),
            retention.full_bytes_per_process(),
            "and resident matches the stated bound"
        );
        assert_eq!(history.total_samples(), 5_100);
    }

    #[test]
    fn store_bound_is_linear_in_process_count_only() {
        let mut store = MetricHistoryStore::new();
        let per_process = RetentionConfig::default().full_bytes_per_process();
        for process in 0..10 {
            let id = format!("p{process}");
            for index in 0..50u64 {
                assert!(store.record(&id, sample_at(index * TICK_MS, 10.0, 1_000)));
            }
        }
        assert_eq!(store.bounded_bytes(), 10 * per_process);
        // 100 processes at the defaults: 100 x 197_280 = 19_728_000 bytes, ~18.8 MiB.
        assert_eq!(100 * per_process, 19_728_000);
    }

    #[test]
    fn configured_retention_is_applied() {
        let retention = RetentionConfig {
            raw_capacity: 10,
            minute_capacity: 3,
        };
        let (applied, adjustments) = retention.sanitised();
        assert_eq!(
            applied.raw_capacity, 10,
            "a usable configuration passes through"
        );
        assert_eq!(applied.minute_capacity, 3);
        assert!(
            adjustments.is_empty(),
            "a usable configuration is not adjusted"
        );

        // The configured capacities are honoured, not approximated: 121 samples cross five minute
        // periods, so raw must evict down to 10 and minutes must evict down to 3.
        let mut history = ProcessMetricHistory::with_retention(retention);
        fill(&mut history, 0, 121);
        assert_eq!(history.raw_len(), 10, "raw holds exactly the configured 10");
        assert_eq!(
            history.minute_len(),
            3,
            "minute tier holds exactly the configured 3"
        );
    }

    #[test]
    fn unusable_retention_falls_back_and_reports() {
        let (applied, adjustments) = RetentionConfig {
            raw_capacity: 0,
            minute_capacity: MAX_TIER_CAPACITY + 1,
        }
        .sanitised();
        assert_eq!(applied.raw_capacity, DEFAULT_RAW_CAPACITY);
        assert_eq!(applied.minute_capacity, MAX_TIER_CAPACITY);
        assert_eq!(adjustments.len(), 2, "both substitutions are reported");
        assert_eq!(adjustments[0].field, "raw_capacity");
        assert_eq!(adjustments[0].configured, 0);
        assert_eq!(adjustments[0].applied, DEFAULT_RAW_CAPACITY);
        assert!(!adjustments[0].reason.is_empty());
        assert_eq!(adjustments[1].field, "minute_capacity");
        assert_eq!(adjustments[1].applied, MAX_TIER_CAPACITY);

        // And a history built from it retains samples rather than discarding them silently.
        let mut history = ProcessMetricHistory::with_retention(RetentionConfig {
            raw_capacity: 0,
            minute_capacity: 0,
        });
        // The substitutions were reported above; here the point is the history still works —
        // 0/0 must behave as the documented defaults, not as "retain nothing".
        assert!(history.record(sample_at(0, 1.0, 1)));
        assert_eq!(history.raw_len(), 1, "default capacity retained the sample");
    }

    // -- Requirement: appending is constant cost -------------------------------------------------

    #[test]
    fn append_touches_a_bounded_number_of_slots_whatever_the_depth() {
        // Cost is asserted structurally rather than by timing, which on a shared CI box measures
        // the scheduler more than the code. The claim is that append neither reallocates nor walks:
        // VecDeque capacity is unchanged after eviction+push, and the retained count is unchanged.
        let retention = RetentionConfig {
            raw_capacity: 16,
            minute_capacity: 4,
        };
        let mut empty = ProcessMetricHistory::with_retention(retention);
        assert!(empty.record(sample_at(0, 10.0, 1_000)));
        let empty_capacity = empty.raw.entries.capacity();

        let mut full = ProcessMetricHistory::with_retention(retention);
        fill(&mut full, 0, 10_000);
        assert_eq!(full.raw_len(), 16, "guard: appending into a full ring");
        assert_eq!(full.minute_len(), 4, "guard: the minute tier is full too");
        let before = full.raw.entries.capacity();
        assert!(full.record(sample_at(10_000 * TICK_MS, 10.0, 1_000)));
        assert_eq!(
            full.raw.entries.capacity(),
            before,
            "a full ring does not reallocate on append"
        );
        assert_eq!(
            full.raw.entries.capacity(),
            empty_capacity,
            "and its allocation is the same size as a fresh one"
        );
        assert_eq!(full.raw_len(), 16);
    }

    #[test]
    fn closing_a_period_does_not_depend_on_retained_depth() {
        // The open period holds four running accumulators, not the period's samples, so closing it
        // is O(1). Asserted by pushing a period boundary after a lot of history and checking that
        // exactly one aggregate was appended.
        let retention = RetentionConfig {
            raw_capacity: 8,
            minute_capacity: 1_000,
        };
        let mut history = ProcessMetricHistory::with_retention(retention);
        fill(&mut history, 0, 5_000);
        let closed_before = history.minute_len();
        assert!(closed_before > 0, "guard: periods have been closing");
        let capacity_before = history.minutes.entries.capacity();

        let next_minute =
            (history.latest().expect("newest").at_ms / MINUTE_PERIOD_MS + 1) * MINUTE_PERIOD_MS;
        assert!(history.record(sample_at(next_minute, 10.0, 1_000)));
        assert_eq!(
            history.minute_len(),
            closed_before + 1,
            "one boundary closes exactly one period"
        );
        assert_eq!(
            history.minutes.entries.capacity(),
            capacity_before,
            "and does not reallocate"
        );
    }

    #[test]
    fn out_of_order_samples_are_dropped_rather_than_inserted() {
        let mut history = ProcessMetricHistory::new();
        assert!(history.record(sample_at(10_000, 10.0, 1_000)));
        assert!(
            !history.record(sample_at(5_000, 10.0, 1_000)),
            "an earlier sample is refused"
        );
        assert_eq!(history.raw_len(), 1);
        assert_eq!(
            history.total_samples(),
            1,
            "a dropped sample is not counted as seen"
        );
        assert!(
            history.record(sample_at(10_000, 20.0, 2_000)),
            "a same-timestamp sample is accepted"
        );
    }

    // -- Requirement: roll-up -------------------------------------------------------------------

    #[test]
    fn summary_reports_min_max_and_mean_of_what_it_covers() {
        let retention = RetentionConfig {
            raw_capacity: 4,
            minute_capacity: 8,
        };
        let mut history = ProcessMetricHistory::with_retention(retention);
        // Three samples inside minute 0, then one in minute 1 to close it.
        assert!(history.record(sample_at(0, 10.0, 100)));
        assert!(history.record(sample_at(TICK_MS, 30.0, 300)));
        assert!(history.record(sample_at(2 * TICK_MS, 20.0, 200)));
        assert!(history.record(sample_at(MINUTE_PERIOD_MS, 99.0, 999)));

        assert_eq!(history.minute_len(), 1, "minute 0 closed");
        let closed = *history.minutes.oldest().expect("closed minute");
        assert_eq!(closed.samples, 3);
        assert_eq!(closed.start_ms, 0);
        assert_eq!(
            closed.end_ms,
            2 * TICK_MS,
            "the period reports the last sample seen, not the nominal edge"
        );
        let cpu = closed.cpu.expect("cpu aggregate");
        assert_eq!(cpu.min, 10.0);
        assert_eq!(cpu.max, 30.0);
        assert_eq!(cpu.mean, 20.0);
        assert_eq!(cpu.count, 3);
        let memory = closed.memory.expect("memory aggregate");
        assert_eq!((memory.min, memory.max, memory.mean), (100.0, 300.0, 200.0));
    }

    #[test]
    fn a_summary_is_never_presented_as_a_single_reading() {
        let mut history = ProcessMetricHistory::with_retention(RetentionConfig {
            raw_capacity: 2,
            minute_capacity: 8,
        });
        assert!(history.record(sample_at(0, 10.0, 100)));
        assert!(history.record(sample_at(TICK_MS, 30.0, 300)));
        assert!(history.record(sample_at(MINUTE_PERIOD_MS, 50.0, 500)));

        // Raw only reaches back to TICK_MS now, so a window starting at 0 must be answered by the
        // minute tier and every entry must carry its count and range.
        let series = history
            .query(MetricKind::Cpu, 0, 2 * MINUTE_PERIOD_MS)
            .expect("retained metric");
        assert_eq!(series.resolution, Resolution::Minute);
        match series.points.first().expect("first summary") {
            SeriesPoint::Summary {
                min,
                max,
                mean,
                count,
                ..
            } => {
                assert_eq!(*count, 2, "the count makes the aggregation visible");
                assert_eq!((*min, *max, *mean), (10.0, 30.0, 20.0));
            }
            other => panic!("expected a summary, got {other:?}"),
        }
    }

    #[test]
    fn a_gap_in_sampling_produces_no_aggregate_rather_than_a_zero() {
        // `raw_capacity: 1` so the first sample is evicted and the minute tier becomes the only
        // source reaching back to minute 0. At capacity 2 both samples were retained and raw
        // answered — correct behaviour, but it tested the wrong tier for a claim about aggregates.
        let mut history = ProcessMetricHistory::with_retention(RetentionConfig {
            raw_capacity: 1,
            minute_capacity: 16,
        });
        // Minute 0 sampled, minutes 1-3 missed entirely (process stopped, or ticks skipped),
        // minute 4 sampled again.
        assert!(history.record(sample_at(0, 10.0, 100)));
        assert!(history.record(sample_at(4 * MINUTE_PERIOD_MS, 20.0, 200)));

        let series = history
            .query(MetricKind::Cpu, 0, 5 * MINUTE_PERIOD_MS)
            .expect("retained metric");
        assert_eq!(series.resolution, Resolution::Minute);
        assert_eq!(
            series.points.len(),
            2,
            "two sampled minutes, not five: the gap is absent, not zero-filled"
        );
        for point in &series.points {
            match point {
                SeriesPoint::Summary { count, min, .. } => {
                    assert!(*count > 0, "no empty aggregate is written");
                    assert!(*min > 0.0, "and no synthesised zero appears as a minimum");
                }
                other => panic!("expected summaries, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_unmeasured_disk_amount_is_not_folded_in_as_zero() {
        let mut history = ProcessMetricHistory::with_retention(RetentionConfig {
            raw_capacity: 4,
            minute_capacity: 8,
        });
        // The first sample after a pid appears has no interval to difference against, so its disk
        // amounts do not exist. src/process.rs models that as no measurement; so does this.
        assert!(history.record(MetricSample::cpu_memory(0, 10.0, 100)));
        assert!(history.record(sample_at(TICK_MS, 10.0, 100)));

        let raw = history
            .query(MetricKind::DiskRead, 0, 10_000)
            .expect("retained metric");
        assert_eq!(
            raw.points,
            vec![SeriesPoint::Sample {
                at_ms: TICK_MS,
                value: 1_024.0
            }],
            "the unmeasured sample contributes no point at all"
        );
        // CPU was measured in both, so the sample count differs per metric — which is why the
        // aggregate carries a per-metric count rather than the period's sample total.
        assert!(history.record(sample_at(MINUTE_PERIOD_MS, 10.0, 100)));
        let closed = *history.minutes.oldest().expect("closed minute");
        assert_eq!(closed.samples, 2);
        assert_eq!(closed.cpu.expect("cpu").count, 2);
        assert_eq!(
            closed.disk_read.expect("disk read").count,
            1,
            "only the measured sample counted"
        );
        assert_eq!(
            closed.disk_read.expect("disk read").min,
            1_024.0,
            "and the missing one did not drag the minimum to zero"
        );
    }

    // -- Requirement: query by window and metric -------------------------------------------------

    #[test]
    fn a_window_inside_raw_returns_samples_and_reports_raw() {
        let mut history = ProcessMetricHistory::new();
        fill(&mut history, 0, 60);
        let series = history
            .query(MetricKind::Cpu, 10 * TICK_MS, 12 * TICK_MS)
            .expect("retained metric");
        assert_eq!(series.resolution, Resolution::Raw);
        assert_eq!(
            series.points.len(),
            3,
            "both window edges are inclusive: samples 10, 11, 12"
        );
        assert!(
            series
                .points
                .iter()
                .all(|point| matches!(point, SeriesPoint::Sample { .. }))
        );
        assert_eq!(series.from_ms, 10 * TICK_MS);
        assert_eq!(series.to_ms, 12 * TICK_MS);
    }

    #[test]
    fn a_window_reaching_past_raw_returns_summaries_and_reports_the_tier() {
        let mut history = ProcessMetricHistory::with_retention(RetentionConfig {
            raw_capacity: 5,
            minute_capacity: 64,
        });
        // 10 minutes of ticks: raw holds only the last 5 samples, minutes hold the rest.
        fill(&mut history, 0, 300);
        assert_eq!(history.raw_len(), 5, "guard: raw is shallow");
        assert!(history.minute_len() >= 9, "guard: minutes cover the span");

        let series = history
            .query(MetricKind::Memory, 0, 300 * TICK_MS)
            .expect("retained metric");
        assert_eq!(
            series.resolution,
            Resolution::Minute,
            "the tier that actually covered the window is reported"
        );
        assert!(!series.points.is_empty());
        assert!(
            series
                .points
                .iter()
                .all(|point| matches!(point, SeriesPoint::Summary { .. }))
        );
    }

    #[test]
    fn an_empty_window_is_empty_not_zero() {
        let mut history = ProcessMetricHistory::new();
        fill(&mut history, 1_000_000, 10);
        // A window entirely before any sample was taken.
        let series = history
            .query(MetricKind::Cpu, 0, 500_000)
            .expect("retained metric");
        assert!(series.is_empty(), "no data covered the window");
        assert!(
            series.points.is_empty(),
            "and no zero-valued sample was synthesised for it"
        );

        // Guard against the trivial pass: the same history answers a covered window with data, so
        // the empty result above is a real absence rather than an empty buffer.
        let covered = history
            .query(MetricKind::Cpu, 1_000_000, 1_100_000)
            .expect("retained metric");
        assert_eq!(covered.points.len(), 10);
    }

    #[test]
    fn a_window_between_two_sampled_periods_is_empty() {
        let mut history = ProcessMetricHistory::with_retention(RetentionConfig {
            raw_capacity: 2,
            minute_capacity: 16,
        });
        assert!(history.record(sample_at(0, 10.0, 100)));
        assert!(history.record(sample_at(10 * MINUTE_PERIOD_MS, 20.0, 200)));

        let series = history
            .query(MetricKind::Cpu, 4 * MINUTE_PERIOD_MS, 6 * MINUTE_PERIOD_MS)
            .expect("retained metric");
        assert!(
            series.is_empty(),
            "the daemon took no readings in that window and says so"
        );
    }

    #[test]
    fn a_history_with_no_samples_answers_empty() {
        let history = ProcessMetricHistory::new();
        let series = history
            .query(MetricKind::Memory, 0, u64::MAX)
            .expect("retained metric");
        assert!(series.is_empty());
        assert_eq!(history.total_samples(), 0);
        assert!(history.latest().is_none());
    }

    #[test]
    fn an_unknown_metric_is_refused() {
        let mut history = ProcessMetricHistory::new();
        fill(&mut history, 0, 5);
        let error = history
            .query_named("network_bytes", 0, 10_000)
            .expect_err("an unretained metric must be refused");
        assert_eq!(error.name, "network_bytes");
        assert!(
            error.to_string().contains("cpu_percent"),
            "the refusal names what is retained: {error}"
        );

        // Refused for an unknown process too, so a typo cannot read as an idle process.
        let store = MetricHistoryStore::new();
        assert!(store.query("nobody", "network_bytes", 0, 1).is_err());
        assert_eq!(
            store
                .query("nobody", "cpu_percent", 0, 1)
                .expect("known metric"),
            None,
            "a known metric on an unsampled process is 'no history', not a refusal"
        );
    }

    #[test]
    fn metric_names_round_trip_and_aliases_resolve() {
        for metric in ALL_METRICS {
            assert_eq!(MetricKind::from_name(metric.as_str()), Some(metric));
        }
        assert_eq!(MetricKind::from_name("cpu"), Some(MetricKind::Cpu));
        assert_eq!(MetricKind::from_name("mem"), Some(MetricKind::Memory));
        assert_eq!(
            MetricKind::from_name("disk_write"),
            Some(MetricKind::DiskWrite)
        );
        assert_eq!(MetricKind::from_name("CPU"), None, "matching is exact");
        assert_eq!(MetricKind::from_name(""), None);
    }

    #[test]
    fn stretched_intervals_are_retained_rather_than_assumed() {
        // MissedTickBehavior::Skip means a tick can be missed and the next sample covers a longer
        // period. The interval is carried per sample so a consumer derives the rate from what was
        // observed instead of dividing by a nominal 2s.
        let mut history = ProcessMetricHistory::new();
        assert!(history.record(MetricSample::cpu_memory(0, 10.0, 100).with_disk(2_000, 0, 2_000)));
        assert!(
            history.record(MetricSample::cpu_memory(9_000, 10.0, 100).with_disk(9_000, 0, 9_000))
        );

        let intervals: Vec<Option<u64>> = history.raw.iter().map(|s| s.interval_ms).collect();
        assert_eq!(intervals, vec![Some(2_000), Some(9_000)]);
        // Same byte count over different intervals is a different rate, which is only derivable
        // because the interval was kept.
        let latest = history.latest().expect("newest");
        assert_eq!(latest.interval_ms, Some(9_000));
    }

    #[test]
    fn resolution_names_are_stable() {
        assert_eq!(Resolution::Raw.as_str(), "raw");
        assert_eq!(Resolution::Minute.as_str(), "minute");
        assert_eq!(MetricKind::Cpu.to_string(), "cpu_percent");
    }
}
