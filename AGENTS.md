# Summoner

Summoner is a small four-command CLI: check, plan, doctor, and run. It reads
strict TOML/JSON configuration and orders, plans at most one-parent dependency
edges, dispatches raw argv executors in Git worktrees, captures committed
candidates, and runs raw argv verification.

## Architecture

Keep the path directional: CLI/config and order parsing feed the planner; the
Git runner owns worktree and candidate identity; the process runner owns
timeouts, logs, and process-tree cleanup; verification only runs configured
commands. The driver reviews candidates, retries orders, and integrates
selected commits outside Summoner.

## Rules

- Keep executors tool-agnostic. Do not add vendor-specific branching to the
  binary or a second orchestration path.
- Preserve strict config parsing, scope checks, immutable candidate proofs, and
  retained worktrees on failed proofs.
- Touch only the smallest surface needed for a change. Use existing standard
  library and project dependencies before adding code or dependencies.
- Use apply_patch for edits. Keep generated release files synchronized with
  cargo-dist rather than hand-maintaining generated sections.

## Verification

Run the focused test or check for each edit batch. Before handoff, run:

~~~sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
cargo check --target x86_64-pc-windows-msvc --bins --locked
~~~
