# Roadmap

This is the implementation roadmap for Rise's active architectural work.
Normative behavior lives in the ADRs; this file tracks delivery order and must
not duplicate or redefine those decisions.

Status legend: `[x]` shipped · `[~]` in progress · `[ ]` planned.

## Governing decisions

- [ADR-0001: Unified Permission Model](docs/engineering/src/content/docs/adr/0001-unified-permission-model.md)
  is the target authorization and identity model.
- [ADR-0002: Generic Resource Subresource Execution Model](docs/engineering/src/content/docs/adr/0002-generic-resource-subresource-execution-model.md)
  is Draft and defines the intended execution seam for `status`,
  `finalizers`, `token`, and future generated or streaming subresources.
- [ADR-0003: Resource Families](docs/engineering/src/content/docs/adr/0003-resource-families.md)
  groups kinds that share a name pool and list as a unit; it gates the
  extension-kind migration in §4.
- Where shipped compatibility behavior differs from an ADR, it is transitional;
  new work converges on the ADR rather than extending the old shape.

## 1. Generic resource and authorization foundation

### Shipped substrate

- [x] Generic resource envelope, Postgres resource store, built-in and external
  kind registration, optimistic concurrency, finalizers, and garbage collection.
- [x] Generic HTTP resource API with operator-only compatibility authorization.
- [x] Default Organization bootstrap and existing typed-row linkage.
- [x] Typed built-in resource registry.

### Unified identity and RBAC

- [x] Move the dep-light `ResourceStore` contract and canonical `SubjectId`,
  dynamic-label `SubjectRef`, group-qualified `ResourceKind`, and `Scope` types into `rise-resource-api`;
  keep SQLX and Postgres adapters in `rise-resource-store-postgres`.
- [x] Implement policy types and validation for `Role`, `RoleBinding`,
  `PlatformRole`, and `PlatformRoleBinding`: structured `roleRef`, one
  subject per binding, normalized PascalCase `subjectMembership: Any |
  ResourceOrganization` on platform bindings (omission becomes `Any`; null is
  invalid), canonical scopes, pure Allow/Deny tuple evaluation, placement
  provenance, wildcard replacement, and Deny-aware subset checks. Activate
  these resources only through transaction-scoped normalization/admission.
- [x] Add closed contracts for the built-in identity resources: root `User`
  and `Controller`; Organization-owned `Group` and `ServiceAccount`; and
  fixed-parent
  `UserIdentity`, `GroupMembership`, `ControllerTrustPolicy`, and
  `ServiceAccountTrustPolicy`. User and UserIdentity carry platform-managed
  `active` state, defaulting true; shipped policy does not allow self-editing.
  GroupMembership is an empty marker named for its User.
  Reserve their collection definitions now, but activate them only through the
  transaction-scoped normalization/admission seam.
- [x] Add optional, UID-authoritative `metadata.ownerReferences` to the generic
  resource envelope and implement transactional reverse lookup, cycle-safe,
  finalizer-respecting dependent garbage collection. GroupMembership may name
  its User as lifecycle owner; backend-managed memberships normally do so,
  while operator-managed markers may intentionally remain unowned. Land this
  before GroupMembership runtime activation.
- [x] Add partial Postgres indexes for unique external User mappings,
  target-parent workload trust lookup, and reverse name-based membership edges.
  These remain internal storage projections and do not change the generic
  resource API shape.
- [~] Implement live membership expansion, per-item list filtering/projection,
  effective-label resolution, typed `SubjectRef` values for dynamic ownership,
  tiered platform/org Deny filtering, admin/operator classification, platform
  ceilings, and the centralized authorization choke point replacing
  `require_operator`. Once Controller writes use that path, remove
  `ResourceDefinition.allowedStatusControllerIds` and authorize `status` and
  `finalizers` exclusively through RBAC.
- [~] Add Role/policy audit and explain diagnostics for semantically inert
  configuration: no-op recipient or membership constraints, owners with no
  current grant, selectors matching nothing, stale references, and shadowed
  Allows. Keep these out of synchronous write rejection when the grant delta is
  safely empty.
- [x] Add request-local `AuthorizationSnapshot` memoization for membership,
  admin classification, and effective policies. Defer cross-request caching
  until it can be invalidated transactionally through an authorization epoch.
- [ ] Decide cross-request authorization caching on measurement, not ahead of
  it. The request-local snapshot already removes the repeated cost inside one
  request; what remains is one role lookup per distinct `roleRef` on a
  snapshot's first evaluation, which batching fixes with no schema commitment —
  do that first. Only if authorization is still a measurable share of request
  time once the typed-object migration (§4) puts real traffic on the engine
  should a cross-request cache land, and then through a transactionally
  incremented global `authorization_epoch` keyed by at least principal UID,
  token-cap hash, epoch, and resource identity. A write path that forgets to
  bump the epoch is a stale-authorization bug no test will surface, which is
  why the measurement comes first.
- [ ] Implement the write-time grant gate for Roles, bindings, membership,
  identity mappings, and access-driving labels. All authorization-changing
  writes use serializable transactions with bounded retry.
- [ ] Seed immutable/healable `system-admin` and platform `resource-owner`
  Roles, plus an operator-editable global `PlatformRole/org-admin` baseline.
  Organization creation atomically creates an exact org-root, scope-only
  `RoleBinding` from that role to an operator-selected existing User. That
  direct binding is the first admin's bootstrap affiliation; no pre-existing
  Group is required and no Group name has implicit authorization meaning.
- [ ] Add conformance coverage for every applicable ADR-0001 acceptance
  scenario, including multi-org admins, membership removal, UID-bound token
  invalidation, token caps, and grant/revocation races.
- [ ] Add the constrained Project ServiceAccount lifecycle operation. It uses
  fresh never-reused canonical names and atomically creates/deletes only its
  fixed Project-scoped policy and trust bundle, applying the effective-delta
  subset check to the result; ordinary Project users do not receive generic
  ServiceAccount or Role/RoleBinding creation authority.
- [ ] Load `operatorIdentities` `(issuer, subject)` selectors at process startup;
  JIT-create a generated User plus first UserIdentity after a validated unknown
  login, and derive operator status live when any identity attached to that User
  is active and matches the configured set. An inactive exact identity or
  inactive parent User fails without JIT; deleting a mapping permits a later
  login to provision a fresh User UID, so durable disablement uses `active:
  false`. Reject email linking and hot reload; configuration changes complete
  only after all old instances are drained. Operators remain the recovery tier;
  legacy admins become qualifying default-org RoleBindings.

### Resource API maturation

- [ ] Reject `create` below an ancestor that is already tombstoned. Built-in
  placement validation walks the whole chain and refuses a deleting ancestor;
  `ResourceDefinition`-registered kinds have no equivalent check, so a create
  can still add a child to a subtree the garbage collector is draining.
  Creation is the case that matters, and it matters most under a deleting
  Organization, whose policy resources are tombstoned with it while an
  operator-authored platform grant survives. Reads, `delete`, and the
  `status`/`finalizers` subresources must keep working, or controllers cannot
  remove their finalizers and the subtree never drains.
- [ ] Add pagination plus label/field selectors. Labels remain JSONB initially;
  measure before introducing a separate label table.
- [ ] Add JSON Merge Patch and RFC 6902 Patch through the common mutation
  pipeline. Main writes preserve protected `status` and `finalizers`;
  subresource writes mutate only their protected fields.
- [ ] Add Server-Side Apply and managed fields without allowing main-resource
  apply to claim protected subresource fields.
- [ ] Add secret/protected schema annotations with encryption at rest and
  response redaction.
- [ ] Serve discovery, JSON Schema, and OpenAPI from the registry using the
  canonical group-qualified `ResourceKind` vocabulary and registered
  subresources.
- [ ] Add a resumable Watch API with explicit backpressure, connection limits,
  and observability.
- [ ] Build `rise-resource-client` with Rise-issued credential providers,
  watch resume, and generic finalizer/subresource helpers.
- [ ] Add built-in version conversion and define external
  `ResourceDefinition` schema-evolution behavior.
- [ ] Close built-in `(group, kind)` shadowing and finish audit coverage for
  RBAC, Patch/Apply, discovery, and subresource execution.

## 2. Rise-issued authentication and token issuance

### Shipped transition

- [x] Extracted JWT verification/signing into `rise-backend-auth`.
- [x] Added the transitional `POST /api/v1/auth/token` exchange endpoint,
  Rise access-token claims, middleware consumption, and CLI auto-exchange.
- [ ] Finish raw-external-token deprecation telemetry, then disable and remove
  direct external JWT acceptance from ordinary resource endpoints.

### Target convergence

- [ ] Issue User sessions with canonical `sub` plus immutable `rise_uid`
  after exact live, active `UserIdentity (issuer, subject)` and active parent
  User resolution.
- [ ] Move workload token exchange to each ServiceAccount or Controller `/token`
  subresource. Validate external assertions only against trust-policy children
  of that URL target; do not perform a global source-identity search, and mask
  target/assertion/policy failures behind the same coarse authentication error.
- [ ] Support delegated issuance on the same `/token` route for an already
  Rise-authenticated principal holding `(create, qualified ResourceKind,
  token)`. Workload exchange and delegated request modes are disjoint.
- [ ] Add a canonical RFC 9396 `authorization_details` claim with
  `type: rise.dev/rbac`, one qualified `scope` per entry, and multiple entries
  whose permissions union. Reject empty permission axes, carry the parsed cap
  on `AuthenticatedPrincipal`, and enforce live RBAC intersected with that cap
  on every primary and secondary decision.
- [ ] Add bounded nested `act` attribution. Delegation chains are allowed only
  across explicit live token-create grants; there is no token-class/one-hop
  bypass rule.
- [ ] Reject stale UID tokens, inactive/deleted principals, Group subjects as
  principals, malformed subjects/scopes, and external workload JWTs on every
  non-token endpoint.
- [ ] Retire synthetic ServiceAccount users/emails and the transitional
  identity-selection contract once built-in identity resources and target
  `/token` routes are live.
- [ ] Keep the platform-global maximum token TTL and add negative tests for
  audience, cap, target trust, mode-confusion, and name-recreation behavior.

## 3. Subresource execution

- [ ] Resolve ADR-0002's open interface questions and move it from Draft to
  Proposed before implementing the general registry seam.
- [ ] Register generic `status` and `finalizers` mutation strategies once in
  the resource layer; individual resource handlers must not reimplement their
  field separation.
- [ ] Register `token` as a generated finite response with the two typed
  authentication outcomes from ADR-0001. Raw assertions are consumed before
  handler invocation.
- [ ] Make discovery report each kind's supported subresources, verbs, media
  types, and execution shape.
- [ ] Design Deployment `logs` as the first streaming product subresource,
  including source selection, retention, follow/tail behavior, backpressure,
  disconnect cancellation, limits, redaction, and audit metadata.
- [ ] Defer `proxy`, `exec`, and upgraded connections until a concrete
  product use case justifies their larger trust boundary.

## 4. Typed-object migration

Ordering: the built-in registry enables data-plane migration; unified RBAC
must land before user-facing routes flip to the generic API. Pagination/Watch
and secret handling remain kind-specific prerequisites.

- [ ] Implement resource families (ADR-0003): `ResourceFamily`, the family
  name pool, the unversioned family collection route, and printer columns.
  A family cannot be retrofitted onto kinds that already have instances, so
  this lands **before** the extension migration below.
- [ ] Migrate extension kinds to external `ResourceDefinition` resources under
  the `Extension` family, preserving today's per-project extension name pool;
  secret-bearing extensions wait for encrypted fields.
- [ ] Migrate `Project` (Organization child) and `Environment` (Project
  child) as built-ins. Authorization ownership is `rise.dev/owner` plus policy;
  lifecycle `metadata.ownerReferences` never grant access.
- [ ] Migrate `User`, `UserIdentity`, `Group`, and `GroupMembership`, mapping
  the existing Rise Team concept to `Group` while preserving stable generated
  User names and exact SSO mappings.
- [ ] Migrate `ServiceAccount` as an Organization child and trust mappings as
  `ServiceAccountTrustPolicy` children.
- [ ] Migrate `Controller` as a root built-in and trust mappings as
  `ControllerTrustPolicy` children.
- [ ] Migrate `Deployment` as a Project child after pagination and Watch;
  controllers update it through registered `status` and `finalizers`
  subresources.
- [ ] Migrate env vars, custom domains, and project app users before dropping
  the remaining Project foreign-key fan-in.
- [ ] Drop each typed table only after its resource-backed path has baked and
  compatibility reads are no longer needed.

## 5. External controllers and multi-org routing

- [ ] Build `rise-k8s-controller` against `rise-resource-client`, using a
  Rise-issued Controller token and Watch rather than a raw controller JWT or
  typed-table polling.
- [ ] Store org-agnostic Controller identities separately from per-org
  selection/configuration. Organizations reference an available controller
  class through ordinary governed resource data; credentials stay in trust
  policy children or secret resources, not embedded config blobs.
- [ ] Prove isolation with two controllers reconciling two Organizations into
  separate clusters/accounts, including controller reassignment and in-flight
  Deployment behavior.

## 6. Codebase decomposition

- [x] Extracted the Docker deployment backend behind the `DeploymentStore`
  seam.
- [ ] Complete remaining backend extraction work only where it supports the
  resource/controller migration above; avoid parallel abstractions that bypass
  the generic resource API or unified authorization engine.

## Verification gates

- Every PR: formatting, clippy with warnings denied, workspace tests, affected
  SQLX preparation, schema checks, and documentation build.
- Unified RBAC complete: all non-deferred ADR-0001 conformance scenarios pass
  against pure policy tests, fake-store engine tests, and server/Postgres
  integration tests.
- Resource API mature: an ordinary governed User can create, patch/apply, list,
  watch, and use subresources without operator-only paths.
- Authentication converged: ordinary endpoints accept only Rise-issued,
  UID-bound principals; workload token exchange and delegated minting are tested
  through target `/token`; cap narrowing is end-to-end.
- Migration complete: typed commands are resource-backed and typed-table writes
  have ceased before tables are removed.
- Multi-controller complete: separate Rise-issued Controller identities
  reconcile separate orgs with no cross-org access or workload loss.
