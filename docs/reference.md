# Reference

## Preflight

`doctor`, `run`, and `resume` all check: Grove 0.3.5 capability contract, git
identity, verification profiles, executor binaries, model CLI health. Malformed
config is an error, never a fallback.

Auth checks are noninteractive, capped at 5s. Kimi has no reliable auth probe,
so its preset records an acknowledgement. Unknown-auth custom backends fail
closed until named in `allow_unknown_auth` or run with `--allow-unknown-auth`.
Repo config can't grant that.

## Trusted policy

Optional `[trusted_policy]` in config declares the run's acceptance bar:
`require_reviewer`, `distinct_reviewer`, `required_profiles`,
`allowed_executors`, `allowed_reviewers`, `protected_paths`,
`completed_satisfies_dependencies`. Orders that violate it fail validation
before any worktree is spent.

Only the operator's global config may declare it: a `.summoner.toml` in the
repository being worked on is refused, since a candidate that could publish its
own bar could erase the one gating it.

`protected_paths` extends the built-in protected list (`.grove.toml`,
`rust-toolchain*`, `.cargo/config*`). Name the files Grove's policy digest
cannot bind: a `ci/verify.sh` a profile shells out to, or any script the
acceptance commands read. An entry matches that exact path or, for a directory,
everything beneath it; a diff touching one caps the order at `unverified`.

The policy is content-addressed. Its digest goes into `manifest.json`, the
review prompt, and `report.json`, so a verdict is provably tied to the bar it
applied. A resumed run is gated by the recorded policy, not today's config, and
a recorded policy whose digest does not match refuses the resume.

With a policy declared, an unverified `completed` upstream no longer satisfies
an `after` edge unless `completed_satisfies_dependencies` says so: a chain is
only as green as its weakest link. Without a policy, behavior is unchanged.

## Dependencies

`after` both orders dispatch and supplies the base. A dependent branches from
its dependencies' `candidate_commit`s, so their code is present without naming
a base by hand. One dependency is inherited directly; several are merged with
`git merge-tree`, which computes the merge without a worktree. Dependencies that
conflict skip the dependent and name the conflicting paths, rather than starting
an executor on a tree missing half its inputs.

The verified commit is used rather than the deterministic branch name, because
releasing a worktree can salvage dirty state into a new commit and move the
branch past what was actually verified. An explicit `base` always wins.

## Run evidence

Each run directory holds:

- `manifest.json`: create-once. Expanded orders, settings, resolved roles,
  executor paths + SHA-256, `--version` evidence. Env var names, never values.
- `events.jsonl`: append-only, sequenced, flushed before streaming. A journal
  write failure stops dispatch.
- `report.json`: projected from terminal journal records after `run_finished`.
  A hard-killed run has no report, correctly. Each order records
  `candidate_commit`, the exact commit captured in the worktree before release.
  Release may salvage dirty state into a new commit and advance the branch, so
  the branch name alone does not identify what was verified and reviewed.

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
