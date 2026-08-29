#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 4 ]]; then
    echo "usage: $0 PREVIOUS_MANIFEST CURRENT_MANIFEST CHEF_GENERATION BUILT_CHEF_GENERATION" >&2
    exit 2
fi

previous_manifest=$1
current_manifest=$2
chef_generation=$3
built_chef_generation=$4

source_roots=(Cargo.toml Cargo.lock crates src migrations static .sqlx)

# Cargo fingerprints use mtimes, while a shared target cache outlives any one
# checkout. Content-identical inputs remain reusable; only new or changed
# inputs need an mtime newer than their cached artifacts.
find "${source_roots[@]}" -type f -exec sha256sum --zero {} + \
    | sort --zero-terminated > "$current_manifest"

if [[ ! -f "$chef_generation" ]]; then
    # BuildKit layer cache entries do not contain mutable target-cache contents.
    # A missing cargo-chef marker therefore represents a fresh generation.
    printf 'cache-miss-%s-%s\n' "$$" "$(date +%s%N)" > "$chef_generation"
fi

if [[ ! -f "$previous_manifest" ]] \
    || [[ ! -f "$built_chef_generation" ]] \
    || ! cmp -s "$chef_generation" "$built_chef_generation"
then
    # Cargo-chef writes workspace placeholders as well as dependencies. Every
    # workspace input must be newer whenever that recipe layer executes.
    find "${source_roots[@]}" -type f -exec touch {} +
    exit 0
fi

while IFS= read -r -d '' changed_record; do
    touch -- "${changed_record#*  }"
done < <(comm --zero-terminated -23 "$current_manifest" "$previous_manifest")

input_removed=false
while IFS= read -r -d '' previous_record; do
    if [[ ! -f "${previous_record#*  }" ]]; then
        input_removed=true
        break
    fi
done < "$previous_manifest"
if [[ "$input_removed" == true ]]; then
    # A removed compiler input has no path left to refresh. Manifests are the
    # owning package inputs, so invalidating them makes Cargo re-evaluate the
    # workspace without discarding third-party dependency artifacts.
    find Cargo.toml crates -name Cargo.toml -type f -exec touch {} +
fi
