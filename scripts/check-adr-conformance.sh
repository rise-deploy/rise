#!/usr/bin/env bash
# Scenarios 1-57 of ADR-0001's appendix must each be claimed by a test via a
# `/// ADR-0001 scenario N` doc comment; deferred scenarios 58-61 must not be.
set -euo pipefail
cd "$(dirname "$0")/.."
paths=(crates/rise-authz/tests crates/rise-resource-api/tests
       crates/rise-resource-store-postgres/tests crates/rise-backend-auth/src src/server)
claimed=$( (grep -rhoE 'ADR-0001 scenario [0-9]+' "${paths[@]}" || true) | grep -oE '[0-9]+$' | sort -un)
status=0
for n in $(seq 1 57); do
  grep -qx "$n" <<<"$claimed" || { echo "missing: ADR-0001 scenario $n"; status=1; }
done
for n in 58 59 60 61; do
  grep -qx "$n" <<<"$claimed" && { echo "deferred scenario claimed: ADR-0001 scenario $n"; status=1; }
done
[ "$status" -eq 0 ] && echo "ADR-0001 conformance: scenarios 1-57 claimed, 58-61 deferred"
exit "$status"
