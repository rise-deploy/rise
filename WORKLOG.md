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
9. **Split into 9a and 9b — mutation grant gate, seeded policy, and the choke
   point.** 9a (merged in PR #435) is the gate itself plus the shipped baseline
   policy: ADR-0001 §5's effective-delta subset check over policy domains,
   §6.6's label gate and its K-inheriting subtree store read, and the seeded
   `system-admin`/`resource-owner`/`org-admin` data. Nothing consulted it yet,
   mirroring 8b. 9b (implemented) is the centralized choke point replacing
   `require_operator`, the `SERIALIZABLE` write path with bounded retry, and
   list projection — plus the live `MembershipResolver`, pulled forward from
   increment 10 so the choke point has a real principal to build a snapshot
   from.
10. **Planned — identity authentication and token convergence.** Add live
   User/UserIdentity resolution, operator selection/JIT, target-bound workload
   exchange, delegated `/token`, UID checks, caps, and actor-chain handling.
   The live `MembershipResolver` moved to 9b; `authorization_details` parsing
   stays here.
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
    engine suite is 27 tests and the generated resource schemas are unchanged.
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
- Review — adversarial pass over the evaluator, fixed in the same increment:
  - **Org-admin standing ignored the recipient boundary.** A qualifying
    `org-admin` binding naming a Group of *another* org classified that Group's
    members as admins of this one, so an inert cross-org binding — which
    ADR-0001 §1 says grants nothing, and which policy auditing would report as
    contributing nothing — silently exempted them from their own org's caps.
    The predicate now requires a Group subject to carry the binding's own
    organization. Regression test confirmed failing before the fix.
  - **The membership seam was trusted without checking its contract.** A
    resolver returning an `org:` predicate among the Group ties would have
    conferred org affiliation with no `GroupMembership` behind it, and one
    returning ties or operator status for a ServiceAccount or Controller would
    have granted authority no identity resource can express. Snapshot
    construction now rejects all three, fail-closed.
  - **A truncated ancestor chain would have dropped an organization's whole
    policy tier.** The store's ancestry walk is depth-bounded and truncates at
    the root end; everything downstream reads position 0 as the root, so a
    truncated chain would evaluate the resource as belonging to no
    organization — losing its Denies, not just its Allows. Registration keeps
    real ancestry inside the bound, so this is unreachable today; the chain is
    now anchor-checked rather than assumed.
  - **One test passed for the wrong reason.** The org-admin Deny exemption case
    capped a Group the admin was not in, so it would have passed with the
    exemption removed entirely. It now caps the whole org population and asserts
    the exemption actually fired.
  - Reviewed and kept as-is: an unresolvable dynamic subject drops its whole
    binding, Denies included, which is correct because a binding with no
    resolvable subject applies to nobody — and §6.7's write-time referential
    integrity is what keeps such a value from being persisted. Wildcard
    replacement runs over step-2's collected set, so a binding made inert by a
    membership boundary does not supersede a wildcard; that is the ADR's literal
    step ordering.
  - Known costs, not defects: binding loads are one query per Role on a
    snapshot's first evaluation, and `filter_list` is O(items × bindings) of
    pure computation over cached facts. Both are bounded per request. Batching
    the role lookups fixes the first with no schema commitment and should come
    before any cross-request cache; the sequencing is tracked under `ROADMAP.md`
    § Unified identity and RBAC.
- Follow-ups this increment deliberately leaves open:
  - The centralized choke point replacing `require_operator`, the write-time
    grant gate, and seeded `system-admin`/`resource-owner`/`org-admin` data are
    increment 9. Two hazards for it: `EffectivePolicy::retained_statements` is
    RBAC only and must be combined with the ceiling before any subset
    comparison, and `Explanation` names bindings and Roles the caller may hold
    no read access to.
  - The live `MembershipResolver` over `GroupMembership` rows and configured
    operator selectors, and `authorization_details` parsing, are increment 10.
  - Policy auditing beyond the inert-binding reasons above — owners granting
    nobody, selectors matching nothing, stale references — remains open.
  - An Organization tombstone takes its policy resources with it, so its whole
    tier stops applying while the subtree drains. That is narrower than it
    sounds: membership lookup requires a live Group *and* a live Organization,
    so every member loses affiliation at the same moment, which also drops
    org-parented bindings, `org:<name>` grants, and every
    `subjectMembership: ResourceOrganization` binding — the seeded ownership
    default included. What survives is exactly operator-authored non-member
    access and Controllers, which is the case ADR-0001 §4 already settles for
    membership loss ("an explicit operator-governance case"). Ordering the
    cascade to collect policy last would preserve Denies that reach nobody, and
    denying everyone but operators would deadlock the drain on controllers that
    still need to remove their finalizers. The one real gap is `create` below a
    deleting ancestor, now tracked under `ROADMAP.md` § Resource API
    maturation, as a lifecycle rule rather than an authorization one.

## Increment 9a — the write-time grant gate and seeded policy

- State: implemented on this branch; not yet reviewed.
- Branch: `claude/milestone-9-l6139m`.
- First half of increment 9: ADR-0001 §5's grant gate and §6.6's label gate as a
  Tier-1 module (`rise-authz::engine::gate`) beside the evaluator, plus the
  shipped baseline policy as data. No enforcement change.
- Acceptance criteria:
  - One comparison serves every authorization-changing write. A change produces
    `GrantClaim`s — recipient, domain, before policy, after policy — and each is
    checked against the writer's own authority over that domain. Role bodies,
    binding create/edit/move/delete, GroupMembership, identity mappings, and
    access-driving labels all reduce to claims.
  - The comparison folds in the writer's credential ceiling. Tier 0 gained
    `unjustified_new_tuples_under_ceiling`, so an intersection that a flat
    statement list cannot express is applied tuple-wise where the algebra
    already enumerates equivalence classes.
  - `ResourceStore::label_inheriting_descendants` returns the K-inheriting
    subtree in one `WITH RECURSIVE` pass, pruning at any node that sets the key.
  - Concrete-scope containment resolves through the registered parent chain
    rather than string equality, so an Organization-scoped writer covers a grant
    on a Project beneath it.
  - `PlatformRole/system-admin` and its `system:operators` binding are seeded,
    immutable through the API, and healed when missing; `resource-owner`,
    `org-admin`, and the ownership binding are shipped defaults that seeding
    never overwrites.
  - No behavior change: `require_operator` is untouched and nothing calls the
    gate.
- Decisions:
  - **Recipients are authored subjects, not expanded identities.** A claim's
    recipient is the `subject` a binding literally carries. Expanding a
    recipient's Groups could only reveal authority they already hold, which
    shrinks the delta — the unsafe direction on a gate. It also keeps the gate
    off a second membership seam, which is the fact that changes fastest.
  - **One exception, where the arrow reverses.** An identity mapping makes the
    parent identity's *whole* policy reachable, so leaving out its Group ties
    would understate the delta. `MembershipResolver` gained `groups_for_user`
    for exactly that case.
  - **Provable reach, not exact-subject reach, on aggregation.** Authored-subject
    equality alone fails ADR-0001 scenario 29: a capped admin must be able to
    appoint another admin under the same platform Deny, and that Deny arrives
    through `org:acme`, not the appointee's name. Aggregation therefore includes
    `system:authenticated`, and `org:<O>` where the recipient's affiliation is
    provable — an org-native subject, or §5's direct admin bootstrap edge. It
    never consults live membership. Mis-modelling a Deny present in *both*
    universes cannot manufacture a grant, because it suppresses the tuple on
    both sides; a Deny the write itself changes belongs to a changed binding,
    whose subject is the claim's recipient by construction.
  - **`subjectMembership` is modelled as part of the domain, not the
    statements.** `ResourceOrganization` narrows which resources a binding
    reaches. An unclamped domain covers its clamped twin and never the reverse,
    which is what makes relaxing the clamp to `Any` register as the grant §5 says
    it is instead of a no-op diff.
  - **The writer's side is measured on the before-state.** For a label write the
    delta is computed over the domain the new value creates, while the writer is
    measured over the same resource with the *old* value pinned. Without that
    split, a resource's current owner — whose access arrives through the very
    label being replaced — could not transfer ownership, which §6.6 explicitly
    requires to work.
  - **Asymmetric containment.** An Allow counts toward the writer only when its
    domain provably covers the claim's; a Deny counts against them unless its
    domain provably misses it. Tier 0 gained `domains_provably_disjoint_with`
    for the second half, since `domain_covers` alone cannot express it.
  - **Binding universes fan out rather than guess.** Editing a `PlatformRole`,
    or writing a wildcard-scoped platform binding, can reach every organization;
    under-loading a tier would drop its Denies, and a missing Deny makes the
    before-policy look larger. `OrganizationScope::All` pays for the cold path.
  - **Scope containment consults the registry.** A scope names its leaf kind and
    its names but not the kinds between, so a prefix rule alone would let a
    scope naming a nonexistent resource claim coverage of a real one. Chains are
    resolved once per comparison into a `ScopeLattice`, keeping the predicate
    pure and the aggregation synchronous.
  - **The reserved-subject rule moved to admission.** `system:operators` used to
    be rejected in context-free `normalize()`, which made the seed unwritable
    through its own contract. Enforcing it needs the resource's name and
    placement, so it now lives in transaction-scoped admission — authoritative
    for direct store calls too — as "only the root binding named `system-admin`,
    and only with its shipped body".
  - **Immutable seeds fail startup rather than self-heal on divergence.** The
    store refuses edits and deletes, so a divergent row can only come from a
    direct database write. Repair would need a privileged path around the very
    rules that keep the rows fixed; an actionable error is more honest.
  - **Writer-side facts load once per call, not once per claim.** A §6.6 subtree
    diff produces one claim per inheriting descendant, and each measures the
    writer over its own domain, so a per-claim reload multiplied a cold path by
    the size of the subtree. The tiers, registry chains, and per-scope
    organizations are resolved together once the claims are known, and an ungated
    write — no claims — costs no reads at all. Nothing is cached beyond the call:
    a gate decision must see current facts.
- Verification:
  - `cargo fmt --all` and `cargo clippy --workspace --all-features --all-targets
    -- -D warnings` pass.
  - `cargo test --workspace --all-features -- --test-threads=1` passes: 1,204
    tests, with two ignored documentation examples.
  - Generated resource, backend-settings, and `rise.toml` schemas are unchanged,
    and both SQLX offline caches verify clean. `cargo audit` and `helm lint` were
    not run locally (neither tool is available in this environment); no
    dependency was added and the chart is untouched, so CI covers both.
  - The `rise-authz` gate suite is 30 tests; the engine suite (28) and policy
    suite (16) are unchanged.
  - The PostgreSQL-backed store suite is at 92 tests, adding the subtree read's
    pruning/tombstone/ordering behaviour and the seed's create-once,
    no-update, no-delete, placement-scoped reservation.
  - Coverage follows ADR-0001 scenarios 29–32, 34, and 39–43: the capped-admin
    appointment and its narrow-writer counterpart, per-binding Role-edit spans
    and the ungated unbound Role, Deny deletion as a grant, exact scope and
    selector containment plus parent-chain containment, clamp relaxation,
    identity mappings including the operator-standing refusal, GroupMembership
    delegation, ceiling narrowing, the unauthorized owner redirect, owner
    transfer, the subtree-wide relabel diff with a shadowing sibling excluded,
    both ungated steps, removing the last owner label in a chain, and the
    creation exception's four cases.
  - Scenario 33 (serializable with revocation) is a property of the transaction
    the gate runs inside, not of the gate; it lands with 9b's write path.
- Review — adversarial pass over the gate, fixed in the same increment. Each
  finding was reproduced as a failing test before the fix, and the
  unresolvable-scope fix was re-verified by disabling it and confirming the test
  fails again:
  - **A binding scoped below the written resource escaped the label gate.** §6.6
    step 2's applicability test was evaluated against the written resource alone,
    so a selecting binding placed *under* it was missed and the write was waved
    through ungated — even though relabelling an ancestor is precisely how such a
    binding is reached, through inheritance rather than coverage. Relabelling an
    Organization could hand a Project-scoped ownership binding to the writer's own
    Group. The early return now tests only whether *any* binding selects on the
    key; the per-resource loop applies coverage where it belongs, once the
    affected set is known.
  - **A membership write could activate a dynamic ownership grant ungated.** A
    templated binding's authored subject is `${ref.subject}`, never the subject it
    resolves to, so authored-subject aggregation could not see the seeded
    ownership rule. Adding a User to a Group that owns resources therefore
    delegated `resource-owner` over them with no check — contradicting scenario
    42's closing requirement that "a later membership write that would activate
    that ownership passes the ordinary effective-delta grant gate". Membership and
    identity-mapping claims now include templated bindings, with the domain
    narrowed twice: the selector pinned to the label value naming the subject, and
    the scope confined to a Group's own Organization, because §6.3 resolves a
    relative `group:<name>` against the matched resource's organization and so
    reaches nothing outside it. It stays intensional — no resource is enumerated.
  - **The `system:operators` reservation stopped covering org `RoleBinding`s.**
    Moving the check out of context-free `normalize()` re-added it only on the
    platform path, and a contract test was changed to assert the relaxation. Inert
    today, because §1's recipient boundary makes such a binding grant nothing —
    but admitting misleading policy on the strength of it being currently inert is
    how it stops being inert. Both binding paths now share one reservation helper.
  - **Two fail-open paths closed.** A `RoleBodyChange` for an org `Role` that
    omitted its Organization loaded no org tier, matched no binding, and produced
    no claims — an ungated Role edit; it is now an error. And
    `domains_provably_disjoint_with` concluded disjointness when a scope could not
    be resolved, silently dropping that binding's Deny from the writer's authority;
    `covers` answers "no" both for a scope that genuinely misses another and for
    one the registry cannot resolve, and only the first is evidence. Resolution is
    now required before disjointness is considered. The state is reachable:
    deleting a `ResourceDefinition` does not delete bindings whose scope named
    that kind.
  - Reviewed and kept as-is: symmetric Deny modelling across the before/after
    universes, which cannot manufacture a grant because a Deny present in both
    suppresses the tuple on both sides; an operator's short-circuited outcome
    carrying no claims, at the cost of an audit trail 9b must supply itself; and
    `get_by_name` returning tombstoned rows, which makes a draining editable
    default skip re-creation until the collector finishes rather than fail
    startup.
  - Known consequence, not a defect: because the seeded ownership binding always
    exists, the gate's membership claim is non-empty for *any* Group — the domain
    `Organization/<org> ∩ {rise.dev/owner: group:<name>}` is non-empty in
    principle even for a Group that owns nothing yet, which is what §5's
    intensional rule ("never merely over resources that exist now") requires.
    Adding a member therefore also requires holding `resource-owner` over it.

    This is a *second* condition, not the primary one. The choke point will apply
    ordinary `create` authority on `GroupMembership` under the parent Group as
    well, and that is where an organization expresses who manages membership; the
    gate only stops that authority being used to hand out more than the writer
    holds. Three principals satisfy it with no special case: an operator, an admin
    of the Group's organization, and a current member of the Group. A dedicated
    group manager who is none of those is expressible with an org-scoped binding
    carrying `labelSelector: {key: rise.dev/owner, value: group:<name>}`, whose
    domain matches the claim's exactly. An organization that finds
    member-adds-member too permissive restricts it with an org-tier Deny on
    `create` for `GroupMembership`, which its own admins ignore by tier.
  - Verified rather than assumed: an admin of an organization can manage any Group
    in it without belonging to that Group, through either delivery form §5 permits
    — a binding naming the User directly, or one naming an ordinary Group they
    belong to. Their authority is scope-only and label-independent, so it covers
    the ownership domain even though no label names them. The same coverage test
    confirms admin standing does not cross organizations.

- Follow-ups this increment deliberately leaves open:
  - 9b: the choke point replacing `require_operator`, `SERIALIZABLE` writes with
    bounded retry, list projection (scenarios 37/38), the live
    `MembershipResolver`, and Organization creation as one atomic transaction
    with its org-admin binding.
  - `GateRejection`'s witnesses include synthetic probe kinds from Tier 0's
    equivalence-class enumeration (`policy-subset-probe.invalid/…`). They mean
    "any other kind" and are correct as data, but 9b's HTTP layer must render
    them rather than echo them.
  - `Explanation` and `GateRejection` both name bindings and Roles the caller may
    hold no read access to. 9b decides what a denial actually returns.
  - 9b owes each refusal a message in the caller's terms, not the gate's. A
    membership refusal should name the operation, the authority it would delegate,
    and who can perform it — "adding user:x to group:acme/platform delegates
    get/list/update/delete over resources that Group owns; you do not hold those.
    An admin of acme, or a current member of the Group, can do this" — rather than
    a raw `(scope, selector)` pair and a tuple list. Naming the *Role* behind a
    claim would read better still, which likely means carrying the contributing
    binding's provenance on `GrantClaim`; that is deliberately left to 9b, where
    the handler shape is known, rather than guessed at here.
  - Policy auditing for semantically inert dynamic grants — a Group named as an
    owner that no longer exists, a selector matching nothing — remains open, and
    is now the natural home for explaining *why* a membership write was refused.

### Bounding an org RoleBinding's subject to its own Organization

Reviewing a worked example of org-admin-authored policy surfaced an asymmetry.
Admission fenced an org `RoleBinding`'s **scope** to its parent Organization, and
a platform binding's **static org-native subject** to that subject's own org, but
nothing constrained an org binding's own subject. So an admin of `acme` could
store a `RoleBinding` naming `group:beta/team-leads` and have it grant nothing:
policy that reads as a cross-org grant and is permanently dead.

The criterion for what admission may reject is **decidability from the stored
row**, not "is this inert right now". §6.7 keeps inert policy admissible, but
every case it protects is *contingently* inert — a membership that can change, a
selector that can match later. The recipient boundary compares the subject's
organization against the *binding's* organization, and both are frozen at write
time, so a mismatch is inert on every resource forever. `subjectMembership:
ResourceOrganization` compares against the *resource's* organization, which
varies per request, and stays admissible for exactly that reason.

`SubjectId::may_belong_to` is the one predicate both tiers read: admission
refuses on `false`, and the engine's `subject_belongs_to` uses it as the
structural arm before falling through to the live affiliation lookup. Kinds that
name no organization report `true` and stay contingent.

Controller subjects were in the original finding and are deliberately out. They
are decidable — a Controller belongs to no organization at all — but the org
opt-in enablement design (#437) would make an org-parented binding naming a
controller the natural way an org admin enables a platform-offered controller.
Shipping the rejection now means unshipping it there.

Alongside it, `RoleBindingSubject` accepts the relative form `group:<name>` on an
org `RoleBinding` and expands it against the parent before storage. Following
§6.1's precedent, it is a separate type rather than an overload of `SubjectId`,
so parsing a subject never becomes context-sensitive and only the one field where
an organization is implied accepts the short spelling. This is ergonomics — it
does not stop anyone writing another organization explicitly, which is what the
check above is for.

The coverage gap the finding named is closed too: the clamped-controller case was
asserted only on an org-contained resource. The other half — live on a
root-scoped resource, because the clamp's guard requires the resource to have an
organization — is what keeps the platform-binding combination legitimate, and it
is now pinned rather than derived.

Adversarial review of this change found no escalation or fail-open path. The
attack worth recording is the one that failed: §1's wildcard-replacement rule
keys on the *authored* subject, so a foreign-subject org binding looked like it
could be load-bearing precisely by granting nothing — suppressing a platform
wildcard Allow for its own scope. It cannot. The engine drops a binding to
`inert` before it enters the applicable set, and `apply_wildcard_replacement`
consumes only applicable bindings, so an inert binding is never a replacement
candidate. Rejecting these rows removes no expressible policy.

Three smaller findings were fixed rather than noted. The rejection message read
the subject's organization through a second, independent `unwrap_or_default()`,
which would render an empty name the moment `may_belong_to` starts refusing a
kind that carries no organization — the exact change #437 is expected to make.
`may_belong_to`'s doc claimed `controller:` was the only such kind, overlooking
`system:operators`. And `a_foreign_subject_is_inert_and_reported` builds a row
admission now refuses, which needs saying: the evaluator is what makes the
boundary a guarantee rather than a write-path convention, and legacy rows,
restores, and direct writes all still reach it.

The operator-facing claims are covered rather than asserted:
`a_foreign_subject_survives_reads_and_blocks_only_its_own_replay` pins that
admission runs on update too, that a foreign row stays readable and deletable,
that re-pointing its subject is accepted, and that the relative form is
idempotent when a read-modify-write client replays the stored spec.

## Increment 9b — the authorization choke point and the serializable write path

- State: implemented on this branch; not yet reviewed.
- Branch: `claude/milestone-9b-lpncl5`.
- Second half of increment 9: the generic resource API stops being
  operator-gated and starts being *authorized*. Every request runs ADR-0001 §4's
  algorithm against the resource it names, every authorization-changing write
  runs 9a's grant gate inside the `SERIALIZABLE` transaction that performs it,
  and `list` gains the two read granularities §4 defines.
- Acceptance criteria:
  - `require_operator` is gone. `crate::server::authz` resolves one
    `AuthenticatedPrincipal`, builds one request-local `AuthorizationSnapshot`,
    and answers one `(verb, ResourceKind, subresource?)` tuple per resource.
    Operator standing is a subject in the model, not a gate in front of it.
  - The store gained a transaction seam. `PgSession` decides where a statement
    runs — the pool, or one caller-owned transaction — and
    `SerializableTransaction` opens the unit of work ADR-0001 §5 requires. The
    gate reads through an ordinary `&dyn ResourceStore` that happens to be bound
    to that transaction, so the facts it compares are the facts the write
    commits against.
  - `StoreError::Serialization` is its own variant, mapped from SQLSTATE 40001
    and 40P01 at both the statement and the commit, and carried up as a
    `retryable` `ServerError`. The write path replays the whole operation — new
    transaction, new snapshot — up to three attempts.
  - `RiseMembershipResolver` implements the engine's one product seam: live
    Group ties from `GroupMembership` resources, and operator standing from the
    configured selectors, both read through the request's own session.
  - `list` returns per-item decisions. An item the caller cannot `list` is
    omitted and its existence masked; one they can `list` but not `get` is
    projected onto `apiVersion`, `kind`, and the documented `metadata` fields.
  - `metadata.effectiveLabels` is on every response, resolved from the same
    ancestor walk authorization performs.
- Decisions:
  - **The transaction seam is a property of the store instance, not a parameter
    on its methods.** The gate takes `&dyn ResourceStore` and calls ordinary
    reads; threading a transaction handle through every signature would have put
    a database concept into the Tier-1 contract that deliberately has none.
    `PgResourceStore::in_session` instead returns the same store bound to one
    transaction, and the existing write paths' nested `begin()` calls become
    `SAVEPOINT`s, so none of them changed.
  - **A borrowed transaction connection fails rather than waits.** A transaction
    has one connection; asking for it while it is already lent out can only mean
    a caller held a guard across a call back into the store, and waiting on a
    guard you hold yourself is a deadlock that presents as a hung request. The
    borrow is a `try_lock` with a message naming the mistake. Two places had it —
    `update`'s preflight and the two collection resolvers that finish by calling
    each other — and both now scope or drop the guard first.
  - **The schema cache is shared but not written from a transaction.** Compiled
    validators are keyed by collection, and an invalidation must reach the
    process-wide store or a committed `ResourceDefinition` edit would go
    unnoticed. Filling it from a transaction is the unsafe direction: a rolled
    back definition would leave a validator behind for a name that never
    existed, so a transaction-scoped store reads and invalidates but never
    inserts.
  - **The principal is the typed user's UID, not their email.** `user:<uid>` is
    opaque, immutable, and already what the credential's `rise_uid` carries;
    ADR-0001 §1's generated `User` resource name replaces it when identity
    resources go live, with nothing above the principal builder changing. Email
    stays what audit records are keyed by and what policy never matches on.
  - **Operator standing is derived from the configuration that governs it
    today.** ADR-0001 §1 defines an operator as an active User with a matching
    live `UserIdentity`; no login path writes those resources yet, so the
    resolver answers from `auth.operator_users` / `auth.operator_idp_groups`
    through the existing `auth::roles` path. The seam is what this increment
    owes the engine — one question, one live answer — and the derivation moves
    without changing anything above it.
  - **The membership resolver is handed the authenticated User rather than
    re-resolving it.** Authentication already resolved the credential to that
    row; what has to be live is the *membership*, which is read inside the
    transaction. It also refuses to answer for a principal other than the one it
    was built for, so a resolver can never attribute one caller's ties to
    another.
  - **A collection is masked; a named item is refused.** §4 requires a caller
    with no applicable `list` grant to receive an empty collection rather than a
    403 confirming the scope is populated. It says nothing about an item the
    caller named exactly, and Kubernetes answers that with a 403 — so a `get`,
    `update`, or `delete` without the grant is a 403 here too. The one masking
    §6.6 does require is preserved by ordering: the gate runs before the store's
    referential-integrity checks, so a refused relabel never reveals whether the
    subject it named exists.
  - **Which collections exist is visible to any authenticated caller.**
    `require_operator` used to run before path classification, so collection
    existence was operator-only. Discovery is a property of the registry rather
    than of any one resource (§8), and Kubernetes serves it to every
    authenticated caller; what a collection *contains* is authorized per item.
  - **The gate is handed the binding that will be stored, not the one that was
    posted.** A binding's `scope` and `subject` are contextually normalized
    against the parent Organization before persistence, so the change set calls
    the same `normalize()` admission calls inside the same transaction. Gating a
    different binding than the one written would be a hole rather than a
    mismatch.
  - **Undecidable means gated.** Where the change set cannot tell whether a
    write delegates authority — any changed label key, for instance — it
    produces a change and lets the gate answer. The gate returns no claims
    cheaply for a write that delegates nothing, and the alternative is a
    second, weaker applicability test living outside the engine.
  - **Refusals are rendered in the caller's terms.** 9a left this open. A
    rejection names the operation, the recipient, the domain, and the missing
    authority; the algebra's synthetic probe kinds — which stand for "every
    other kind" — are rendered as that rather than echoed as invented resource
    kinds, and a long witness list is truncated because it is a symptom rather
    than information.
  - **The operator short-circuit is audited.** An operator produces no claims,
    so `resource.grant_gate` is the only evidence the write was gated at all;
    it records the operator flag, the claim count, and the rejection count.
  - **Write audit records are deferred to the commit.** A retry rolls its
    attempt back, and a `resource.created` line for a create that never happened
    makes the trail worse than useless. Records for writes are queued on the
    context and emitted after the commit succeeds. Decisions stay inline: a
    refusal and a gate comparison both happened, whatever the transaction goes
    on to do.
  - **An unresolvable ancestry skips one item rather than failing a listing.**
    The `pending-deletion` diagnostic spans every kind, and a tombstoned row
    whose chain does not reach a root cannot be authorized — defaulting it to
    visible is the fail-open direction. It is skipped and named in the log,
    because failing the whole listing would hide every other draining resource
    behind one anomaly.
- Verification:
  - `cargo fmt --all`, `cargo clippy --workspace --all-features --all-targets --
    -D warnings`, and `cargo test --workspace --all-features` pass.
  - The generic resource API's dispatch suite covers the new behavior: masked
    collections, refused items, the list-only projection and its expansion under
    `get`, inherited `effectiveLabels` and their shadowing, and the grant gate
    refusing and permitting a delegation by the same non-operator writer.
- Follow-ups this increment deliberately leaves open:
  - **Atomic Organization creation with its org-admin binding** (ADR-0001 §5)
    stays open, and is now blocked rather than deferred: the binding names an
    "operator-selected existing User", and admission resolves a literal `user:`
    subject against a live `User` resource. None exist until increment 10
    activates identity resolution. The transaction that makes the pair atomic is
    in place; the subject it would name is not.
  - For the same reason, the subjects that can reach an ordinary caller today
    are `system:authenticated`, `org:<name>`, and whoever a `rise.dev/owner`
    label names. A binding naming a `user:` or `group:` subject is writable only
    once those resources exist.
  - A cascading delete of an Organization tombstones the Roles and bindings
    beneath it, and the gate diffs only the resource named by the request. That
    is a grant the gate does not see. It needs `delete` on the Organization,
    which is operator or org-admin authority, so nothing escalates through it
    today; closing it properly means diffing the policy resources a cascade
    would take with it.
  - `ResourceDefinition.allowedStatusControllerIds` still gates controller
    status and finalizer writes. Controllers become ordinary principals when
    their identity resources go live, which is when that allowlist can go.
  - A user's `status` write still lands in the slot named `operator:<actor>`.
    The name is now wrong for a non-operator writer; ADR-0002's subresource
    execution model owns the field separation and is where the naming is
    settled.
  - Cross-request authorization caching stays measured rather than assumed, per
    `ROADMAP.md` §1: the request-local snapshot already removes the repeated
    cost inside one request.
  - Two lifecycle levers are now reachable by a non-operator holding ordinary
    write access, and neither is an authorization escalation — both are
    availability ones, and both predate this increment. A caller who may
    `update` resource X can attach an owner reference to Y with
    `blockOwnerDeletion: true`, which makes Y's deletion wait for X to be
    collected; and a caller who may `create` can attach arbitrary non-reserved
    finalizers. Neither grants access to anything, and the second only affects
    the caller's own resource. The owner-reference case affects a resource the
    caller may hold nothing on, so it wants either an admission rule (an owner
    reference the caller cannot `use`) or policy auditing surfacing it.
