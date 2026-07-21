# Changelog

## 0.1.0 — 2026-07-21

### Added

- Initial scaffold: configuration via `.summoner.toml`, work orders, and the `init` command that installs the config, the `AGENTS.md` orchestration contract, and the Claude skill. Existing files are skipped or appended to rather than replaced.
- Fleet dispatch over grove (release-qualified compatibility contract: grove 0.3.4) — `run`, `doctor`, `status`. Each order gets an isolated grove worktree with a scope-claimed task, runs the configured executor CLI under `grove task exec --timeout-secs`, drives finish-driven verification (runs exactly the profiles a structured refusal names), and emits one ranked JSON report saved as `report.json` under the run directory.
- `{git_common_dir}` placeholder so sandboxed executors can commit from a linked worktree whose index and locks live under the main repository's `.git/worktrees/`.
- `after` field: one run executes a dependency DAG of orders. A ready queue dispatches an order once every dependency reached `verified` or `completed`; dependents of failed orders are reported `skipped` with the dependency's outcome named; unknown references, self-references, and cycles are rejected at validation.
- `--stream`: lifecycle events are appended to the authoritative, flushed `events.jsonl` and mirrored to stdout as NDJSON, ending with a single-line `report` event. A full `order_checkpoint` preserves the gate result before cleanup, and `report.json` is projected from terminal journal records.
- Immutable run manifests record exact expanded orders, effective non-secret settings, resolved executor/reviewer roles, and selected backend definitions before dispatch; replay materializes run-owned order snapshots and ignores later source-order/default changes.
- Crash-safe `summoner resume <run-id>` reconciles the immutable manifest and journal with Grove's durable task verification. Only matching `verified`/`approved` work carries; other outcomes resume their recorded branch and executor session, while a nonterminal Grove task blocks duplicate dispatch.
- Swarm control: `fail_fast = N` skips the remaining queue after N executor failures (blocked, interrupted, and skipped outcomes do not trip the breaker); executors with a `usage_marker` record per-order and summed per-run token counts.
- README documenting installation, configuration, the work-order schema, executor templates, and exit codes.
- Binary archives, SHA-256 checksums, shell and PowerShell installers, and a standalone updater
  generated for GitHub Releases without requiring users to install Rust.

### Fixed

- Planner parallelism: package/dependency couplings remain visible as advisory topology instead of becoming mandatory `after` edges, and overlapping scopes already serialized by the declared DAG no longer warn or make `plan` reject the batch.
- Adversarial cross-backend review: a failed worktree release now downgrades the outcome to `error` so the run cannot exit 0 nor schedule dependents on a leaked worktree.
- Process hygiene from the same review: the stdin writer thread is no longer joined (rogue descendants could hang a worker); interrupts are observed between verification phases; a backup kill also SIGKILLs the executor's recorded process group so a wedged grove cannot leave a paid model running; worker lock poisoning no longer cascades.
- Validation and reporting from the same review: timeouts are range-validated (1..=604800) with saturating backup arithmetic; `{order_file}` is canonicalized before executors resolve it; `{prompt}` is substituted last so placeholder-shaped text in a brief arrives verbatim; log tails seek instead of loading whole files; order-directory read errors surface; grove domain outcomes are accepted only on exit 0 or 1.
- Doctor from the same review: accepts a setup with no default executor, requires the executable bit on each executor binary, and ignores an empty `XDG_CACHE_HOME`.
- Outcome reporting: a finish refusal without verification detail reports `unverified` rather than being promoted to `completed`.
- Hardening round: the SIGTERM teardown path gets an automated fleet test (partial report, interrupted outcome, abandoned task, released worktree) instead of manual-only coverage; `doctor` now requires a git identity in the repo because the charter tells executors to commit; the `git-common-dir` lookup drops `--path-format=absolute` (git >= 2.31 only) and absolutizes the plain answer instead, so older git works.

### Verification

- Release tip: 159/159 Nextest tests, strict workspace/all-target Clippy, and format check passed against the release-qualified Grove 0.3.4 contract; CI fleet lifecycle jobs green on Ubuntu, macOS, and Windows.
