# Host Metrics

Oxmgr reports the state of the machine it runs on: memory, CPU, load, filesystems,
network interfaces, and optionally temperatures. These are the **host's** figures,
collected separately from per-process metrics and never mixed with them.

Three ways to read them:

- `GET /api/host` — one full snapshot, no connection held.
- `GET /api/host/stream` — Server-Sent Events, pushing only what changed.
- `oxmgr_host_*` series on `/metrics`, and the dashboard's Host sidebar.

## Host-level, not container-aware

Every figure here describes the machine. If the daemon runs inside a container whose
limits are lower than the host's capacity, **these numbers are not your limits**.

A container capped at 512 MB running on a 64 GB host will report 64 GB of host
memory. That is not a bug: the figure is true, and it is the host's. Oxmgr does not
read cgroup quotas, so it cannot tell you the applicable limit — treat the host
totals as context for the machine, and get container limits from your orchestrator.

Memory accounting also differs between platforms in what counts as "used" — cache
and buffers are attributed differently on Linux and macOS. Watch one host over time;
do not compare `used_percent` between hosts and conclude one is busier.

## Unavailable is not zero

Any figure the platform does not supply is **absent**, never zero:

| Surface | Unavailable | Measured zero |
|---|---|---|
| `/api/host` | key absent | key present, `0` |
| `/metrics` | series omitted | series present, `0` |
| Dashboard | `–`, dimmed italic | `0 B`, normal |

This is the single rule the whole design turns on. A load average of `0` reads as an
idle machine; a temperature of `0` reads as a cold one. Emitting either as a
placeholder would be a fabricated measurement, and a fabricated measurement in a
dashboard is worse than a gap because nobody questions it.

Consequences worth knowing:

- **A host with no swap** has no `swap` key at all. It is not "0 of 0 used".
- **CPU utilisation is withheld** until two samples at least the platform minimum
  apart have been taken, so a freshly started daemon shows `–` rather than an idle
  machine.
- **Zero-capacity filesystems** (`/proc`, `/sys`, and similar) report their capacity
  figures as `0`, which is true, but their *utilisation* is absent — 0/0 has no
  answer, and emitting `0%` or `100%` would either hide a problem or invent one.
- **A subsystem that failed** is named in `failures[]` with a reason, so a gap caused
  by a collection error is distinguishable from a platform that never had the figure.
  The first is actionable; the second is not.

## Collection cadence

Host collection runs on **its own task**, not on the supervision path. A slow disk
enumeration cannot delay a restart or a health check, because the two share no
command channel. Verified: the process metrics interval stays at 1999–2002 ms against
a nominal 2000 ms with host collection active.

Subsystems have separate intervals because they cost different amounts and change at
different rates:

| Subsystem | Default | Environment variable |
|---|---|---|
| CPU, memory | 2 s | `OXMGR_HOST_CPU_INTERVAL_MS` |
| Filesystems, interfaces | 10 s | `OXMGR_HOST_IO_INTERVAL_MS` |
| Temperatures | 30 s | `OXMGR_HOST_COMPONENTS_INTERVAL_MS` |

Measured on an 8-core arm64 macOS host with 2 filesystems and 24 interfaces, mean of
20 refreshes:

```
memory           0.004 ms
cpu              0.003 ms
filesystems     22.449 ms
network          0.619 ms
components      64.867 ms   (temperatures; off by default)
```

Filesystem enumeration and temperature sensors are the expensive ones, which is
exactly why they are not on the CPU interval. Under the shipped defaults a tick costs
**0.004 ms when nothing is due** and **23 ms when the 10 s I/O refresh comes round**:

```
tick, nothing due    0.004 ms
tick, all due       23.051 ms
```

Reproduce with:

```sh
cargo test host_collection_cost -- --ignored --nocapture
cargo test host_default_tick_cost -- --ignored --nocapture
```

### The CPU interval has a floor

Per-core and global CPU utilisation are derived from the delta between two samples,
and the platform needs a minimum gap for that delta to mean anything: 200 ms on
Apple, 100 ms on BSD. A shorter interval does not produce a faster reading, it
produces a meaningless one.

A configured value below the floor is **raised and reported**, in the daemon log and
in `interval_adjustments[]` on the snapshot. Setting 50 ms and silently getting 200 ms
would leave you wondering why your configuration had no effect.

## Optional detail

Off by default, because both are noise for most hosts:

- **Per-core CPU** — one figure per core. On a 64-core machine that is 64 numbers
  crowding out the ones you came for.
- **Temperatures** — commonly unavailable (VMs, containers without sensor access,
  platforms sysinfo does not cover), and the most expensive subsystem to read at
  ~65 ms. An empty sensor set is omitted entirely rather than published as a heading
  with no rows.

## Network figures are the host's

`network.scope` is always `host`, and every `oxmgr_host_network_*` series carries
`scope="host"`. Interfaces are reported **individually and never aggregated**: a
single combined figure hides which interface is busy, which is the only reason to
look.

This labelling is deliberate. Per-process network I/O is **not measurable** — the
operating system reports counters per interface, not per process (see
`docs/PROCESS-IO-METRICS.md`). An interface total is therefore the closest available
number, and presenting it as one service's traffic would be a plausible-looking lie.
On a host running a single service it would even look right, which is what makes it
dangerous. The scope label is what stops a dashboard query from making that mistake.

Interface rates are computed from the **observed** interval, and omitted on an
interface's first measurement where no interval exists yet.

## Prometheus types

- **Counters**: cumulative interface totals and error counts. They only grow.
- **Gauges**: everything else — utilisation, capacity, load, temperatures, uptime.
  They move in both directions.

Static identity (hostname, OS, kernel, architecture) is exposed as labels on
`oxmgr_host_info`, since a string cannot be a metric value. It is collected once at
startup and never refreshed, because none of it changes while the daemon runs.
Uptime is derived from boot time rather than re-queried.

## Streaming: `GET /api/host/stream`

Server-Sent Events. One connection, and the daemon pushes only the subsystems whose
values have changed.

```
event: snapshot
data: {"identity":{...},"memory":{...},"cpu":{...},...}

event: cpu
data: {"global_percent":42.5,"sample_interval_ms":2000}

event: memory
data: {"total_bytes":...,"used_bytes":...,"used_percent":63.2,"swap":{...}}
```

**Streaming does not mean event-driven collection.** The daemon still samples on an
interval. There is no operating-system notification for a change in memory, processor
or filesystem usage — not on macOS, not on Linux. `kqueue` watches file descriptors,
vnodes and process exit; `inotify` watches files. Neither reports "memory changed".

The reference implementations work the same way: btop (C++) runs a collection thread
on a fixed `update_ms`, default 2000, and the Rust ports (`jw/brt`, the `btop` crate)
are terminal front-ends over the same polled model. Streaming changes **which side
initiates delivery**, and nothing else. What it buys is that the browser stops asking
and the daemon stops re-sending figures the client already holds.

### The contract

| Event | Payload | When |
|---|---|---|
| `snapshot` | the whole snapshot | on connect, and after the daemon detects this client lagged |
| `memory` | `HostMemory` or `null` | memory or swap changed |
| `cpu` | `HostCpu` or `null` | utilisation changed |
| `load` | `HostLoadAverage` or `null` | load changed |
| `filesystems` | array or `null` | capacity or utilisation changed |
| `network` | `HostNetwork` or `null` | interface totals changed |
| `components` | array or `null` | a temperature changed |

A client holds the `snapshot` and replaces one field per event. Rules that matter:

- **`null` means the subsystem became unavailable.** Apply it. Ignoring it leaves the
  previous reading on screen looking current, which is the failure the whole
  unavailable-is-not-zero rule exists to prevent.
- **Static identity arrives only in `snapshot`.** It is never repeated in an update,
  because it never changes while the daemon runs.
- **No event is sent when nothing changed.** An idle host costs a connected client
  zero bytes.
- **A `snapshot` can arrive mid-stream.** That means the daemon saw this connection
  fall behind. Replace the whole held state; do not merge it.

Measured on this host: an opening snapshot is ~5.6 KB, a delta averages 570 bytes. In
a 42-second window a stream sent 30,687 bytes against 44,952 for polling `/api/host`
every 5 seconds — about a third less, and the gap widens the longer a client stays
connected, since the snapshot is paid once.

### What counts as a change

Comparing floating-point figures exactly would mark processor utilisation changed on
every single sample, which is the polling this replaces. So each subsystem has a rule:

| Subsystem | Rule |
|---|---|
| CPU, temperatures | 0.5 percentage points |
| Load average | 0.05 |
| Memory, swap, filesystems, interfaces | exact byte comparison |

Byte counts are discrete and do not drift on their own, so they are compared exactly.
Interfaces are compared on their **cumulative** totals rather than the recent amounts,
because an idle interface reports zero every tick — comparing amounts would either
miss traffic or report a change constantly.

**A transition into or out of unavailable is always a change**, regardless of numeric
distance. `None` and `Some(0.0)` are different answers, and a threshold must never
collapse them.

### Cost with clients connected

Collection happens once regardless of how many clients are connected; the daemon fans
one result out to all of them. Verified with three simultaneous clients over 42
seconds: byte-identical streams, 45 events and 30,777 bytes each.

A client that stops reading is bounded, not buffered without limit: the channel holds
eight updates — four collection intervals at the 2-second cadence — and a client that
falls further behind is resynchronised with a fresh `snapshot` rather than
disconnected. Dropping the connection would make a phone on a slow link flap.

Closing the connection releases everything held for it. The dashboard disconnects a
hidden tab for that reason: the daemon sends to every subscriber, so an abandoned tab
would cost a send per collection for a panel nobody can see.

## Collection memory

Enabling host collection costs roughly 3-4 MB resident on this host, against a daemon
baseline of 8.23 MB. That is above the 8% tolerance in `runtime-efficiency`, and it is
recorded as an open regression rather than an accepted cost.

What it is, measured rather than guessed: allocator working set acquired by the **first**
collection of all five subsystems and never returned to the OS. Three things establish
that:

- Steady state is flat. Daemon RSS holds at 12.1-12.5 MB from t=10s to t=60s, and 22
  successive in-process collections added 0.00 MB after the first. It is not a leak.
- Freeing does not recover it. A `release()` method that dropped and rebuilt every
  sysinfo collection recovered **0.00 MB**, and so did dropping the whole collector.
  The pages outlive the object that allocated them.
- It is not one reducible allocation. Removing the largest single allocation — the
  filesystem enumeration, 4.33 MB in isolation — recovered 0.48 MB of daemon RSS, 13%
  of what it allocates.

Three fixes were tried and none worked: constructing the temperature collection lazily
(targeted 0.52 MB), sharing the snapshot behind `Arc` (targeted per-tick churn, roughly
1 MB), and releasing every collection when nothing had read `/api/host` for 90 seconds
(disproved outright by the 0.00 MB release figure). The `Arc` sharing was kept anyway,
because it removes real per-tick copying and makes the change detection above cheap —
but not as a memory fix.

Note on measuring this yourself: figures move by ~3 MB with machine state. One session
measured 11.83-12.19 MB and another 14.84-15.08 MB on identical code, with the
difference explained by load average, memory pressure and leftover browser processes.
Measure on a quiet machine, take several runs, and treat a single number as noise.

### Turning it off

`OXMGR_HOST_METRICS=0` (or `off`, `false`, `no`, `disabled`) skips host collection entirely
and recovers roughly 4 MB of resident memory. `/api/host` then answers 503, no
`oxmgr_host_*` series are emitted, and the dashboard sidebar stays hidden — the same path
as "before the first collection", so there is no separate degraded mode.

Per-process metrics are unaffected.

This exists because the cost cannot be reduced from inside the collector: freed pages stay
with the allocator, so not collecting is the only measure that changes the figure. Any
unrecognised value leaves collection on, so a typo cannot silently disable it.
