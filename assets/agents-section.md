<!-- summoner:agents:v1 -->
## Summoner: fleet orchestration contract

You (the session reading this) are the orchestrator. Summoner runs fleets of
executor agents inside grove-managed worktrees, and it owns the whole grove
lifecycle for delegated work, so prefer it over hand-driving
`grove task begin/exec/finish`. Inline changes are different. For those,
`grove check` / `grove test` remain yours.

1. Decompose the plan into work orders: one TOML or JSON file per independent
   task in an `orders/` directory. Keep scopes tight; when the work maps to
   packages, seed `scope` from `grove plan --json` `claim_scopes`. Give every
   order explicit acceptance criteria and a verify profile.
2. Preflight with `summoner doctor`: it checks each configured executor binary
   and its required environment, and the grove version.
3. `summoner run orders/` executes the fleet. Each order gets an isolated
   worktree, a grove task holding its scope claim, the configured executor CLI,
   then verification. Exit 0: every order verified. Exit 1: at least one order
   needs review. Exit 2: usage or infrastructure error.
4. Read the ranked JSON report (stdout, and report.json in the run directory).
   Review worst-first. Diffs live on each order's branch; verification receipts
   and log tails are in the report. Re-dispatch failures with revised orders.
   Never accept work from an executor's claim alone; the receipts are the
   evidence.

Work order fields: `id`, `title`, `brief`, `scope` (paths or `crate:<name>`),
`acceptance` (list), `verify_profile`, `executor`, `timeout_secs`. Executors are
argv templates in `.summoner.toml`; `summoner config` prints the resolved
settings and their sources.
