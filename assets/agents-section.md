<!-- summoner:agents:v1 -->
## Summoner: fleet orchestration contract

You (the session reading this) are the orchestrator. Summoner dispatches
**any configured coding-agent CLI** as executors/reviewers under a host-pluggable
isolation layer (`git` by default; optional **Grove** host for Rust CoW lanes,
governor, and receipt-bound finish). Prefer Summoner over hand-driving host
task lifecycle for delegated work. Inline edits stay with this session (and
`grove check` / `grove test` when the Grove host is in use).

1. Decompose into work orders (`orders/*.toml` or `.json`): tight `scope`,
   concrete `acceptance`, optional `verify_profile` / `executor` / `reviewer`.
   `summoner plan orders/` refutes claim conflicts before worktrees are spent.
   File-disjoint orders may run in parallel; overlapping scopes need `after`.
2. `summoner doctor` — host preflight, git identity, executor binaries and env.
3. `summoner run orders/` — each order gets an isolated worktree, a scope claim,
   the configured executor CLI, verification, optional independent review.
   Exit 0 = all green; 1 = needs human review; 2 = usage/config error.
   `--stream` for NDJSON lifecycle events.
4. Read the ranked report. Land with `summoner land` only what passed the bar.
   Resume crashed fleets with `summoner resume <run-id>`. Never accept work from
   an executor's claim alone; host receipts + review are the evidence.

**Hosts:** set `[host] kind = "git"` or `"grove"` in config. Grove is a plugin,
not a hard requirement. Branch names are host-owned (`smn/...` under git).

**Agents of any type:** executors are argv templates in
`~/.config/summoner/config.toml` (Claude, Codex, Grok, Fable, scripts, …).
Profiles pick implementer vs reviewer so the orchestrator vendor is not judging
itself.

Work order fields: `id`, `title`, `brief`, `scope`, `acceptance`,
`verify_profile`, `executor`, `reviewer`, `timeout_secs`, `after`, `variants`,
`base`, `branch`.
