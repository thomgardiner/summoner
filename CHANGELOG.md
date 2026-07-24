# Changelog

All notable changes to Summoner are documented here. Summoner follows semantic
versioning.

## Unreleased

## 0.3.0 — 2026-07-24

### Added

- Setup wizard with **no default model**: `summoner setup` lists recipes and
  PATH status; choose **this session** or **permanent** config. Session-only
  via `--session` / `SUMMONER_SESSION_CONFIG`; clear with `--clear-session`.
- Policy authentication: legacy MAC and **ed25519** (`ed25519:` signatures,
  `SUMMONER_POLICY_PUBKEY`). CLI: `summoner policy keygen|sign|verify`.
- Integration land seals immutable candidate `I`, optional
  `SUMMONER_LAND_CRUCIBLE` and `SUMMONER_LAND_REVIEW` gates against `I`,
  retains refs only after gates pass, FF specifically to `I`.
- `assurance_envelope.json` on each run; Grove `candidate capture` identity
  on grove-host finish when available.
- `summoner impact` baseline deltas for delivery economics (descriptive only).
- Host conformance tests for land seal and gate refusal.

### Fixed

- Doctor / installer / skill first-run text no longer push a Codex-only default.
- Git host dirty-worktree refuse and honest land aggregate (carried from 0.2.x
  tip work).
- A present policy signature that fails verification always refuses dispatch
  (not only when `require_signature = true`).
- `summoner land` picks the latest finished run by `report.json` mtime, not
  lexicographic path order.
- Successful land rewrites `assurance_envelope.json` with the sealed integration
  candidate `I`.
- Resume re-checks live `allowed_executors` / `allowed_reviewers` (not only
  revocations).
- Kimi preset argv no longer pairs `--prompt` with `--auto`/`--plan` (kimi-code
  ≥0.28 rejects those combinations).

## 0.2.0 — 2026-07-23

Requires Grove 0.4.0 (task record schema 6, task-status schema 4, the `edit`
exec capability, and pinned verification policy).

### Added

- Host capability surface for exact-state guarantees
  (`scope_includes_committed_delta`, `verification_bound_to_source`,
  `immutable_inspection_snapshot`, `review_process_isolated`,
  `finish_source_compare_and_swap`). Trusted policy can pin `required_host`
  and `required_capabilities`; held review requires a host that can isolate
  the candidate (git host now uses a detached private worktree capsule).
- Git host records `start_commit` at task begin, scopes against committed
  deltas since begin (not only dirty tree), binds verification to HEAD, and
  enforces finish-time source compare-and-swap. Review runs in a detached
  worktree so the reviewer cannot mutate the live executor tree.
- Executor `identity` (provider/model label) and trusted policy
  `distinct_reviewer_identity` so two aliases of the same model cannot
  satisfy independence.
- Trusted policy `required_profiles`: every listed profile runs for each order
  (mandatory multi-profile), in addition to the one-of `allowed_profiles`.
- `summoner land` merges candidates onto a temporary integration branch, runs
  an optional aggregate verify (`SUMMONER_LAND_VERIFY` or `cargo test` when a
  Cargo.toml exists), and only then fast-forwards the protected target. Partial
  integration never advances the target on conflict.
- Built-in tripwire protection for `.crucible` and `Cargo.lock`; trusted
  policies also protect `checks/`, `hooks/`, and `.github/workflows/`.
- `summoner overview` prints one pane across every fleet and Grove repo on the
  machine: each summoner run's repo and order tally (active first), and each
  Grove repo's recent coordination activity by category, folded from the same
  best-effort NDJSON journals `watch` reads. `--watch` redraws it live. No more
  visiting a dozen repos to see what is running.
- `summoner land [run-id]` integrates a finished run's verified candidate
  commits (see above). `--dry-run` prints the plan.
- `[notify] command = [...]` runs when a run finishes, an order lands non-green,
  or a review starts, so you can leave a fleet unattended. The command gets the
  event's JSON line on stdin and `SUMMONER_NOTIFY_TITLE`/`_BODY`/`_EVENT` in the
  environment — one seam for an OS notifier (`osascript`, `notify-send`) or a
  webhook (`curl` reading stdin). Best-effort and time-bounded; the run journal
  stays authoritative.

### Changed

- Pin the release-qualified compatibility contract to Grove 0.4.0. Grove 0.4.0
  reports task-status schema 4 (it adds live `outside_scope` drift), so the
  0.3.5 contract's preflight and CI capability checks rejected it. Grove's
  breaking `--agent` change (no shared default identity) needs no code change
  here: Summoner already passes an explicit `--agent` on every `task begin` and
  `worktree acquire`.

- `after` now supplies the base as well as the ordering: a dependent branches
  from its dependencies' verified candidate commits, merged when there are
  several. Conflicting dependencies skip the dependent with the paths named.
  An explicit `base` still overrides, but is refused when it does not contain a
  dependency's candidate. Previously `after` only ordered dispatch and building
  on a dependency required hand-writing `base = "grove/smn-<dep-id>"`.
- Executors launch under `grove task exec --capability edit`, so a fleet is no
  longer throttled to `max_builders` live sessions.
- An order that finishes with uncommitted work records no `candidate_commit`:
  HEAD does not identify a dirty candidate, and dependents refuse to build on a
  dependency without an immutable candidate rather than silently missing work.
- Resume no longer pins the run's start commit as the base of an `after`
  order, which silently disabled dependency inheritance on replay.

### Added

- Profiles self-register through `detect_env`: any harness that exports an
  identifying environment variable selects its profile automatically, with no
  vendor list compiled into summoner. Ambiguous matches select nothing.
- A review verdict wrapped in one markdown fence parses; several chat-first
  CLIs fence JSON regardless of instruction, and the fence carries no
  authority. Anything else around the object still fails closed.
- A configured `usage_marker` that never matches an attempt's output is now
  reported instead of silently tracking nothing.
- The Claude Code skill file is written only where Claude Code is already in
  evidence (`.claude/` or `CLAUDE.md` present), so onboarding other harnesses
  leaves no vendor residue.
- Optional `[trusted_policy]` in the operator's global config: required
  reviewer, reviewer distinct from executor by name, allowed executors,
  reviewers, and profiles, protected paths, and whether unverified `completed`
  work satisfies dependency edges. Content-addressed into the manifest, review
  prompt, and report; a resumed run is gated by the recorded policy.
- `candidate_commit` in each order report: the exact commit reviewed, captured
  before worktree release can salvage and advance the branch.

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
