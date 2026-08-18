#!/bin/bash
set -e

CLAUDE="${CLAUDE:-claude}"
CODEX="${CODEX:-codex}"
RELEASE_NOTES_AI="${RELEASE_NOTES_AI:-auto}"

select_release_notes_ai() {
    case "$RELEASE_NOTES_AI" in
        auto)
            if command -v "$CLAUDE" &> /dev/null; then
                RELEASE_NOTES_AI="claude"
            elif command -v "$CODEX" &> /dev/null; then
                RELEASE_NOTES_AI="codex"
            else
                return 1
            fi
            ;;
        claude)
            command -v "$CLAUDE" &> /dev/null || return 1
            ;;
        codex)
            command -v "$CODEX" &> /dev/null || return 1
            ;;
        *)
            echo "Error: --ai-cli must be one of: auto, claude, codex"
            exit 1
            ;;
    esac
}

# Check prerequisites
check_prerequisites() {
    local missing=()

    if ! command -v git &> /dev/null; then
        missing+=("git")
    fi

    if ! command -v cargo &> /dev/null; then
        missing+=("cargo")
    fi

    if [ "$SKIP_AI_RELEASE_NOTES" = false ] && ! select_release_notes_ai; then
        if [ "$RELEASE_NOTES_AI" = "auto" ]; then
            missing+=("claude or codex (AI release notes CLI)")
        else
            missing+=("${RELEASE_NOTES_AI} CLI")
        fi
    fi

    if [ ${#missing[@]} -gt 0 ]; then
        echo "Error: Missing required tools:"
        for tool in "${missing[@]}"; do
            echo "  ✗ $tool"
        done
        echo ""
        echo "Install missing tools:"
        if [ "$SKIP_AI_RELEASE_NOTES" = false ]; then
            echo "  - claude: https://github.com/anthropics/claude-code"
            echo "  - codex: https://github.com/openai/codex"
        fi
        exit 1
    fi
}

# Parse arguments
DRY_RUN=false
SKIP_AI_RELEASE_NOTES=false
AI_GUIDANCE=""
VERSION=""
COMMIT_RANGE=""

while [[ $# -gt 0 ]]; do
    case $1 in
        --dry-run|-n)
            DRY_RUN=true
            shift
            ;;
        --skip-ai-release-notes)
            SKIP_AI_RELEASE_NOTES=true
            shift
            ;;
        --ai-cli)
            if [ -z "${2:-}" ]; then
                echo "Error: --ai-cli requires a value"
                exit 1
            fi
            RELEASE_NOTES_AI="$2"
            shift 2
            ;;
        --ai-cli=*)
            RELEASE_NOTES_AI="${1#*=}"
            if [ -z "$RELEASE_NOTES_AI" ]; then
                echo "Error: --ai-cli requires a value"
                exit 1
            fi
            shift
            ;;
        --ai-guidance|--claude-guidance)
            if [ -z "${2:-}" ]; then
                echo "Error: $1 requires a value"
                exit 1
            fi
            if [ -n "$AI_GUIDANCE" ]; then
                AI_GUIDANCE="${AI_GUIDANCE}"$'\n'"${2}"
            else
                AI_GUIDANCE="$2"
            fi
            shift 2
            ;;
        --ai-guidance=*|--claude-guidance=*)
            GUIDANCE_VALUE="${1#*=}"
            if [ -z "$GUIDANCE_VALUE" ]; then
                echo "Error: ${1%%=*} requires a value"
                exit 1
            fi
            if [ -n "$AI_GUIDANCE" ]; then
                AI_GUIDANCE="${AI_GUIDANCE}"$'\n'"${GUIDANCE_VALUE}"
            else
                AI_GUIDANCE="$GUIDANCE_VALUE"
            fi
            shift
            ;;
        *)
            if [ -z "$VERSION" ]; then
                VERSION="$1"
            else
                COMMIT_RANGE="$1"
            fi
            shift
            ;;
    esac
done

# Check if version argument is provided
if [ -z "$VERSION" ]; then
    echo "Usage: $0 [--dry-run] [--skip-ai-release-notes] [--ai-cli <auto|claude|codex>] [--ai-guidance <text>] <version> [commit-range]"
    echo ""
    echo "Examples:"
    echo "  $0 0.1.4                    # Create release for version 0.1.4"
    echo "  $0 0.21.0-rc1               # Create prerelease for version 0.21.0-rc1"
    echo "  $0 --dry-run 0.1.4          # Preview release notes for next version"
    echo "  $0 --dry-run --ai-guidance \"Emphasize Helm chart changes\" 0.1.4"
    echo "  $0 --dry-run --ai-cli codex 0.1.4  # Generate notes with Codex"
    echo "  $0 --skip-ai-release-notes 0.1.4   # Tag without AI-generated notes"
    echo "  $0 --dry-run 0.1.4 v0.13.0..HEAD  # Preview notes for specific range"
    exit 1
fi

# Check prerequisites before proceeding
check_prerequisites

TAG="v${VERSION}"

# Validate version format (X.Y.Z with optional prerelease suffix)
if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[a-zA-Z0-9.]+)?$ ]]; then
    echo "Error: Version must be in format X.Y.Z or X.Y.Z-prerelease (e.g., 0.1.4, 0.21.0-rc1)"
    exit 1
fi

# Check for uncommitted changes (only in non-dry-run mode)
if [ "$DRY_RUN" = false ]; then
    if ! git diff-index --quiet HEAD --; then
        echo "Error: You have uncommitted changes in your working directory"
        echo "Please commit or stash your changes before creating a release"
        git status --short
        exit 1
    fi

    # Require develop branch
    CURRENT_BRANCH=$(git rev-parse --abbrev-ref HEAD)
    if [ "$CURRENT_BRANCH" != "develop" ]; then
        echo "Error: Releases can only be created from the develop branch (current: $CURRENT_BRANCH)"
        exit 1
    fi
fi

# Determine commit range
if [ -z "$COMMIT_RANGE" ]; then
    # Check if this is a full release (no prerelease suffix like -rc1, -beta.2, etc.)
    IS_FULL_RELEASE=false
    if ! [[ "$VERSION" =~ - ]]; then
        IS_FULL_RELEASE=true
    fi

    if [ "$IS_FULL_RELEASE" = true ]; then
        # For full releases, find the previous full version tag (skip prereleases)
        # so the changelog covers everything since the last stable release.
        PREV_TAG=$(git tag -l 'v*' --sort=-version:refname | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | head -n 1)
    else
        # For prereleases, use the most recent tag of any kind
        PREV_TAG=$(git describe --tags --abbrev=0 HEAD^ 2>/dev/null || echo "")
    fi

    if [ -z "$PREV_TAG" ]; then
        echo "No previous tag found, using all commits"
        COMMIT_RANGE="HEAD"
    else
        echo "Analyzing commits since ${PREV_TAG}..."
        COMMIT_RANGE="${PREV_TAG}..HEAD"
    fi
else
    echo "Using provided commit range: ${COMMIT_RANGE}"
fi

# Get commit messages
COMMITS=$(git log "${COMMIT_RANGE}" --pretty=format:"commit %h%n%B%n---" --no-merges)

# Generate release notes with AI analysis
echo "Generating release notes..."

if [ -z "$COMMITS" ]; then
    echo "No commits found in range ${COMMIT_RANGE}"
    RELEASE_SUMMARY="No changes in this release."
elif [ "$SKIP_AI_RELEASE_NOTES" = true ]; then
    echo "Skipping AI release notes generation (--skip-ai-release-notes)"
    RELEASE_SUMMARY="Release ${TAG}"
else
    # Create a temporary file for the analysis prompt
    TEMP_PROMPT=$(mktemp)
    cat > "$TEMP_PROMPT" << EOF
Analyze the following Git commit messages and provide a concise summary for release notes. Focus on:
1. Breaking changes (if any) - mark these clearly with ⚠️ BREAKING CHANGE
2. New features
3. Bug fixes
4. Other notable changes

Be concise and user-focused. Use markdown formatting. Start with a brief overview, then list key changes.

EOF

    if [ -n "$AI_GUIDANCE" ]; then
        cat >> "$TEMP_PROMPT" << EOF
Additional guidance:
${AI_GUIDANCE}

EOF
    fi

    cat >> "$TEMP_PROMPT" << EOF
Commits:
${COMMITS}
EOF

    # Call the selected AI CLI (required unless --skip-ai-release-notes is set).
    if [ "$RELEASE_NOTES_AI" = "claude" ]; then
        if ! RELEASE_SUMMARY=$("$CLAUDE" -p "$(cat "$TEMP_PROMPT")"); then
            rm "$TEMP_PROMPT"
            echo "Error: Failed to generate AI summary with Claude CLI."
            echo "Use --skip-ai-release-notes to bypass AI note generation."
            exit 1
        fi
    else
        TEMP_RESPONSE=$(mktemp)
        if ! "$CODEX" exec --ephemeral --sandbox read-only --color never \
            --output-last-message "$TEMP_RESPONSE" - < "$TEMP_PROMPT" > /dev/null; then
            rm "$TEMP_PROMPT" "$TEMP_RESPONSE"
            echo "Error: Failed to generate AI summary with Codex CLI."
            echo "Use --skip-ai-release-notes to bypass AI note generation."
            exit 1
        fi
        RELEASE_SUMMARY=$(<"$TEMP_RESPONSE")
        rm "$TEMP_RESPONSE"
    fi
    rm "$TEMP_PROMPT"

    if [ -z "${RELEASE_SUMMARY//[[:space:]]/}" ]; then
        echo "Error: ${RELEASE_NOTES_AI} returned empty release notes."
        echo "Use --skip-ai-release-notes to bypass AI note generation."
        exit 1
    fi
fi

# DRY RUN: Just print the release notes and exit
if [ "$DRY_RUN" = true ]; then
    echo ""
    echo "=========================================="
    echo "Release Notes Preview for ${TAG}"
    echo "=========================================="
    echo ""
    echo "${RELEASE_SUMMARY}"
    echo ""
    echo "---"
    echo ""
    echo "## Full Changelog"
    echo ""
    echo "(GitHub auto-generated changelog would appear here)"
    echo ""
    exit 0
fi

# REAL RUN: Show summary and ask for confirmation
echo ""
echo "=========================================="
echo "Release Plan for ${TAG}"
echo "=========================================="
echo ""
echo "The following actions will be performed:"
echo "  1. Update version in Cargo.toml to ${VERSION}"
echo "  2. Update Cargo.lock"
echo "  3. Commit changes with message: 'chore: bump version to ${VERSION}'"
if [ "$SKIP_AI_RELEASE_NOTES" = true ]; then
    echo "  4. Create git tag: ${TAG} (default annotation, AI notes skipped)"
else
    echo "  4. Create git tag: ${TAG} (with AI-generated notes in tag annotation)"
fi
echo "  5. Push commit and tag to origin"
echo ""
echo "Release notes preview:"
echo "---"
echo "${RELEASE_SUMMARY}"
echo "---"
echo ""

read -p "Proceed with release? (y/n) " -n 1 -r
echo
if [[ ! $REPLY =~ ^[Yy]$ ]]; then
    echo "Release cancelled."
    exit 1
fi

# ============================================================================
# MUTATION ZONE: All changes happen below this point
# ============================================================================

echo ""
echo "Creating release ${VERSION}..."
echo ""

# Step 1: Update version in Cargo.toml and Chart.yaml
echo "[1/5] Updating Cargo.toml and Chart.yaml..."
sed -i.bak "s/^version = \".*\"/version = \"${VERSION}\"/" Cargo.toml && rm Cargo.toml.bak
sed -i.bak "s/^version: .*/version: ${VERSION}/" helm/rise/Chart.yaml && rm helm/rise/Chart.yaml.bak
sed -i.bak "s/^appVersion: .*/appVersion: \"${VERSION}\"/" helm/rise/Chart.yaml && rm helm/rise/Chart.yaml.bak

# Step 2: Update Cargo.lock
echo "[2/5] Updating Cargo.lock..."
cargo update --workspace --quiet

# Step 3: Commit the changes
echo "[3/5] Committing version bump..."
git add Cargo.toml Cargo.lock helm/rise/Chart.yaml
git commit -m "chore: bump version to ${VERSION}"

# Step 4: Create the tag with AI-generated notes in the annotation
echo "[4/5] Creating tag ${TAG}..."
git tag -a "${TAG}" --cleanup=verbatim -m "${RELEASE_SUMMARY}"

# Step 5: Push commit and tag
echo "[5/5] Pushing to remote..."
git push origin develop
git push origin "${TAG}"

echo ""
echo "✓ Successfully created and pushed version ${VERSION} with tag ${TAG}"
if [ "$SKIP_AI_RELEASE_NOTES" = true ]; then
    echo "✓ AI-generated release notes were skipped (--skip-ai-release-notes)"
else
    echo "✓ AI-generated release notes stored in tag annotation"
fi
echo ""
echo "CI will now:"
echo "  - Build release artifacts via cargo-dist"
echo "  - Create the GitHub release"
echo "  - Update release notes from tag annotation (update-release-notes workflow)"
