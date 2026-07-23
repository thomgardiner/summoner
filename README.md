# summoner

A host-pluggable fleet runner for coding-agent CLIs. You write work orders;
Summoner dispatches any configured model CLI into isolated worktrees, verifies
and reviews each candidate, and hands back one ranked evidence report.

**Hosts:** isolation is a plugin. Choose the host that matches the assurance
you need — they are **not** equivalent.

| | **Git host** | **Grove host** |
| --- | --- | --- |
| Isolation | git worktrees + local ledger | CoW worktrees + claims + governor |
| Exact-state | **Clean committed** candidates only (dirty tree refuses verify/finish); detached review worktree | Workspace snapshot digests, inspection capsules, finish source CAS |
| Verification | Profiles in Summoner `[verification]` | Profiles in `.grove.toml`, receipt-bound |
| Default when | No `.grove.toml` / no `grove` on PATH (Unix) | Explicit `[host] kind = "grove"` or `.grove.toml` + `grove` |
| High-assurance fleets | OK if clean commits + trusted policy | **Preferred** for multi-agent Rust monorepos |

```toml
[host]
kind = "grove"   # high-assurance (recommended for verified fleets)
# kind = "git"   # independence; requires clean committed candidates
# bin = "grove"
```

Resolution: explicit `[host] kind` → legacy `grove_bin` → `.grove.toml` plus
`grove` on `PATH` → else `git` (with a stderr notice that guarantees are weaker).

## Install

macOS or Linux:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/thomgardiner/summoner/releases/latest/download/summoner-installer.sh | sh
summoner setup --preset codex    # harness skills (/summoner) + executor recipe
```

Windows PowerShell:

```powershell
$ErrorActionPreference = "Stop"
irm https://github.com/thomgardiner/summoner/releases/latest/download/summoner-installer.ps1 | iex
summoner setup --preset codex
```

Optional Grove (Rust monorepos, warm lanes) — install both if you want fleets
**and** CoW lanes:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/thomgardiner/grove/releases/latest/download/grove-installer.sh | sh
grove setup
```

Use either product alone: Summoner with `[host] kind = "git"` needs no Grove;
Grove never launches models and never depends on Summoner.

The installers verify release checksums and also install `summoner-update` (and
`grove-update` when you install Grove). Source install:
`cargo install --git https://github.com/thomgardiner/summoner --locked`.
You need git and at least one authenticated model CLI
([Codex](https://github.com/openai/codex#installing-and-running-codex-cli),
[Claude Code](https://code.claude.com/docs/en/installation),
[Kimi](https://www.kimi.com/code/docs/en/)).

### First-run setup (skills + recipe)

```sh
summoner setup --preset codex    # or claude / kimi
# In a project you want fleets in:
summoner setup --preset codex --repo
```

`setup` installs a **user-level skill** so harnesses can invoke Summoner without
copy-pasting docs:

| Harness | Path | Invoke |
| --- | --- | --- |
| Claude Code | `~/.claude/skills/summoner/SKILL.md` | `/summoner` |
| Codex | `~/.codex/skills/summoner/SKILL.md` | ask to plan/run a fleet |
| Agents / Grok | `~/.agents/skills` · `~/.grok/skills` | skill name `summoner` |

Reload Claude Code (new session) after setup so `/summoner` appears. `summoner
doctor` notes whether skills are installed. Re-run with `--refresh` after
upgrades.

## Use

```sh
summoner setup --preset codex --repo   # skills + executors + AGENTS.md
summoner init --example                # sample order (if not already)
# optional: force independence even in a Rust tree
#   echo '[host]\nkind = "git"' >> .summoner.toml
summoner doctor orders/example.toml
summoner plan orders/example.toml
summoner run --stream orders/example.toml
```

`setup` / `init` are idempotent and never overwrite your personal config
blindly. Non-Rust repos get a git-host sample order (no Grove required). Rust
repos can still scaffold `.grove.toml` for the Grove host plugin. `run --stream`
prints lifecycle NDJSON ending in a `report` event; `summoner watch` shows the
live board.

### Why switch from a worktree dashboard?

Most multi-agent tools give you isolation tiles. Summoner gives you a **process**:
scope claims, optional verification profiles, cross-vendor review, tripwires
against reward hacking, immutable run manifests, and resume after a crash.

**Honest outcomes:** `verified` means required profiles actually ran and passed
against a bound candidate. With no verification configured, a successful fleet
lands `completed`, not a fake green. On the git host, a dirty worktree never
verifies or finishes: commit first. Grove host + `.grove.toml` is how you get
full workspace-snapshot `verified` on Rust monorepos.

**Landing:** `summoner land` merges onto a temporary integration branch, runs an
aggregate verify (`SUMMONER_LAND_VERIFY` argv, or `cargo test` when a root
`Cargo.toml` exists), then fast-forwards the protected branch. It refuses a
silent no-op aggregate; set `SUMMONER_LAND_ALLOW_NO_AGGREGATE=1` only when you
mean it.

| Platform | Git host | Grove host |
| --- | --- | --- |
| Unix | Independence (clean commits) | Full exact-state depth |
| Windows | Not yet (use grove host) | Supported |

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
| `scope` | yes | Paths; `crate:<name>` requires the **grove** host (rejected on git host). |
| `acceptance` | no | Criteria, included in prompt and report. |
| `verify_profile` | no | Host profile: `.grove.toml` under grove host, or `[verification]` under git host. |
| `executor`, `reviewer` | no | Configured names; `reviewer = "none"` opts out. |
| `timeout_secs`, `base`, `branch` | no | Deadline and worktree base/branch. |
| `after` | no | Order ids that must finish first. Supplies the base too: the dependent branches from its dependencies' verified commits (several are merged; conflicts skip the order). An explicit `base` overrides, but must contain every dependency's candidate. |
| `variants` | no | N-version dispatch: one sibling per executor, orchestrator picks the winner. |

## Config

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

Revision loops, budgets, resume, the review-gate protocol, and profile
inheritance: [docs/reference.md](docs/reference.md). Boundary: the host owns
worktrees, claims, lanes, and receipts; Summoner owns dispatch, review, and
reports.

## Assurance

Stack invariants for Grove · Summoner · Crucible: [ASSURANCE.md](ASSURANCE.md)
(epoch 1). Features that violate an invariant are bugs, not tradeoffs.

## License

MIT
