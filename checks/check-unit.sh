#!/usr/bin/env sh
# T1: unit + host independence + anti-reward tests stay green.
set -e
cd "$(dirname "$0")/.."
if command -v grove >/dev/null 2>&1; then
  grove exec --tag crucible-unit -- cargo test --bins --test host_git --test anti_reward -q
else
  cargo test --bins --test host_git --test anti_reward -q
fi
