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

# A fixed port silently hands the check over to whatever else is listening. A
# leftover server from an earlier run serves that run's artifacts, so the
# installer fetches stale bytes: this check once validated a binary that was
# neither the local build nor the published release. Worse than the confusing
# failure is the false pass, where an old good binary hides a broken new one.
# So refuse an occupied port, then prove the served archive is byte-identical
# to the one just built before trusting anything installed from it.
port=8765
if command -v lsof >/dev/null && lsof -nP -iTCP:"${port}" -sTCP:LISTEN >/dev/null 2>&1; then
  echo "port ${port} is already in use; refusing to qualify against another server" >&2
  exit 1
fi
python3 -m http.server "${port}" --bind 127.0.0.1 --directory "${artifacts}" >/dev/null 2>&1 &
server=$!
trap 'kill "${server}" 2>/dev/null || true' EXIT
for _ in {1..20}; do
  curl -fIsS "http://127.0.0.1:${port}/summoner-installer.sh" >/dev/null && break
  sleep 0.25
done
curl -fIsS "http://127.0.0.1:${port}/summoner-installer.sh" >/dev/null

served="${install_root}.served"
mkdir -p "$(dirname "${served}")"
curl -fsS "http://127.0.0.1:${port}/${archive}" -o "${served}"
if ! cmp -s "${served}" "${artifacts}/${archive}"; then
  echo "the server on ${port} is not serving the archive under test" >&2
  exit 1
fi
rm -f "${served}"

# The installer appends the artifact filename itself: base URL only.
SUMMONER_DOWNLOAD_URL="http://127.0.0.1:${port}" \
CARGO_DIST_FORCE_INSTALL_DIR="${install_root}" \
SUMMONER_NO_MODIFY_PATH=1 \
  sh "${artifacts}/summoner-installer.sh"

expected=$(jq -er '[.releases[] | select(.app_name == "summoner") | .app_version] | if length == 1 then .[0] else error("expected one Summoner release") end' <<<"${PLAN}")
test "$("${install_root}/bin/summoner" --version)" = "summoner ${expected}"
