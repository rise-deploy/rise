# ADR-0001 implementation work log

This log tracks the staged implementation of
[ADR-0001](docs/engineering/src/content/docs/adr/0001-unified-permission-model.md).
Normative behavior remains in the ADR, delivery order remains in `ROADMAP.md`,
and live rollout status remains in GitHub issue #371 and the Rise Rollout
Tracker.

## Run configuration

- Operating mode: feature branch and draft PR per independently reviewable increment.
- Base branch: `develop`.
- Merge policy: never merge without explicit user approval; pause after each PR for review.
- Sub-agent dispatch: native sub-agents.
- Review gate: maximum-effort adversarial review for authorization/security changes.
- Verification: focused tests first, then the repository-mandated Rust checks relevant to the diff.
- Deployment: CI only; no manual deployment is in scope.
- Status and escalation: this file plus milestone recaps in the conversation.
- Canonical backlog: `ROADMAP.md`; live rollout gate: GitHub issue #371.

## Staged delivery

The sequence is re-sized after each review, while preserving the ADR's complete
scope and avoiding dead-end compatibility layers.

1. **Ready for draft-PR review — dependency-light resource authorization foundation.** Move
   the `ResourceStore` contract and canonical `SubjectId`, `SubjectRef`,
   `ResourceKind`, and `Scope` types into `rise-resource-api`; keep SQLX and
   PostgreSQL adapters in `rise-resource-store`. Cover parsing, canonicalization,
   rejection behavior, serialization, and store-implementation compatibility.
2. **Planned — policy resources and pure policy algebra.** Add the Role/binding
   schemas, validation, Allow/Deny tuple evaluation, wildcard replacement, and
   subset checks with database-free conformance coverage.
3. **Planned — identity resources and storage projections.** Register the
   built-in identity kinds and add the ADR-required uniqueness and reverse-lookup
   indexes with migration/integration coverage.
4. **Planned — live authorization engine.** Add membership expansion, org-admin
   classification, effective labels, tier filtering, per-item list filtering,
   request-local snapshots, and explain/audit foundations.
5. **Planned — mutation grant gate and seeded policy.** Add serializable
   authorization-changing writes, label-subtree deltas, bootstrap policy, and
   the centralized generic-resource authorization choke point.
6. **Planned — identity authentication and token convergence.** Add live
   User/UserIdentity resolution, operator selection/JIT, target-bound workload
   exchange, delegated `/token`, UID checks, caps, and actor-chain handling.
7. **Planned — full conformance and finalization.** Close every applicable
   ADR-0001 acceptance scenario, update documentation/status, and audit the
   implementation requirement by requirement.

## Increment 1 — dependency-light resource authorization foundation

- State: implementation and maximum-effort review complete; ready for draft-PR review.
- Branch: `feat/adr0001-resource-api-contract`.
- Acceptance criteria:
  - `rise-resource-api` owns the store interface and every type required to use
    it without depending on SQLX.
  - `rise-resource-store` remains the PostgreSQL/SQLX implementation and compiles
    as an implementation of the API-owned contract.
  - Canonical subject, subject-reference, qualified-kind, and scope parsing is
    fail-closed and follows ADR-0001 scenarios 1-5 where the prerequisite layer
    has enough registry context to decide.
  - Existing resource-store behavior remains covered and all downstream callers
    compile against the new ownership boundary.
  - `rise-backend-docker` depends only on `rise-resource-api` for the store
    contract and no longer depends on `rise-resource-store`.
  - No compatibility re-export leaves the contract apparently owned by
    `rise-resource-store`.
  - Focused unit tests plus the relevant workspace format, lint, and test gates pass.
- Decisions:
  - The API-owned contract includes its full signature closure: row, error,
    create/update parameters, collection metadata, path/delete results,
    validator abstraction, and shared constants.
  - PostgreSQL queries decode into a store-private SQLX row and convert to the
    API-owned row; the API crate does not gain an optional SQLX feature.
  - API store errors carry an opaque backend source; PostgreSQL maps SQLX errors
    at the adapter boundary while preserving the source chain and HTTP 500 behavior.
  - Canonical-type parsing proves syntax and canonical representation only.
    Registered-kind resolution, subject/target existence, parent-chain checks,
    binding defaults/placement, and same-transaction visibility remain atomic
    write-time validation in later increments. PR 1 must not add a non-atomic
    `validate(&ResourceStore)` convenience.
  - Exact `ResourceKind` excludes policy wildcards, and `Scope` has no default
    because omission depends on binding placement and subject type.
- Verification:
  - `mise run lint` passed, including all-target checks, Clippy with warnings
    denied, formatting, SQLX cache validation, resource-schema consistency,
    Helm lint, and the backend auth/core suites.
  - Backend settings, `rise.toml`, and CRD generated-schema checks passed.
  - `cargo test --workspace --all-features -- --test-threads=1` passed: 1,054
    tests, with two ignored documentation examples. The first parallel run
    exposed an existing shared-name race in a runtime-sync lease test; that test
    and the complete runtime-sync crate both passed in isolation and serially.
  - The PostgreSQL-backed `rise-resource-store` integration suite passed all 56
    tests, including contract invocation and version/path behavior.
  - The normal dependency tree for `rise-resource-api` contains neither SQLX
    nor the dev-only JSON Schema validator.
- Review:
  - Three independent investigations established the contract closure and
    syntax-versus-transactional-validation boundary before implementation.
  - Maximum-effort adversarial review covered correctness, security,
    dependency ownership, cleanup, and solution altitude. Confirmed findings
    were fixed, including exhaustive subject classification, constrained JSON
    Schemas, restored store-contract semantics, pure validator errors,
    fail-closed row conversion, and consistent HTTP response conversion.
  - Three independent focused reviewers found no issues in the final fix layer.
- PR: pending.
