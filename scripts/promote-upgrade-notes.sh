#!/usr/bin/env bash
set -euo pipefail

# Move the Upgrade Notes "Unreleased" section under a version heading.
#
# Usage: scripts/promote-upgrade-notes.sh <version> [file]
#
# The Unreleased heading and its italic intro paragraph stay in place, followed
# by an empty section; everything else in the section moves under
# `## <version>`. When that heading already exists, the entries go at its top.
# Exits 0 without changing the file when Unreleased holds no entries.

version="$1"
file="${2:-docs/engineering/src/content/docs/upgrade-notes.md}"

tmp=$(mktemp)
trap 'rm -f "$tmp" "$tmp.status"' EXIT

awk -v ver="$version" -v out_file="$tmp" '
    { line[NR] = $0 }
    END {
        for (i = 1; i <= NR; i++) if (line[i] == "## Unreleased") { u = i; break }
        if (!u) { print "error: no \"## Unreleased\" heading" > "/dev/stderr"; exit 2 }
        n = NR + 1
        for (i = u + 1; i <= NR; i++) if (line[i] ~ /^## /) { n = i; break }
        for (i = 1; i <= NR; i++) if (line[i] == "## " ver) { v = i; break }

        # The intro is the italic paragraph opening the section.
        i = u + 1
        while (i < n && line[i] == "") i++
        intro_start = i
        if (i < n && line[i] ~ /^_/) {
            while (i < n && line[i] !~ /_$/) i++
            intro_end = i
            body_start = i + 1
        } else {
            intro_end = intro_start - 1
            body_start = intro_start
        }
        # Trim blank lines and the section separator off both ends of the body.
        while (body_start < n && line[body_start] == "") body_start++
        body_end = n - 1
        while (body_end >= body_start && (line[body_end] == "" || line[body_end] == "---")) body_end--
        if (body_end < body_start) { print "unchanged"; exit 0 }

        out = ""
        for (i = 1; i <= u; i++) out = out line[i] "\n"
        out = out "\n"
        for (i = intro_start; i <= intro_end; i++) out = out line[i] "\n"
        out = out "\n---\n\n"
        if (!v) {
            out = out "## " ver "\n\n"
            for (i = body_start; i <= body_end; i++) out = out line[i] "\n"
            out = out "\n---\n\n"
        }
        for (i = n; i <= NR; i++) {
            out = out line[i] "\n"
            if (i == v) {
                out = out "\n"
                for (j = body_start; j <= body_end; j++) out = out line[j] "\n"
            }
        }
        printf "%s", out > out_file
        print "promoted"
    }
' "$file" > "$tmp.status"
status=$(cat "$tmp.status")
rm -f "$tmp.status"

if [ "$status" = "promoted" ]; then
    cat "$tmp" > "$file"
    echo "Moved the Unreleased upgrade notes under '## ${version}' in ${file}"
else
    echo "No Unreleased upgrade notes to move"
fi
