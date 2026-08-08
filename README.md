# summoner

For one order in one checkout, run your agent CLI directly. For independent or
dependent multi-order work, Summoner shortens wall time by running ready orders
concurrently and starting children immediately from accepted parents. It turns
TOML or JSON work orders into isolated Git worktrees, captures candidates,
proves scope and candidate state, runs verifiers, and emits JSON evidence. The
CLI has four commands: check, plan, doctor, and run.

## Compose with other tools

- **Summoner** dispatches arbitrary agent CLIs, runs ready orders concurrently,
  and owns order and candidate orchestration.
- **Grove** independently owns Rust build/cache lanes and scoped Cargo
  execution. Together, Summoner creates the candidate worktree and `grove exec`
  uses a Grove lane inside it. Neither binary requires the other.
- **Crucible** is an independent smoke/acceptance gate (`crucible --strict`).
  The intended pipeline is Summoner dispatch and candidate proof, then
  Grove/Cargo verification, then Crucible acceptance; this README does not
  claim a wired Summoner profile or add Crucible configuration.

## Install

The release installers work on macOS, Linux, and Windows. A source install is:

~~~
cargo install --git https://github.com/thomgardiner/summoner --locked
~~~

Summoner needs Git and Rust for a source install. check, plan, and doctor only
need Git and the configured command names. run needs a command-line executor
in your personal config; Summoner does not contain a vendor-specific executor.

## Use

~~~
summoner check orders/
summoner plan orders/
summoner doctor orders/
summoner run --jobs 5 --stream orders/
~~~

doctor with order paths checks only the distinct executors and verification
profiles selected by those orders. doctor without paths inventories the full
configured roster.

--config PATH is a global option and may be placed before the subcommand.
Without it, Summoner reads the platform config path, then the nearest
.summoner.toml from the current directory or an ancestor. An explicit file
overrides both.

An order is one TOML or JSON file:

~~~
id = "readme"
title = "Update the README"
brief = "Document the requested behavior."
scope = ["README.md"]
acceptance = ["README.md explains the four commands"]
executor = "agent"
verify_profile = "cargo"
timeout_secs = 900
~~~

An order id contains lowercase ASCII letters, digits, _ or -. scope must
contain at least one relative path and cannot traverse a parent directory.
after may name zero or one other order. base and branch are optional; base
references resolve to an immutable commit before dispatch.

## Configuration

Configuration is strict: unknown keys fail parsing, every executor argv must
have a non-empty program, and every verification profile must contain
non-empty commands. Global values are the base; .summoner.toml and then
--config override scalar values. A named executor or verification profile
from a later file replaces that complete named entry.

Put personal executors in the global config. This example uses a generic
agent-cli; substitute the command and arguments for your tool:

~~~
default_executor = "agent"
default_verify_profile = "cargo"
max_parallel = 5

[executors.agent]
argv = ["agent-cli", "--worktree", "{worktree}", "{prompt}"]
prompt = "arg"
timeout_secs = 900
env_required = []

[verification.profiles.cargo]
commands = [
  ["cargo", "fmt", "--all", "--", "--check"],
  ["cargo", "clippy", "--all-targets", "--all-features", "--", "-D", "warnings"],
  ["cargo", "test", "--all-targets", "--all-features"],
]
~~~

Executor argv supports {prompt}, {prompt_file}, {worktree}, {branch},
{base}, and {git_common_dir}. The corresponding SUMMONER_* values are also
exported to the child, including SUMMONER_GIT_COMMON_DIR for the absolute
shared Git directory. prompt = "arg" appends the prompt when {prompt} is
absent, "stdin" pipes it, and "file" writes and passes a prompt file.
Verification commands support {prompt_file}, {worktree}, {branch}, and {base}
and run in the candidate worktree.

Verification commands are ordinary argv. For a Rust repository they can use
Cargo directly as above. If you use Grove for faster local builds, an
optional profile can instead contain commands such as
["grove", "exec", "--", "cargo", "test", "--all-targets", "--all-features"];
run `grove warm` once on the primary checkout before a fleet. Summoner only
launches the argv and does not require Grove.

The repository .summoner.toml supplies a Cargo profile so this repository can
be checked without Grove. It intentionally does not choose an executor; add
one to personal config, then select it globally or on an individual order.

Headless executors need noninteractive read/write access to `{worktree}`. A
no-change result is unverified, not a successful candidate.

## Speed proof

One 2026-08-08 A/B run (not a statistical model benchmark) used the same four
DeepSeek V4 Flash Max tasks:

| Path | Work | Wall |
| --- | --- | ---: |
| Manual sequential | OpenCode DeepSeek V4 Flash Max plus ordinary Cargo; 18.27s, 21.88s, 14.84s, 13.57s | 68.56s |
| Summoner (`jobs=3`) | Same model, base, tasks, and acceptance; two raw Cargo and two Grove | 32.14s |

Summoner was 2.13x faster, saving 36.42s (53.1%) of wall time. The win comes
from automatic ready-order concurrency and immediate parent-to-child handoff,
not a faster model. It finished 0.52s above the measured 31.62s dependency
critical path (~1.6% overhead). The documented default is five for wider
fleets; this four-order proof kept `jobs=3` because only three roots were ready.

Method: both paths used fixture base `064cc0d`, the same four briefs and
acceptance tests, and the exact parent-to-child dependency. Both changed only
`src/lib.rs`, kept the increment in the child, and passed all focused Cargo
tests. This establishes semantic acceptance equivalence, not byte equality;
model output is stochastic. Summoner verified all four; successful worktrees
were removed and branches retained.

### Grove microbenchmark

Separately, current Grove 0.1.3 on Apple M2 Max/macOS 26.5.1/rustc 1.97.1
with ripgrep pinned to `3fce3b5` used a warm seed and three fresh worktrees to
run `cargo check -p ripgrep`:

| Runner | Samples | Median |
| --- | --- | ---: |
| Grove | 1.28s, 0.92s, 0.91s | 0.92s |
| Bare Cargo, fresh per-worktree target | 2.87s, 2.89s, 3.01s | 2.89s |

Grove was 3.14x faster here, saving 68.2% of wall time. Same commit, same
command, exit 0, and clean source provide output-equivalence evidence. A
hand-warmed shared `CARGO_TARGET_DIR` measured 0.98s, 0.61s, 0.61s (median
0.61s), faster than Grove here; Grove's measured win is against ordinary
isolated-worktree Cargo, and the shared-target gap is a real optimization
target. Grove does not always win.

The immediately preceding run without noninteractive OpenCode permissions had
one worker exit 0 with no change; Summoner marked it unverified and skipped its
child, so exit 0 alone is not a candidate.

## Run behavior

run resolves the repository root and initial HEAD, then creates one worktree
per order on smn/<id>-<run-id> (or the explicit branch). A dependent order
starts from its parent's verified candidate commit. Orders whose parent does
not return a verified candidate are skipped.

After the executor exits, Summoner records changed paths before salvaging any
dirty files. A path outside scope produces scope_violation and keeps the
worktree. Otherwise dirty files are committed as a candidate. The candidate
must remain clean, on the expected branch, at the expected commit, and at the
branch tip before verification, after every verification command, and before
cleanup. Successful cleanup removes the worktree and clears its path in the
report; a failed safety proof retains the path and branch for inspection.

Executors and verification commands run in the worktree with captured stdout
and stderr logs. Timeouts terminate the process tree; Unix SIGINT/SIGTERM are
also handled, while Windows uses taskkill /T /F. --stream emits an
order_finished event for each order followed by a final report event. Without
it, run prints the report as pretty JSON.

Outcomes include verified, verification_failed, executor_failed, timeout,
interrupted, scope_violation, unverified, skipped, and error. Exit code 0
means every order is verified; 1 means a domain outcome needs attention; 2
means invalid input or infrastructure failure.

Summoner never merges branches or chooses which candidate to keep. The driver
inspects candidates with normal Git tools, reviews the result, retries failed
orders when desired, and integrates the selected commits.

## License

MIT
