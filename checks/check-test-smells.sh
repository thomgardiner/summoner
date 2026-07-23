#!/usr/bin/env sh
# T1: no obvious reward-hacked tests in host/gate/tripwire surface.
set -e
cd "$(dirname "$0")/.."
if command -v crucible >/dev/null 2>&1; then
  crucible test-smells src/host src/gate.rs src/tripwires.rs tests/host_git.rs tests/anti_reward.rs 2>&1
else
  # Fallback: forbid assert!(true) in those paths.
  if rg -n 'assert!\s*\(\s*true\s*\)' src/host src/gate.rs src/tripwires.rs tests/host_git.rs tests/anti_reward.rs 2>/dev/null; then
    echo "check-test-smells: found smell markers without crucible binary" >&2
    exit 1
  fi
fi
echo "check-test-smells: ok"
