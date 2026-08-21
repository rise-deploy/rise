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
- Sixth review — a fifth adversarial round, over the item-path masking and the
  disclosure axis the fourth round added. Seven findings, all fixed here; the
  first two show the masking was half-built and the third retracts a claim the
  fourth round's commit message made:
  - **The masked 404 and the genuine one had different bodies.** Status codes
    matched — which is what the test asserted — while the messages read
    `"rise.dev/Organization 'acme' not found"` versus `"resource not found"`.
    The layers below have four ways of saying no (leaf missing, ancestor
    missing, wrong kind, undeclared version) and each is itself a fact about a
    resource the caller may hold nothing on. Every item-path 404 now carries one
    constant body, funnelled through `resolve_leaf`, and the test compares the
    two messages rather than just their status.
  - **The update path answered four questions before authorizing.** The
    non-storage-version 422, both name comparisons, and the finalizer screen all
    ran ahead of the decision. Two were oracles on their own — a body naming the
    wrong kind returned `400` for a resource that exists and `404` for one that
    does not, on every collection — and the finalizer screen was worse than an
    oracle: it compares against the *stored* list, so a caller with no grant at
    all could read a resource's finalizers off the difference between a `403`
    and a `404`. The `uid:` form's name comparison quoted the stored name
    outright. Authorization now runs first, before a word of the body is
    inspected, as the delete path already did.
  - **`Disclosure`'s tuple axis rested on a false premise.** The fourth round
    kept the witness list for Role and binding edits, reasoning that their
    after-side is the caller's own submitted statements. It is not:
    `claim.after` is `aggregate(...)`, the recipient's *whole* effective policy
    over the domain with the written change overlaid onto the bindings it
    touches. So removing a Deny from a Role you may edit but not read names the
    kinds a different, unrelated Role was Allowing all along. No change shape
    has request-provenance witnesses, so the axis is gone and the list is
    withheld unconditionally. The refusal loses its actionable detail; the
    `resource.grant_gate` audit record already carried the full comparison, and
    narrowing the rendered list to tuples the request body itself enumerates
    would restore some of it without the leak — recorded as a follow-up rather
    than built here, because it is new logic in a renderer whose whole job is
    not to say too much.
  - **A create leaked the existence of a parent the caller cannot read.** The
    leaf does not exist yet, so the fourth round left the create path on the
    ordinary refusal — but the answer is a fact about the *parent*, and `403`
    for a real parent against `404` for an absent one enumerates precisely the
    ancestor tree the list path masks. A refused create under an unreadable
    parent now reads as "no such path"; under a readable one it still names the
    verb.
  - **The masking fallback compared the verb instead of the tuple.** A caller
    holding full `get` on a resource but not `(get, Kind, deletion-blockers)`
    was masked, because the guard tested `verb != Get` and a subresource `get`
    is still `Get`. Not a disclosure — just a `404` for a resource the caller
    can plainly see. The guard compares `(verb, subresource)` now.
  - **An unlisted controller could probe item existence.** The allowlist check
    ran after the row lookup, so a controller token not on a collection's
    `allowedStatusControllerIds` got `404` for an absent item and `403` for a
    real one. The allowlist decides first; collection existence stays
    observable, which it already was.
  - Recorded rather than fixed: the `blockOwnerDeletion` refusal names the
    owner's kind and name, unlike the `use` refusal beside it, which is masked
    to a bare UID. It runs only after the `use` check has passed, so the caller
    has already established they may reference that owner — but the asymmetry is
    easy to widen by accident and is worth keeping in view.
- Seventh review — a sixth adversarial round, over the masking funnel and the
  reordering the fifth round did. Four findings, all fixed here; the first two
  are the fifth round's own fix, applied to update but not to create:
  - **The create path answered three body questions before authorizing.**
    Exactly the defect the previous round fixed on update: the non-storage
    422 and both body-identity 400s ran after the parent was resolved, so a
    body naming the wrong kind returned `400` under a real parent and a masked
    `404` under an absent one. Reachable on every built-in parented collection —
    `useridentities/<user>` enumerates Users, `groupmemberships/<org>/<group>`
    enumerates an organization's Groups — by a caller holding nothing. The
    checks read only the request and the collection registry, so they now run
    *before* the parent is touched at all: the same 400 comes back either way,
    and the parent is only reached once the answer can no longer depend on the
    body. The previous round's test passed because it only ever sent a
    well-formed body.
  - **Two more 404 bodies were outside the funnel.** `resolve_parent_row` maps
    `ParentNotFound` to "parent path segment not found" and has its own
    "parent resource not found", neither routed through `mask_not_found`. On a
    depth-1 create the wording coincided with the constant by luck; at depth 2
    it did not, so an Organization's existence was readable off a create under
    `groupmemberships/<org>/<group>`. Both now carry the constant, and the
    remaining five `authz.tree()` call sites that can 404 are masked too — one
    of them echoed the UID of a resource the caller was never authorized for.
  - **The finalizer screen still leaked one bit.** Authorization now precedes
    it, which closed the no-grant leak — but `update` without `get` is a real
    combination, and the screen compares against the stored list while that
    caller's write response is projected to a shape that deliberately omits
    finalizers. So 403-versus-success reported whether the resource carries any
    finalizer at all. The mismatch is now reported only to a caller who may read
    them; for anyone else the stored list is preserved silently, which is the
    only non-leaking answer and changes nothing about what persists.
  - Recorded rather than fixed: a masked 404 for an absent resource returns
    after one path query, while one for an existing-but-invisible resource also
    runs the ancestry walk and one or two policy evaluations. Status and body
    are identical; wall-clock is not. Closing that means padding every masked
    answer to the slow path, which is a real cost for a real but weak signal —
    worth stating rather than pretending the masking is perfect.
- Eighth review — a seventh adversarial round, ranging past the masking work
  into the change-set construction. Three findings, one of them the most serious
  of the whole increment and one of them a follow-up this log had already
  recorded as harmless:
  - **An owner reference was an ungated delete of any policy row.** `Role` and
    `RoleBinding` accept owner references — only `ResourceDefinition` and
    `Organization` are excluded at the store — and `change_for_update` diffs
    only the spec, so attaching one is invisible to the gate. But the edge is a
    scheduled delete: when the owner goes, the store tombstones the dependent,
    and the engine filters bindings on liveness rather than on collection, so a
    tombstoned Deny stops applying immediately. A caller refused a direct delete
    of a Deny that caps them could therefore attach it to a resource they own,
    delete that, and have the cap lifted with no gate anywhere in the sequence —
    the ordinary-authority checks on the attachment (`delete` on the dependent,
    `use` on the owner) are exactly the authority the gate exists to look past.
    Introducing a reference onto a policy row now runs the same change the
    delete would, so the two routes are refused identically.
  - **The Organization cascade was the same hole with a different lever**, and
    this log had recorded it as safe on the grounds that it needs `delete` on
    the Organization. That is the wrong test: the question is not how privileged
    the delete is but whether the resulting policy change is one the writer
    could have made directly. `load_organization_bindings` short-circuits on a
    tombstoned Organization, so the delete drops the *entire* org policy tier at
    once, Denies included, while every resource beneath it stays addressable
    until the collector drains. An Organization delete is now diffed as the
    deletion of each Role and RoleBinding beneath it — individually, so each is
    compared the way a direct delete of that row would be.
  - **Creating a `User` was an ungated activation.** The change set gated
    `active: false → true` on the update path and treated a create as bringing a
    blank identity into being. ADR-0001 §1 says the opposite in as many words: a
    `GroupMembership` and a `user:` subject bind to the *name*, deleting the
    User leaves the markers, and recreating the name reactivates them. So
    delete-and-recreate reached, ungated, exactly what the update path refuses —
    and `active` defaults to true, so an empty spec is the payload. A create
    with `active` now produces the same `IdentityMapping` change the activation
    does.
  - Also fixed, smaller: a write response gave list-granularity metadata —
    including org-wide inherited `effectiveLabels` — to a caller holding a write
    verb and *neither* read verb, on the reasoning that §4 puts those in a
    listing; §4 puts them there because `list` is the grant that says "you may
    survey this scope", which `update` does not say. There is now a third
    granularity that echoes back the caller's own input. The name-mismatch `400`
    on a `uid:` URL likewise stopped quoting the stored name. `mask_not_found`
    folds `ResourceTree`'s malformed-ancestry `400` as well, since only a row
    that exists can produce it. `record_gate` carries the attempt number, so a
    replayed write's inline gate records no longer read as separate attempts to
    delegate. And the membership resolver's operator check uses a form of the
    role lookup that reports a lost `SERIALIZABLE` race instead of failing
    closed to "not an operator" — inside a write transaction, that answer is
    decided from a transaction already doomed.
- Ninth review — an eighth adversarial round, pointed past the recently-worked
  code on the theory that it is now the hardest part. Four findings, the first of
  them in the previous round's own fix:
  - **The `User` gate was blind to exactly what it was built to catch.** Both the
    new create arm and the activation arm it copied emit an `IdentityMapping`
    change, and `identity_mapping_claims` is the one place a recipient's Groups
    are expanded — through `groups_for_user`, which resolved ties by finding a
    live, *active* `User` row of that name. At the moment either write is gated
    there is no such row: the create has not happened yet and the activation's
    stored spec still says `false`. So the tie set was empty for precisely the
    two writes whose effect is to make those ties deliver again, and the claim
    collapsed to bindings naming the User directly. A helpdesk principal holding
    Users and nothing in an organization could reactivate an offboarded name and
    hand it back its org-admin Group. The seam now resolves ties by *name*,
    which is what a name-bound marker means; the caller's own snapshot keeps the
    strict lookup, since there the question really is what they reach now.
  - Corrected while fixing it: the first test written for this passed against
    the unfixed code. It refused, but for an unrelated reason — the seeded
    ownership binding credits any recipient with `resource-owner` over the
    domains it templates, so a writer lacking `delete` is refused every
    activation whatever the ties say. The test now gives the writer every verb
    on every main resource and puts the Group's grant on a *subresource*, so the
    tie is the only thing in the delta, and it was checked to fail against the
    unfixed seam before being kept.
  - **The Organization cascade was quadratic.** Each change costs the gate two
    full loads of the binding universe, so an Organization with a few hundred
    policy rows turned one delete into hundreds of thousands of queries inside a
    `SERIALIZABLE` transaction — undeletable in practice, and holding a snapshot
    open long enough to cost concurrent policy writes their races. An operator's
    claims are all permitted, so that path skips construction entirely (mirroring
    the gate's own short-circuit, not adding a second one), and a cascade past 64
    policy rows is refused with a 409 rather than allowed to degrade into a
    timeout.
  - **A refused Organization delete named the policy rows inside it.** The
    operation phrase is prepended to every refusal outside any `Disclosure`
    check — fine when the caller addressed that row by name, a disclosure when
    the row came from a cascade they may hold no read on. Cascaded refusals now
    name the Organization.
  - **The cascade enumerated tombstoned rows.** `ResourceStore::list` returns
    them, and a tombstoned binding is already not applying, so diffing one
    charged the writer for authority that was gone and made an Organization
    containing a draining `Deny` undeletable. Fail-closed, but wrong.
  - **Raised, not changed — a platform `Deny` can be escaped by dropping a
    membership.** Subject matching consults *live* standing, so a platform-tier
    `Deny` whose subject is `org:<name>` or `group:<org>/<name>` stops matching a
    caller who leaves that organization — while their grants elsewhere survive.
    Deleting your own `GroupMembership` is ungated by ADR-0001 §4's explicit
    decision, and the equivalent move through a *binding* is correctly caught
    (`provable_affiliations` sees it). §4 also says escaping an org ceiling while
    retaining a platform grant is "an explicit operator-governance case", which
    reads as though this should not be reachable without one. Closing it means
    either gating membership removal against such Denies or confining
    `org:`-subject bindings the way `group:` ones are confined — both changes to
    the permission model rather than its wiring, and a decision for the ADR
    rather than for a review. The comment at the cascade says so at the seam.
- Tenth review — a ninth adversarial round, half aimed at the previous round's
  fix and half sent outside every area the earlier rounds had covered. Both
  halves found something, and the second found a privilege escalation that has
  nothing to do with this increment.
  - **The by-name tie lookup de-indexed itself.** The previous round's new SQL
    dropped a clause that reads as redundant beside `api_version =
    'rise.dev/v1alpha1'` — `split_part(api_version, '/', 1) = 'rise.dev'` — but
    that expression is the *predicate of the partial index*, and PostgreSQL's
    implication prover cannot derive it from the equality. Without it the lookup
    sequentially scans `resource_store.resources`, which under `SERIALIZABLE`
    takes a relation-wide `SIReadLock` and puts every gated identity write in
    conflict with every other resource write in the install. The existing
    plan-regression test says in its own comment that it exists so an edit
    "cannot silently drop to a sequential scan" — the new constant was never
    added to it. It is now, and the assertion was checked to fail against the
    de-indexed form.
  - **A self-service team survived an IdP takeover with its members.** Outside
    this increment entirely, in the login path: `sync_user_groups` adopts a
    pre-existing team whose name matches an IdP group, and removed only its
    *owners*. Team names are first-come, first-served while
    `allow_team_creation` is on (the default), and an IdP-managed team's
    membership is what `list_idp_group_names_for_user` reads to grant operator,
    admin, and platform access by group. So: create the team named after
    `auth.operator_idp_groups` before anyone in that group has signed in, list
    yourself as a member, wait for one genuine login, and you hold operator —
    which on this API means the gate short-circuits and the seeded `system-admin`
    binding applies to you. Both takeover paths (`group_sync` and the Entra
    sync) now drop every pre-existing membership; the IdP re-adds its real
    members as they log in. Reproduced as a test before the fix and after.
  - **The Organization cascade's cap did not bound the work it exists to bound.**
    It was tested after the per-kind row loop, and each row in that loop costs a
    database round-trip, so an Organization with a hundred thousand bindings did
    all of it before refusing. The cap now runs on the live row count before any
    per-row work, and the per-row parent fetch is gone — the parent is the
    Organization, already in hand.
  - **The cascade's refusal message was the only thing identifying its rows.**
    Generalizing the phrase to hide stored policy names (previous round) left
    every record of one cascade reading identically in the audit log. A gated
    change now carries a `detail` field that `record_gate` logs and the renderer
    never reads.
  - **An operator's Organization delete produced no gate record.** Skipping
    construction for an operator also skipped `record_gate`, whose own doc calls
    it the only evidence an operator's write was gated at all. The short-circuit
    logs explicitly now.
  - Raised, not changed: a typed **admin** can edit an IdP-managed team's
    membership (`is_admin` bypasses the `idp_managed` guard in the team update
    handler), which is a second path from admin to operator standing. Admins are
    documented as bypassing the typed APIs' own checks but *not* as reaching the
    generic resource API; this route does reach it. Whether an admin should be
    able to confer operator standing is a product decision, not a review one.
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
    than information. The half 9a also asked for — *who* can perform the write
    instead — is not here: answering it means searching policy for principals
    who hold the missing authority, which is the policy-auditing work
    `ROADMAP.md` §1 still tracks, not something the gate's verdict carries.
  - **A denial returns no `Explanation`.** Both `Explanation` and
    `GateRejection` name bindings and Roles the refused caller may hold no read
    access to. The 403 names the verb, the kind, and the resource; the
    provenance goes to the audit log, where a reader with access to it can find
    it.
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
  - `cargo fmt --all` and `cargo clippy --workspace --all-features --all-targets
    -- -D warnings` pass.
  - `cargo test --workspace --all-features` passes: 1,252 tests, with two
    ignored documentation examples.
  - Generated resource, backend-settings, `rise.toml`, and CRD artifacts were
    regenerated; only the resource schemas changed, by the one additive
    `effectiveLabels` field. `cargo audit` and `helm lint` were not run locally
    (neither tool is available in this environment); no dependency was added
    beyond promoting `tokio` from a dev-dependency of
    `rise-resource-store-postgres`, and the chart is untouched, so CI covers
    both.
  - The generic resource API's dispatch suite is at 85 tests, adding masked
    collections, refused items, the list-only projection and its expansion under
    `get`, inherited `effectiveLabels` and their shadowing, the grant gate
    refusing and permitting a delegation by the same non-operator writer, and
    §6.6's three label cases: the creation exception carrying a new resource's
    own ownership label, a non-owner refused both spellings of a redirect, and
    an owner transferring ownership on.
  - The PostgreSQL-backed store suite is at 97 tests, adding the transaction
    seam: a transaction-scoped store reading its own uncommitted write while the
    pool cannot see it, a dropped transaction rolling back, and two conflicting
    transactions producing exactly one `StoreError::Serialization` at commit.
  - Coverage follows ADR-0001 scenarios 33, 37, 38, 39, 41, and 42. Scenario 33
    is covered at the store — the mechanism is the isolation level and the
    error's classification, and the retry loop above it turns that classification
    into a replay; driving a grant and a revocation concurrently through the HTTP
    surface is left to the conformance suite in increment 11.
- Review — adversarial pass over the choke point and the transaction seam,
  fixed in the same increment:
  - **Three re-entrant borrows of the transaction's single connection.** Each
    presented as a hung request rather than an error: `update`'s preflight query
    shadowed its guard instead of dropping it, the two collection resolvers
    finish by calling each other while still holding one, and the Organization
    delete guard held one across `store.delete`. All three are fixed, and the
    borrow itself changed from a wait to a `try_lock` that names the mistake —
    waiting on a guard the caller holds is a deadlock, and a deadlock in a
    request path is the worst failure mode available.
  - **Audit records claimed writes a retry could roll back.** `resource.created`
    was emitted inside the transaction, so a lost race left a record of a create
    that never happened. Write records are deferred to the commit; decisions
    stay inline.
  - **One unresolvable row failed a whole diagnostic.** A tombstoned resource
    whose ancestry does not reach a root cannot be authorized, and the
    `pending-deletion` listing propagated that as an error, hiding every other
    draining resource. It is skipped and logged instead — visible without being
    fail-open.
  - **A transaction opened before the credential was checked.** `begin_write`
    now resolves the principal first, so a credential this API does not accept
    costs no transaction, and a retry loop does not open one per attempt.
  - Corrected rather than found: the first version of the relabel test asserted
    that handing ownership to another subject is refused. It is not, and should
    not be — the writer was the resource's *current owner*, so the authority
    being delegated is authority they hold. §6.6 requires that transfer to work,
    and it is what pinning the writer's side to the old label value is for. The
    test now covers both halves: a non-owner refused, an owner permitted.
- Second review — three independent adversarial passes over the choke point,
  the response paths, and the transaction seam. Nine findings, all fixed here:
  - **A main-resource write could change `metadata.finalizers`.** ADR-0001 §2
    reserves that for `(update, Kind, finalizers)`, and the reserved-namespace
    screen lived only on the subresource path. Plain `update` — which any editor
    holds — could therefore clear a finalizer a controller was holding a
    deletion with, and `create` could plant a `system.rise.dev/*` name that
    makes a resource undeletable through every route the API offers, operators
    included. Inert while the API was operator-only; not inert now. A main write
    must now carry the stored list back unchanged, and the store refuses any
    change to the reserved subset on both `create` and `update`, so a direct
    store caller is held to it too.
  - **A write returned the full stored object regardless of read access.** A
    caller holding only `(update, Kind, status)` got the entire `spec` back in
    the response — a status grant acting as a full read, which is exactly the
    implicit flow §2 forbids. Write responses now come back at the granularity
    the caller may read, through the same projector the list path uses. The
    `update` and soft-delete responses had the same shape.
  - **A gate refusal enumerated stored policy and topology.** A refused label
    write named every descendant inheriting the key — full resource paths, for a
    caller entitled to none of them — and a refused Role edit named the subjects
    and scopes of every binding referencing it, across organizations. The
    refusal now carries only what the request itself supplied: a `Disclosure`
    marks whether a change's recipients and domains were reconstructed from the
    body or read out of the store, the renderer suppresses the latter, and the
    full detail goes to the `rise::audit` record where a reader with access to
    it belongs.
  - **`allowedStatusControllerIds` was an ungated authorization grant.** Every
    id on it confers `status` and `finalizers` writes over every resource of the
    kind, in every organization — but a Controller is not a subject the engine
    can evaluate, so there is no binding to diff and the gate never saw the
    change. An ordinary `update` on a `ResourceDefinition` could hand out
    authority no `RoleBinding` granted. Changing the list now requires operator
    standing, until Controller identities make it expressible as policy.
  - **The Organization delete guard's doc claimed a guarantee PostgreSQL does
    not give.** Predicate locks are only checked against writers that are
    themselves `SERIALIZABLE`, and every typed link write runs at
    `READ COMMITTED` — so moving the count inside the transaction bought no
    mutual exclusion and, by reading an older snapshot, widened the window. The
    `TODO(multi-org)` that carried the real remedy had been deleted in favour of
    the false claim. Both are restored, corrected.
  - **Two effects fired at savepoint release rather than at commit.** Inside a
    caller's transaction a store method's own `commit()` is a `RELEASE
    SAVEPOINT`, so the compiled-schema cache was evicted while the new
    definition was still uncommitted — long enough for a concurrent reader to
    refill it with the *superseded* validator and leave it in force
    indefinitely — and cascade-deletion audit records were emitted for
    deletions a retry could roll back. `PgSession::on_commit` defers both to the
    real commit, and runs them immediately on a pool-backed session where there
    is no later one.
  - **Two paths lost the retryable classification.** A swallowed serialization
    failure inside `resolve_idp_groups` leaves the transaction aborted, and
    every statement after it returns `25P02`, which surfaced as a hard 500
    instead of a replay; and the Organization child count reports `anyhow`,
    which carries no store classification. `25P02` now maps to
    `StoreError::Serialization` — an aborted transaction can only be answered by
    replaying it — and the count's failure is inspected for the SQLSTATE.
  - **A listing under a nonexistent ancestor answered differently from an
    unauthorized one.** `404` versus masked-empty made the ancestor path
    enumerable by name — which organizations exist, which projects they hold —
    directly beside a per-item filter that carefully masks their contents. A
    listing now answers empty either way. An *item* under a missing ancestor is
    still a `404`, and a create under one still fails.
  - **The membership resolver identified the same User two incompatible ways.**
    `resolve` passed the credential's UID with the subject's name, while
    `groups_for_user` resolved the resource by name first. The store assigns its
    own UID, so the first form could only ever match if two independently
    generated identifiers coincided — group ties would have stayed empty even
    after identity resolution lands. Both halves now resolve by name.
  - Also hardened without a finding behind it: list decisions are matched to
    rows by UID rather than by position, so a future change to the engine's
    filter cannot silently pair one item's row with another item's verdict; the
    unused `PgSession::pool_handle` escape hatch is gone; and
    `list_deletion_blockers` no longer issues `SET TRANSACTION` when it runs
    inside a caller's transaction, where the statement is a subtransaction and
    PostgreSQL would abort the whole thing.
  - Reviewed and kept as-is at the time: an item `get`/`update`/`delete` the
    caller does not hold answers `403` on an existing resource and `404` on a
    missing one, which confirms existence by name. ADR-0001 §4 requires masking
    for *collections* and says nothing about a caller who already names one
    resource exactly, and Kubernetes answers the same way. The fifth review
    below overturns this on an argument neither the ADR nor Kubernetes
    supplies — that it makes the masking on the sibling paths decorative — and
    the item paths now mask too.
  - Reviewed and kept as-is at the time: the `deletion-blockers` subresource
    names the blocking children whether or not the caller could read them
    individually. Naming them is the whole content of the grant; the alternative
    is a subresource that reports "something blocks this" and nothing more. The
    third review below reverses the unfiltered part — the blockers are a
    collection and are filtered per item — while keeping a count of what was
    withheld, which is what preserves the grant's content.
- Third review — a second adversarial round, over the fixed code and treating
  the fixes themselves as unreviewed surface. Seven findings, all fixed here;
  two of them were in the previous round's fixes:
  - **An owner reference turned `update` into `delete`.** Attaching one grants
    the dependent nothing (ADR-0001 §1), but deleting the owner starts deletion
    of the dependent and the collector finishes it. So `update` on a resource
    plus `delete` on anything the caller owns composed into `delete` on that
    resource: attach the victim as a dependent of something you own, delete
    your own resource, and the victim goes with it — no gate, no `delete` check
    on the victim, and an audit trail that never names it. A `Deny` on `delete`
    was bypassable this way, and so was the write-time gate on a Deny-bearing
    binding, whose spec never changed. Attaching a *new* owner reference now
    requires `use` on the owner — §2's verb for referencing a resource from
    another's fields — and, when the dependent already exists, `delete` on the
    dependent, which is the authority the edge actually confers.
  - **The cascade did not honour the immutable seeds.** `delete` refuses to
    remove `PlatformRole/system-admin` or its binding, but the owner-reference
    cascade tombstoned by UID with no such check — so the one pair with no
    recovery authority above it was collectable through a resource someone else
    controls. The cascade now exempts them, from the same
    `IMMUTABLE_POLICY_SEEDS` declaration rather than a second copy of the names.
  - **The disclosure classification was wrong for two change shapes.** A
    `GroupMembership` gate's domains come from `authored_domains` — every stored
    binding naming the Group — and a `UserIdentity` gate's *recipients* are
    expanded from the User's live ties. Both were marked as coming from the
    request, so the refusal rendered exactly the stored topology the previous
    round's fix existed to suppress. Membership and trust-policy writes are now
    recipient-only; identity mappings and activations disclose neither.
  - **The `ResourceDefinition` write paths bypassed the reserved-finalizer
    screen.** It was wired into `create` and `update`, and an RD goes through
    `register_resource_definition` / `update_resource_definition` instead. A
    `create` could plant `system.rise.dev/*` on a definition and freeze its
    schema, parent, and controller allowlist permanently — every removal route,
    operators included, refuses a reserved name. Both entry points now screen it.
  - **The create response was the one write not projected.** Every other write
    path answers at the granularity the caller may read; `create` returned the
    full envelope, disclosing the server-assigned UID (the input the owner
    reference attack needs), inherited `effectiveLabels`, and — for a policy
    kind — the contextual normalization admission applied to the spec.
  - **`deletion-blockers` returned an unfiltered child inventory.** Because §3
    makes a subresource statement grant *only* subresources, a Role written for
    subresource work confers `(get, Organization, deletion-blockers)` while
    conferring no `list` on anything — and the response named every child by
    kind, name, and UID, both directly addressable. Every other collection-shaped
    response in this increment is filtered per item; this one now is too, and
    what is withheld is counted rather than silently dropped, because a report
    that omits blockers reads as "nothing is blocking this".
  - **An inactive `User` still yielded live Group ties.** The lookup filtered
    only on the tombstone. ADR-0001 §1 makes an inactive User unable to log in
    and fails every token already issued for them, and it is the premise the
    activation gate rests on, so the tie path has to agree.
  - Recorded rather than fixed, with reasons: a refusal still names the *tuples*
    the writer cannot justify even when the recipient and domain are suppressed,
    so a caller can learn which of a Role's statements exceed their own
    authority. That is a verb and a kind, never an identity or a path, and it is
    what makes a refusal actionable at all; if Role bodies are ever meant to be
    confidential, `Disclosure` needs a third axis rather than a special case.
    The fifth and sixth reviews close this: the witness list is withheld
    unconditionally, because it is drawn from the recipient's whole effective
    policy rather than from anything the caller wrote.
  - Raised against increment 9a's algebra and deliberately not changed here:
    `aggregate` credits a recipient's `before` policy with org-tier bindings
    that §1's recipient boundary makes inert for a non-member, which shrinks the
    delta a membership write has to justify. 9a chose provable-reach aggregation
    deliberately, and scenario 29 depends on it; revising it is a change to the
    gate's model, not to its wiring, and belongs with the conformance work in
    increment 11 where the scenario suite can hold it.
- Fourth review — a third adversarial round, aimed at the previous round's own
  fixes on the theory that the newest code has had the least scrutiny. Four
  findings, all fixed here:
  - **A refused label write still named a stored binding's subject.** The
    previous round marked label changes recipient-only on the reasoning that the
    recipient is the ownership rule's authored *template*. It is not: the gate
    resolves the recipient through the selecting binding, and for a
    literal-subject binding that subject is read straight out of stored policy —
    another organization's Group or ServiceAccount, named to a caller who may
    read none of it. Label refusals now disclose neither side. The test that was
    supposed to cover this hand-built a template recipient, a shape the gate
    cannot produce, which is why the classification survived two rounds; it now
    pins the literal shape and calls the production classifier rather than
    restating a constant.
  - **`deletion-blockers` filtered on the wrong verb.** The previous round
    filtered per item on `list`, but each item carries more than list
    granularity projects — a UID and the item's finalizers — so a caller holding
    only `list` on the children received both. The per-item verb is now `get`,
    which is the grant that confers item detail.
  - **The two immutable-seed predicates disagreed on the API group.** The
    cascade's SQL exemption pins `api_version`; `is_immutable_policy_seed` did
    not, so a row one treats as a seed the other would collect. The API group is
    part of a seed's identity — a kind registered under another group is not
    made reserved by borrowing the name — so the predicate takes it too, and the
    two now compare the same three fields.
  - **The owner-reference refusal named the owner.** Its comment claimed the
    refusal tells a caller nothing about a resource they cannot see; it went
    through the ordinary `require`, which names the kind and the name. A UID
    travels further than the standing to read what it points at, so the refusal
    now names only the UID the caller supplied — and reads identically for a UID
    that resolves to nothing, so it is not an existence oracle. Only "no such
    resource" is folded in; a store failure or a lost `SERIALIZABLE` race still
    propagates to the retry loop.
  - Found in the same pass and fixed alongside: *removing* an owner reference
    was not authorized at all. Attaching one is gated because it borrows the
    owner's lifecycle; detaching escapes a lifecycle that someone with standing
    over the owner put the dependent under, which would let anyone holding
    `update` on a dependent outlive the cascade meant to collect it. Both
    directions now require `use` on the owner. `delete` on the dependent stays
    an attach-only requirement, because detaching confers deletion on nobody.
- Fifth review — a fourth adversarial round, again aimed at the previous round's
  own fixes. Six findings, all fixed here; one of them reverses a decision two
  earlier rounds recorded as deliberate:
  - **Addressing a resource by name was an existence oracle.** Recorded twice as
    a considered decision — Kubernetes answers the same way, and ADR-0001 §4
    mandates masking only for collections. The argument that overturns it is
    internal rather than external: the *same handler* answers a listing the
    caller has no grant in with an empty `200`, and a listing under a
    nonexistent ancestor the same way, both explicitly so that the tree is not
    enumerable by name. An item path that answers `403` for a resource that
    exists and `404` for one that does not hands back, one name at a time,
    exactly what the listing withholds — which makes the masking decorative. The
    answer now turns on `get` rather than on the verb attempted: a caller who
    may read the resource gets an ordinary `403` naming the verb they are short,
    and a caller who may not gets the `404` a nonexistent name gets. Both are
    audited as denials.
  - **A refusal's witness tuples were the third thing worth suppressing.** The
    previous round split disclosure into recipient and domain and noted that the
    missing *tuples* still went out, judging them a verb and a kind rather than
    an identity. They are also, for a derived change, a read of a stored Role
    body: a label write's claim is `before ⊔ the selecting binding's statements`,
    so the refusal reported the contents of a Role behind a binding the caller
    may not read — and paired with the writes that *succeed* (a key no binding
    selects on is not gated at all), it enumerates the install's access-driving
    label keys and profiles the authority behind each. This round suppressed the
    tuples for the derived shapes only, on the belief that a `Role` body edit's
    witnesses are the statements the caller just submitted. The sixth review
    shows that belief is false for every shape, and the tuples are now withheld
    unconditionally.
  - **A refused label *removal* named a stored label key.** The operation string
    is prepended outside every `Disclosure` check, and for a removal the key
    comes from the stored label map rather than the request. A caller holding
    `update` without `get` could read back a resource's access-driving keys one
    refused write at a time — and `metadata.revision` is a small integer, so the
    read-modify-write that path requires is not much of an obstacle. A removal
    now says only that an access-driving label was removed; a *set*, whose key
    and value are the caller's own, still names both.
  - **`deletion-blockers` counted a store failure as a hidden blocker.** Every
    error from resolving a blocker's ancestry was flattened to "not visible", so
    a backend failure reported a number instead of an error and a retryable
    classification would never have reached the loop. Only the not-found answer
    is folded in now.
  - **`blockOwnerDeletion` was bought with `use`.** The flag is not a reference
    but a hold: the owner cannot be collected until the dependent drains, and a
    dependent carrying a finalizer of its own never does. So `use` — §2's price
    for *referencing* a resource — bought interference with someone else's
    `delete`, on the create path with no dependent-side check at all. Raising
    the flag now requires `delete` on the owner.
  - **The detach gate could freeze a dependent permanently.** Requiring `use` to
    remove a reference, added last round, has no answer for an owner that is
    already gone: the store refuses to carry such a reference back, so the
    dependent could neither keep the edge nor drop it, and every unrelated field
    was frozen behind an owner nobody can revive. Detaching from an owner that
    is absent or draining is ungated — it confers nothing on anyone.
  - Recorded rather than fixed: `hiddenBlockers` is a live count of resources the
    caller cannot read, so it moves as others create and delete them. It stays,
    because a blocker report that silently omits blockers reads as "nothing is
    blocking this" — but it is now documented as a disclosure at the grant
    rather than left to be discovered.
  - Confirmed as intended rather than changed: the shipped `resource-owner` role
    excludes `use`, which a test pins deliberately. With owner references now
    gated on `use`, that means owning a resource does not by itself let you make
    it the owner of another. Widening shipped policy is a product decision, not a
    review one; the operator note says so.
- Follow-ups this increment deliberately leaves open:
  - **Group-targeted policy is dark until identity resources exist.** Ties
    resolve against `User` and `Group` resources that no login path writes yet,
    so every principal has an empty tie set. Harmless for an Allow — a group
    binding grants nothing — but a cap expressed as a group-targeted `Deny` is
    never collected and therefore does not bite. Until increment 10, a
    restriction has to name a subject that resolves today. The resolver's module
    doc says so at the seam, and the upgrade notes say so to operators.
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
    is a grant the gate does not see. Recorded here as needing only `delete` on
    the Organization and therefore escalating nothing today — the seventh review
    shows that reasoning was too comfortable, and it is closed there.
  - `ResourceDefinition.allowedStatusControllerIds` still gates controller
    status and finalizer writes. Controllers become ordinary principals when
    their identity resources go live, which is when that allowlist can go.
  - A user's `status` write still lands in the slot named `operator:<actor>`.
    The name is now wrong for a non-operator writer; ADR-0002's subresource
    execution model owns the field separation and is where the naming is
    settled.
  - A gate refusal no longer names what the recipient would have gained, which
    costs a legitimate author the one detail that made it actionable. Narrowing
    the rendered witness list to the intersection with the tuples the *request
    body* enumerates would give that back with request provenance — but it is
    new logic in the one renderer whose job is to say too little, and matching
    algebra witnesses against a body's wildcard matchers is where it would go
    wrong. Worth doing with the conformance suite in increment 11, which can
    hold both halves.
  - Cross-request authorization caching stays measured rather than assumed, per
    `ROADMAP.md` §1: the request-local snapshot already removes the repeated
    cost inside one request.
  - One lifecycle lever is reachable by a non-operator holding ordinary write
    access, and it is not an authorization escalation but an availability one:
    a caller who may `create` can attach arbitrary non-reserved finalizers,
    which only affects their own resource. The owner-reference half of this —
    holding another resource's deletion open with `blockOwnerDeletion` — is
    closed by the `use` requirement on both attaching and detaching an edge.
