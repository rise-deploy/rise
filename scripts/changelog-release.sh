#!/usr/bin/env bash
set -euo pipefail

# Publish structured upgrade notes and a documentation page for a full release.
# Usage: scripts/changelog-release.sh <version> <release-notes-file> [YYYY-MM-DD]
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
exec node "${script_dir}/../docs/scripts/changelog-release.mjs" "$@"
