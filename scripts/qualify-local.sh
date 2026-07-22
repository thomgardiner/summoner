#!/usr/bin/env bash
# Run the release qualification on this machine before any tag is pushed.
# Builds real artifacts with the pinned cargo-dist, installs them through the
# real installer, and runs the reviewed fleet smoke: the same scripts CI runs.
# Windows-only steps are the sole coverage gap; they run on the private
# staging mirror (release-staging), never first on a public repo.
set -euo pipefail

cd "$(dirname "$0")/.."
command -v dist >/dev/null || { echo "install cargo-dist 0.32.0 first" >&2; exit 2; }
dist --version | grep -q "0\.32\.0" || echo "warning: local cargo-dist is not the pinned 0.32.0" >&2
grove --version >/dev/null || { echo "grove must be on PATH" >&2; exit 2; }

host=$(rustc -vV | sed -n 's/^host: //p')
scratch=$(mktemp -d "${TMPDIR:-/tmp}/summoner-qualify-XXXXXX")
trap 'rm -rf "${scratch}"' EXIT

echo "==> repo quality gates (fmt, clippy, distribution checks)"
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
node .github/workflows/patch-release-condition.mjs --test-permissions
node .github/workflows/patch-release-condition.mjs --test-qualification
node .github/workflows/patch-release-condition.mjs
git diff --exit-code -- dist-workspace.toml .github/workflows/release.yml

echo "==> dist build (${host})"
dist build --artifacts local --target "${host}" >/dev/null
dist build --artifacts global >/dev/null 2>&1 || true
PLAN=$(dist plan --output-format=json)
export PLAN

artifacts="${scratch}/artifacts"
mkdir -p "${artifacts}"
cp target/distrib/summoner-installer.sh "${artifacts}/" 2>/dev/null || cp target/distrib/*/summoner-installer.sh "${artifacts}/"
cp "target/distrib/summoner-${host}.tar.xz" "target/distrib/summoner-${host}.tar.xz.sha256" "${artifacts}/"
(cd "${artifacts}" && shasum -a 256 summoner-*.tar.xz source.tar.gz 2>/dev/null | sed 's/  / */' > sha256.sum) || \
  (cd "${artifacts}" && shasum -a 256 summoner-*.tar.xz | sed 's/  / */' > sha256.sum)

echo "==> install check"
bash scripts/qualify/install-check.sh "${host}" "${artifacts}" "${scratch}/install"

echo "==> fleet smoke"
SUMMONER_BIN="${scratch}/install/bin/summoner" bash scripts/qualify/fleet-smoke.sh "${scratch}"

echo "==> powershell script syntax"
if command -v pwsh >/dev/null; then
  for script in scripts/qualify/*.ps1; do
    [ -e "${script}" ] || continue
    pwsh -NoProfile -Command "[void][System.Management.Automation.Language.Parser]::ParseFile('$(pwd)/${script}', [ref]\$null, [ref]\$err); if (\$err) { \$err; exit 1 }"
  done
else
  echo "pwsh not installed; skipping PowerShell syntax check" >&2
fi

echo "LOCAL QUALIFICATION: GREEN"
