#!/usr/bin/env sh
# T1: git host must not invent verified without required profiles.
set -e
cd "$(dirname "$0")/.."
# verified = true only after non-empty required profiles (not on empty required).
if ! rg -n 'verification\.verified = !required\.is_empty\(\)' src/host/git_host.rs >/dev/null; then
  echo "check-host-honesty: git finish must set verified only when required profiles exist" >&2
  exit 1
fi
# Missing profile must fail, not soft-pass.
if ! rg -n 'Missing profile is a hard miss' src/host/git_host.rs >/dev/null; then
  echo "check-host-honesty: missing verify profile must fail closed" >&2
  exit 1
fi
# Gate must distinguish verified vs completed.
if ! rg -n 'if verification\.verified' src/gate.rs >/dev/null; then
  echo "check-host-honesty: finish gate must map verified flag to Outcome" >&2
  exit 1
fi
# host_git integration asserts completed without profiles.
if ! rg -n 'must complete, not claim verified' tests/host_git.rs >/dev/null; then
  echo "check-host-honesty: host_git must assert completed not verified" >&2
  exit 1
fi
echo "check-host-honesty: ok"
