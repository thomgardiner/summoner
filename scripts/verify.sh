#!/usr/bin/env sh
# Required per-change lane: every T1 checker runs here.
set -e
cd "$(dirname "$0")/.."
sh checks/check-unit.sh
sh checks/check-host-honesty.sh
sh checks/check-tripwires.sh
sh checks/check-test-smells.sh
