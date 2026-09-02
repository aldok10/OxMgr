#!/bin/bash
# scripts/check-change-artifacts.sh
# Artifact integrity check for spec-lifecycle-discipline (§D5).
# Run from the repo root. Exits non-zero, naming every offending instance.
#
# Checks:
#   4.2 malformed task markers (a line starting `- [` whose checkbox is not
#       exactly `- [ ] ` or `- [x] `; catches the `- [[x]]` rewrite)
#   4.3 proposal-declared capabilities have a matching specs/<cap>/ directory
#       in the change (or already synced in openspec/specs/)
#   4.4 decision identifiers unique per document; a decision marked SUPERSEDED
#       names its replacement (`by D<n>`)
#   4.5 no `TBD` placeholders in openspec/specs/
#   4.6 `openspec list` task totals match the artifacts' own checkbox counts
#   6.4 a change declaring skip_specs records a basis in its proposal.md
set -u

fail=0
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

warn() { echo "FAIL: $1" >&2; fail=1; }

# ---- 4.2 malformed task markers ------------------------------------------
while IFS= read -r f; do
  matches=$(grep -nE '^- \[' "$f" 2>/dev/null | grep -vE '^[0-9]+:- \[[ x]\] ')
  if [ -n "$matches" ]; then
    warn "4.2 malformed task markers in $f:"
    echo "$matches" >&2
  fi
done < <(find openspec/changes -name tasks.md)

# ---- 4.3 proposal capabilities vs specs dirs ------------------------------
# For ACTIVE changes that carry a specs/ directory, every capability declared
# in the proposal's `## Capabilities` list must exist as specs/<cap>/ in the
# change or as openspec/specs/<cap>/ (already synced). Would have caught
# process-tree-awareness naming two capabilities that never existed.
#
# Archived changes are deliberately out of scope: they are historical records
# whose declared capabilities resolved against the tree at their archive time.
# A later rename (e.g. spec-consolidation absorbing web-dashboard into
# dashboard-layout) must not fail old records — see spec-consolidation design,
# "archives stay untouched". Before that change, the archive sweep was harmless
# because absorbed directories still existed; afterwards it became false
# failures, so the sweep is scoped to active changes.
for proposal in openspec/changes/*/proposal.md; do
  [ -f "$proposal" ] || continue
  dir="$(dirname "$proposal")"
  [ -d "$dir/specs" ] || continue   # no delta specs yet: nothing to reconcile
  declared=$(sed -n '/^## Capabilities/,/^## /p' "$proposal" \
    | grep -oE '^\- `[a-z0-9-]+`' | sed -E 's/^\- `([a-z0-9-]+)`/\1/')
  for cap in $declared; do
    if [ ! -d "$dir/specs/$cap" ] && [ ! -d "openspec/specs/$cap" ]; then
      warn "4.3 capability \`$cap\` declared in $proposal but no specs/$cap directory present"
    fi
  done
done

# ---- 4.4 decision identifiers: unique, supersession names replacement -----
for f in openspec/changes/*/design.md openspec/changes/archive/*/design.md; do
  [ -f "$f" ] || continue
  ids=$(grep -oE '^\*\*D[0-9]+' "$f" | sed -E 's/^\*\*(D[0-9]+)/\1/')
  dup=$(printf '%s\n' "$ids" | sort | uniq -d)
  if [ -n "$dup" ]; then
    warn "4.4 duplicate decision identifiers in $f: $(echo $dup)"
  fi
  while IFS= read -r line; do
    case "$line" in
      *'by D'[0-9]*) : ;;
      *) warn "4.4 decision marked SUPERSEDED without naming its replacement in $f: $line" ;;
    esac
  done < <(grep -n 'SUPERSEDED' "$f" 2>/dev/null)
done

# ---- 4.5 TBD placeholders in merged specs ---------------------------------
tbd=$(grep -rn 'TBD' openspec/specs/ 2>/dev/null)
if [ -n "$tbd" ]; then
  warn "4.5 TBD placeholder in openspec/specs/:"
  echo "$tbd" >&2
fi

# ---- 4.6 openspec list totals vs artifact checkbox counts -----------------
# `openspec list` reports "N/M tasks"; M must equal the artifact's own count of
# `- [ ] ` / `- [x] ` lines. A silent miscount (the 43-checkbox case) surfaces
# here as a mismatch.
while read -r name done total; do
  f="openspec/changes/$name/tasks.md"
  [ -f "$f" ] || continue
  actual=$(grep -cE '^- \[( |x)\] ' "$f")
  if [ "$actual" -ne "$total" ]; then
    warn "4.6 openspec list reports $total tasks for $name but tasks.md has $actual checkboxes"
  fi
done < <(openspec list 2>/dev/null | sed -n 's/^  \([a-z0-9-]*\) *\([0-9]*\)\/\([0-9]*\) tasks.*/\1 \2 \3/p')

# ---- 6.4 skip_specs requires a recorded basis -----------------------------
for y in openspec/changes/*/.openspec.yaml openspec/changes/archive/*/.openspec.yaml; do
  [ -f "$y" ] || continue
  if grep -q 'skip_specs: *true' "$y" 2>/dev/null; then
    p="$(dirname "$y")/proposal.md"
    if [ ! -f "$p" ]; then
      warn "6.4 $y sets skip_specs but $p does not exist to carry the basis"
      continue
    fi
    if ! grep -qE 'skip_specs.*(no authoritative requirement|no spec-level|pure refactor|tooling|docs|wrong call|Corrected|corrected)' "$p"; then
      warn "6.4 $y sets skip_specs but $p records no basis (no stated reason naming why no authoritative requirement is affected)"
    fi
  fi
done

if [ "$fail" -ne 0 ]; then
  exit 1
fi
echo "Artifact check passed"
exit 0
