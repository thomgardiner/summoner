<!-- summoner:agents:v1 -->
## Summoner: fleet orchestration contract

You (the session reading this) are the orchestrator. Summoner runs fleets of
executor agents inside grove-managed worktrees, and it owns the whole grove
lifecycle for delegated work, so prefer it over hand-driving
`grove task begin/exec/finish`. Inline changes are different. For those,
`grove check` / `grove test` remain yours.

1. Decompose the plan into work orders: one TOML or JSON file per independent
   task in an `orders/` directory. Decompose along the real package seams
   (`grove plan --topology` prints them, with the claim scope owning each);
   keep scopes tight and give every order explicit acceptance criteria and a
   verify profile. Then `summoner plan orders/` refutes the batch before any
   worktree is spent: claim conflicts, package couplings, suggested waves,
   and missing `after` edges. Revise until `clean`.
2. Preflight with `summoner doctor`: it checks each configured executor binary
   and its required environment, and the grove version.
3. `summoner run orders/` executes the fleet. Each order gets an isolated
   worktree, a grove task holding its scope claim, the configured executor CLI,
   then verification. Exit 0: every order verified. Exit 1: at least one order
   needs review. Exit 2: usage or infrastructure error. Add `--stream` for
   NDJSON lifecycle events on stdout (final line: a `report` event with the
   full report); every run also writes the same events to `events.jsonl` in
   the run directory for live monitoring.
4. Read the ranked JSON report (stdout, and report.json in the run directory).
   Review worst-first. Diffs live on each order's branch; verification receipts
   and log tails are in the report. Re-dispatch failures with revised orders,
   or `summoner resume <run-id>` to re-run only what did not succeed. Set
   `fail_fast = N` in `.summoner.toml` so a doomed fleet stops early. Never
   accept work from an executor's claim alone; the receipts are the evidence.

Work order fields: `id`, `title`, `brief`, `scope` (paths or `crate:<name>`),
`acceptance` (list), `verify_profile`, `executor`, `timeout_secs`, `after`.
Chain dependent work with `after = ["<id>"]`: one run executes the whole DAG,
and dependents of failed orders come back `skipped`. The chain is ordering
only, so an order that builds on a dependency's changes must also set
`base = "grove/smn-<dep-id>"` (branch names are deterministic). Executors are
argv templates defined by the user, personal ones in
`~/.config/summoner/config.toml` (template via `summoner init --global`) and
repo overrides in `.summoner.toml`; `summoner config` prints the resolved
settings and their sources, and `summoner doctor` says what is missing.
