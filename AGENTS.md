# AI Engineering Guidelines - OxMgr

OxMgr is a lightweight, cross-platform Rust process manager (PM2 alternative).
The repository runs **spec-driven development with OpenSpec**: every change must
be specced, implemented, verified, and archived. The OpenSpec CLI is installed
(`openspec` 1.7.0, Homebrew). Prefer the CLI over hand-editing artifact files.

## 1. The Workflow Loop

Always operate in the loop defined by `change-evidence-gates`. Never skip steps,
and ensure each transition is supported by the evidence the stage requires.

1. **Explore** (`/opsx-explore`, optional): brainstorm or investigate when the
   change is vague or needs a design decision.
2. **Propose** (`/opsx-propose`): create the change and its artifacts —
   proposal, delta specs, design, tasks. Capability names are kebab-case
   (e.g. `process-intelligence`, `host-metrics`).
3. **Apply** (`/opsx-apply`): implement tasks, ticking `- [ ]` → `- [x]` in
   `openspec/changes/<name>/tasks.md` as each lands.
4. **Update** (`/opsx-update`): when implementation diverges from the plan,
   revise the planning artifacts first. Contract consistency over code.
5. **Sync** (`/opsx-sync`): merge delta specs back into `openspec/specs/`.
6. **Archive** (`/opsx-archive`): finalize and move the change to
   `openspec/changes/archive/` (archived changes gain a `YYYY-MM-DD-` prefix).

Commands live in `.opencode/commands/` (slash commands) and the matching
skills in `.opencode/skills/` — both are part of the flow, not alternatives.

## 2. Artifact Contract

- **Never implement un-specced work.** No change, no code.
- **Code and spec stay in sync.** When the implementation drifts
  (feasibility, new decisions), update the spec — don't silently diverge.
- **`openspec validate --all` must pass** after every change. It currently
  validates 47 specs/changes (46 passed, 1 failed as of last full check — run
  `openspec validate --all --strict` to see the failures).
- **Canonical locations:**
  - `openspec/specs/<capability>/spec.md` — merged, authoritative behavior.
  - `openspec/changes/<name>/` — proposal, delta specs, design, tasks (WIP).
- **Measured claims carry their command.** Every figure, count, timing, or
  pass/fail result recorded in an artifact carries the command that reproduces
  it, recorded when measured. A claim without a reproduction path is not
  evidence. Specified normatively in `openspec/specs/change-evidence-gates/spec.md`.
- **Task states mean what they say.** `[x]` is verified-complete only; blocked
  and partial work stay on `- [ ]` and name their blocker or remainder. Archive
  reconciles, it does not declare: capability names must match what shipped, no
  placeholders, open tasks resolved or owned. Specified normatively in
  `openspec/specs/change-completion-integrity/spec.md`.
- **Artifacts are machine-checkable.** Run `./scripts/check-change-artifacts.sh`
  before archive and in CI: it catches malformed checkboxes, unmatched
  capability names, unmarked supersession, `TBD` placeholders, and progress
  miscounts.
- **Spec style used in this repo:** top-level `## Purpose`, then
  `### Requirement:` written as SHALL/MUST/SHOULD statements, each with
  `#### Scenario:` blocks in WHEN/THEN form. Delta specs use `## ADDED`,
  `## MODIFIED`, `## REMOVED`.
- **Proposal style used in this repo:** `## Why` (motivation with measured
  evidence and `file:line` references), `## What Changes`, `## Capabilities`
  (new capabilities listed with one-line definitions), `## Impact`
  (Code / Dependencies / Constraints / Non-goals).

## 3. Verification Gates

CI (`.github/workflows/ci.yml`) runs across ubuntu/macos/windows. Reproduce it
locally before claiming anything is done:

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
OXMGR_RUN_E2E=1 cargo test --test e2e_cli    # e2e, Unix
openspec validate --all
```

**Done** = build passes + tests pass + lint clean + `openspec validate` clean.
Anything less is a partial result. Separate the generator from the evaluator:
run the suite yourself and cite the actual output; never self-review with
"it works". Coverage and resource-budget jobs are report-only (a map of what
is untested, not a number to game).

## 4. Development Principles

These are load-bearing conventions of this codebase — read them before
touching the daemon or its analytics:

- **Deterministic, evidence-based analytics.** Detectors and rules must be
  plain arithmetic (median/MAD z-score, EWMA, CUSUM, regression) — same inputs,
  same verdict, auditable by hand. No ML, no inference, no network calls in
  the decision path.
- **Efficiency is a hard constraint.** The daemon must stay lean: sample from
  existing collection paths (never a second pass), fixed-capacity rings for
  history retention, incremental O(1) updates, bounded per-cycle budgets.
- **Safety: decisions are recordable, rules are pure.** A decision to act
  (e.g. restart) must be recorded with its rule, findings, and withheld
  reasons. Rule ordering is part of the spec (refusals before acting rules).
- **No silent failures.** A resource figure that cannot be measured is
  reported as "unavailable", never as zero. Delete paths must be wired
  everywhere a resource is owned. Tests that pin a fix must be "verified to
  bite" — remove the fix and the test must fail.

## 5. Domain Skills

Load the matching skill **before** starting work, not after getting stuck. Each
carries the conventions, current-version facts, and review checklists for its
domain, so an agent does not re-derive them per session.

| Touching | Load | Covers |
|----------|------|--------|
| `.rs`, `Cargo.toml`, daemon, analytics | `rust-expert` | ownership, unsafe soundness, async/tokio, error design, perf, Cargo |
| `web/dashboard.css` | `css-expert` | Baseline status per feature, cascade layers, container queries, replacing JS with CSS |
| `web/dashboard.js` | `javascript-expert` | ES2026 features, platform APIs, DOM teardown, INP, dependency deletion |

Non-negotiables these skills add to this repo:

- **State the Baseline tier** for any web platform feature you introduce:
  Widely (safe), Newly (interoperable), Limited (needs `@supports`/feature test
  **and** a working fallback). Safari is the usual constraint in 2026.
- **Every `unsafe` block carries a `// SAFETY:`** naming the invariant and who
  upholds it. Every `unsafe fn` carries a rustdoc `# Safety` section.
- **Never `as` for narrowing an integer.** `as` truncates silently; use
  `try_from` and handle the error. This matters most in the decision path,
  where a truncated figure becomes a wrong verdict.
- **No blocking call inside an `async fn`.** `std::fs`, a long CPU loop, or a
  large parse stalls a runtime worker and every task sharing it. Use
  `tokio::fs` or `spawn_blocking`.
- **Every spawned task has a shutdown path.** A `tokio::spawn` whose handle is
  dropped and which nothing can stop is a leak that surfaces in production, not
  in tests.
- **No lock guard held across `.await`.**
- **Measure before claiming a performance change.** `criterion`/`divan` for
  Rust, a browser trace for the frontend. Two numbers, or it is an opinion.

For a code review, use the skill's review prompt
(`rust-expert/tools/rust-review-prompt.md` and the equivalents) rather than
reading the diff top to bottom.

## 6. Working with the OpenSpec State

Check where things stand before starting:

```bash
openspec status --change <name> --json   # artifact order + status per change
openspec list                            # task completion per change
openspec validate --all                  # spec/code coherence
./scripts/check-change-artifacts.sh      # checkbox format, capability names, decisions, TBD
```

Reading the state result is part of the contract: task checkboxes must
reflect reality, blocked work is marked "BLOCKED BY DESIGN" explicitly rather
than left silently unchecked, and a change marked Complete should be archived
on the next pass.