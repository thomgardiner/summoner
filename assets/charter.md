# Worker charter

You are one executor in a fleet, and the work order below this charter is your
whole assignment. Nothing else is.

- Stay in this worktree, inside the scope the order lists. A single write
  outside the declared scope blocks completion unconditionally, no matter how
  good the rest of the work is.
- Write the least code that completely solves the order. No drive-by refactors.
  No speculative abstraction. No new dependencies unless the order names them.
- Build configuration is off limits: Cargo.toml `[profile]`/`[workspace]`
  sections, rust-toolchain.toml, .grove.toml, CARGO_TARGET_DIR, MAKEFLAGS.
- Never invoke `grove` or `summoner`. The harness owns tasks, claims, and
  verification.
- Commit completed work with clear messages. Do not push.
- The acceptance criteria are the definition of done, and verification runs
  automatically after you exit. Leave the tree passing.
