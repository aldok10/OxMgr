# Per-Process Disk I/O

Oxmgr reports disk I/O for each managed process, and reports per-process *network*
I/O as explicitly unsupported. This page states what the figures mean, what they
deliberately exclude, and how to tell "measured zero" from "no measurement".

## The three answers

Every I/O figure carries one of three meanings, and conflating them is what turns a
metric into a false alarm:

| Meaning | API | Prometheus | Dashboard |
|---|---|---|---|
| Measured a value | number | series present | the value |
| Measured zero | `0` | series present, `0` | `0 B/s` |
| No measurement | `null` | series omitted | `–` |

A zero means the process was watched over a known interval and did nothing. An
absence means Oxmgr cannot say. Alerting on the first is meaningful; alerting on the
second is alerting on your own blind spot.

There is no measurement when:

- the process is not running,
- it is the first sample after a process ID appears (nothing to difference against),
- the process ID changed since the last sample,
- the interval between samples was under 100 ms, so dividing by it yields noise.

## Amounts, rates, and totals

Three different fields, three different questions:

- `disk_read_bytes` / `disk_write_bytes` — bytes since the **previous sample**. An
  amount, not a rate. Meaningless without `metrics_interval_ms`.
- `disk_read_bytes_per_second` / `disk_write_bytes_per_second` — the amount divided
  by the interval actually observed. `null` when no rate can be derived.
- `disk_read_total` / `disk_write_total` — bytes since the managed process was first
  started, accumulated **across restarts**.

`metrics_interval_ms` is the measured gap between samples, not the nominal one. The
daemon's maintenance tick is nominally 2 s but uses `MissedTickBehavior::Skip`, so
under load it stretches — measured 1999–2000 ms in an idle lab, and longer when
busy. Dividing by an assumed 2 s overstates the rate whenever that happens, which is
precisely when you are looking.

If you compute your own rate, divide by `metrics_interval_ms` and refuse to divide
when it is absent. Do not assume the tick.

## The totals survive restarts, and are a lower bound

`disk_*_total` is scoped to the **managed process**, not to the operating-system
process. It is non-decreasing for as long as the process exists in Oxmgr, including
across restarts that change the PID.

This matters because the underlying platform counters are per-PID. Copying them
would make the total fall backwards on every restart — measured before this
behaviour was fixed:

```
before: pid 53658  write_total 33353728
after:  pid  4032  write_total  1814528
```

A counter that decreases is worse than no counter: anything deriving a rate sees a
negative delta, and Prometheus treats a decrease as a counter reset. Oxmgr therefore
sums the per-sample amounts rather than copying the platform's totals. Verified
across a live restart: `5317472256 → 6626082816 → 7803576320`.

The total resets only when the process is deleted, never on a restart.

Read it as a **lower bound**. I/O performed between two samples is counted, but I/O
performed after the last sample before an exit is not — the process is gone before
it can be measured. The figure tells you the shape of a workload over time, not an
audited byte count.

## What is not counted

Only the I/O of the process ID Oxmgr tracks. This has three consequences that look
like bugs and are not:

**A process that only writes to stdout or stderr reports zero disk I/O.** The
process writes to a pipe; the daemon performs the log file write. The bytes are real
and they are on disk, but they were not written by the process, so they are not
attributed to it. Verified: a workload emitting continuous stdout reports `0 B/s`
with a valid interval.

**A child's I/O is not attributed to the parent.** A process that shells out to
`dd`, or spawns workers that do the writing, reports zero for itself. Verified: a
wrapper looping over `dd` reports `0 B/s` while the file grows. Oxmgr samples the
tracked PID, not the process tree.

**Adopted processes start from zero.** The first sample after a PID appears is used
only to seed state. The platform's first reading for a PID is that PID's lifetime
figure — for an already-running process that can be gigabytes, and accumulating it
would credit the managed process with I/O from before Oxmgr was watching.

If a figure surprises you, check which process is actually doing the writing before
concluding the measurement is broken.

## Comparing across hosts

Don't. Use these figures to watch one host over time.

Disk accounting differs between platforms in what it counts — whether cached reads
are included, whether it measures at the syscall or block layer, how write-back is
attributed. The numbers are consistent enough on one host to show a trend, a leak,
or a change after a deploy. They are not absolute throughput, and comparing a Linux
figure with a macOS one measures the platforms, not the workloads.

## Per-process network I/O is unsupported

Not missing, not zero, not "coming later" — **unsupported**, and reported as such on
every surface:

```json
"network_io": {
  "status": "unsupported",
  "reason": "Per-process network I/O is not measurable on the supported platforms: ..."
}
```

The operating system reports network counters per **interface**, not per process.
Attributing traffic to a PID requires per-socket accounting: eBPF or `/proc/net`
socket-inode correlation on Linux, private `nettop`-style APIs on macOS. None of it
is portable, and all of it is a data-collection subsystem rather than a field.

Oxmgr will not present an interface total as one process's traffic. On a host
running one service that number happens to be close; on any other host it is a
plausible-looking lie, and a plausible lie in a metric is worse than an absence
because nobody checks it.

Host-level interface figures are reported separately, labelled as the host's. See
`docs/HOST-METRICS.md`.
