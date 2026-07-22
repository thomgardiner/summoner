#!/usr/bin/env bash
# Reviewed fleet smoke against the installed Summoner and a fake codex CLI.
# Single source of truth: CI qualification and scripts/qualify-local.sh both
# run this file. SUMMONER_BIN names the installed binary; grove must be on PATH.
#
# usage: fleet-smoke.sh <scratch-dir>
set -euo pipefail

smoke="$1/summoner fleet ü"
rm -rf "${smoke}"
mkdir -p "${smoke}/fake-bin"
cat >"${smoke}/fake-bin/codex" <<'EOF'
#!/bin/sh
set -eu
if [ "${1:-}" = login ]; then exit 0; fi
prompt=$(mktemp)
trap 'rm -f "$prompt"' EXIT
cat >"$prompt"
if [ -f docs/summoner-demo.md ]; then
  python3 - "$prompt" <<'PY'
import json
import sys

templates = []
for line in open(sys.argv[1], encoding="utf-8"):
    try:
        value = json.loads(line)
    except json.JSONDecodeError:
        continue
    if value.get("protocol_version") == 1:
        templates.append(value)
if len(templates) != 1:
    raise SystemExit(f"expected one review protocol template, found {len(templates)}")
verdict = templates[0]
verdict["verdict"] = "approve"
verdict["findings"] = []
verdict["reviewer"]["model"] = "fake"
print(json.dumps(verdict, separators=(",", ":")))
PY
  exit 0
fi
mkdir -p docs
printf '%s\n' '# Summoner demo' '' 'This repository builds the Summoner fleet runner.' > docs/summoner-demo.md
git add docs/summoner-demo.md
git commit -qm 'demo executor'
printf '%s\n' '{"summoner_status":"complete","unmet":[]}'
EOF
chmod +x "${smoke}/fake-bin/codex"

cd "${smoke}"
git init -q
git config user.name release-smoke
git config user.email release-smoke@example.com
printf '%s\n' '[verification]' 'required = ["fast"]' '[verification.profiles.fast]' 'continue_on_failure = false' 'commands = [{ argv = ["git", "diff", "--check"], allow_zero_tests = true }]' > .grove.toml
export XDG_CONFIG_HOME="${smoke}/config"
export PATH="${smoke}/fake-bin:${PATH}"
"${SUMMONER_BIN}" init --preset codex --example
git add -A
git commit -qm 'initialize smoke repository'
"${SUMMONER_BIN}" doctor orders/example.toml
"${SUMMONER_BIN}" plan orders/example.toml
"${SUMMONER_BIN}" run --stream orders/example.toml > events.jsonl
jq -s -e '
  (map(.event) | index("run_started") != null and index("order_started") != null) and
  (last.event == "report") and
  (last.report.summary.approved == 1) and
  ((last.report.summary.error // 0) == 0) and
  ((last.report.summary.rejected // 0) == 0)
' events.jsonl
