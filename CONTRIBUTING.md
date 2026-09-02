# Contributing to Oxmgr

Thanks for contributing. Oxmgr targets Linux, macOS, and Windows, so changes should stay portable and easy to review.

## Development Setup

Prerequisites:

- Rust stable toolchain
- Git
- A local clone of this repository

Get started:

```bash
git clone https://github.com/Vladimir-Urik/OxMgr.git
cd OxMgr
cargo build
cargo run -- --help
```

If you want a release-style binary while developing:

```bash
cargo build --release
./target/release/oxmgr --help
```

## Project Layout

- `crates/oxmgr/`: binary crate — CLI parsing, command dispatch, UI, and the
  CLI-owned format modules (`oxfile.toml`, `.oxpkg` bundles)
- `crates/oxmgr-core/`: domain types and pure logic (zero I/O)
- `crates/oxmgr-store/`: event retention, content hashing, JS config extraction
- `crates/oxmgr-metrics/`: host/process/container metric collection
- `crates/oxmgr-analytics/`: detectors, baselines, failure-pattern analysis
- `crates/oxmgr-manager/`: process lifecycle, ecosystem import, log storage
- `crates/oxmgr-daemon/`: HTTP/SSE surface, IPC socket protocol, config loader
- Layering is enforced by `scripts/check-crate-layering.sh` (downward-only edges)
- `crates/oxmgr/tests/e2e_cli.rs`: end-to-end CLI coverage
- `docs/`: user-facing guides, CLI docs, oxfile docs, deployment notes
- `packaging/`: npm and Chocolatey packaging assets
- `scripts/`: packaging/release helper scripts

For a higher-level internal overview, see [docs/ARCHITECTURE.md](./docs/ARCHITECTURE.md).

## Daily Workflow

1. Create a focused branch for one change.
2. Make the smallest coherent change that solves the problem.
3. Add or update tests for behavior changes.
4. Update docs when flags, commands, config format, or workflows change.
5. Run the relevant checks before opening a PR — including the spec-side gates
   in `## Local Checks`, which this repo's OpenSpec workflow requires. See
   `AGENTS.md` §1 for the loop (explore → propose → apply → update → sync →
   archive); every change goes through it.

## Local Checks

These match the core CI workflow, which runs all of them on ubuntu, macOS and Windows:

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
openspec validate --all
./scripts/check-change-artifacts.sh
```

Clippy is not optional: CI passes `-D warnings`, so a warning fails the build. The
matrix uses `fail-fast: false` deliberately, so a failure on one platform still
reports results for the others — a Windows-only break should not hide a macOS one.

`openspec validate --all` and `scripts/check-change-artifacts.sh` are the spec-side
gates: the first checks spec/code coherence, the second checks the change artifacts
themselves (checkbox syntax, capability names, decision identifiers, placeholders,
task counts). The normative rules behind them live in
`openspec/specs/change-evidence-gates/spec.md` and
`openspec/specs/change-completion-integrity/spec.md`; this file only points at them.

## Resource Budgets

oxmgr's claim is that it is lightweight, and `docs/BENCHMARKS.md` records the measured
figures that back it: binary size, idle and loaded RSS, idle CPU. A `resource-budget`
job measures them on every change and compares against a per-platform baseline.

It is report-only for now, because CI-runner variance has not been characterised and a
gate tighter than the noise fails randomly and then gets disabled. Run it locally when
touching the log write path, the maintenance tick, or anything inlined into the binary:

```bash
python3 scripts/measure_resource_budget.py --out budget-report.json
python3 scripts/compare_resource_budget.py budget-report.json \
  bench/baselines/darwin-arm64.json
python3 scripts/check_memory_growth.py budget-report.json
```

## Release Profile

`Cargo.toml` tunes the release profile, and each setting is there for a reason:

| Setting | Why |
| --- | --- |
| `opt-level = "s"` | Optimise for size over raw speed. oxmgr spends its time waiting on processes and sockets, not in hot loops, so a smaller binary serves the single-binary goal better than marginal throughput. |
| `lto = true` | Link-time optimisation across crate boundaries, which both shrinks the binary and removes cross-crate call overhead. |
| `codegen-units = 1` | One unit gives the optimiser the whole picture. Slower to compile, smaller and faster to run. |
| `panic = "abort"` | No unwinding tables, so a smaller binary. A panicking supervisor should stop rather than half-unwind and continue in an unknown state. |
| `strip = true` | Drops debug symbols from the shipped artifact. |

If you change any of these, re-measure the resource budgets: `binary_bytes` is the
figure most likely to move, and the baseline should be updated deliberately with the
reason recorded in `docs/BENCHMARKS.md`.

## End-to-End Tests

The E2E suite is opt-in and skips by default unless `OXMGR_RUN_E2E=1` is set.

Unix shells:

```bash
OXMGR_RUN_E2E=1 cargo test --test e2e_cli -- --nocapture
```

PowerShell:

```powershell
$env:OXMGR_RUN_E2E = "1"
cargo test --test e2e_cli -- --nocapture --test-threads=1
```

Use the E2E suite when touching daemon behavior, lifecycle management, CLI flows, or cross-process interactions.

## Testing Expectations

- Parser or config changes should include focused tests near the affected module.
- CLI behavior changes should include integration coverage when practical.
- Daemon and process-management changes should prefer deterministic assertions over long sleeps.
- UI changes should update tests only where logic is covered; keep visual-only adjustments well explained in the PR.

## Documentation Expectations

Please update the relevant docs alongside code changes:

- `README.md` for user-visible behavior or project positioning changes
- `docs/CLI.md` for command or flag changes
- `docs/OXFILE.md` and `docs/examples/` for config-format changes
- `docs/UI.md`, `docs/PULL_WEBHOOK.md`, `docs/DEPLOY.md`, or other guides when workflows change

If behavior changes but docs stay untouched, explain why in the PR.

## Pull Requests

Open PRs with enough context for review:

- problem statement
- short design summary
- test evidence
- migration or backward-compatibility notes when applicable

Keep PRs focused. Avoid mixing unrelated refactors with behavior changes unless the refactor is required to make the change safe.

## Release Notes

Releases are tag-driven through GitHub Actions. For normal contributions, do not manually prepare a release or bump packaging versions just to ship a feature. If a change affects packaging or release automation, document that impact clearly in the PR and update the relevant files under `packaging/`, `scripts/`, or `docs/RELEASE.md`.
