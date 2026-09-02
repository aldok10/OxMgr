# Benchmarks

This repository includes an automated `oxmgr` vs `pm2` benchmark harness.

The latest committed benchmark snapshots live in [`/BENCHMARK.md`](../BENCHMARK.md) and [`/benchmark.json`](../benchmark.json).

It is designed to run both:

- locally on Linux or macOS
- automatically in GitHub Actions on `ubuntu-latest`

## What It Measures

The suite focuses on process-manager behavior instead of application throughput.

- empty-daemon boot time
- empty-daemon RSS
- config-driven startup time at scale
- time until all managed processes are online
- `oxmgr list` vs `pm2 jlist` latency
- single-app restart latency, split into:
  - command completion
  - replacement PID visible in manager state
  - ready event emitted by the workload
  - ready event visible to the benchmark
  - TCP endpoint serving the replacement PID
- crash recovery latency after `SIGKILL`, split into:
  - replacement PID visible in manager status
  - ready event emitted by the workload
  - ready event visible to the benchmark
  - TCP endpoint serving the replacement PID

The benchmark workload is a minimal idle Node.js process in [`bench/fixtures/idle.js`](../bench/fixtures/idle.js).

## Local Run

Prerequisites:

- Rust toolchain
- Node.js + npm
- Linux or macOS

Run the default suite:

```bash
python3 scripts/benchmark_oxmgr_vs_pm2.py
```

Useful options:

```bash
python3 scripts/benchmark_oxmgr_vs_pm2.py \
  --process-counts 1,25,100 \
  --scale-trials 5 \
  --restart-samples 10 \
  --crash-samples 10 \
  --keep-workspaces
```

By default the script:

- builds `target/release/oxmgr`
- uses `pm2` from `PATH` if present
- otherwise installs a local pinned `pm2` into `bench/.cache/tools/`
- writes reports into `bench/results/<timestamp>/`

Outputs:

- `report.json`: raw samples plus summary statistics
- `benchmark.json`: stable machine-readable snapshot of the published benchmark tables
- `summary.md`: Markdown report suitable for GitHub summary or sharing

## GitHub Workflow

The workflow lives in [`.github/workflows/benchmark.yml`](../.github/workflows/benchmark.yml).

It:

- installs a pinned `pm2`
- builds `oxmgr`
- runs the same Python harness as local runs
- refreshes the tracked latest snapshots in [`/BENCHMARK.md`](../BENCHMARK.md) and [`/benchmark.json`](../benchmark.json)
- uploads `report.json`, `benchmark.json`, and `summary.md` as workflow artifacts
- publishes `summary.md` into the GitHub Actions step summary

## Interpreting Results

GitHub-hosted runners are noisy. Use these numbers as trend data:

- compare runs over time on the same workflow
- look at medians and p95, not a single sample
- prefer directionality over tiny deltas

If you want tighter numbers, run the same script on a dedicated machine or self-hosted runner.

---

# Resource Budgets

Separate from the pm2 comparison above, and answering a different question. That
suite asks "is oxmgr competitive with pm2". This asks "is oxmgr still lightweight
compared to its own last release" — which nothing checked before, so a daemon that
quietly grew to 80 MB over a few releases would not have been noticed.

## What Is Measured

Four figures, each tied to something an operator or maintainer actually asks. More
metrics would mean more noise for less signal.

| Figure | Question it answers |
| --- | --- |
| `binary_bytes` | The single-binary promise. Most likely to creep, since the dashboard assets are `include_str!`-ed into the binary. |
| `idle_rss_bytes` | The cost of merely running, with nothing managed. |
| `loaded_rss_bytes` | RSS under log-heavy load, where allocation behaviour on the hot path shows. |
| `idle_cpu_percent` | A supervisor should be invisible when nothing is happening. |

Per-cycle maintenance cost is reported as a distribution rather than a mean,
because the tail is what an operator feels and a mean hides it.

The load generator emits mixed-format lines — plain, ANSI-coloured, JSON, logfmt,
stack traces, over-long payloads — with a 60-line burst every 40 lines. Uniform
output would not stress the log path the way a real service does, and the bursts
are what expose batching problems.

## Running It

```sh
# Measure (builds a release binary unless --skip-build)
python3 scripts/measure_resource_budget.py --out budget-report.json

# Compare against the recorded baseline for this platform
python3 scripts/compare_resource_budget.py budget-report.json \
  bench/baselines/darwin-arm64.json

# Assert memory plateaus rather than climbing
python3 scripts/check_memory_growth.py budget-report.json
```

## Baseline

Baselines live in `bench/baselines/<platform>-<arch>.json` and are compared
**per platform**. Comparing across platforms would flag an allocator difference as
a regression, so `compare_resource_budget.py` refuses to do it.

Recorded on `darwin-arm64`, 2026-08-15, three managed workload processes:

| Figure | Value |
| --- | --- |
| `binary_bytes` | 3,837,408 (3.66 MB) |
| `idle_rss_bytes` | 11,124,736 (10.61 MB) |
| `loaded_rss_bytes` | 8,634,368 (8.23 MB) |
| `idle_cpu_percent` | 0.0 |
| `loaded_cpu_percent` | 0.1 |

### Why Idle Exceeds Loaded

That pairing looks wrong and is not. Sampling one daemon's RSS from boot gives:

```
t=01s   6.59 MB
t=02s  10.68 MB   <- startup allocation settles here
t=12s  10.68 MB   (idle sampled in this window)
        --- workload started ---
t=15s   8.45 MB
t=18s   8.01 MB   <- steady state under load
```

A freshly booted daemon holds the memory its startup work needed, and the allocator has
no reason to release it while nothing is happening. Once real work arrives the pages are
recycled and RSS *falls*. So `idle_rss_bytes` means "booted and unused", not a floor that
load rises above. It is still worth tracking — it is what an operator sees after
installing and before configuring anything.

## Tolerances, And Why They Are What They Are

Derived from observed variance, not chosen in advance. A gate tighter than the noise
fails randomly and then gets deleted, which is worse than having no gate.

Four consecutive runs on the same machine, on unchanged code:

| Figure | Observed spread | Tolerance | Reasoning |
| --- | --- | --- | --- |
| `binary_bytes` | 0.0% | 5% | Deterministic build, so a tight bound is honest. |
| `loaded_rss_bytes` | 0.6% | 8% | Roughly ten times the observed spread. This is the figure that describes a supervisor doing work. |
| `idle_rss_bytes` | 32% | not gated | Bimodal — see below. Reported, not enforced. |
| `*_cpu_percent` | 50% | not gated | At 0.1–0.2%, `ps` resolution is coarser than the signal. Reported, not enforced. |

### Idle RSS Is Bimodal, So It Cannot Be Gated

Sampling five freshly booted daemons once a second, with no workload:

```
run1  10.59 ×14                                  (never dropped)
run2  10.51 ×12  then 6.21                       (dropped at t=13s)
run3  10.53 ×14                                  (never dropped)
run4  10.48 ×7   then 6.21                       (dropped at t=8s)
run5  10.57 ×3   8.14  then 6.64                 (dropped at t=4s)
```

RSS settles at ~10.5 MB after startup and then falls to ~6.2 MB when the allocator
releases the startup pages — at a time that is not deterministic, and sometimes not
within 14 seconds. So the figure depends on when sampling happens to land, which
produced a 32% run-to-run spread on unchanged code.

A gate on that would fail randomly, which is exactly the failure this section exists to
avoid. It stays measured and reported, because "what does a freshly installed daemon
hold" is still worth watching; it just cannot be an assertion.

### A Bad Instrument Produces A Useless Gate

The first version of this harness reported `max()` of the sampled series, and measured
8.8% spread on idle RSS — which would have justified a 20% tolerance. It then flagged a
+60% "regression" on a change that could only reduce startup work.

Bisecting showed the same swing on the *unmodified* code: 6.53 MB one run, 10.56 MB the
next. The instrument was the problem, not the daemon — a single allocator spike was
deciding the figure.

Switching to p95 dropped the spread to 1.6% and let the tolerance tighten from 20% to 8%.
A tolerance derived from a noisy instrument does not just permit noise; it hides real
regressions behind it.

CPU being ungated is a deliberate admission: measuring it properly needs finer
instrumentation than `ps`, and pretending otherwise would produce a check that
fails on rounding.

## Memory Growth

A point measurement of RSS cannot distinguish a daemon that sits at 8 MB from one
that will reach 800 MB in a week, and the second is the failure mode that matters
for a process manager meant to run for months.

`check_memory_growth.py` fits a least-squares slope over the sampled RSS series and
reports `climbing` only when **all** of these hold:

- the slope is positive
- the fit's r² is at least 0.50, so the line actually describes the data
- projected growth across the window exceeds 15% of the starting value

Requiring all three is what separates a leak from oscillation. Verified in both
directions: an injected steady climb (8.4 MB → 13.3 MB over 24 samples, r² 0.9999)
is caught, while injected oscillation of the same amplitude with no trend
(r² 0.026) is correctly reported as a plateau.

This is the same trend arithmetic the `process-intelligence` change specifies for
detecting leaks in managed processes, applied to oxmgr itself. A supervisor that
flags leaks in others should be held to the same test.

## Updating A Baseline

Baselines rot. One recorded and never revisited becomes either meaningless or an
obstacle, so updating is deliberate rather than automatic:

1. Confirm the change is a legitimate cost, not a regression. A new feature that
   genuinely needs memory is a reason; "the check was annoying" is not.
2. Re-measure at least three times on the same machine and take a representative
   run, not the most favourable one.
3. Copy the report to `bench/baselines/<platform>-<arch>.json`, dropping
   `loaded_rss_series` and `loaded_rss_distribution` — those belong to one run, and
   a baseline is a record of figures.
4. Record the date and the reason for the change in this file.

## CI

The `resource-budget` job in [`ci.yml`](../.github/workflows/ci.yml) measures on
`ubuntu-latest` and uploads the report as an artifact. It is **report-only**: it
prints regressions without failing the build.

Enforcement waits on measuring the runner's own variance, which is higher than a
local machine's. Enabling a gate before that is known would mean random failures,
and a check people learn to ignore is worse than one that does not exist yet. When
the variance is recorded here, pass `--enforce` to
`compare_resource_budget.py`.

No baseline is committed for `linux-x86_64` yet: the job prints the measurement and
explains how to adopt it, rather than inventing a baseline from a single run on
hardware nobody has characterised.
