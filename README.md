# Summoner

A Rust-aware fleet runner for coding-agent CLIs. You write work orders; Summoner
dispatches any configured model CLI into isolated Grove worktrees, verifies and
reviews each candidate, and hands back one ranked evidence report instead of a
pile of transcripts.

## Install

Summoner requires [Grove](https://github.com/thomgardiner/grove). Install both:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/thomgardiner/grove/releases/download/v0.4.0/grove-installer.sh | sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/thomgardiner/summoner/releases/latest/download/summoner-installer.sh | sh
```

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://github.com/thomgardiner/grove/releases/download/v0.4.0/grove-installer.ps1 | iex"
powershell -ExecutionPolicy ByPass -c "irm https://github.com/thomgardiner/summoner/releases/latest/download/summoner-installer.ps1 | iex"
```

Or from a checkout: `cargo install --locked --path .`. You also need git, a Rust
repo with Grove verification profiles, and at least one authenticated model CLI
([Codex](https://github.com/openai/codex#installing-and-running-codex-cli),
[Claude Code](https://code.claude.com/docs/en/installation),
[Kimi](https://www.kimi.com/code/docs/en/)).

## Five minutes

```sh
summoner init --preset codex --example   # or claude / kimi
summoner doctor orders/example.toml
summoner plan orders/example.toml
summoner run --stream orders/example.toml
```

`init` is idempotent and never overwrites your config. `run --stream` prints
lifecycle NDJSON ending in a `report` event; `summoner watch` in another
terminal shows the live board.

## Work orders

One independent task per TOML/JSON file:

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

`summoner check orders/` validates. `summoner plan orders/` resolves scopes the
way dispatch will and reports conflicts, couplings, and execution waves.

| Field | Required | Meaning |
| --- | --- | --- |
| `id`, `title`, `brief` | yes | Identifier (`[a-z0-9_-]+`), title, executor instructions. |
| `scope` | yes | Paths or `crate:<name>` claims. |
| `acceptance` | no | Criteria, included in prompt and report. |
| `verify_profile` | no | Grove profile to run before finishing. |
| `executor`, `reviewer` | no | Configured names; `reviewer = "none"` opts out. |
| `timeout_secs`, `base`, `branch` | no | Passed through to grove. |
| `after` | no | Order ids that must finish first. Supplies the base too: the dependent branches from its dependencies' verified commits (several are merged; conflicts skip the order). An explicit `base` overrides, but must contain every dependency's candidate. |
| `variants` | no | N-version dispatch: one sibling per executor, orchestrator picks the winner. |

## Executors

Argv templates, no vendor code. Presets install only via
`summoner init --global --preset <name>`; custom executors live in your
personal config:

```toml
default_executor = "agent"

[executors.agent]
argv = ["agent-cli", "run", "--worktree", "{worktree}", "--", "{prompt}"]
prompt = "arg"
timeout_secs = 900
```

Placeholders: `{prompt}`, `{prompt_file}`, `{worktree}`, `{git_common_dir}`,
`{order_file}`. Never disable a CLI's own sandbox to make a config work.

## After a run

Summoner never merges. Take the branch and task id from the report:

```sh
git diff <base>...grove/smn-<order-id>
grove task status --json <task-id>
```

Review, then merge with your normal process. Non-green outcomes: `rejected`
(reviewer findings), `unverified`, `scope_violation`, `skipped`, and friends,
ranked worst-first.

Exit codes: 0 all verified, 1 domain outcome needs review, 2 usage or
infrastructure error.

## More

Revision loops, budgets, resume, the review-gate protocol, and profile
inheritance: [docs/reference.md](docs/reference.md). Boundary: Grove owns
worktrees, claims, lanes, and receipts; Summoner owns dispatch, review, and
reports.
