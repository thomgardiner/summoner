# Summoner

Summoner is a fleet runner for LLM coding agents. The invoking session—Claude Code, Codex, or another harness—acts as the orchestrator: it writes work orders, and Summoner deterministically dispatches executor CLIs in grove-managed worktrees and returns one ranked JSON report.

## Install

From this repository:

```sh
cargo install --path .
```

Summoner also requires the `grove` binary and at least one configured executor CLI.

## Usage

### 1. Initialize a repository

Run this at the repository root:

```sh
summoner init
```

This creates `.summoner.toml`, adds the orchestration contract to `AGENTS.md`, and installs the Claude skill at `.claude/skills/summoner/SKILL.md`. Existing files are skipped or appended to rather than replaced. Inspect the resolved configuration and its source files with:

```sh
summoner config
```

### 2. Check the environment

```sh
summoner doctor
```

The JSON result checks the grove binary, the default executor, each configured executor binary, and required environment variables.

### 3. Write a work order

Put one independent task in each TOML or JSON file. For example, `orders/readme.toml`:

```toml
id = "readme"
title = "Write the README"
brief = "Document installation, configuration, and the fleet workflow."
scope = ["README.md"]
acceptance = ["README.md is complete", "documentation checks pass"]
verify_profile = "fast"
executor = "codex"
timeout_secs = 900
```

Order IDs must match `[a-z0-9_-]+`. Scope entries are passed to grove and may be repository paths or `crate:<name>` claims. A directory input contributes its immediate `.toml` and `.json` files in filename order. Validate without dispatching with:

```sh
summoner check orders/
```

Before dispatching a batch, analyze it: `summoner plan orders/` resolves every scope exactly as dispatch will and reports claim conflicts, package couplings from the workspace dependency graph, suggested execution waves, and any `after` edges the orders should declare but do not (`grove plan --topology` prints the package map to decompose against in the first place). Exit 0 means the batch is clean as written.

### 4. Run the fleet

```sh
summoner run orders/
```

Summoner validates all orders before dispatch, runs up to the configured concurrency, and prints the report as JSON.

### 5. Read the report

The same JSON is saved as `report.json` under `$XDG_CACHE_HOME/summoner/runs/<run-id>/`, or under the equivalent home-cache or temporary directory fallback. Every run also appends lifecycle events (`run_started`, `order_started`, `order_dispatched`, `order_exec_done`, `order_verify`, `order_finished`, `run_finished`) to `events.jsonl` in that directory; `summoner run --stream` mirrors them to stdout as NDJSON, ending with a single `report` event carrying the complete ranked report, so any consumer — an orchestrating session, an IDE, `tail -f` — can watch a fleet live. The `order_dispatched` event names the grove task, worktree, and log paths to follow.

Fleet control: `fail_fast = N` in `.summoner.toml` skips the remaining queue after N orders fail. An executor with a `usage_marker` (codex prints `tokens used`) gets its token count recorded per order and summed per run. `summoner resume <run-id>` re-runs an earlier fleet: orders that reached `verified`, `approved`, or `completed` carry over, and the rest dispatch again on their original branches, continuing from whatever grove salvaged. Orders are ranked worst-first, with ties sorted by ID. Review non-green outcomes, log tails, diffs, conflicts, and verification receipts before accepting executor work. `summoner status` prints Summoner-owned grove tasks as JSON.

## Review gate

Set `default_reviewer = "<executor name>"` (or per-order `reviewer`; `reviewer = "none"` opts an order out) and every order that verifies is judged by an independent reviewer before it counts as green. The reviewer is any configured executor, spawned fresh in the order's worktree under the same grove supervision, prompted with the review charter, the order's brief and acceptance criteria, and the diff — deliberately never the implementing executor's transcript, and ideally a different vendor than the implementer (summoner warns when they match). Its last output line must be `{"verdict":"approve"|"reject","findings":[...]}`. Approve upgrades `verified` to `approved`; reject lands the order as `rejected` with the findings in the report (the work stays finished and salvaged on its branch for re-dispatch). A reviewer that modifies the worktree has its writes undone and its verdict voided (`review_failed`). Configure reviewer CLIs read-only (tool allowlists, read-only sandbox modes); the gate enforces it after the fact.

Anti-reward-hacking runs before the reviewer does: summoner scans the diff deterministically and reports `tripwires` per order — deleted test files, added skip markers (`#[ignore]`, `.skip(`), net assertion loss, Cargo `[profile]` edits. Touching verification config itself (`.grove.toml`, `.summoner.toml`, `rust-toolchain*`, `.cargo/config*`) is a hard stop: the receipts a modified config produces are untrustworthy, so the order lands `unverified` and its task is abandoned, whatever the tests said.

## Work-order fields

| Field | Required | Meaning |
| --- | --- | --- |
| `id` | yes | Unique non-empty identifier matching `[a-z0-9_-]+`. |
| `title` | yes | Non-empty task title. |
| `brief` | yes | Non-empty executor instructions. |
| `scope` | yes | Non-empty list of non-empty path or `crate:<name>` claims. |
| `acceptance` | no | List of acceptance criteria included in the executor prompt and report. |
| `verify_profile` | no | Grove verification profile to run before finishing. |
| `executor` | no | Configured executor name; otherwise uses `default_executor`. |
| `reviewer` | no | Executor name that judges this order after verification (see Review gate); overrides `default_reviewer`. `"none"` opts out. |
| `timeout_secs` | no | Per-order execution timeout. |
| `base` | no | Base passed when grove acquires the worktree. |
| `branch` | no | Branch passed when grove acquires the worktree. |
| `after` | no | Order ids that must reach `verified` or `completed` first; dependents of failed orders are skipped. Ordering only — an order that builds on a dependency's changes also sets `base = "grove/smn-<dep-id>"`. |
| `variants` | no | N-version dispatch: executor names that each attempt the order independently. Expands into one sibling per executor (`<id>-<executor>`), all sharing a grove claim group so the identical scope does not conflict; each attempt lands on its own branch and carries `variant_of` in the report, and the orchestrator reviews and lands one winner. Mutually exclusive with `executor`. |

## Executor configuration

Executors are argv templates; Summoner contains no vendor-specific dispatch logic and ships no presets — which agent CLIs you run, under which flags and accounts, is personal configuration. Define executors once in `~/.config/summoner/config.toml` (`summoner init --global` drops an annotated template there); a repo's `.summoner.toml` overrides same-named executors. An example:

```toml
default_executor = "codex"

[executors.codex]
argv = [
  "codex", "exec", "-s", "workspace-write", "-C", "{worktree}",
  "-c", "sandbox_workspace_write.writable_roots=[\"{git_common_dir}\"]",
  "--", "{prompt}",
]
prompt = "arg"
timeout_secs = 900
```

Never disable an executor CLI's own permission or sandbox system to make a configuration work; prefer explicit allowlists scoped to the worktree plus `{git_common_dir}`.

An executor supports the fields `argv`, `prompt`, `timeout_secs`, and `env_required`. Prompt routing is `arg`, `stdin`, or `file`. Templates may use:

- `{prompt}`: composed worker charter and work order, for `arg` routing.
- `{prompt_file}`: path to that prompt in the run directory, for `file` routing.
- `{worktree}`: absolute grove worktree path.
- `{git_common_dir}`: shared Git directory, useful as a writable sandbox root.
- `{order_file}`: absolute source order path.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success; for `run`, every order is verified. |
| `1` | A domain outcome needs review, including any non-verified run result or an unhealthy `doctor` report. |
| `2` | Usage, validation, configuration, or infrastructure error. |

## Summoner and grove

Grove owns worktrees, scope claims, verification receipts, and the execution deadline. Summoner owns work orders, executor dispatch, lifecycle orchestration, and the ranked report. This keeps agent choice data-driven while grove remains the authority for isolation and verified completion.
