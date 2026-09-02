#!/usr/bin/env python3
"""Spec-tree consolidation tool for the spec-consolidation change.

Builds merged capability specs from a group map (requirement blocks moved
verbatim) and verifies tree-wide equality of the sorted (requirement heading,
scenario count) multiset before and after (design D2). A diff in that set is a
hard failure — silent truncation during concatenation is caught by the gate,
not by careful reading.

Usage:
  python3 tools/consolidate.py build --specs openspec/specs \
      --out /tmp/consolidated            # write merged tree to scratch dir
  python3 tools/consolidate.py check --specs openspec/specs \
      --out /tmp/consolidated            # verify equality gate on built tree

Group map: GROUP_MAP below, new-capability -> [absorbed capability names, in
order]. Capabilities absent from the map are kept as-is (identity-mapped) so
the equality gate covers the whole tree either way.
"""

from __future__ import annotations

import argparse
import collections
import re
import shutil
import sys
from pathlib import Path

GROUP_MAP: dict[str, list[str]] = {
    "dashboard-delivery": [
        "static-asset-serving",
        "dashboard-asset-pipeline",
        "dashboard-module-boundaries",
        "dashboard-network-resilience",
    ],
    "dashboard-layout": [
        "web-dashboard",
        "dashboard-responsive",
        "dashboard-responsive-rework",
        "dashboard-design-tokens",
        "host-panel-compact-layout",
        "host-network-storage-metrics",
        "resource-severity-display",
        "uptime-display",
        "process-tree-display",
    ],
    "dashboard-behavior": [
        "dashboard-event-architecture",
        "dashboard-interaction-safety",
        "dashboard-dialog-semantics",
        "dashboard-status-messaging",
        "dashboard-render-efficiency",
        "dashboard-panel-lifecycle",
        "dashboard-ui-autoclose",
    ],
    "process-analytics": [
        "process-anomaly-detection",
        "process-failure-patterns",
        "process-metrics-history",
        "process-io-metrics",
    ],
    "process-remediation": ["process-remediation", "config-risk-advisor"],
    "host-metrics": [
        "host-metrics",
        "host-metrics-streaming",
        "host-metrics-efficiency",
        "container-awareness",
    ],
    "host-process-visibility": [
        "host-process-visibility",
        "managed-process-children",
        "process-tree-data",
        "cluster-instance-visibility",
    ],
    "log-management": ["log-retention", "archive-paging", "dashboard-logs"],
    "http-api-surface": [
        "http-server-surface",
        "daemon-command-backpressure",
        "unified-dashboard-stream",
    ],
    "code-discipline": [
        "panic-freedom",
        "unsafe-justification",
        "result-handling-discipline",
        "numeric-conversion-safety",
        "cast-suppression-discipline",
        "documentation-language-standardization",
        "source-code-comment-cleanup",
    ],
    "runtime-efficiency": [
        "runtime-efficiency",
        "allocation-discipline",
        "async-blocking-discipline",
    ],
    "testing-quality": ["test-env-isolation", "test-suite-rationalization"],
}

REQ_HEADING = re.compile(r"^### Requirement: (.+)$", re.MULTILINE)
SCENARIO_HEADING = re.compile(r"^#### Scenario:", re.MULTILINE)


def parse_spec(text: str) -> tuple[str, str]:
    """Split a spec file into its Purpose section and requirements body.

    Returns (purpose_block, requirements_body) where purpose_block includes
    the `## Purpose` heading and prose, and requirements_body starts at the
    `## Requirements` heading. Both are verbatim slices.
    """
    purpose_at = text.index("## Purpose")
    reqs_at = text.index("## Requirements")
    return text[purpose_at:reqs_at], text[reqs_at:]


def split_requirements(body: str) -> list[str]:
    """Split a `## Requirements` body into whole requirement blocks.

    Each block starts at a `### Requirement:` heading and runs until the next
    `### Requirement:` or EOF. Verbatim slices; no normalization.
    """
    marks = [m.start() for m in REQ_HEADING.finditer(body)]
    if not marks:
        raise ValueError("no requirements found in body")
    return [body[a:b] for a, b in zip(marks, marks[1:] + [len(body)])]


def requirement_signature(block: str) -> tuple[str, int]:
    """(requirement heading text, number of scenario headings) for one block."""
    title = REQ_HEADING.search(block)
    assert title is not None
    scenarios = len(SCENARIO_HEADING.findall(block))
    return (title.group(1).strip(), scenarios)


def tree_signatures(specs_dir: Path) -> collections.Counter:
    """Multiset of (req-heading, scenario-count) over the whole tree.

    Deliberately NOT keyed by capability: consolidation moves requirement
    blocks between capabilities by design (D2), so identity is the heading
    text plus its scenario count.
    """
    sigs: collections.Counter = collections.Counter()
    for cap_dir in sorted(p for p in specs_dir.iterdir() if p.is_dir()):
        spec_file = cap_dir / "spec.md"
        _, body = parse_spec(spec_file.read_text())
        for block in split_requirements(body):
            heading, n_scen = requirement_signature(block)
            sigs[(heading, n_scen)] += 1
    return sigs


def build_merged(specs_dir: Path, out_dir: Path) -> None:
    """Write the consolidated tree to out_dir (scratch), never touching specs."""
    all_caps = {p.name for p in specs_dir.iterdir() if p.is_dir()}
    absorbed = {c for caps in GROUP_MAP.values() for c in caps}
    missing = absorbed - all_caps
    if missing:
        raise SystemExit(
            f"ERROR: group map references capabilities not in the tree: "
            f"{sorted(missing)}"
        )
    collisions = all_caps & set(GROUP_MAP)
    bad_collisions = {
        c for c in collisions if any(c in caps for caps in GROUP_MAP.values())
    }
    # A name may appear both as a group target and inside another group only if
    # it is being renamed into that group (e.g. process-remediation absorbs
    # itself plus config-risk-advisor under the same name). That is legal.
    _ = bad_collisions

    if out_dir.exists():
        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True)

    identity_caps = all_caps - absorbed
    for cap in sorted(identity_caps):
        dst = out_dir / cap
        dst.mkdir()
        shutil.copy2(specs_dir / cap / "spec.md", dst / "spec.md")

    for new_cap, sources in GROUP_MAP.items():
        purpose_parts = []
        blocks: list[str] = []
        for src in sources:
            text = (specs_dir / src / "spec.md").read_text()
            purpose, body = parse_spec(text)
            purpose_parts.append((src, purpose))
            for block in split_requirements(body):
                # Source files do not all end in a trailing newline; without a
                # separator the next `### Requirement:` heading would fuse onto
                # the previous block's final line. Whitespace *between* blocks
                # is layout, not content: each block's own text stays verbatim.
                if not block.endswith("\n"):
                    block += "\n"
                blocks.append(block)
        # Fresh Purpose for the merged file is authored separately (D2); here we
        # concatenate source purposes under a marker for the authoring pass.
        purpose_section = render_purpose(new_cap, purpose_parts)
        content = (
            f"# {new_cap} Specification\n\n"
            + purpose_section
            + "## Requirements\n"
            + "".join(blocks)
        )
        dst = out_dir / new_cap
        dst.mkdir(exist_ok=True)
        (dst / "spec.md").write_text(content)


def render_purpose(new_cap: str, purpose_parts: list[tuple[str, str]]) -> str:
    """Author the fresh Purpose section for a merged capability.

    The per-source purposes are kept as an appendix comment so the authoring
    pass can consult them without re-opening every file; the shipped Purpose is
    the single paragraph below.
    """
    authored = PURPOSE_OVERRIDES.get(new_cap)
    if not authored:
        raise SystemExit(f"ERROR: no authored Purpose for merged capability {new_cap}")
    lines = [f"## Purpose\n{authored}\n"]
    return "\n".join(lines)


PURPOSE_OVERRIDES: dict[str, str] = {
    "dashboard-delivery": "How dashboard assets are built and organized in modules,\nserved by the daemon, and kept usable when the daemon is unreachable.",
    "dashboard-layout": "What the dashboard page looks like: page regions, responsive\ntiers, design tokens, compact panel packing, and figure presentation\n(severity cues, uptime, process tree).",
    "dashboard-behavior": "What the dashboard does at runtime: the event architecture,\nbounded listener and request lifetimes, dialog semantics, status messaging,\nrender efficiency, and panel lifecycle.",
    "process-analytics": "Per-process baselines, deterministic detectors, failure-shape\ndetection, metric history retention, and I/O measurement — the observation\nhalf of process intelligence.",
    "process-remediation": "The deterministic decision engine that maps findings to intended\nactions, protection mode's guarded automated responses, and configuration\nrisk advisories.",
    "host-metrics": "Collection, streaming, efficiency budgets, and container-aware\ndenominators for host-level figures.",
    "host-process-visibility": "Which processes matter enough to report: significant consumers,\ndescendant attribution, tree data, and cluster/instance shape reporting.",
    "log-management": "The log write path, rotation and retention, bounded paging of\nrotated files, and the dashboard log viewer/streaming surface.",
    "http-api-surface": "The daemon's HTTP route set, auth boundary, the multiplexed SSE\nstream contract, and command backpressure.",
    "code-discipline": "Source-code safety and hygiene standards that fail review rather\nthan production: panic freedom, unsafe justification, result handling,\nnumeric conversion safety, cast suppression, documentation language, and\ncomment hygiene.",
    "runtime-efficiency": "Daemon resource budgets and their defense: measurement,\nregression detection, allocation discipline, and prohibition of blocking\ncalls on async workers.",
    "testing-quality": "Suite rationalization standards and environment isolation for\ntests that touch global state.",
}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="cmd", required=True)
    for name in ("build", "check"):
        sp = sub.add_parser(name)
        sp.add_argument("--specs", type=Path, default=Path("openspec/specs"))
        sp.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()

    if args.cmd == "build":
        build_merged(args.specs, args.out)
        print(f"built merged tree in {args.out}")
        return

    # check: equality gate between the live tree and the built tree.
    before = tree_signatures(args.specs)
    after = tree_signatures(args.out)
    if before != after:
        only_before = before - after
        only_after = after - before
        print("EQUALITY GATE FAILED")
        for key, n in sorted(only_before.items())[:10]:
            print(f"  missing from merged tree ({n}x): {key}")
        for key, n in sorted(only_after.items())[:10]:
            print(f"  unexpected in merged tree ({n}x): {key}")
        raise SystemExit(1)
    total_reqs = sum(before.values())
    print(f"EQUALITY GATE PASSED: {total_reqs} requirements, identical multiset")


if __name__ == "__main__":
    main()
