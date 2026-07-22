#!/usr/bin/env bash
# Install the built Summoner archive through the real installer and verify it.
# Single source of truth: CI qualification and scripts/qualify-local.sh both run
# this file. PLAN holds the dist plan JSON.
#
# usage: install-check.sh <target> <artifacts-dir> <install-root>
set -euo pipefail

target="$1"
artifacts="$2"
install_root="$3"

archive="summoner-${target}.tar.xz"
for path in "${artifacts}/summoner-installer.sh" "${artifacts}/sha256.sum" "${artifacts}/${archive}" "${artifacts}/${archive}.sha256"; do
  test -s "${path}"
done
(cd "${artifacts}" && shasum -a 256 -c sha256.sum)
(cd "${artifacts}" && shasum -a 256 -c "${archive}.sha256")

port=8765
python3 -m http.server "${port}" --bind 127.0.0.1 --directory "${artifacts}" >/dev/null 2>&1 &
server=$!
trap 'kill "${server}" 2>/dev/null || true' EXIT
for _ in {1..20}; do
  curl -fIsS "http://127.0.0.1:${port}/summoner-installer.sh" >/dev/null && break
  sleep 0.25
done
curl -fIsS "http://127.0.0.1:${port}/summoner-installer.sh" >/dev/null

# The installer appends the artifact filename itself: base URL only.
SUMMONER_DOWNLOAD_URL="http://127.0.0.1:${port}" \
CARGO_DIST_FORCE_INSTALL_DIR="${install_root}" \
SUMMONER_NO_MODIFY_PATH=1 \
  sh "${artifacts}/summoner-installer.sh"

expected=$(jq -er '[.releases[] | select(.app_name == "summoner") | .app_version] | if length == 1 then .[0] else error("expected one Summoner release") end' <<<"${PLAN}")
test "$("${install_root}/bin/summoner" --version)" = "summoner ${expected}"
