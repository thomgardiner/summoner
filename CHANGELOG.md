# Changelog

## 0.1.0 — 2026-08-08

This release ships the four-command simplified core:

- `check`, `plan`, `doctor`, and `run` validate and dispatch TOML/JSON work
  orders through arbitrary configured agent CLIs.
- `run` schedules ready orders (five-wide by default), starts one-parent
  dependents from accepted candidates, and creates isolated Git worktrees.
- Candidate capture, scope and identity proofs, ordinary argv verification,
  cleanup, and JSON evidence keep the driver in control of review and
  integration.
- Grove is optional and composable through verification argv; Summoner remains
  vendor-neutral and does not require a Grove or Crucible configuration.
