#!/usr/bin/env python3
"""Compares a resource-budget measurement against a recorded baseline.

Deliberately separate from `measure_resource_budget.py`: measuring and judging are
different jobs, and keeping them apart means a baseline can be re-measured without
running the comparison, and a stored report can be re-judged without re-running the
daemon.

Tolerances are derived from observed variance, not chosen in advance. Three runs on
the lab machine gave: binary 0.0%, loaded RSS 2.0%, idle RSS 8.8%, and CPU 50%
(0.1 vs 0.2 — a single ps sample rounding, not a real change). A gate tighter than
the noise fails randomly and then gets deleted, which is worse than no gate.

CPU is therefore reported and not gated: at oxmgr's idle draw, `ps` resolution is
coarser than the signal. Saying so is better than pretending to measure it.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

# Tolerance per figure, as a fraction over baseline. Each is roughly four times the
# observed spread: wide enough that ordinary noise passes, narrow enough that a real
# regression does not.
#
# Measured across four runs on unchanged code, after the harness was changed to report
# p95 rather than max:
#
#   binary_bytes      0.0% spread — deterministic build, so a tight bound is honest
#   idle_rss_bytes    1.6% spread
#   loaded_rss_bytes  1.7% spread
#
# The first pass at this used max() and measured 8.8% spread on idle RSS, which would
# have meant a 20% tolerance. That was the instrument being noisy, not the daemon, and a
# tolerance derived from a bad instrument hides real regressions.
#
# `idle_rss_bytes` was gated at 8% and is now report-only. Sampling five fresh daemons
# once a second showed why: RSS sits at ~10.5 MB after startup and then drops to ~6.2 MB
# when the allocator releases it — at t=4s, t=8s, t=13s, or not at all within 14s. The
# figure is bimodal and the mode depends on when sampling lands, which produced a 32%
# run-to-run spread. Gating on it would fail randomly, which is the failure this whole
# section exists to avoid. `loaded_rss_bytes` is the figure that describes a supervisor
# doing work, and it is stable at 0.6%.
TOLERANCE = {
    "binary_bytes": 0.05,
    "loaded_rss_bytes": 0.08,
}

# Measured but not gated, with the reason recorded rather than left implicit.
REPORT_ONLY = {
    "idle_cpu_percent": "ps resolution is coarser than oxmgr's idle draw",
    "loaded_cpu_percent": "observed 50% run-to-run spread at 0.1-0.2%",
    "idle_rss_bytes": "bimodal: the allocator releases startup memory at a "
    "non-deterministic time, so the figure depends on when sampling happens",
}

# `idle_rss_bytes` measures a freshly booted daemon that has not been asked to do
# anything, so it is expected to exceed `loaded_rss_bytes`: measured trajectory shows RSS
# peaking at 10.7 MB after startup and falling to 8.0 MB once work arrives. Noted here so
# the pairing does not read as a contradiction.


def load(path: Path) -> dict:
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        raise SystemExit(f"missing file: {path}")
    except json.JSONDecodeError as error:
        raise SystemExit(f"{path} is not valid JSON: {error}")


def human(key: str, value: float) -> str:
    if key.endswith("_bytes"):
        return f"{value / 1048576:.2f} MB"
    if key.endswith("_percent"):
        return f"{value:.2f}%"
    return str(value)


def main() -> int:
    parser = argparse.ArgumentParser(description="Compare a budget report to a baseline.")
    parser.add_argument("report", type=Path)
    parser.add_argument("baseline", type=Path)
    parser.add_argument(
        "--enforce",
        action="store_true",
        help="exit non-zero on a regression beyond tolerance (default: report only)",
    )
    args = parser.parse_args()

    report = load(args.report)
    baseline = load(args.baseline)

    report_platform = report.get("platform", "?")
    baseline_platform = baseline.get("platform", "?")
    if report_platform != baseline_platform:
        # Comparing across platforms would flag an allocator difference as a
        # regression. Refuse rather than produce a misleading verdict.
        print(
            f"platform mismatch: report {report_platform!r} vs baseline {baseline_platform!r}",
            file=sys.stderr,
        )
        return 2

    print(f"platform: {report_platform}")
    print(f"{'figure':22} {'baseline':>12} {'measured':>12} {'change':>9}  verdict")

    regressions: list[str] = []
    for key, tolerance in TOLERANCE.items():
        if key not in baseline or key not in report:
            print(f"{key:22} {'—':>12} {'—':>12} {'—':>9}  missing")
            continue
        before = float(baseline[key])
        after = float(report[key])
        change = 0.0 if before == 0 else (after - before) / before
        over = change > tolerance
        verdict = f"REGRESSED (>{tolerance:.0%})" if over else "ok"
        if over:
            regressions.append(f"{key}: {human(key, before)} -> {human(key, after)} ({change:+.1%})")
        print(
            f"{key:22} {human(key, before):>12} {human(key, after):>12} {change:>+8.1%}  {verdict}"
        )

    for key, reason in REPORT_ONLY.items():
        if key in report:
            before = baseline.get(key)
            shown = human(key, float(before)) if before is not None else "—"
            print(
                f"{key:22} {shown:>12} {human(key, float(report[key])):>12} {'—':>9}"
                f"  not gated: {reason}"
            )

    if not regressions:
        print("\nno regression beyond tolerance")
        return 0

    print("\nregressions:")
    for line in regressions:
        print(f"  - {line}")
    if not args.enforce:
        print("\nreport-only: pass --enforce to fail on these")
        return 0
    return 1


if __name__ == "__main__":
    sys.exit(main())
