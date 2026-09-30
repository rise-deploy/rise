#!/usr/bin/env bash
set -euo pipefail

# Docker COPY can restore source mtimes older than cached Cargo artifacts.
# Refresh every workspace input so Cargo rechecks every cached feature variant.
# Third-party dependency inputs remain untouched and their artifacts reusable.
find Cargo.toml Cargo.lock crates src migrations static .sqlx -type f -exec touch {} +
