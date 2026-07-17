---
name: summoner
description: Dispatch a fleet of executor agents (any model CLI) over grove-managed worktrees from work orders with tight acceptance criteria. Use when delegating parallel implementation work to fast models while this session orchestrates and reviews.
---

# Summoner: run an executor fleet

You are the orchestrator. Summoner is the deterministic dispatch layer under
you, owning the whole grove lifecycle for each order: worktree, scope claim,
durable task, verification, salvage. You write work orders. You review
outcomes. Everything between is Summoner's job.

## Workflow

1. **Decompose.** Split the plan into independent work orders, one file per
   task, in an `orders/` directory. TOML or JSON:

   ```toml
   id     = "auth-refactor"                  # [a-z0-9_-]+, becomes branch grove/smn-<id>
   title  = "Extract token validation into auth-core"
   brief  = """Full instructions for the executor."""
   scope  = ["crate:auth-core", "src/api/token.rs"]   # paths or crate:<name>
   acceptance     = ["grove verify fast passes", "no new public API"]
   verify_profile = "fast"                   # optional; grove profile to run
   executor       = "glm"                    # optional; else config default
   timeout_secs   = 900                      # optional
   ```

   Decompose against the real workspace, not intuition: `grove plan
   --topology` prints the package map (names, paths, dependency edges, and
   the claim scope owning each). Tight scope and concrete acceptance criteria
   are what keep fast models honest. Dependent work chains with
   `after = ["<id>"]` (one run executes the DAG; dependents of failures are
   skipped), and an order that builds on a dependency's changes also sets
   `base = "grove/smn-<dep-id>"`. For a hard or ambiguous order, set
   `variants = ["glm", "codex"]` instead of `executor`: each named executor
   attempts the order independently on its own branch (same scope, shared
   claim group), and you review the attempts and land the best one.

   Then refute your decomposition before spending worktrees on it:
   `summoner plan orders/` resolves every scope exactly as dispatch will and
   reports claim conflicts (those orders WILL block), package couplings,
   suggested execution waves, and any `after` edges the workspace demands
   that the orders do not declare. Revise until the verdict is `clean`.

2. **Preflight.** `summoner doctor` checks every configured executor binary,
   required environment variables, and the grove version. Fix what it flags
   before dispatching.

3. **Dispatch.** `summoner run orders/`. Orders run in parallel (config
   `max_parallel`), each in its own grove worktree and task. Mixing executors
   (GLM, Codex, Claude, anything configured) in one run is normal. For long
   fleets, `--stream` emits NDJSON lifecycle events as they happen (the
   `order_dispatched` event carries the log paths to tail) and ends with a
   `report` event instead of the pretty report.

4. **Review.** The report (stdout and `report.json` in the run directory) is
   ranked worst-first: `error`, `blocked`, `stalled`, `executor_failed`,
   `scope_violation`, `unverified`, `review_failed`, `rejected`,
   `interrupted`, `skipped`, `completed`, `verified`, `approved`.
   With `default_reviewer` configured (or per-order `reviewer`), verified
   work is judged by an independent backend — fresh context, diff and
   requirements only — and lands as `approved` or `rejected` with findings;
   deterministic `tripwires` (deleted tests, skip markers, verification-config
   edits) ride in each entry.
   Each order carries its branch, diff stats, verification receipts, acceptance
   criteria, and log tails. Review the diff on the order's branch against its
   acceptance criteria before landing anything. Re-dispatch failures with a
   revised order; do not hand-patch inside the executor's worktree.

## Rules

- Never mark delegated work done from an executor's output alone; receipts and
  your own diff review are the evidence.
- Do not run plain cargo in summoner worktrees; grove owns build isolation.
- Exit codes: 0 all verified, 1 review needed, 2 usage/infra error.
