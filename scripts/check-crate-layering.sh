#!/usr/bin/env bash
# Crate layering gate for the OxMgr Cargo workspace (workspace-crate-layout).
#
# Validates three structural rules over `cargo metadata`:
#   1. Every member manifest declares `[lints] workspace = true` — no member
#      may weaken the shared deny set.
#   2. The workspace-internal dependency graph is acyclic.
#   3. Every internal edge points downward in the declared layer order
#      (core=0 … bin=6). An edge from a lower layer to a higher layer is a
#      defect: the compiler would allow it, the architecture forbids it.
#
# Deterministic by construction — same workspace state, same verdict.
# Exit codes: 0 = pass, 1 = structural violation, 2 = usage/environment error.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

command -v cargo >/dev/null 2>&1 || { echo "FAIL: cargo not on PATH" >&2; exit 2; }
command -v python3 >/dev/null 2>&1 || { echo "FAIL: python3 not on PATH" >&2; exit 2; }

cargo metadata --format-version=1 --no-deps > /tmp/oxmgr-layering-metadata.json

python3 - <<'PY'
import json
import os
import sys

LAYER_ORDER = [
    "oxmgr-core",       # 0 — domain types, pure logic, zero I/O
    "oxmgr-store",      # 1 — persistence and retention
    "oxmgr-metrics",    # 2 — host/process/container collection
    "oxmgr-analytics",  # 3 — detectors, baselines, findings
    "oxmgr-manager",    # 4 — process lifecycle
    "oxmgr-daemon",     # 5 — HTTP server and event streaming (owns fan-out)
    "oxmgr",            # 6 — binary crate: CLI parsing, command dispatch
]
LAYER_INDEX = {name: i for i, name in enumerate(LAYER_ORDER)}

# Zero-I/O guard (buzz-core convention): oxmgr-core must not declare any
# dependency that pulls an async runtime, an HTTP server, or system collection.
CORE_FORBIDDEN_DEPS = {"tokio", "axum", "sysinfo", "nix", "hyper", "tower"}

failures = []

with open("/tmp/oxmgr-layering-metadata.json") as f:
    metadata = json.load(f)

packages = {p["name"]: p for p in metadata["packages"]}
# workspace_members lists package IDs (e.g. path+file:///…#0.5.0), not names;
# resolve each ID to its package name through the packages table.
id_to_name = {p["id"]: p["name"] for p in metadata["packages"]}
members = {id_to_name[m] for m in metadata["workspace_members"]}

# Rule 1: every member inherits the workspace lint table.
for name in sorted(members):
    pkg = packages.get(name)
    if pkg is None:
        continue
    manifest = pkg["manifest_path"]
    with open(manifest) as f:
        content = f.read()
    if "[lints]\nworkspace = true" not in content:
        failures.append(
            f"{name}: member manifest does not declare '[lints] workspace = true' ({manifest})"
        )

# Zero-I/O rule for oxmgr-core.
core = packages.get("oxmgr-core")
if core is not None:
    declared = {d["name"] for d in core["dependencies"]}
    forbidden_hits = sorted(declared & CORE_FORBIDDEN_DEPS)
    if forbidden_hits:
        failures.append(
            f"oxmgr-core: zero-I/O violation — declares I/O-bearing deps: {forbidden_hits}"
        )

# Rules 2 + 3: acyclicity and downward-only internal edges.
edges = []  # (from_layer, to_layer, from_name, to_name)
adjacency = {}
for name in members:
    pkg = packages[name]
    adjacency.setdefault(name, [])
    for dep in pkg["dependencies"]:
        dep_name = dep["name"]
        if dep_name in members and dep_name != name:
            # Resolve renames: the dep's real package name.
            adjacency[name].append(dep_name)
            if name in LAYER_INDEX and dep_name in LAYER_INDEX:
                edges.append((LAYER_INDEX[name], LAYER_INDEX[dep_name], name, dep_name))

for from_l, to_l, frm, to in edges:
    # Lower index = lower layer. A legal edge points downward (to_l < from_l).
    # Same-layer and upward edges are defects.
    if to_l >= from_l:
        direction = "same layer" if to_l == from_l else "UPWARD"
        failures.append(
            f"layering violation ({direction}): '{frm}' (layer {from_l}) depends on "
            f"'{to}' (layer {to_l})"
        )

# Cycle detection (DFS, three-colour).
WHITE, GREY, BLACK = 0, 1, 2
colour = {n: WHITE for n in members}
cycle_found = False


def dfs(node, stack):
    global cycle_found
    colour[node] = GREY
    for nxt in adjacency.get(node, []):
        if colour.get(nxt, BLACK) == GREY:
            cycle_found = True
            failures.append(f"dependency cycle detected through: {' -> '.join(stack + [node, nxt])}")
        elif colour.get(nxt, BLACK) == WHITE:
            dfs(nxt, stack + [node])
    colour[node] = BLACK


for node in sorted(members):
    if colour[node] == WHITE:
        dfs(node, [])

if failures:
    print(f"FAIL: {len(failures)} layering/lint-conformance violation(s):")
    for f_ in failures:
        print(f"  - {f_}")
    sys.exit(1)

print(f"OK: {len(members)} members, {len(edges)} internal edges, all downward, acyclic, lint-inherited")
PY

status=$?
rm -f /tmp/oxmgr-layering-metadata.json
exit $status
