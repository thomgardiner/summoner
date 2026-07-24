# Reference

## Preflight

`doctor`, `run`, and `resume` all check: Grove 0.4.0 capability contract, git
identity, verification profiles, executor binaries, model CLI health. Malformed
config is an error, never a fallback.

Orchestrator profiles select themselves through config, not a compiled vendor
list: a profile with `detect_env = "SOME_VAR"` is chosen automatically when
that variable is present, so any harness that exports an identifying variable
self-registers. Ambiguous matches select nothing and say so. The built-in
Claude Code and Codex detection still applies to profiles without `detect_env`.

Auth checks are noninteractive, capped at 5s. Kimi has no reliable auth probe,
so its preset records an acknowledgement. Unknown-auth custom backends fail
closed until named in `allow_unknown_auth` or run with `--allow-unknown-auth`.
Repo config can't grant that.

## Trusted policy

Optional `[trusted_policy]` in config declares the run's acceptance bar:
`require_reviewer`, `distinct_reviewer_name`, `allowed_profiles`,
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
an executor on a tree missing half its inputs. A dependency that finished
without a candidate commit also skips the dependent: there is nothing safe to
build on. An explicit `base` overrides the derivation, but is refused when it
does not contain a dependency's candidate, because the order would wait for
work it then builds without.

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
  `candidate_commit`, the exact commit captured in the worktree before release,
  and only when the tree is clean. **Git-host** verify/finish refuse a dirty
  tree (clean committed candidates only). **Grove-host** verification is
  snapshot-bound to the workspace digest rather than "uncommitted is fine."
  Release may salvage dirty state into a new commit and advance the branch, so
  the branch name alone never identifies what was reviewed either.

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

The verdict is one strict JSON object binding the nonce and digests. A markdown
fence wrapping the whole payload is stripped before parsing, because several
chat-first CLIs fence JSON regardless of instruction; the fence carries no
authority, and anything else around the object still fails closed. Anything off voids approval: unknown fields, replayed bindings, source or
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

## Notifications

`[notify] command = [...]` runs when a run reaches a moment worth looking up
from other work: the run finishes, an order lands non-green, or a review
starts. Green orders stay silent on purpose. The command receives the event's
JSON journal line on stdin and `SUMMONER_NOTIFY_TITLE`, `SUMMONER_NOTIFY_BODY`,
and `SUMMONER_NOTIFY_EVENT` in the environment. One seam covers both an OS
notifier and a webhook:

```toml
# macOS notification center
[notify]
command = ["sh", "-c", "osascript -e \"display notification \\\"$SUMMONER_NOTIFY_BODY\\\" with title \\\"$SUMMONER_NOTIFY_TITLE\\\"\""]

# webhook: the JSON line is piped to curl
# command = ["sh", "-c", "curl -m 5 -sX POST -H 'content-type: application/json' -d @- https://example.invalid/hook"]
```

Notifications are a side-channel over the run journal, never authoritative: a
command that fails is ignored, and each is time-bounded so a hung one (a `curl`
with no timeout) cannot wedge the run. Summoner waits for the final
`run_finished` notification before exiting.

## Cache accounting

`usage_marker` scrapes the executor's total token count after each attempt
(codex prints `tokens used\nN`). The prompt-cache split is recorded per order
(`cache_read_tokens` / `cache_write_tokens`) and summed into the run report:
`cache_read` is context served warm from cache (~0.1x price), `cache_write` is
context written cold (1.25x–2x). A run whose write dwarfs its read is thrashing
the prompt cache.

The split is read from Claude Code's `--output-format json` result envelope,
whose cumulative `usage` carries `cache_read_input_tokens` and
`cache_creation_input_tokens` — not a substring marker, because Claude repeats
those keys inside `usage.iterations` (one entry per turn), so a text scan would
report a single turn instead of the run total. The bundled `claude` preset
already requests JSON output; codex reports no cache split, so it is simply not
measured there. Both fields are absent from the report until an executor
reports them.

## Landing

`summoner land [run-id]` is the gated apply onto a protected branch. It defaults
to the latest finished run (by `report.json` mtime). You still own the review;
`land` only uses candidates that already passed the run's bar (`verified`, or
`approved` when a reviewer ran) and merges the exact `candidate_commit` the
report recorded.

**What it does (integration seal I):**

1. Plan landable orders in dependency order; skip non-green / missing commits.
2. Merge candidates onto a **temporary integration branch** (not the protected
   tip yet). A conflict aborts that merge, leaves the protected target
   unchanged, and exits 1.
3. Capture sealed integration candidate `I` (commit + tree of the merged tip),
   then run optional aggregate / Crucible / holder gates against `I`.
4. Bind `I` into the run's `assurance_envelope.json` **before** advancing the
   protected branch.
5. Fast-forward the protected target specifically to sealed `I`; drop the temp
   branch.

`land` refuses a dirty working tree and must run in the repository the run
targeted. `--dry-run` prints the plan without merging.

## Overview

`summoner overview` prints one pane across every fleet and Grove repo on the
machine, so you don't visit a dozen repos to see what is running. `--watch`
redraws it live. Two sections, both folded from best-effort NDJSON:

- **Fleets** — every summoner run journal (`runs/*/events.jsonl`), one line
  each: repo, running or finished, and an order tally (verified / running /
  failed). Active fleets sort first, then most recent.
- **Grove coordination** — Grove's per-repo event streams under its cache root
  (`events/*.jsonl`, resolved via `grove config`), one line each: the repo slug,
  last activity, and a tally of recent events by category (`task`, `verify`,
  `claim`, ...). A torn line is skipped, never blanking the pane. Empty when
  Grove is not installed.

## Hosts

Isolation is a plugin. `doctor` reports a top-level `host` object (`kind`,
`version`, `ok`, `capabilities`); when `kind` is `grove`, a nested `grove`
object carries the Grove capability pin.

| | **git** | **grove** |
| --- | --- | --- |
| When | Default when no `.grove.toml` / grove on PATH, or `[host] kind = "git"` | Explicit `[host] kind = "grove"`, legacy `grove_bin`, or `.grove.toml` + `grove` on PATH |
| Worktrees | `git worktree` under a Summoner cache root | `grove worktree acquire` |
| Claims / tasks | Local ledger + flock registry | Grove registry + durable task records |
| Deadline | Summoner process-group supervision (Unix; Windows not yet) | `grove task exec --timeout-secs` |
| Verify | Optional `[verification]`; missing profile is a **fail**, not a free pass. No required profiles → finish is `completed`, not `verified` | `.grove.toml` profiles + receipts |
| CoW lanes / governor | No | Yes |
| Inspection capsule | Detached private worktree + post-exec HEAD/clean checks | Private Grove capsule |

Resolution order: explicit `[host] kind` → legacy `grove_bin` → `.grove.toml`
plus grove on PATH → **git**.

Resume pins the host kind recorded in `manifest.json`. A config that resolves
to a different host fails closed until you match the recorded kind.

Rust monorepos: prefer the grove host. Multi-agent rebuild tax and claim-free
collisions are what Grove is for; Summoner alone is enough for process and
multi-vendor dispatch.
