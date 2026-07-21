# Reference

## Preflight

`doctor`, `run`, and `resume` all check: Grove 0.3.4 capability contract, git
identity, verification profiles, executor binaries, model CLI health. Malformed
config is an error, never a fallback.

Auth checks are noninteractive, capped at 5s. Kimi has no reliable auth probe,
so its preset records an acknowledgement. Unknown-auth custom backends fail
closed until named in `allow_unknown_auth` or run with `--allow-unknown-auth`.
Repo config can't grant that.

## Run evidence

Each run directory holds:

- `manifest.json`: create-once. Expanded orders, settings, resolved roles,
  executor paths + SHA-256, `--version` evidence. Env var names, never values.
- `events.jsonl`: append-only, sequenced, flushed before streaming. A journal
  write failure stops dispatch.
- `report.json`: projected from terminal journal records after `run_finished`.
  A hard-killed run has no report, correctly.

`resume <run-id>` replays from the manifest and journal, not order files.
Different executor path or digest: refused. Carried forward: `verified` and
`approved` with matching Grove receipts (approvals also need Grove's source
digest to equal the review snapshot). Everything else reruns on its recorded
branch. A nonterminal Grove task blocks resume until resolved.

`watch` renders the live board. `scorecard` aggregates all runs per repo and
executor from `scorecard.jsonl`.

## Review gate

The reviewer runs in a Grove inspection capsule: a private clone with no
origin, no shared git metadata, no build lane. Its prompt: charter,
requirements, verification evidence, candidate diff, nonce, snapshot digests.
Never the executor's transcript.

The verdict is one strict JSON object binding the nonce and digests. Anything off voids approval: unknown fields, replayed bindings, source or
capsule drift, surviving processes, truncated logs. Approve: `verified` → `approved`.
Reject: `rejected` with findings.

The capsule is tamper evidence, not an OS sandbox. Keep the vendor CLI's own
sandbox on, and never give a reviewer `{git_common_dir}`.

Tripwires run before review: deleted tests, skip markers, assertion loss,
`[profile]` edits. They raise scrutiny. Edits to `.grove.toml`,
`.summoner.toml`, `rust-toolchain*`, or `.cargo/config*` are a hard stop:
`unverified`, task abandoned.

## Revisions and budgets

`revise = N`: rejected or unverified orders retry up to N times on the same
branch, failure evidence in the prompt. With `session_marker` + `resume_argv`,
the executor's session resumes; otherwise fresh context.

`run_token_budget` is a circuit breaker, not a quota: spend is scraped from
backend output after each exit, so in-flight attempts can overshoot. The grove
deadline is the hard stop. `max_tokens` caps one order (requires a
`usage_marker`). `fail_fast = N` abandons the queue after N failures.

## Profiles

Global config is the base; `[profiles.<name>]` overlays it; repo
`.summoner.toml` overrides field-by-field. Empty string clears an inherited
value. Selection: `--profile`, then `SUMMONER_PROFILE`, then a config pin.
Harness auto-detection only picks profiles that exist.
