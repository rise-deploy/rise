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
3. **Merged in PR #418 — policy resources and pure policy algebra.** Add the
   Role/binding schemas, validation, Allow/Deny tuple evaluation, wildcard
   replacement, and subset checks with database-free conformance coverage.
4. **Merged in PR #419 — identity resource contracts.** Add the closed identity,
   membership, and workload-trust schemas plus reserved built-in collection
   definitions, without making the resources writable yet.
5. **Merged in PR #420 — generic lifecycle owner references.** Add the canonical
   owner-reference DAG, cascading collection, blocker reporting, and structured
   lifecycle logging required before GroupMembership activation.
6. **Merged in PR #421 — identity activation and storage projections.** Add the
   transaction-owned admission seam, activate all eight identity built-ins,
   and add the ADR-required identity/trust/membership indexes and narrow lookup
   adapters. Policy activation remains a separate reviewable increment.
7. **Merged in PR #430 — policy activation.** Activate the four policy built-ins through
   contextual normalization, reference validation, and concurrency-safe
   admission without yet enforcing live authorization.
8. **Merged in PR #432 (8a) — live authorization engine.** Add membership expansion,
   org-admin classification, effective labels, tier filtering, per-item list
   filtering, request-local snapshots, and explain/audit foundations. Split into
   8a (generic label and ancestry store surface) and 8b (the engine itself).
9. **Planned — mutation grant gate and seeded policy.** Add serializable
   authorization-changing writes, label-subtree deltas, bootstrap policy, and
   the centralized generic-resource authorization choke point.
10. **Planned — identity authentication and token convergence.** Add live
   User/UserIdentity resolution, operator selection/JIT, target-bound workload
   exchange, delegated `/token`, UID checks, caps, and actor-chain handling.
11. **Planned — full conformance and finalization.** Close every applicable
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

- State: merged in PR #418 at commit
  `86c7b9a900204a11f32ea733826ff637f9569737`.
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
- PR: #418, merged.

## Increment 4 — identity resource contracts

- State: merged in PR #419 at commit `a20c32c`.
- Branch: `feat/adr0001-identity-contracts`.
- Acceptance criteria:
  - `rise-resource-api` owns closed serialized contracts for `User`,
    `UserIdentity`, `Controller`, `ControllerTrustPolicy`, `Group`,
    `GroupMembership`, `ServiceAccount`, and `ServiceAccountTrustPolicy`.
  - User and UserIdentity `active` fields default to `true` when omitted and
    reject explicit `null`; User profile fields remain optional and
    non-authoritative.
  - External issuers are canonical absolute HTTP(S) URLs without credentials,
    query, fragment, or trailing slash; external subjects remain opaque and
    case-sensitive. Both are bounded so the future composite B-tree key has a
    safe index-entry budget.
  - Group membership is an empty marker named for the canonical User. Optional
    UID-bound lifecycle owner references are deferred to the generic resource
    GC follow-up; workload trust policies require public issuer and claim
    constraints, including a non-empty audience constraint.
  - The eight built-in collection definitions are reserved with their ADR-fixed
    root, Organization-owned, or fixed-parent placement, but are not registered
    as writable runtime resources in this increment.
  - Custom ResourceDefinitions cannot claim either a reserved collection or
    any kind in the platform-reserved `rise.dev` API group.
  - Serde and JSON Schema tests cover valid shapes, defaults, closed-object
    rejection, invalid references, and malformed trust constraints.
  - Focused tests plus the relevant workspace format, lint, schema, and test
    gates pass.
- Decisions:
  - Split contracts from activation after independent review of the current
    store boundary. `SpecValidator` is pure and pre-transactional, while these
    resources require atomic normalization, parent/reference checks,
    immutability and delete guards before persistence.
  - Do not add PostgreSQL indexes, identity lookup adapters, runtime registry
    entries, authentication behavior, or compatibility aliases in this
    increment. They land with the transaction-owned admission seam so invalid
    security data cannot be persisted between preparatory releases.
  - Issuer parsing uses one validated API-owned type for UserIdentity and both
    trust-policy kinds. It permits HTTP for development and service-network
    issuers, rejects noncanonical URL aliases with the required spelling in the
    error, and caps the canonical ASCII URI at 1,024 bytes so Serde and JSON
    Schema enforce the same indexed representation budget. External subjects
    are nonblank, otherwise opaque, and capped at 255 Unicode scalar values.
  - `IDENTITY_KIND_DEFINITIONS` is the placement source that activation must
    consume, rather than copying an independent identity registry table.
  - This increment blocks new ResourceDefinition conflicts. Activation must
    also fail closed after scanning for conflicting definitions persisted before
    these reservations existed; doing that scan before any routing change would
    break upgrades without removing a current conflict.
  - `ResourceDefinition` remains a globally reserved structural kind: the store
    projection and dedicated lifecycle discriminate on that kind, so admitting
    an ordinary custom resource with the same name in another group would create
    data that cannot follow the generic lifecycle safely.
  - Activation must convert trust constraints into `rise-backend-auth` matcher
    inputs with cross-crate conformance tests, keeping resource-shape validation
    and runtime claim matching from drifting.
- Verification:
  - `cargo fmt --all` and `git diff --check` pass.
  - `mise run lint` passes, including all-feature checks, strict Clippy, SQLX
    metadata, generated resource-schema consistency, Helm lint, and the auth
    and backend-core suites.
  - The serial all-features workspace suite passes with offline SQLX metadata,
    including all 59 PostgreSQL-backed integration tests.
  - Focused API contract tests cover all eight identity shapes, URL/schema
    parity, defaults, bounds, references, placement, and reservations.
- Review:
  - Maximum-effort adversarial review covered API construction boundaries,
    Serde/schema parity, PostgreSQL index budgets, reservation semantics,
    activation scope, and upgrade compatibility.
  - Confirmed findings fixed before publication include unchecked public
    strings, invalid defaults/aliases, unbounded future index keys, issuer URL
    aliases and overlong inputs, reserved API-group bypasses, generic
    ResourceDefinition creation bypassing admission, and updates to legacy
    definitions becoming frozen by newly introduced reservations.
  - Activation remains deliberately deferred: the runtime registry and lookup
    adapters are unchanged, and the next increment must audit pre-existing
    conflicts transactionally before enabling identity routes.
- PR: #419, merged.

## Increment 5 — generic lifecycle owner references

- State: merged in PR #420 at commit `718b8a8`.
- Branch: `feat/resource-owner-references`.
- Acceptance criteria:
  - The generic create, update, response, row, and store contracts carry optional
    typed `metadata.ownerReferences` without activating identity resources.
  - References are UID-authoritative and admitted only when API group/kind,
    canonical name, and UID identify the same live resource; duplicate owner
    UIDs and lifecycle cycles are rejected.
  - Cycle detection treats structural parent edges and owner-reference edges as
    one DAG and is serialized for edge-creating writes.
  - Owner deletion stamps direct owner-reference dependents, respects their
    finalizers, and optionally keeps the owner observable until explicitly
    blocking dependents drain.
  - `resources.owner_references` is the only persisted representation. A GIN
    containment index supplies reverse UID lookup; there is no edge table,
    trigger projection, or application dual-write to keep synchronized.
  - GroupMembership's optional matching-User owner rule remains deferred to
    transaction-scoped identity admission and runtime activation.
- Decisions:
  - Keep lifecycle ownership separate from structural parentage and from
    authorization. It does not change URLs, subject expansion, policy matching,
    or grants.
  - Multiple owners are supported generically. Deletion of any owner starts
    dependent collection, while removing owner references explicitly leaves the
    resource independent.
  - Owner references default to non-blocking cleanup. An explicit
    `blockOwnerDeletion: true` retains the owner's aggregate cascade finalizer;
    structural children remain inherently blocking.
  - An operator-only `deletion-blockers` subresource reports the concrete
    structural and cross-tree blockers from one repeatable-read snapshot.
    Newly tombstoned dependents best-effort emit one structured
    `resource.deletion_cascaded` audit log after commit; durable delivery is
    explicitly deferred to an outbox/Event increment.
  - `Organization` and `ResourceDefinition` cannot be cross-tree dependents
    until their kind-specific deletion admission moves into the generic
    lifecycle transaction. They remain valid owners.
  - Resource names are immutable. Bootstrap creates the configured default
    Organization only when none exist and fails startup when existing names do
    not match, rather than exposing a generic rename operation.
  - Serialize edge-creating/replacing writes for cycle admission. Deletion and
    collection synchronize through referenced owner rows rather than the global
    graph lock, so unrelated cascades remain concurrent. Empty ordinary writes
    and edge removal retain the existing concurrent fast path.
  - Add the owner-reference column and constraint separately from concurrent
    GIN index construction and constraint validation, avoiding a long write
    outage on a populated resource table.
- Verification:
  - API contract tests pass, including Serde/JSON Schema parity and support for
    ResourceDefinition DNS-subdomain names.
  - Focused PostgreSQL tests pass for persistence, replacement, indexed
    cascading, finalizer blocking, stale identity and duplicate rejection,
    mixed structural/owner-reference cycles, protected-kind admission, the
    add-reference/delete race, and fail-closed default Organization bootstrap
    matching.
  - The all-features workspace check and strict Clippy pass.
  - The serial all-features workspace suite passes, including all 62
    PostgreSQL-backed resource-store integration tests.
  - SQLX metadata verification, generated resource-schema consistency, and
    Helm lint pass.
- PR: #420, merged.

## Increment 6 — identity activation and storage projections

- State: merged in PR #421 at commit
  `39a7daa31d3202b3abc6421ad1dc6f0c7aaddaec`.
- Branch: `feat/resource-identity-activation`.
- PR: #421.
- Acceptance criteria:
  - Keep the public `ResourceStore` and pure/local `SpecValidator` contracts
    unchanged. Contextual built-in admission is selected by the immutable exact
    registration and owns all database reads and row locks in the mutation
    transaction, including direct-store calls with no trustworthy validator.
  - Activate exactly the eight kinds in `IDENTITY_KIND_DEFINITIONS`, consuming
    that table for placement. Persist typed canonical specs and defaults; lock
    exact live parents; enforce immutable external mapping identities and the
    optional matching-User GroupMembership owner rule.
  - Audit all legacy definitions and rows that route activation could shadow,
    including tombstones, before activation. Install a durable definition
    guard and actionable remediation failure without reserving the four policy
    collections in this increment.
  - Add the three exact partial expression indexes for live UserIdentity
    uniqueness, target-and-issuer trust lookup, and reverse membership lookup.
    Serialize/recover no-transaction concurrent index migrations across their
    SQLX bookkeeping crash window and fail closed on recorded index drift.
  - Export only narrow concrete Postgres identity, workload-trust, and
    membership lookup adapters. Do not add generic JSON filtering, JIT, token
    exchange, policy evaluation, or grant enforcement.
  - Cover exact routing/placement, validator bypass, canonical persistence,
    failed-write atomicity, upgrade conflicts and guards, concurrent mapping
    uniqueness, membership ownership/deletion races, lookup decoys and
    tombstones, maximum index keys, and query/index-plan conformance.
- Decisions:
  - Lock ordering for every write that adds lifecycle edges is global graph
    lock, referenced owners and structural parents, then the dependent row.
    Fresh creates skip only the recursive cycle walk because their new UID
    cannot already be reachable; retaining the graph lock prevents owner/parent
    lock-order inversions with concurrent updates.
  - External identity fields and complete workload trust matchers are immutable;
    retargeting is delete plus create. An intentionally unowned membership can
    still receive ordinary metadata/finalizer updates after its named User is
    deleted, but membership expansion requires a current live User of that name.
  - Each concurrent index has its own one-statement no-transaction migration.
    The migration runner uses SQLX's public database migration lock across
    structural index/version reconciliation and the migration pass, then runs
    SQLX with its nested lock disabled. This preserves rolling-upgrade
    serialization without copying SQLX's internal lock-ID algorithm.
  - UID generation continues to use UUID v4 in this increment. Enforcing
    `u`-prefixed ULIDs is explicitly deferred until the resource identity
    contract and migration path are designed together.
- Verification:
  - `cargo fmt --all` passes.
  - `RUSTC_WRAPPER= cargo check -p rise-resource-store-postgres --all-targets`
    passes.
  - `SQLX_OFFLINE=true RUSTC_WRAPPER= cargo check --workspace --all-features
    --all-targets` passes.
  - `SQLX_OFFLINE=true RUSTC_WRAPPER= cargo clippy --workspace --all-features
    --all-targets -- -D warnings` passes.
  - `RUSTC_WRAPPER= cargo test -p rise-resource-api --test identity_contracts`
    passes all 10 tests.
  - The PostgreSQL-backed `rise-resource-store-postgres` integration suite
    passes all 75 tests serially, including the new activation, migration,
    concurrency, lookup, key-budget, and EXPLAIN coverage.
  - `SQLX_OFFLINE=true RUSTC_WRAPPER= cargo test --workspace --all-features
    -- --test-threads=1` passes serially; 2 documentation tests are ignored.

## Increment 7 — policy activation

- State: merged in PR #430 at commit
  `71baf3f`.
- Branch: `claude/policy-activation-admission-1ojwzz`.
- Stacked on increment 6 (PR #421), whose admission seam it generalizes.
- Acceptance criteria:
  - Activate exactly the four kinds in `POLICY_KIND_DEFINITIONS`, consuming that
    table for placement: `Role` and `RoleBinding` under an Organization,
    `PlatformRole` and `PlatformRoleBinding` at the root.
  - Keep the public `ResourceStore` and pure/local `SpecValidator` contracts
    unchanged. Contextual admission owns every database read and row lock it
    needs inside the mutation transaction and stays authoritative for direct
    store calls with no trustworthy validator.
  - Normalize each binding contextually and persist the normalized spec: an org
    binding's omitted `scope` becomes its parent Organization's scope, a
    platform binding's becomes its static org-native subject's organization or
    `*`, and `subjectMembership` persists as PascalCase `Any` when omitted while
    explicit null and unknown values fail closed.
  - Resolve every reference a binding carries against live rows in the same
    transaction: `roleRef` at the exact placement its kind implies, `scope`
    down a registry- or ResourceDefinition-derived parent chain, and any
    literal `subject`. Enforce both containment rules on the resolved rows.
  - Audit legacy definitions and rows that policy route activation could shadow
    and install the durable four-collection reservation guard, with the same
    single-transaction, actionable-diagnostic behavior as increment 6.
  - Add no authorization evaluation, grant gate, seeded policy, or binding
    index. Nothing consults these resources yet.
- Decisions:
  - `IdentityAdmission` generalizes to `BuiltInAdmission`, splitting the
    identity and policy contracts into sibling modules behind one seam. The
    contextual `admit_*` methods now take `&mut` params, because policy
    normalization rewrites the spec that gets persisted; identity admission,
    which only validates, is unaffected.
  - Canonicalization stays split rather than moving wholesale into the
    transaction. A Role body and a binding's syntax are context-free and fail
    before any lock is taken; only the parts that genuinely need a parent
    Organization, a subject, or a live target run inside it.
  - A binding's references must resolve at write time. This follows the ADR's
    explicit "the target must exist or be created in the same atomic
    transaction" for `scope`, its "nonexistent literals are rejected" for
    subjects, and increment 6's precedent for GroupMembership's live User.
    Lifecycle after the fact is unchanged: deleting a referenced Role leaves a
    dangling binding exactly as deleting a User leaves a dangling membership,
    which owner references — not write-time validation — are the answer to.
  - Policy specs get no immutable-field enforcement. ADR-0001 governs Role and
    binding edits through the write-time grant gate's effective before/after
    delta, not by freezing fields, so adding immutability here would contradict
    the increment that implements it.
  - Reference locks are `FOR SHARE` and sit in the same band as increment 6's
    identity lookups: after the global graph lock, owners, and structural
    parents, and before the dependent row lock.
  - No storage projection for bindings. The identity indexes serve a
    per-request login path; the authorization engine's binding access patterns
    are not yet measurable, and a speculative index would be a schema
    commitment made blind.
- Verification:
  - `cargo fmt --all` passes.
  - `cargo clippy --workspace --all-features --all-targets -- -D warnings` passes.
  - The PostgreSQL-backed `rise-resource-store-postgres` integration suite
    passes all 83 tests serially, including the nine new policy routing,
    normalization, reference-resolution, containment, subject-resolution,
    update-renormalization, concurrency, and migration-guard tests.
  - `cargo test --workspace --all-features -- --test-threads=1` passes.

## Increment 8a — generic resource labels and ancestry

- State: merged in PR #432 at commit `ecb32b6`.
- Branch: `claude/policy-activation-admission-1ojwzz` (restarted from `develop`
  after #430 merged).
- First half of increment 8: the generic store surface the authorization engine
  reads, with no authorization semantics of its own.
- Acceptance criteria:
  - Resources carry `metadata.labels` end to end — request bodies, storage,
    responses, and the typed envelope — validated against the same `LabelKey`
    grammar a binding's `labelSelector` parses, with bounded single-line values.
  - `ResourceStore` gains `ancestors(uid)`, returning the root-first structural
    chain including the leaf in one query.
  - No authorization behavior changes. `require_operator` is untouched and no
    label is consulted for access.
- Decisions:
  - Labels live in a dedicated column, not inside `metadata`. That column
    stores exactly the annotations map and is read back as a flat
    string-to-string map, so nesting a label object inside it would break every
    read and require rewriting every row; a new column is additive and needs no
    backfill.
  - `effectiveLabels` is deliberately *not* a store operation. ADR-0001 §6.1
    resolves it nearest-wins over an already-fetched ancestor chain, which makes
    it a pure function; only the chain itself needs SQL.
  - `ancestors` is a `ResourceStore` trait method rather than a side adapter.
    The ADR puts tree and label reads on the existing trait and reserves narrow
    Postgres adapters for the identity lookups. It has no default
    implementation: a defaulted method returning an empty chain would let a
    forgotten implementor fail open later.
  - Binding collection needs no new store method or index. Org `RoleBinding`s
    are parented under their Organization and `PlatformRoleBinding`s at the
    root, so the existing `list` reads serve them through the existing
    `(parent_uid, group, kind)` indexes. This settles the index question
    deliberately deferred in #430.
  - Label writes are ungated in this increment, which is safe only because the
    generic resource API is still operator-only. ADR-0001 §6.6's write gate for
    access-driving labels must land with the increment-9 choke point, before
    the API opens to non-operators.
- Verification:
  - `cargo fmt --all` and `cargo clippy --workspace --all-features --all-targets
    -- -D warnings` pass.
  - `cargo test --workspace --all-features -- --test-threads=1` passes; the
    PostgreSQL-backed store suite is at 86 tests and the backend suite at 652.
  - New coverage: label round-tripping and validation at the store and HTTP
    layers, the database shape constraint, the ancestor chain (deep chain, root
    resource, unknown UID, tombstoned ancestors, labels carried along), and an
    EXPLAIN assertion that both binding-collection reads stay index-served.
  - Generated resource schemas regenerated for the envelope change.

## Increment 8b — the live authorization engine

- State: implemented on this branch; not yet reviewed.
- Branch: `claude/milestone-8b-4y4scb`.
- Second half of increment 8: ADR-0001 §4's algorithm as a Tier-1 module
  (`rise-authz::engine`) beside the pure Tier-0 algebra, testable end to end
  against fakes.
- Acceptance criteria:
  - The evaluator runs §4 steps 1–5 for one resource: membership expansion,
    binding collection against `effectiveLabels`, wildcard replacement, Deny
    filtering by placement tier, and the token authorization-detail ceiling.
  - Its entry point accepts only a typed `AuthenticatedPrincipal`; Group and
    virtual subjects are rejected at construction rather than in the evaluator.
  - `MembershipResolver` is the engine's only product-specific seam. Group ties
    and operator status arrive through it; every other fact is an ordinary
    `ResourceStore` read.
  - Org-admin standing is the exact structural predicate — org-root placement,
    no selector, `PlatformRole/org-admin` — computed before Deny filtering and
    never inferred from the Role's current statements.
  - Collections filter per item with `list` and `get` decided independently.
  - One immutable `AuthorizationSnapshot` per request memoizes membership,
    standing, and loaded bindings. Nothing is reused across requests.
  - Explain output retains every contribution's binding UID and tier, including
    Denies the caller's tier ignored, plus the bindings that target the caller
    but grant nothing.
  - No behavior change: `require_operator` is untouched, nothing calls the
    engine yet, and no seed data or write gate is added.
- Decisions:
  - Tier 0 and Tier 1 stay one crate with a hard module boundary, as ADR-0001's
    implementation structure leans. `policy` gained no dependency; `engine`
    depends on `rise-resource-api` and nothing else.
  - Binding collection reads the existing `ResourceStore`: platform bindings at
    the root and org bindings under the target's Organization. Write-time
    containment makes that pair complete for any resource in that org, so no
    scope index is needed — the question deferred in #430 and settled in #432.
  - The evaluation target is an ancestry chain of `(kind, name, labels)` rather
    than a UID. A create request has no row yet, so it supplies the proposed
    leaf; a list item reuses its siblings' ancestry. One target shape serves
    every verb, and the whole algorithm is exercisable without a store.
  - An operator's Allow is hardcoded in the evaluator, not read from the seeded
    `system-admin` binding. ADR-0001 §1 requires the guarantee to survive a bad
    restore or a direct database write, which a row cannot.
  - The token ceiling lands here rather than with token convergence. It is step
    5 of the algorithm; parsing `authorization_details` into `AuthorizationCap`
    stays authentication-plane work in `rise-backend-auth`.
  - Stored policy that no longer parses fails the request instead of being
    skipped. Skipping would silently drop that row's Deny statements, turning
    corrupt data into a privilege gain. A dangling `roleRef` is different and
    resolves to no statements: ADR-0001 deletes a Role without deleting its
    bindings.
  - Inert-binding reporting is deliberately narrow — only bindings that match
    the caller and are then removed by a recipient boundary, a
    `subjectMembership` clamp, or an unresolvable template. Reporting every
    binding aimed at someone else would bury the cases that answer "why don't I
    have access?".
  - `LocallyNormalized{,Platform}RoleBindingSpec` gained `Deserialize` with
    `scope` and `subjectMembership` required. Admission always persists both, so
    a row missing either never passed admission and now fails closed instead of
    being re-defaulted at evaluation time.
- Verification:
  - `cargo fmt --all` and `cargo clippy --workspace --all-features --all-targets
    -- -D warnings` pass.
  - `cargo test --workspace --all-features -- --test-threads=1` passes; the new
    engine suite is 24 tests and the generated resource schemas are unchanged.
  - Coverage follows the ADR's acceptance scenarios 11–23: Allow union and
    default deny, retained Deny wins, platform Deny reaching an admin, org Deny
    exempting only that org's admins, operator ignoring every Deny, Deny
    provenance surviving replacement, live Group expansion, the group-tie
    requirement and its direct-admin bootstrap exception, foreign-subject
    inertness, the `ResourceOrganization` clamp and Controller exclusion,
    absolute `org:` subjects on a root resource, authored-form wildcard
    collision, value-narrowed selectors, and multi-org admin standing. Plus
    nearest-wins ownership through `effectiveLabels`, per-item list filtering
    with independent `get`, ceiling narrowing including for operators, explain
    retention, tombstoned/dangling bindings, corrupt policy, scope subtree
    matching, and a guard that no mutex guard is held across an await.
- Follow-ups this increment deliberately leaves open:
  - The centralized choke point replacing `require_operator`, the write-time
    grant gate, and seeded `system-admin`/`resource-owner`/`org-admin` data are
    increment 9.
  - The live `MembershipResolver` over `GroupMembership` rows and configured
    operator selectors, and `authorization_details` parsing, are increment 10.
  - Policy auditing beyond the inert-binding reasons above — owners granting
    nobody, selectors matching nothing, stale references — remains open.
