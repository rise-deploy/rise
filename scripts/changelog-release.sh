#!/usr/bin/env bash
set -euo pipefail

# Add a released version's page to the operator docs changelog.
#
# Usage: scripts/changelog-release.sh <version> <release-notes-file> [date]
#
# Writes docs/engineering/src/content/docs/changelog/v<version>.md (dots become
# dashes) from the release notes, followed by the "Unreleased" section of the
# changelog's "Upcoming releases" page as the version's upgrade notes. That
# section is then emptied, keeping its heading and italic intro paragraph.

version="$1"
notes_file="$2"
date="${3:-$(date -u +%Y-%m-%d)}"

dir="docs/engineering/src/content/docs/changelog"
upcoming="${dir}/index.md"
page="${dir}/v${version//./-}.md"

if ! [[ "$version" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
    echo "Error: changelog pages are for full releases (X.Y.Z), got '${version}'"
    exit 1
fi
# Newest release first: Starlight sorts sidebar entries by ascending order, and
# the Upcoming releases page holds order 0.
order=$((1000000000 - BASH_REMATCH[1] * 1000000 - BASH_REMATCH[2] * 1000 - BASH_REMATCH[3]))

upgrade_notes=$(mktemp)
rest=$(mktemp)
trap 'rm -f "$upgrade_notes" "$rest"' EXIT

# Split the Upcoming releases page: the Unreleased entries go to
# $upgrade_notes, the page without them to $rest.
awk -v notes="$upgrade_notes" '
    { line[NR] = $0 }
    END {
        for (i = 1; i <= NR; i++) if (line[i] == "## Unreleased") { u = i; break }
        if (!u) { print "error: no \"## Unreleased\" heading" > "/dev/stderr"; exit 2 }
        n = NR + 1
        for (i = u + 1; i <= NR; i++) if (line[i] ~ /^## /) { n = i; break }

        # Keep the italic intro paragraph opening the section.
        i = u + 1
        while (i < n && line[i] == "") i++
        if (i < n && line[i] ~ /^_/) {
            while (i < n && line[i] !~ /_$/) i++
            keep = i
        } else {
            keep = u
        }
        # Trim blank lines and the section separator off both ends.
        s = keep + 1
        while (s < n && line[s] == "") s++
        e = n - 1
        while (e >= s && (line[e] == "" || line[e] == "---")) e--

        for (i = s; i <= e; i++) print line[i] > notes
        for (i = 1; i <= keep; i++) print line[i]
        if (n <= NR) {
            print ""
            print "---"
            print ""
            for (i = n; i <= NR; i++) print line[i]
        }
    }
' "$upcoming" > "$rest"

# Drop a leading "Release Notes" heading and the download table cargo-dist
# appends; the GitHub release carries the downloads.
release_notes=$(awk '
    !n && /^[[:space:]]*$/ { next }
    !n && /^#+[[:space:]]+[Rr]elease [Nn]otes/ { next }
    /^## Download / { exit }
    { line[++n] = $0 }
    END {
        while (n > 0 && (line[n] ~ /^[[:space:]]*$/ || line[n] == "---")) n--
        for (i = 1; i <= n; i++) print line[i]
    }
' "$notes_file")

{
    echo "---"
    echo "title: \"${version}\""
    echo "description: Rise ${version}, released ${date}."
    echo "sidebar:"
    echo "  order: ${order}"
    echo "---"
    echo ""
    echo "_Released ${date} · [GitHub release](https://github.com/rise-deploy/rise/releases/tag/v${version})_"
    if [ -n "${release_notes//[[:space:]]/}" ]; then
        echo ""
        echo "$release_notes"
    fi
    if [ -s "$upgrade_notes" ]; then
        echo ""
        echo "## Upgrade notes"
        echo ""
        echo "Read these before upgrading to ${version}. Badges follow the"
        echo "[impact legend](/operator-docs/changelog/#impact-legend)."
        echo ""
        cat "$upgrade_notes"
    fi
} > "$page"
cat "$rest" > "$upcoming"

echo "Wrote ${page}"
