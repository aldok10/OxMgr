#!/usr/bin/env python3
"""Asserts oxmgr's memory reaches a plateau under sustained load rather than climbing.

A point measurement of RSS cannot tell a daemon that sits at 8 MB from one that will
reach 800 MB in a week, and the second is the failure mode that matters most for a
supervisor meant to run for months. So this is a separate check from the budget
harness: that one asks "how much", this asks "is it still growing".

The arithmetic is the same trend test `process-intelligence` specifies for detecting
leaks in managed processes — least-squares slope over the sample series, with a fit
quality gate — applied to oxmgr itself. A supervisor that flags leaks in others
should be held to the same test.

Reads the `loaded_rss_series` produced by `measure_resource_budget.py`, so the load
generator and sampling live in one place.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

# Growth beyond this share of the starting value across the window counts as a climb.
# Allocator behaviour makes small drift normal; a leak shows up as sustained slope.
GROWTH_LIMIT = 0.15
# Below this coefficient of determination the line does not describe the data, so a
# slope from it is not evidence of anything. Same gate the leak spec requires.
MIN_FIT = 0.50
# Fewer samples than this cannot support a trend claim either way.
MIN_SAMPLES = 12


def fit(series: list[float]) -> tuple[float, float]:
    """Least-squares slope per sample, and r-squared."""
    count = len(series)
    xs = list(range(count))
    mean_x = sum(xs) / count
    mean_y = sum(series) / count
    sxx = sum((x - mean_x) ** 2 for x in xs)
    sxy = sum((x - mean_x) * (y - mean_y) for x, y in zip(xs, series))
    if sxx == 0:
        return 0.0, 0.0
    slope = sxy / sxx
    intercept = mean_y - slope * mean_x
    ss_res = sum((y - (slope * x + intercept)) ** 2 for x, y in zip(xs, series))
    ss_tot = sum((y - mean_y) ** 2 for y in series)
    r_squared = 1.0 if ss_tot == 0 else max(0.0, 1 - ss_res / ss_tot)
    return slope, r_squared


def evaluate(series: list[float]) -> dict:
    if len(series) < MIN_SAMPLES:
        return {
            "verdict": "inconclusive",
            "reason": f"{len(series)} samples, need {MIN_SAMPLES}",
            "samples": len(series),
        }

    slope, r_squared = fit(series)
    first, last = series[0], series[-1]
    projected = slope * (len(series) - 1)
    growth = 0.0 if first == 0 else projected / first

    # A leak needs BOTH a rising slope the fit supports AND growth past the limit.
    # Either alone is noise: a good fit on a flat line means nothing, and a large
    # apparent change with a poor fit is oscillation, not a trend.
    climbing = slope > 0 and r_squared >= MIN_FIT and growth > GROWTH_LIMIT

    return {
        "verdict": "climbing" if climbing else "plateau",
        "samples": len(series),
        "first_bytes": int(first),
        "last_bytes": int(last),
        "min_bytes": int(min(series)),
        "max_bytes": int(max(series)),
        "slope_bytes_per_sample": round(slope, 1),
        "projected_growth_over_window": round(growth, 4),
        "r_squared": round(r_squared, 4),
        "growth_limit": GROWTH_LIMIT,
        "min_fit": MIN_FIT,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description="Check oxmgr RSS plateaus under load.")
    parser.add_argument("report", type=Path, help="a measure_resource_budget.py report")
    parser.add_argument(
        "--enforce",
        action="store_true",
        help="exit non-zero when the series is still climbing",
    )
    args = parser.parse_args()

    try:
        data = json.loads(args.report.read_text())
    except FileNotFoundError:
        raise SystemExit(f"missing file: {args.report}")

    series = [float(v) for v in data.get("loaded_rss_series", [])]
    if not series:
        print("report contains no loaded_rss_series", file=sys.stderr)
        return 2

    result = evaluate(series)
    print(json.dumps(result, indent=2, sort_keys=True))

    if result["verdict"] == "plateau":
        print("\nRSS plateaus under sustained load", file=sys.stderr)
        return 0
    if result["verdict"] == "inconclusive":
        print(f"\ninconclusive: {result['reason']}", file=sys.stderr)
        return 0 if not args.enforce else 2

    print(
        f"\nRSS still climbing: {result['slope_bytes_per_sample']} bytes/sample, "
        f"r²={result['r_squared']}, projected {result['projected_growth_over_window']:.1%}",
        file=sys.stderr,
    )
    return 1 if args.enforce else 0


if __name__ == "__main__":
    sys.exit(main())
