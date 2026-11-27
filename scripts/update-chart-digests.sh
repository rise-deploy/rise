#!/usr/bin/env bash
set -euo pipefail

# Rewrite the digest lines of helm/rise/charts.sha256 from the tarballs
# currently vendored in helm/rise/charts/, keeping the leading comment block.
#
# Run after `mise helm:deps` (or after Renovate re-vendors a subchart via its
# helmUpdateSubChartArchives post-update option). Review the resulting diff:
# a digest change for a tarball whose version did not change means the
# upstream chart was re-released or the file was altered.

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
charts_dir="$repo_root/helm/rise/charts"
manifest="$repo_root/helm/rise/charts.sha256"

if command -v sha256sum >/dev/null 2>&1; then
  hash_cmd=(sha256sum)
else
  hash_cmd=(shasum -a 256)
fi

shopt -s nullglob
tarballs=("$charts_dir"/*.tgz)
if [ "${#tarballs[@]}" -eq 0 ]; then
  echo "ERROR: no chart tarballs in $charts_dir" >&2
  exit 1
fi

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

if [ -f "$manifest" ]; then
  awk '/^#/ { print; next } { exit }' "$manifest" >"$tmp"
fi
(cd "$charts_dir" && LC_ALL=C "${hash_cmd[@]}" -- *.tgz) >>"$tmp"
cat "$tmp" >"$manifest"
