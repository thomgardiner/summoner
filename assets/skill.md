---
name: summoner
description: >
  Dispatch a fleet of coding-agent CLIs (Claude, Codex, Grok, Fable, …) from
  work orders. Orchestrate while heterogeneous executors and reviewers run under
  Summoner. Isolation is host-pluggable (git by default; Grove optional for Rust
  CoW lanes and receipts). Invoke with /summoner (Claude) or by asking to run a
  Summoner fleet. Use when delegating parallel implementation, multi-model
  races, or cross-vendor review.
---
<!-- summoner:skill:v1 -->

# Summoner

You are the **orchestrator**. Summoner is the deterministic fleet layer: work
orders in, ranked evidence out. Shell the `summoner` CLI; do not invent a
parallel dispatch path.

## Invoke (any harness)

| Harness | How |
| --- | --- |
| Claude Code | `/summoner` or “run a Summoner fleet” |
| Codex | skill loads from `~/.codex/skills/summoner`; ask to plan/run orders |
| Shell | `summoner plan orders/` → `summoner run --stream orders/` |
| First install | `summoner setup` (wizard) or `--preset <codex|claude|kimi>` |

## Framing

- Any configured CLI can be an **executor** or a **reviewer** (argv templates).
- This session is the orchestrator; profiles avoid a vendor grading itself.
- **Hosts** own isolation: `git` (default independence) or `grove` (Rust depth).
- You do not hand-drive host task lifecycle for fleet work; Summoner does.

## Hosts

```toml
[host]
kind = "git"     # no Grove required (Unix)
# kind = "grove" # optional: CoW lanes, governor, receipt finish
# bin = "grove"
```

Resolution: explicit `kind` → legacy `grove_bin` → `.grove.toml` + grove on PATH → else `git`.

## Workflow

1. **Decompose.** One independent order per file under `orders/`:

   ```toml
   id     = "auth-refactor"                  # [a-z0-9_-]+
   title  = "Extract token validation"
   brief  = """Full instructions for the executor."""
   scope  = ["src/auth.rs"]                  # paths; crate:<name> needs grove host
   acceptance     = ["tests pass", "no new public API"]
   verify_profile = "fast"                   # optional
   executor       = "codex"                  # any configured name
   reviewer       = "claude-review"          # optional independent gate
   timeout_secs   = 900
   after          = ["prior-id"]             # DAG
   ```

   Multi-model race: `variants = ["codex", "claude"]` instead of `executor`.

   `summoner plan orders/` refutes claim conflicts before worktrees are spent.

2. **Preflight.** `summoner doctor` — host, git identity, executors, env.

3. **Dispatch.** `summoner run orders/` (optional `--stream` NDJSON).

4. **Review.** Ranked report + receipts; never trust executor self-claims alone.
   `summoner resume <run-id>` after crashes. `summoner land` for gated integrate.

## Evidence

Each run directory: immutable `manifest.json`, authoritative `events.jsonl`,
terminal `report.json`. Resume uses run-owned inputs + host durable task state.

## Honest outcomes

- `verified` = required profiles ran and passed.
- No verification configured → `completed` (not a fake green).
- Protected config touch (e.g. `.grove.toml`) caps at `unverified`.

## Non-goals

Summoner is not a chat product, model router, or GUI. It is the control plane
under whatever model is best this week.
