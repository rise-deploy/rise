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

1. **Merged in PR #416 — dependency-light resource authorization foundation.** Move
   the `ResourceStore` contract and canonical `SubjectId`, `SubjectRef`,
   `ResourceKind`, and `Scope` types into `rise-resource-api`; keep SQLX and
   PostgreSQL adapters in `rise-resource-store-postgres`. Cover parsing,
   canonicalization, rejection behavior, serialization, and
   store-implementation compatibility.
2. **Merged in PR #417 — name the PostgreSQL adapter boundary explicitly.** Rename the
   concrete adapter crate to `rise-resource-store-postgres` without changing
   behavior or adding compatibility aliases.
3. **In progress — policy resources and pure policy algebra.** Add the Role/binding
   schemas, validation, Allow/Deny tuple evaluation, wildcard replacement, and
   subset checks with database-free conformance coverage.
4. **Planned — identity resources and storage projections.** Register the
   built-in identity kinds and add the ADR-required uniqueness and reverse-lookup
   indexes with migration/integration coverage.
5. **Planned — live authorization engine.** Add membership expansion, org-admin
   classification, effective labels, tier filtering, per-item list filtering,
   request-local snapshots, and explain/audit foundations.
6. **Planned — mutation grant gate and seeded policy.** Add serializable
   authorization-changing writes, label-subtree deltas, bootstrap policy, and
   the centralized generic-resource authorization choke point.
7. **Planned — identity authentication and token convergence.** Add live
   User/UserIdentity resolution, operator selection/JIT, target-bound workload
   exchange, delegated `/token`, UID checks, caps, and actor-chain handling.
8. **Planned — full conformance and finalization.** Close every applicable
   ADR-0001 acceptance scenario, update documentation/status, and audit the
   implementation requirement by requirement.

## Increment 1 — dependency-light resource authorization foundation

- State: merged in PR #416 at commit
  `95d5a1f132707b1e7c65a286d7acf287f33beef7`.
- Branch: `feat/adr0001-resource-api-contract`.
- Acceptance criteria:
  - `rise-resource-api` owns the store interface and every type required to use
    it without depending on SQLX.
  - The PostgreSQL/SQLX adapter (now named `rise-resource-store-postgres`)
    compiles as an implementation of the API-owned contract.
  - Canonical subject, subject-reference, qualified-kind, and scope parsing is
    fail-closed and follows ADR-0001 scenarios 1-5 where the prerequisite layer
    has enough registry context to decide.
  - Existing resource-store behavior remains covered and all downstream callers
    compile against the new ownership boundary.
  - `rise-backend-docker` depends only on `rise-resource-api` for the store
    contract and no longer depends on the PostgreSQL adapter crate.
  - No compatibility re-export leaves the contract apparently owned by the
    PostgreSQL adapter crate.
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
  - The PostgreSQL-backed `rise-resource-store-postgres` integration suite
    passed all 56 tests, including contract invocation and version/path
    behavior.
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
- PR: #416, merged.

## Increment 2 — explicit PostgreSQL adapter crate name

- State: merged in PR #417 at commit
  `1eabe9cc12c32fefbc71d7ed54ffddcfa2448caa`.
- Branch: `refactor/resource-store-postgres`.
- Acceptance criteria:
  - Rename the package and directory to `rise-resource-store-postgres` and
    update Rust imports to `rise_resource_store_postgres`.
  - Update workspace membership, dependency and feature wiring, lockfile,
    Docker build inputs, CI publishing guidance, SQLX/mise tasks, repository
    guidance, database documentation, roadmap wording, and ADR implementation
    records.
  - Leave no compatibility package, dependency alias, or old import path.
  - Preserve every historical migration byte-for-byte so existing SQLX
    checksums remain valid; make no runtime, schema, or store-behavior changes.
- Verification:
  - `cargo fmt --all -- --check` passes.
  - Locked Cargo metadata resolves `rise-resource-store-postgres` from
    `crates/rise-resource-store-postgres` and contains no old package.
  - SQLX-offline all-target checks pass for both the renamed adapter package
    and `rise-deploy` with `cli,backend` enabled.
  - `mise run lint` passes with the configured compiler cache disabled for the
    sandbox, including all feature combinations, Clippy with warnings denied,
    formatting, SQLX validation, resource schemas, Helm lint, and the backend
    auth/core suites.
  - Backend settings, `rise.toml`, and CRD generated-schema checks pass.
  - The full serial workspace suite accounts for 1,054 passing tests and two
    ignored documentation examples. It initially reproduced the existing
    `rise-runtime-sync` shared lease-name flake after every preceding suite had
    passed; the failing test passed alone and all 28 runtime-sync tests then
    passed serially.
  - The renamed adapter's 11 library tests and all 56 PostgreSQL-backed
    integration tests pass; all 22 `rise-resource-api` unit/integration tests
    pass.
  - The Docker planner stage builds successfully with the renamed manifest and
    source paths.
- Review:
  - Three independent finders covered line-by-line correctness,
    removed-behavior/cross-file tracing, and cleanup/solution altitude.
  - A confirmed finding caught edits to comments in an already-shipped SQLX
    migration. Because SQLX hashes the full migration text, the edits would
    have caused version-mismatch startup failures on upgraded databases; the
    historical migration was restored byte-for-byte.
- PR: #417, merged.

## Increment 3 — policy resource contracts and pure policy algebra

- State: implementation, local verification, and independent review complete;
  ready for draft PR review.
- Branch: `feat/adr0001-policy-algebra`.
- Acceptance criteria:
  - `rise-resource-api` owns the closed serialized contracts for `Role`,
    `RoleBinding`, `PlatformRole`, and `PlatformRoleBinding`, reusing its
    canonical `ResourceKind`, `Scope`, `SubjectId`, and `SubjectRef` types.
  - A new `rise-authz` crate owns a hard, database-free `policy` module for
    authorization tuple matching, Deny-wins evaluation, placement-provenance
    preservation, wildcard replacement, subject substitution, and
    scope/selector subset checks.
  - Policy syntax rejects unqualified kinds, empty pattern lists, unknown
    fields and enum values, plural subjects, invalid templates, invalid
    static/dynamic selector combinations, and explicit null defaults.
  - Omitted `PlatformRoleBinding.subjectMembership` normalizes to `Any` in the
    typed contract; binding scope normalization remains an explicit contextual
    operation because its default depends on placement, parent, and subject.
  - Database-free conformance tests cover the applicable portions of ADR-0001
    scenarios 1, 2, 4-6, 11, 15, 20, 31, 32, 49, 50, and 57.
  - Focused tests plus the relevant workspace format, lint, and test gates pass.
- Decisions:
  - Use one `rise-authz` crate with a hard `policy` module rather than splitting
    Tier 0 and Tier 1 before the engine seam exists; the pure module remains
    extractable later.
  - Reuse API-owned canonical types instead of creating a second kind/scope/
    subject vocabulary and a security-sensitive mapping layer.
  - Do not register writable policy built-ins in this increment. The current
    `SpecValidator` is deliberately pure and pre-transactional, cannot rewrite
    the JSON that the store persists, and cannot atomically validate scope,
    subject, or role references. Registration lands with a transaction-scoped
    normalization/admission seam.
  - Preserve binding placement on statement contributions, but perform live
    operator/admin classification and Deny-tier filtering in Tier 1 alongside
    membership expansion; these are not ordinary statement algebra.
- Verification:
  - Focused `rise-resource-api` and `rise-authz` tests pass with the lockfile in
    offline mode, including policy contract and algebra regression coverage.
  - `mise run lint` passes, including all-features checks, strict Clippy,
    formatting, SQLX metadata checks, generated resource schemas, Helm lint,
    and the existing auth/core tests.
  - The full all-features workspace test suite passes serially with offline
    SQLX metadata, including all 56 PostgreSQL-backed integration tests.
  - The Docker planner stage builds successfully with the new crate included.
- Review:
  - Independent reviews covered the public API and normalization boundary,
    policy-domain/subset semantics, migration and integration safety, and the
    final pinned diff.
  - Confirmed findings fixed before publication include domain widening on an
    unchanged policy, grant creation by deleting Deny statements, ordinary use
    of `system:operators`, construction of unchecked normalized values, and
    fail-open handling of concrete-scope ancestry.
