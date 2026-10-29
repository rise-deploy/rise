#!/usr/bin/env bash
set -euo pipefail

# Check that every commit in BASE..HEAD carries a DCO sign-off from its author.
#
# Usage: scripts/check-dco.sh <base> <head>
#
# A commit passes when one of its `Signed-off-by:` trailers names the author,
# matched by email or by name (case-insensitively), so a sign-off with a real
# address still covers a commit authored under a GitHub noreply address.
# Only parsed trailers count: the same text elsewhere in the message does not.
#
# Skipped:
# - Merge commits. They carry the sign-offs of the commits they merge, and
#   GitHub's "Update branch" merges cannot be signed off.
# - Bot authors (`*[bot]` noreply addresses, and GitHub Copilot). A bot cannot
#   certify the DCO; the person who merges its pull request answers for it.

base="$1"
head="$2"

lower() { tr '[:upper:]' '[:lower:]'; }

is_bot() {
  local email="$1"
  case "$email" in
    *"[bot]@users.noreply.github.com" | *"+copilot@users.noreply.github.com") return 0 ;;
  esac
  return 1
}

missing=0
while read -r sha; do
  [ -z "$sha" ] && continue
  if [ "$(git rev-list --parents -n1 "$sha" | wc -w)" -gt 2 ]; then
    continue
  fi
  name="$(git show -s --format='%an' "$sha" | lower)"
  email="$(git show -s --format='%ae' "$sha" | lower)"
  if is_bot "$email"; then
    continue
  fi

  signed=0
  while IFS= read -r value; do
    value="$(printf '%s' "$value" | lower)"
    signer_name="$(printf '%s' "$value" | sed -E 's/[[:space:]]*<.*$//')"
    signer_email="$(printf '%s' "$value" | sed -nE 's/.*<([^>]*)>.*/\1/p')"
    if [ -n "$signer_email" ] && { [ "$signer_email" = "$email" ] || [ "$signer_name" = "$name" ]; }; then
      signed=1
      break
    fi
  done < <(git show -s --format='%B' "$sha" |
    git interpret-trailers --no-divider --parse |
    sed -nE 's/^signed-off-by:[[:space:]]*(.*)$/\1/Ip')

  if [ "$signed" -eq 0 ]; then
    echo "::error::commit $sha by $(git show -s --format='%an <%ae>' "$sha") has no matching Signed-off-by trailer"
    missing=1
  fi
done < <(git rev-list "$base..$head")

if [ "$missing" -ne 0 ]; then
  echo "Add sign-offs with: git rebase --signoff $base"
  echo "See CONTRIBUTING.md."
  exit 1
fi
echo "All commits signed off."
