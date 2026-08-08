# Reference

## Commands

The executable has four subcommands:

* check PATH... parses orders and validates them against the merged config.
* plan PATH... performs the same validation and prints dependency waves.
* doctor [PATH...] prints Git, executor, verification-command, and order
  availability as JSON.
* run [--stream] [--jobs N] PATH... dispatches orders in Git worktrees.

PATH may be a TOML or JSON file, or a directory containing those files. A
directory is read in sorted filename order. The global option --config PATH
adds one final config file:

~~~
summoner --config ~/.config/summoner/ci.toml check orders/
~~~

check prints {"ok":true,"orders":[...]} and plan prints
{"waves":[["first"],["second"]]}. doctor returns exit code 0 only when Git and
all executables and required environment variables in the inspected set are
available.
With order paths, doctor checks only the distinct executors and verification
profiles selected by those orders. Without paths, it inventories the full
configured roster. It reports the fields ok, git, executors, verification, and
orders.

run defaults to five concurrent orders. --jobs N overrides the config value
max_parallel; zero is invalid. With --stream, each completed order is emitted
as an order_finished event and the final report is emitted as a report event.
Without --stream, the final report is pretty JSON.

## Configuration

On Unix, the global file is XDG_CONFIG_HOME/summoner/config.toml when that
variable is an absolute path, otherwise ~/.config/summoner/config.toml. On
Windows it is APPDATA/summoner/config.toml, with USERPROFILE/AppData/Roaming
as the fallback. Summoner then reads the nearest .summoner.toml from the
current directory or an ancestor, and finally the explicit --config file.

Every file is parsed with deny-unknown-fields. Scalar values in a later file
replace earlier values. A named entry in executors or
verification.profiles replaces that complete named entry; it is not a
field-by-field overlay.

Supported top-level keys are:

* default_executor and default_verify_profile select names.
* max_parallel sets the default run concurrency.
* order_timeout_secs supplies a command timeout when an order and executor
  do not set one.
* executors is a map of named command definitions.
* verification.profiles is a map of named command lists.

An executor has argv, prompt, timeout_secs, and env_required. argv must have a
non-empty first item. prompt is arg, stdin, or file. A verification profile
has commands; each command is either an argv array or a table with an argv
array, and every command must have a non-empty first item.

Executor placeholders are {prompt}, {prompt_file}, {worktree}, {branch},
{base}, and {git_common_dir}. The prompt is appended for prompt = arg when no
{prompt} placeholder exists; prompt = stdin pipes it; prompt = file writes it
to the per-order prompt file and passes that path when no {prompt_file}
placeholder exists. The child receives SUMMONER_BASE, SUMMONER_WORKTREE,
SUMMONER_BRANCH, SUMMONER_ORDER, and SUMMONER_GIT_COMMON_DIR, the absolute
shared Git directory.

Verification placeholders are {prompt_file}, {worktree}, {branch}, and {base}.
Verification commands run with the candidate worktree as their current
directory and receive SUMMONER_BASE, SUMMONER_WORKTREE, SUMMONER_BRANCH, and
SUMMONER_ORDER.

The repository .summoner.toml contains only a Cargo profile:

~~~
default_verify_profile = "cargo"

[verification.profiles.cargo]
commands = [
  ["cargo", "fmt", "--all", "--", "--check"],
  ["cargo", "clippy", "--all-targets", "--all-features", "--", "-D", "warnings"],
  ["cargo", "test", "--all-targets", "--all-features"],
]
~~~

It deliberately has no executor. A check or run for an order therefore needs
an executor in the global config or an explicit config file; an order may then
select that configured name.

## Orders

Required fields are id, title, brief, and scope. Optional fields are
acceptance, executor, verify_profile, timeout_secs, base, branch, and after.
IDs match [a-z0-9_-]+. title and brief cannot be blank. scope has at least one
relative path; backslashes are normalized, . components are removed, and
parent traversal, absolute paths, and drive prefixes are rejected.

after contains zero or one parent id. Self-dependencies, unknown parents, and
cycles fail validation. A parent must finish with outcome verified and a
candidate commit before its child starts. A child's base is the parent's
candidate; otherwise it is the explicit base or the initial HEAD. run
resolves every explicit base to a commit before creating worktrees.

branch is checked with Git's native branch-format validator and may not begin
with a dash. timeout_secs is between 1 and 604800 seconds. A profile and
executor are selected from the order first, then the corresponding defaults.
The selected names must exist in the merged config.

## Worktree and candidate rules

run resolves the repository root and initial HEAD, creates a temporary run
directory under the system temporary directory, and adds one worktree per
ready order. The default branch is smn/<order-id>-<run-id>; branch may replace
it. The order prompt includes the id, title, brief, scope, acceptance items,
worktree, branch, and base commit.

The executor may leave staged, unstaged, or untracked files. Summoner first
collects changed paths from the committed range, index, worktree, and
untracked files. A path outside scope yields scope_violation and retains the
worktree. Otherwise dirty files are added and committed as
summoner: salvage <id>. No-change orders are unverified.

Before verification, after every verification command, and before cleanup,
Summoner proves that the worktree is clean, HEAD is the candidate commit, the
expected branch is checked out, and that branch still points at the candidate.
Any failed proof retains the worktree and records the reason. A successful
proof permits Git worktree removal; only then is the report's worktree path
cleared. The candidate commit and changed paths remain in the report.

## Processes and evidence

Every executor and verification command has stdout and stderr log files under
the run directory. The effective timeout is order timeout, then executor
timeout, then order_timeout_secs, then 600 seconds, clamped to 1 through
31536000 seconds. A timeout kills the process tree on every platform. On Unix,
SIGINT and SIGTERM are also intercepted and terminate the process tree; on
Windows the timeout/direct-child cleanup uses taskkill /T /F. These cases are
recorded as timed_out or interrupted; no synthetic exit code is reported.

Each order report contains id, title, outcome, detail, after, branch,
base_commit, candidate_commit, changed_paths, worktree, executor evidence, and
verification evidence. Command evidence contains the expanded argv, exit,
timed_out, interrupted, success, duration_ms, stdout_log, and stderr_log.
The run report adds run_id, repo, jobs, orders, and a summary by outcome.

Possible outcomes are verified, verification_failed, executor_failed, timeout,
interrupted, scope_violation, unverified, skipped, and error. Exit code 0
means all orders are verified. Exit code 1 means at least one domain outcome
needs attention. Exit code 2 means parsing, validation, or infrastructure
failed before a normal report could be produced.

Summoner does not merge or select candidates. The driver inspects each retained
worktree and candidate commit, reviews the diff, retries an order when useful,
and integrates selected commits with ordinary Git commands.
