#!/usr/bin/env sh
# T1: anti-reward-hack tripwires inventory is present and tested.
set -e
cd "$(dirname "$0")/.."
# Fixed-string needles (rg -F) so patterns like .skip( do not break the engine.
for needle in 'protected file modified' '#[ignore' '.skip(' 'PROTECTED' 'Tripwires'; do
  if ! rg -F -n "$needle" src/tripwires.rs >/dev/null; then
    echo "check-tripwires: missing inventory for $needle" >&2
    exit 1
  fi
done
if ! rg -n '#\[test\]' src/tripwires.rs >/dev/null; then
  echo "check-tripwires: tripwires module must carry unit tests" >&2
  exit 1
fi
# Cheat-order tests must exist (anti-reward-hack round).
if [ ! -f tests/anti_reward.rs ]; then
  echo "check-tripwires: tests/anti_reward.rs required" >&2
  exit 1
fi
echo "check-tripwires: ok"
