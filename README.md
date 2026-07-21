# Summoner

Summoner is a Rust-aware fleet runner for coding-agent CLIs. Codex, Claude Code, and Kimi can already edit code in worktrees; Summoner and Grove add the repository control plane around them: Cargo-aware scope planning, isolated warm build lanes, repository-owned verification receipts, and an independent reviewer bound to the exact candidate digest. The invoking harness writes work orders, Summoner dispatches any configured model CLI, and the result is one ranked evidence report instead of a pile of agent transcripts.

## Install

Summoner requires Grove. For published current releases, install both binaries:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/thomgardiner/grove/releases/download/v0.3.4/grove-installer.sh | sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/thomgardiner/summoner/releases/latest/download/summoner-installer.sh | sh
```

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://github.com/thomgardiner/grove/releases/download/v0.3.4/grove-installer.ps1 | iex"
powershell -ExecutionPolicy ByPass -c "irm https://github.com/thomgardiner/summoner/releases/latest/download/summoner-installer.ps1 | iex"
```

Building from a checkout works too: `cargo install --locked --path .`. Summoner requires the exact release-qualified Grove 0.3.4, Git, a Rust repository with real Grove verification profiles, and one installed/authenticated model CLI:

- [Codex install](https://github.com/openai/codex#installing-and-running-codex-cli) and [authentication](https://help.openai.com/en/articles/11381614-api-codex-cli-and-sign-in-with-chatgpt)
- [Claude Code install](https://code.claude.com/docs/en/installation) and [authentication](https://code.claude.com/docs/en/authentication)
- [Kimi Code install and login](https://www.kimi.com/code/docs/en/)

## Five-minute setup

Choose explicitly; Summoner never guesses a model and never stores credentials:

```sh
summoner init --preset codex --example  # or claude / kimi
summoner doctor orders/example.toml
summoner plan orders/example.toml
summoner run --stream orders/example.toml
```

The final command prints lifecycle NDJSON as each Rust-aware order is claimed, built in its own Grove lane, verified, and reviewed. While it runs, open another terminal in the repository and run `summoner watch` for the live fleet board.

The first command installs the explicitly selected writable worker and separately named read-only/plan reviewer, initializes the repository contract, and writes the example order. It prints the exact `doctor`, `plan`, and `run` commands to use next. Existing global comments and unrelated settings survive; same-named custom executors are never overwritten. Codex and Claude auth checks are noninteractive and bounded to five seconds. Kimi currently exposes a config check but no reliable noninteractive auth-status check. Choosing the Kimi preset explicitly persists that acknowledgement for its two generated roles; custom unknown-auth backends fail closed until their exact names are listed in `allow_unknown_auth` in the personal global config, or a single invocation uses `--allow-unknown-auth`. Repository config cannot grant this acknowledgement.

`doctor`, `run`, and `resume` share the same preflight: the exact Grove 0.3.4 machine-capability contract (task schemas, inspection schemas, process-tree supervision, read-only digest sealing, and captured logs), Git repository and author identity, verification-profile existence, executable/environment checks, and bounded model lifecycle diagnostics. An unreadable or malformed existing config is an error, never an ignored fallback.

The older `summoner init --global --preset <name>` and `summoner init --example` forms remain available. In a Rust workspace without `.grove.toml`, `--example` creates a real required `rust-check` profile that runs `cargo check --workspace --all-targets --locked` through Grove. If the repository already owns `.grove.toml`, the example pins its one usable profile only when that profile is explicitly listed in `verification.required`, and never edits the file; a missing or ambiguous required profile is an actionable error, not a false-green demo.

Normal `summoner init` remains idempotent: it creates `.summoner.toml`, appends the managed `AGENTS.md` contract, and installs `.claude/skills/summoner/SKILL.md` without replacing user-owned content. `summoner config` shows the resolved settings and sources.

## Usage

### Write a work order

Put one independent task in each TOML or JSON file. For example, `orders/readme.toml`:

```toml
id = "readme"
title = "Write the README"
brief = "Document installation, configuration, and the fleet workflow."
scope = ["README.md"]
acceptance = ["README.md is complete", "documentation checks pass"]
verify_profile = "fast"
executor = "agent"
timeout_secs = 900
```

Order IDs must match `[a-z0-9_-]+`. Scope entries are passed to grove and may be repository paths or `crate:<name>` claims. A directory input contributes its immediate `.toml` and `.json` files in filename order. Validate without dispatching with:

```sh
summoner check orders/
```

Before dispatching a batch, analyze it: `summoner plan orders/` resolves every scope exactly as dispatch will and reports claim conflicts, package couplings from the workspace dependency graph, and suggested execution waves (`grove plan --topology` prints the package map to decompose against in the first place). Package couplings are advisory because file-disjoint orders run in isolated worktrees and build lanes. An overlapping scope requires an `after` edge; an overlap already ordered by the declared DAG is clean. Exit 0 means the batch is dispatchable as written.

### Run the fleet

```sh
summoner run --stream orders/
```

Summoner validates all orders before dispatch, runs up to the configured concurrency, and streams lifecycle NDJSON ending in a `report` event. Use `summoner run orders/` when a plain final JSON report is more convenient than live events.

### Read the report

The same JSON is saved as `report.json` under `$XDG_CACHE_HOME/summoner/runs/<run-id>/`, or under the equivalent home-cache or temporary directory fallback. Every run also appends lifecycle events (`run_started`, `order_started`, `order_dispatched`, `order_exec_done`, `order_verify`, `review_started`, `order_review`, `order_checkpoint`, `order_finished`, `run_finished`) to `events.jsonl` in that directory; `summoner run --stream` mirrors them to stdout as NDJSON, ending with a single `report` event carrying the complete ranked report, so any consumer — an orchestrating session, an IDE, `tail -f` — can watch a fleet live. The `order_dispatched` event names the grove task, worktree, and log paths to follow, and `review_started` names the reviewer's logs so the gate is tailable the moment it spawns.

### Durable runs and recovery

Each run directory is a versioned evidence bundle: `manifest.json` and `report.json` are create-once snapshots, while `events.jsonl` is append-only. The manifest records the exact expanded orders, effective non-secret settings, resolved executor/reviewer roles, selected backend definitions, canonical executable paths, binary SHA-256 digests, and bounded `--version` exit/timeout evidence before dispatch; diagnostic output is discarded to keep the probe resource-bounded. Required environment-variable names are recorded; credential values are not. Journal records are sequenced and flushed before their streamed copies, and a journal failure stops further dispatch. `order_checkpoint` preserves the full gate result before worktree cleanup. `report.json` is created only after `run_finished` and is projected from terminal journal records, so a hard-killed run may correctly have no report.

`summoner resume <run-id>` reads the run-owned manifest and journal, not the original order files or current executor defaults. It refuses replay when an executor or resume binary resolves to a different path or SHA-256 digest; intentional upgrades start a new run and therefore a new evidence bundle. It reconciles every recorded task with Grove's durable status. Only `verified` and `approved` results with matching finished Grove verification are carried, and an approval additionally requires Grove's persisted source digest to equal the review's candidate snapshot digest. Every non-green result, including `completed`, is rerun on its recorded branch. Recorded executor sessions are reused when the backend defines `resume_argv`. A nonterminal Grove task (`active`, `idle`, `stalled`, or `failed`) blocks resume with a retry-later error so Summoner never duplicates its claim or execution. Resolve or explicitly abandon that task, then retry the same resume command.

`summoner watch` renders a live terminal board over the latest run's events (or `summoner watch <run-id>`): one row per order with phase, attempt, branch, elapsed time, and token usage, exiting when the run finishes. Finished rows carry attach handles: the branch holds the work and the session id (when captured) resumes the executor's context.

`summoner scorecard` aggregates every past run into per-repository, per-executor stats — orders, green count (`verified`/`approved`), attempts, tokens, and an outcome histogram (`--repo <substring>` filters). Each run appends its outcomes to a machine-wide `scorecard.jsonl` in the runs directory, so "this backend keeps failing scope in that repo" is a number the orchestrator reads before picking executors, not a hunch.

## Revision loop and budgets

`revise = N` (or `SUMMONER_REVISE`) turns rejections into a bounded feedback loop: an order that lands `rejected` or `unverified` re-dispatches up to N extra times on the same branch, with the failure evidence (the reviewer's findings, or the verification failure) injected into the prompt. When the executor's backend defines `session_marker` (a substring before its printed session identifier) and `resume_argv` (an argv template with `{session_id}`), the revision resumes the executor's own session, so only the evidence travels; without them, revisions run with a fresh context and the full charter. Attempt counts and the captured `session_id` appear in the report, so the orchestrator can also resume a session manually after the run.

Budgets are enforced two ways. `run_token_budget = N` (or `SUMMONER_RUN_TOKEN_BUDGET`) is a run-wide breaker over live spend: usage counts against it the moment it is scraped from any attempt or review on any worker, the remaining queue lands as `skipped` once crossed, and the revision loop stops revising. A per-order `max_tokens` blocks revisions once reached and calls the overage out in its report entry; an order can only set it when its executor defines a `usage_marker`, or the cap could never be measured (validation refuses the combination). Usage is only knowable after an executor exits, so one in-flight attempt can overshoot before the breaker sees it; the grove deadline remains the hard stop for a runaway process.

Fleet control: `fail_fast = N` in `.summoner.toml` skips the remaining queue after N orders fail. An executor with a `usage_marker` gets its token count recorded per order and summed per run. Orders are ranked worst-first, with ties sorted by ID. Review non-green outcomes, log tails, diffs, conflicts, and verification receipts before accepting executor work. `summoner status` prints Summoner-owned grove tasks as JSON.

### Human handoff

Summoner deliberately does not merge agent branches. Take the `branch` and Grove task id from the final report, then inspect the candidate and its durable receipt before deciding whether to land it:

```sh
git log --oneline <base>..grove/smn-<order-id>
git diff --stat <base>...grove/smn-<order-id>
git diff --check <base>...grove/smn-<order-id>
grove task status --json <task-id>
```

The task status must show the terminal task state and its recorded verification. Review the full diff and report findings; only then use your repository's normal merge or cherry-pick process. The branch is the work handoff, the Grove receipt is the verification handoff, and neither substitutes for human acceptance.

## Review gate

Set `default_reviewer = "<executor name>"` (or per-order `reviewer`; `reviewer = "none"` opts an order out) and every order that verifies is judged by an independently configured reviewer before it counts as green. Grove captures the still-live task into a standalone leased inspection capsule with no origin, shared Git metadata, or write-capable build lane; the reviewer runs only there under its configured native read-only/plan sandbox. The prompt contains the charter, requirements, verification evidence, live candidate diff, a random nonce, and exact snapshot/diff SHA-256 digests—never the implementing executor's transcript. The reviewer must return one strict protocol-v1 JSON object with those exact bindings; unknown fields, injected prose, stale/replayed bindings, oversized findings, process-tree leaks, source changes, or capsule changes void approval. Raw logs and their hashes remain in the run evidence. Approve upgrades `verified` to `approved`; reject lands as `rejected` with typed findings.

The capsule is defense in depth, not a universal same-user OS sandbox: Grove 0.3.4 enforces read-only permissions plus before/after digests and uses a Windows Job Object or a best-effort Unix process group. A same-user process may be able to change permissions elsewhere on the host, so retain each vendor CLI's native sandbox and do not grant reviewer argv `{git_common_dir}`. File-routed reviewer prompts are rejected because the prompt lives outside the sealed capsule; shipped presets use argument or stdin routing.

## Orchestrator profiles

Profiles are named overlays for executor and reviewer defaults. They let different invoking environments select different policies without putting a vendor into Summoner's dispatch logic:

```toml
[profiles.interactive]
default_executor = "implement"
default_reviewer = "review"

[profiles.automation]
default_executor = "batch"
default_reviewer = "audit"
```

A profile only overrides `default_executor` and `default_reviewer`; executors stay shared. Inheritance is layered and field-level: global config is the base, `[profiles.<name>]` overlays it, and repository config overrides only the fields it names. An explicit empty string clears an inherited marker. Selection, highest first: `--profile <name>`, `SUMMONER_PROFILE`, then a `profile = "<name>"` config pin. As opt-in conveniences, harness markers select profiles named `claude` or `codex` only when those profiles exist; if both markers are present, selection is left explicit. Naming an absent profile is an error. `summoner config` lists the applied profile in `sources`.

Before review, Summoner scans the diff and reports deterministic `tripwires` per order — deleted test files, added skip markers (`#[ignore]`, `.skip(`), net assertion loss, Cargo `[profile]` edits. These are anomaly indicators that raise review scrutiny; they cannot prove that tests remain semantically strong or prevent reward hacking. Touching verification config itself (`.grove.toml`, `.summoner.toml`, `rust-toolchain*`, `.cargo/config*`) is a separate hard stop: the receipts a modified config produces are untrustworthy, so the order lands `unverified` and its task is abandoned, whatever the tests said.

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

Executors remain argv templates; the dispatch engine contains no vendor branches. Versioned Codex, Claude, and Kimi recipes live in one embedded data catalog and are installed only after an explicit `summoner init --global --preset <name>`. Custom executors belong in the platform-native personal config (XDG on Unix, `%APPDATA%\summoner\config.toml` on Windows); a repo's `.summoner.toml` overrides same-named executors. An example:

```toml
default_executor = "agent"

[executors.agent]
argv = [
  "agent-cli", "run", "--worktree", "{worktree}", "--", "{prompt}",
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
