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

### 4. Run the fleet

```sh
summoner run orders/
```

Summoner validates all orders before dispatch, runs up to the configured concurrency, and prints the report as JSON.

### 5. Read the report

The same JSON is saved as `report.json` under `$XDG_CACHE_HOME/summoner/runs/<run-id>/`, or under the equivalent home-cache or temporary directory fallback. Every run also appends lifecycle events (`run_started`, `order_started`, `order_dispatched`, `order_exec_done`, `order_verify`, `order_finished`, `run_finished`) to `events.jsonl` in that directory; `summoner run --stream` mirrors them to stdout as NDJSON, ending with a single `report` event carrying the complete ranked report, so any consumer — an orchestrating session, an IDE, `tail -f` — can watch a fleet live. The `order_dispatched` event names the grove task, worktree, and log paths to follow.

Fleet control: `fail_fast = N` in `.summoner.toml` skips the remaining queue after N orders fail. An executor with a `usage_marker` (codex prints `tokens used`) gets its token count recorded per order and summed per run. `summoner resume <run-id>` re-runs an earlier fleet: orders that reached `verified` or `completed` carry over, and the rest dispatch again on their original branches, continuing from whatever grove salvaged. Orders are ranked worst-first, with ties sorted by ID. Review non-`verified` outcomes, log tails, diffs, conflicts, and verification receipts before accepting executor work. `summoner status` prints Summoner-owned grove tasks as JSON.

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
| `timeout_secs` | no | Per-order execution timeout. |
| `base` | no | Base passed when grove acquires the worktree. |
| `branch` | no | Branch passed when grove acquires the worktree. |
| `after` | no | Order ids that must reach `verified` or `completed` first; dependents of failed orders are skipped. Ordering only — an order that builds on a dependency's changes also sets `base = "grove/smn-<dep-id>"`. |

## Executor configuration

Executors are argv templates in `.summoner.toml`; Summoner contains no vendor-specific dispatch logic. One shipped preset is:

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
