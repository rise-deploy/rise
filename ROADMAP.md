# Roadmap

Single source of truth for in-flight architectural workstreams in Rise.
Forward-looking PRs and shipped milestones live here together; check items
off as they merge.

New roadmap files should not be created — add new workstreams as top-level
sections here.

Status legend: `[x]` shipped · `[~]` in progress · `[ ]` planned.

## Workstreams

1. [Multi-Tenancy & Generic Resource API](#workstream-1--multi-tenancy--generic-resource-api)
   — Organizations, generic resource substrate, external controllers, typed-
   object migration.
2. [Authentication & Token Exchange](#workstream-2--authentication--token-exchange)
   — Centralized JWT verify, `POST /api/v1/auth/token` exchange, snap-
   decision middleware, removal of the legacy in-handler verification path.

---

# Workstream 1 — Multi-Tenancy & Generic Resource API

## The arc

Rise needs a Kubernetes-like external-controller pattern without making
Kubernetes the control plane. The generic resource substrate provides that:
typed objects with `apiVersion`/`kind`/`metadata`/`spec`/`status`,
controller-owned status keys, finalizer-gated deletion, optimistic concurrency
on `revision`. Built-in kinds (Organizations, Projects, Deployments) and
external custom kinds (registered via `ResourceDefinition`) share the same
table, the same API, the same lifecycle. Organizations are the tenant
boundary, so the same substrate also gives us multi-tenancy: two customers
deploying to two different clusters or AWS accounts, each with their own
controller, all configured through the resource API.

Three phases:

- **Phase 0 — Compatibility substrate** (shipped). The minimal slice that
  introduces the substrate alongside the existing typed APIs without changing
  end-user behavior. Operator-only generic API, single default Organization,
  every existing typed row backfilled into it.
- **Phase A — Substrate maturation.** Everything the generic resource API
  needs to be the primary storage path: end-user RBAC, pagination, watch,
  PATCH/apply, secret-marked fields, schema discovery, a client SDK,
  built-in version evolution.
- **Phase B — Typed-object migration.** Move Extensions, Project, Environment,
  ServiceAccount, and Deployment onto the substrate. Drop the typed tables.
- **Phase C — Multi-controller routing.** External controller binary +
  Controller-as-a-resource registry. Per-Organization controllers in
  different clusters.

## Dependency rules

These constraints govern PR ordering — keep them current as the plan evolves.

- **A1 (typed built-in registry) → B2/B3/B4.** Built-ins like Project route
  through the registry; B2/B3/B4 add new built-in kinds. **A1 does *not* block
  B1**, which lands extensions as *external* `ResourceDefinitions` validated
  by JSON Schema with rows in the `resource_definitions` projection.
- **A2 (org-scoped RBAC)** gates the **API-surface flip** for B2/B3/B4 (the
  user-facing routes move to `/api/v1/resources/…`). It does **not** gate the
  **data-plane move** (typed APIs become facades writing through the resource
  store with system credentials) — that needs only A1.
- **A3 (pagination + selectors) → B4.** Deployments are high-cardinality.
- **A6 (secret-marked fields) → B1 for OauthClient and SnowflakeOauth.** Both
  store credentials we can't read back to operators.
- **A8 (watch) → C2.** Cross-cluster external controllers can't poll `list`.
- **A9 (rise-resource-client) → C1.** The reference controller consumes it.

## Phase 0 — Compatibility substrate

All shipped. Listed for historical context and so links from code comments
have somewhere to land.

- [x] **PR #296** — `crates/rise-resource-api` workspace crate (envelope
  types, Organization/ResourceDefinition spec/status, validation helpers,
  JSON Schema derives).
- [x] **PR #298** — `crates/rise-resource-store` workspace crate
  (`ResourceStore` trait, Pg-backed implementation, `resources` table and
  `resource_definitions` view, discriminator generation, optimistic
  concurrency, finalizer/cascade deletion semantics).
- [x] **PR #301** — Operator role (`auth.operator_users`) and controller-JWT
  auth context, separate from user + project-SA auth.
- [x] **PR #303** — Generic HTTP API under `/api/v1/resources/{*path}` with
  wildcard path routing, built-in/external collection registry resolution,
  operator-only CRUD, controller status/finalizer subresources,
  `pending-deletion` listing.
- [x] **PR #317** — Resource GC worker (drives `try_collect` over rows
  returned by `list_pending_collection`).
- [x] **PR #326** — Multi-org linkage: nullable `organization_resource_uid`
  on users (via membership table), teams, projects; default-Organization
  bootstrap with advisory-lock-serialized backfill; K8s controller
  `controller_class_name` config and filter via
  `webhook.rs::enforce_controller_class`.
- [x] **PR #325** — Schema generation (`rise backend schemas generate`) +
  `mise run resource:schema:check` + operator docs section under
  `docs/engineering/src/content/docs/resources/`.

## Phase A — Substrate maturation

The work to make the generic resource API the canonical storage path for the
rest of Rise.

### Substrate maturation — shipped

- [x] **PR A1 — Typed built-in resource registry** (PR #341). Replaces the
  two hardcoded matches in `pg_store.rs` with a `BuiltInRegistry`
  indexed by plural and `(group, kind)`. `PgResourceStore::new(pool)` keeps
  using `BuiltInRegistry::defaults()`; `with_builtin_registry()` is exposed
  for tests and future feature-flagged built-ins. **Deferred to follow-ups:**
  `schema_fn` (lands with A7); `status_writers` (lands when the first
  built-in actually has a controller writing its status); driving
  `generate_schemas()` off the registry (re-evaluate at >2 built-ins).

### Substrate maturation — planned

- [ ] **PR A2 — Org-scoped RBAC on the generic resource API.** Adds
  `OrganizationRole` (member, admin) sourced from `user_organization_memberships`,
  cached per-request on `AuthContext`. Adds a per-resource ownership model
  via `metadata.ownerRef = { kind, name }` pointing at a User or Team.
  **Ownership propagates down the parent chain (additive)**: a descendant
  resource (Deployment, Environment, ServiceAccount, env var) does not need
  its own `ownerRef` — the authz check walks the parent chain looking for
  the nearest ancestor that declares one; descendants can carry their own
  `ownerRef` to *extend* the access set, never to restrict it. Replaces
  `require_operator` in `src/server/resources/handlers.rs`: operators
  bypass, org-admins have CRUD on their org's subtree, org-members get CRUD
  on resources they own by inherited or direct `ownerRef`. Walking the
  parent chain reuses the already-resolved row set from `resolve_path`.
  Files: `src/server/auth/context.rs`, `src/server/resources/handlers.rs`,
  new `src/server/resources/authz.rs`. **Blocker removed:** API-surface flip
  for B2/B3/B4. **Open in design:** `auth.admin_users` vs `auth.operator_users`
  overlap (see "Open questions" below — must resolve during this PR's design
  phase, not as a separate PR).

- [ ] **PR A3 — Pagination + label/field selectors on `list`.** Add
  `metadata.labels BTreeMap<String, String>` to the resource envelope plus a
  GIN index on `resources.metadata->'labels'`. Extend `ResourceStore::list*`
  with `ListOptions { limit, continue_token, label_selector, field_selector }`,
  using opaque Kubernetes-style `continue` tokens encoding `(updated_at, uid)`.
  HTTP parses `?limit=&continue=&labelSelector=&fieldSelector=`.
  **Blocker removed:** Deployment + env-var migration.

- [ ] **PR A4 — JSON Merge Patch + RFC 6902 PATCH.** Wire
  `PATCH /api/v1/resources/{*path}` through the dispatcher.
  `application/merge-patch+json` → RFC 7396; `application/json-patch+json` →
  RFC 6902. Both share the revision-checked update path with PUT. PATCH on
  `/status` stays controller-only; PATCH on `spec` rejects identity-field
  changes.

- [ ] **PR A5 — Server-Side Apply (`application/apply-patch+yaml`).** Field-
  manager tracking via `metadata.managedFields` so the CLI, controllers, and
  declarative tools can co-own different fields without clobbering. Can land
  after migration if A4 is enough for CLI ergonomics.

- [ ] **PR A6 — Secret-marked fields with encryption-provider integration.**
  Two schema annotations on a leaf string property:
  - `x-rise-secret: true` — encrypted at rest, wrapped in `SecretString`
    in-process (no `Debug`/`Display` leak); authorized callers receive the
    decrypted value inline. Matches today's "secret" env-var behavior.
  - `x-rise-secret: protected` — encrypted at rest, response masks the
    value with a sentinel (`{ encrypted: true }`). Matches today's
    "protected" env-var behavior. Controllers that need plaintext fetch via
    a controller-identity-scoped decrypt endpoint.

  Single hard invariant: cleartext never persisted. Visibility tier (returned
  vs. masked) is configurable per field. **Blocker removed:** B1 OauthClient
  + SnowflakeOauth extensions, env-var migration.

- [ ] **PR A7 — Serve JSON Schemas + OpenAPI discovery via HTTP.**
  - `GET /api/v1/resources/_discovery` — every collection (built-in + RD) with
    served versions.
  - `GET /api/v1/resources/{group}/{version}/{plural}/_schema` — spec JSON
    Schema for that version.
  - `GET /api/v1/resources/_openapi` — combined OpenAPI 3.1 document for
    language servers and `kubectl explain`-style CLI commands.

  Pulls `schema_fn` onto `BuiltInRegistration` (the A1 deferral) so this
  endpoint isn't a new hardcoded match. The on-disk artifacts in
  `docs/engineering/public/schemas/` stay the CI-checked copy.

- [ ] **PR A8 — Watch API (LISTEN/NOTIFY-backed change feed).** Postgres
  trigger on `resources` publishes UID-only events; backend fans out via an
  in-process broadcaster. `GET /api/v1/resources/{*path}?watch=true` returns
  chunked NDJSON of `{type, object, revision}` events, resumable from a
  client-supplied `resourceVersion`. Cursor: `(updated_at, uid)`. The GC
  worker keeps its `list_pending_collection` poll; watch is for end-user and
  controller reconcile loops.

- [ ] **PR A9 — `crates/rise-resource-client`.** Async HTTP SDK that consumes
  A2/A3/A4/A8. Typed `Resource<TSpec, TStatus>` accessors per built-in/RD,
  controller-JWT auth provider, watch-with-resume helpers, finalizer add/
  remove helpers. First consumer is the in-tree K8s controller (goes through
  the SDK even though it shares the process); same SDK powers C1.

- [ ] **PR A10 — Built-in version evolution.** A1 collapses
  `storage`/`served`/`declared` to a single api_version per built-in. That's
  fine indefinitely for Organization/ResourceDefinition; it breaks the moment
  Project moves `v1alpha1 → v1` post-B2. Extend `BuiltInRegistration` with
  multiple typed `versions[]` (storage + served lists) and a `convert_spec`
  hook between adjacent versions; reuse the existing `(group, kind)` index.
  Lands before any built-in needs a second version.

- [ ] **PR A11 — ResourceDefinition spec evolution for existing rows.**
  Today, registering a new RD version is structurally supported but the
  store does not constrain stored rows on an old version when the new schema
  tightens. Define and enforce a concrete validation policy: do tightened
  schemas reject reads of valid-by-old-schema rows? Block updates that don't
  migrate? Lands alongside (or just before) the first external controller
  that actually rolls a schema.

- [ ] **PR A12 — Audit logging on PATCH / RBAC paths and discovery.**
  Existing audit trail covers POST/PUT/DELETE + controller/operator
  finalizer-and-status writes. PATCH (A4/A5), end-user RBAC writes (A2), and
  the discovery endpoints (A7) need the same coverage — particularly
  attribution for end-user actors once A2 lands. Consolidated cleanup PR
  after A4/A5/A2/A7.

- [ ] **PR A13 — Reserve `(group, kind)` for built-ins; close shadowing.**
  Pre-existing gap: `validate_resource_group` is DNS-shape-only and
  `RESERVED_COLLECTION_NAMES` covers plurals only, so an external RD *can*
  declare `(rise.dev, Organization)` under a non-`organizations` plural and
  be silently shadowed in by-kind resolution. (The registry's
  `lookup_by_group_kind` enforces the right behavior at routing time; the RD
  validator does not stop the registration in the first place.) Add a
  `register_resource_definition` check: reject any external RD whose
  `(group, kind)` collides with a registered built-in, regardless of plural.
  Pass `Arc<BuiltInRegistry>` into the RD validation path.

- [ ] **PR A14 — Watch backpressure, connection limits, observability.**
  Follow-up to A8. Per-connection rate limits, max-concurrent-watch caps per
  principal, NDJSON chunk timeouts, metrics (active-watch gauge,
  events-fanned-out counter, drop counter for slow consumers).

## Phase B — Typed-object migration

B1 is independent of A1. B2/B3/B4 each have two stages: a **data-plane move**
(typed APIs become facades over the resource store; needs A1 only) and an
**API-surface flip** (user-facing routes go to `/api/v1/resources/…`; needs
A2 for end-user RBAC).

- [ ] **PR B1 — Extensions: one ResourceDefinition per type.** Four
  ResourceDefinitions registered at startup (`AwsRdsPostgres`, `AwsS3Bucket`,
  `OauthClient`, `SnowflakeOauth`), each parented to `Project`. Provider
  validation moves onto each kind's JSON Schema. Reconcilers in
  `src/server/extensions/providers/` switch from the typed `project_extensions`
  table to listing/watching via the resource store (still in-process; A9
  lets them go external later). One-shot copy of existing
  `project_extensions` rows under an advisory lock; one release of read-only
  fallback to the typed table before B5a. OauthClient/SnowflakeOauth depend
  on A6.

- [ ] **PR B2 — Project + Environment as built-in resources.** `Project`
  parent: Organization; spec covers visibility/access class/owner ref/primary
  domain; status carries lifecycle + active deployment ref. `Environment`
  parent: Project. Typed tables stay authoritative for one release behind a
  feature flag; new writes go to resources, reads union both. K8s webhook
  keeps the typed projection until B4 rewires reconciliation.

- [ ] **PR B3 — ServiceAccount as a built-in resource.** Parent: Project.
  Spec: claims + trust policy. Status: last-used. Smaller and self-contained
  — useful warm-up for the B4 migration patterns.

- [ ] **PR B4 — Deployment as a built-in resource.** Parent: Project (needs
  A3 pagination). K8s controller reads Deployments via Watch (A8) instead of
  the typed table. Finalizers gate K8s-resource teardown; the existing
  `RiseProject` CRD becomes a controller-managed projection of the
  resource-store row. End-to-end test plan must cover deploy/rollback/stop/
  expire flows + CLI compatibility.

- [ ] **PR B5 — Drop typed tables** (one PR per table, gated on per-kind bake
  time). B1–B4 land on different clocks, so dual-read bake is per-kind:
  - [ ] **B5a** — drop `project_extensions` (depends on B1 bake)
  - [ ] **B5b** — drop `service_accounts` (depends on B3 bake)
  - [ ] **B5c** — drop `environments` (depends on B2 bake)
  - [ ] **B5d** — drop `deployments` (depends on B4 bake)
  - [ ] **B5e** — drop `projects`. Blocked on the full FK fan-in to
    `projects(id)`, not just the parent-chain children. Live FKs: the four
    above (→ B5a–B5d) plus `env_vars`, `custom_domains`, `project_app_users`.
    The last three are not yet on the roadmap — treat as B5e prerequisites;
    they get concrete PRs once the typed-table dependencies above resolve.
  - [ ] **B5f** — drop `teams` (depends on Team-as-built-in, post-roadmap
    backlog).

## Phase C — Multi-controller routing

C1 ships before C2 deliberately: build the reference controller against
today's static `deploymentControllerClass` mechanism, then let its lessons
shape the registry that comes next.

- [ ] **PR C1 — External K8s controller reference implementation.** A
  separate `rise-k8s-controller` binary that uses `rise-resource-client` (A9),
  authenticates with a controller JWT, watches Projects + Deployments scoped
  to its controller class, reconciles into the configured cluster. Validates
  the whole stack end-to-end: a second copy runs against a second cluster
  for a second Organization, proving the multi-tenant story.

- [ ] **PR C2 — `Controller` as a resource kind + per-Org controller
  registry.** Promote controller identity from a config-file entry to a
  first-class resource kind. Each `Controller` row carries: class name,
  trusted JWT issuer/JWKS, per-class config blobs (KubeConfig ref, AWS
  account, default region). An Organization's `spec.deploymentControllerClass`
  then points at a row rather than a config string; provisioning a customer
  with their own cluster becomes a `Controller` create + Organization update,
  not a backend redeploy.

## Open questions

Decide before coding the indicated PR.

- **A2 — `auth.admin_users` vs `auth.operator_users` overlap.** CLAUDE.md
  flags this as intentionally deferred: admins are default-Org admins,
  operators have generic-API access, the two roles do not overlap by
  configuration. Once A2 introduces end-user RBAC the question becomes
  user-visible (an admin who is also an operator should see consistent
  semantics in both API paths). Pick one of: (a) make admin a strict subset
  of org-admin under A2's model, (b) keep them disjoint and document in the
  operator docs, (c) merge into a single role.
- **A2 RBAC model.** Does `ownerRef` carry a user/team *name* or *uid*?
  Cross-Org references must be illegal — enforce at validation time.
- **A3 labels storage.** Labels in `metadata.labels` JSONB with a GIN index,
  or a separate `resource_labels` table for selector performance? JSONB is
  simpler; measure before adding the extra table.
- **A4 PATCH on `/status`.** PATCH must scope the controller's write to its
  own `status.controllers.<id>` key, mirroring `update_controller_status`.
- **A6 secrets — protected length.** Do we mask the *length* of a protected
  value (constant sentinel) or expose `len`? Recommend constant sentinel.
- **A8 watch.** `resourceVersion` granularity. The store's `revision` is
  per-row; use `(updated_at, uid)` as the cursor. Postgres LISTEN/NOTIFY has
  an 8 KB payload limit — publish only the UID and let the broadcaster
  re-fetch.
- **B1 — extensions migration cutover.** Dual-write or one-shot? Dual-write
  needs a per-table view that flips per release; one-shot is risky if a
  reconciler is mid-flight. Recommend a one-shot copy under an advisory lock
  plus a read-only fallback to the typed table for one release.
- **B4 Deployment cardinality.** Project the resource-store row into the
  existing typed-summary view for the dashboard, or have the dashboard call
  the resource API directly with pagination + cursor? Probably both —
  dashboard paginates the resource API, the K8s controller watches.
- **URL shape.** Should the operator API + end-user RBAC share one URL space
  (`/api/v1/resources/…` with authz inferred from token) or split into
  `/api/v1/org/{name}/resources/…`?
- **Project DTO future.** Should `Project` *replace* the typed Project DTO
  returned by today's typed API, or should the typed API keep its richer
  projection (computed URL, status summary, etc.) as a view over the
  resource?
- **Other typed objects worth migrating.** Custom domains, env vars, OAuth
  tokens. Env vars in particular are a prime candidate but high-cardinality
  + secret-bearing, so they pull in A3 + A6 together.

## Verification

- **Per PR** — `cargo fmt --all` + `cargo clippy --workspace --all-features
  --all-targets -- -D warnings` + `cargo test --workspace --all-features`. DB
  shape changes also need `mise run sqlx:prepare` and
  `mise run resource:schema:check`.
- **Phase A done** — an operator can `POST` + `PATCH` + `WATCH` a sample
  ResourceDefinition + custom resource end-to-end via only the resource
  API; client SDK demo connects from a small test binary.
- **Phase B done** — existing `rise project create / deployment create /
  service-account create` flows execute against the resource API with zero
  user-visible behavior change; typed-table writes are no-ops and reads are
  deprecated.
- **Phase C done** — two `rise-k8s-controller` instances against two separate
  kubeconfigs reconcile two Organizations' Projects independently; flipping
  `deploymentControllerClass` migrates Projects between them with no
  in-flight Deployment loss.

---

# Workstream 2 — Authentication & Token Exchange

## The arc

Today Rise accepts several JWT types **directly at every request** and
interprets them inline in the auth middleware and handlers. The costly case
is service-account (SA) authentication, which is inherently two-phase: the
middleware can only JWKS-validate the token's signature, since deciding
*which* SA the token represents needs the project context that only arrives
at the handler. So every project-scoped handler runs a second resolution
phase — looking up SAs by `(project_id, issuer)`, matching claims (with
glob support), handling zero/multi-match, resolving a synthetic user, then
enforcing environment restrictions. That logic is spread across ~10
handlers, where it drifts.

The principle: a caller presents one source JWT (plus optional project
context) to a dedicated **exchange endpoint** and receives a Rise-issued
**access token** that fully encodes the resolved principal. After that, the
middleware and handlers make snap decisions inspecting the Rise token — no
DB lookups, no context gathering, no logic that can drift. As a bonus,
`GET /api/v1/platform/capabilities` (today forced public, because it has no
project context) can become auth-only.

Phases:

- **Phase 0 — Pure-core crate extraction** (shipped). Move claim types,
  verify entry points, signing, and pure matchers into
  `crates/rise-backend-auth`. Behavior-preserving with two reviewed deltas;
  no exchange endpoint yet.
- **Phase 1 — Add the exchange endpoint** (effectively complete, additive).
  Ship `POST /api/v1/auth/token`, `AccessClaims`, access-token consumption in
  handlers, signer methods, and the `auth.allow_raw_external_tokens` operator
  toggle (default `true` — old raw-token CI keeps working). Capabilities stays
  public in this phase. [#367](https://github.com/rise-deploy/rise/pull/367)
  shipped the crate-side scaffolding (claim types, `Access` variant,
  `header.typ` discriminator, `sign_access_jwt`) *and*, beyond its title's
  scope, the server-side endpoint: the live `POST /api/v1/auth/token` handler
  (`src/server/auth/exchange/`), its `auth_token_max_ttl_seconds` /
  `auth.allow_raw_external_tokens` settings, the middleware +
  `platform_access_middleware` Access-token handling, **and access-token
  consumption** — access tokens flow through the existing `AuthContext::Access`
  path, so all project-scoped handlers already accept them (see PR 1B). That
  covers 1A, 1B, and 1C. The only Phase-1 remainder is the optional
  deprecation metric (1D) — deprioritized; the structured log is enough.
- **Phase 2 — CLI auto-exchange** (in progress). Pure CLI change — an
  `ExchangingTokenSource` decorator in `cli/token_source.rs` that calls the
  exchange endpoint with the inner OIDC token + project name and caches the
  returned Rise access token, reusing the existing `CachedToken`/`is_fresh`
  machinery.
- **Phase 3 — Remove the legacy path** (planned, breaking). Flip
  `auth.allow_raw_external_tokens` default to `false`. Delete the middleware
  external branch, `resolve_for_project`, `VerifiedExternalToken`, and the
  extractor fallback. Move `platform/capabilities` to auth-only routes.
  Tracked in [#374](https://github.com/rise-deploy/rise/issues/374).

## Hard ordering constraints

- **Phase 0 must precede Phase 1** — the exchange handler depends on the
  crate's `verify_external_jwt` and `RiseTokenSigner::sign_access`.
- **`header.typ` discriminator is a hard prerequisite of Phase 1.** Phase 0
  ships a `verify_rise_jwt` that dispatches *only* on `alg` (HS256→`Session`,
  RS256→`Ingress`) and never reads `header.typ`. Since Session and the new
  Access token are both HS256, they would be indistinguishable on the HS256
  branch. Before adding `RiseToken::Access`, `verify_rise_jwt` MUST branch
  on `header.typ` first (missing/unknown ⇒ `Session`, never require a
  specific `typ` on legacy sessions because `Header::new` emits `"JWT"`).
  Access carries a distinct custom `typ` (e.g. `rise-access+jwt`). **The
  `typ` check and the `Access` variant land in the same change** — never
  the variant first.
- **CLI Phase 2 has no server prerequisites beyond Phase 1.**
- **Phase 3 cannot land until the deprecation metric in Phase 1 shows raw-
  token traffic has drained.** The `auth.allow_raw_external_tokens` toggle
  defaults `true` initially specifically to avoid breakage; the deprecation
  metric (counted by issuer) tells operators when it's safe to flip.

## Phase 0 — Pure-core crate extraction (shipped)

- [x] **`crates/rise-backend-auth`** ([PR #364](https://github.com/rise-deploy/rise/pull/364)
  — extract auth core). Claim types (`RiseClaims`, `WorkloadClaims`,
  `ExternalClaims`), `verify_external_jwt` / `verify_rise_jwt`,
  `RiseTokenSigner`, pure matchers (`match_controller_identity`,
  `validate_custom_claims`, `audience_matches`/`matches_wildcard_pattern`,
  `build_controller_indexes`, `validate_controller_id`,
  `validate_oidc_issuer`), `is_rise_issued_jwt`, `JwksKeySource` trait,
  `AuthError`. No `reqwest`/`axum`/`sqlx`. `rise-deploy` depends on it.

Reviewed deltas (deliberate, not "byte-for-byte refactor"):

1. **Three relocated matchers' signatures change** from `anyhow::Result` to
   the crate's `AuthError`: `build_controller_indexes`,
   `validate_controller_id` (hand-rolled off `regex`),
   `validate_custom_claims`. `match_controller_identity` is unchanged.
2. **`verify_rise_jwt`'s RS256 branch is `Ingress`-only**; the output enum
   has no `Workload` variant. Matches today's behavior (nothing inbound ever
   verified a workload token) — modeling clarification, not a runtime
   change.

## Phase 1 — Add the exchange endpoint

- [x] **Crate-side scaffolding + exchange endpoint** ([PR #367](https://github.com/rise-deploy/rise/pull/367)
  — Access token kind, `typ` discriminator, exact issuer match).
  `AccessClaims` / `PrincipalClaims` / `Scope`; `RiseToken::Access` arm of
  `verify_rise_jwt` with the `header.typ` discriminator (access typ →
  `Access`, otherwise `Session` — couples the variant and the typ branch in
  a single change as the hard prerequisite requires);
  `RiseTokenSigner::sign_access_jwt` and `RISE_ACCESS_TYP`. Legacy adapters
  hardened to reject access tokens; `verify_jwt_skip_aud` also rejects any
  principal-carrying payload on the ingress path. `is_rise_issued_jwt`
  tightened to exact-issuer match (drops fuzzy port prefix) so the exchange
  can reliably reject Rise-issued source tokens.
  `rise_token_disambiguation_matrix` round-trip/rejection test added.
  Beyond the original "scaffolding" scope, also shipped server-side: the live
  `POST /api/v1/auth/token` exchange handler (`src/server/auth/exchange/`,
  mounted in `src/server/mod.rs`), the shared `auth/sa_match.rs` SA-matching
  module (used by both the exchange handler and the legacy
  `resolve_for_project`), the `auth_token_max_ttl_seconds` (default 600s) and
  `auth.allow_raw_external_tokens` (default `true`) settings,
  `#[serde(deny_unknown_fields)]` on `RiseClaims`, the main-middleware
  `RiseToken::Access` arm, and `platform_access_middleware` Access handling —
  i.e. the bulk of PR 1A and all of PR 1C below.
- [x] **PR 1A — Exchange handler** (folded into
  [#367](https://github.com/rise-deploy/rise/pull/367)). New module
  `src/server/auth/exchange/` (`mod.rs`, `handlers.rs`, `models.rs`,
  `routes.rs`) — `POST /api/v1/auth/token` (RFC 8693 token exchange), mounted
  in `src/server/mod.rs`. The SA-matching body of `resolve_for_project` was
  extracted to the shared `src/server/auth/sa_match.rs` (consumed by both the
  exchange handler and the still-live legacy path); the handler reuses the
  crate's `verify_external_jwt` + `match_controller_identity`,
  `RiseTokenSigner::sign_access_jwt`, plus `oauth_rate_limiter` /
  `extract_client_ip`. Settings `auth_token_max_ttl_seconds` (default 600s)
  and `auth.allow_raw_external_tokens` (default `true`);
  `#[serde(deny_unknown_fields)]` hardening on `RiseClaims`, paired with the
  `header.typ` check. **No separate `AccessPrincipal` extractor was built** —
  #367 instead added access-token consumption directly to `AuthContext`
  (the `Access` variant), so handlers accept access tokens through the
  existing path (see PR 1B).
- [x] **PR 1B — Handlers consume access tokens** (satisfied by
  [#367](https://github.com/rise-deploy/rise/pull/367), implemented
  differently than originally planned). The goal — every project-scoped
  handler accepts an exchanged access token — is already met: #367 folded
  access tokens into the existing `AuthContext` (the `Access(AccessClaims)`
  variant, picked up by `from_request_parts`, resolved by
  `resolve_for_project` → `resolve_access_for_project`). All 13 project-scoped
  sites in `deployment/handlers.rs` + `registry/handlers.rs` already route
  through `resolve_for_project`, and none call `auth.user()?`, so they consume
  access tokens unchanged — **no separate `AccessPrincipal` extractor and no
  handler swap were needed.** The originally-planned snap-decision form
  (`require_project(project.id)?` with no per-handler DB resolution) buys
  nothing while the legacy `VerifiedExternalToken` path must coexist, since
  that path still needs `resolve_for_project`. That simplification — deleting
  `resolve_for_project` and collapsing the per-handler boilerplate — is folded
  into **Phase 3 (PR 3A)**, where the legacy path is removed in the same pass.
- [x] **PR 1C — Middleware platform-access compatibility** (folded into
  [#367](https://github.com/rise-deploy/rise/pull/367)).
  `platform_access_middleware` recognizes the `AccessClaims` extension:
  `ServiceAccount`/`Controller` principals bypass the allowlist exactly as a
  legacy external token does, and a (reserved, not-yet-minted) `User` access
  token is rejected explicitly rather than 500ing. See
  `src/server/auth/middleware.rs`.
- [ ] **PR 1D — Deprecation metric + operator docs.** The structured
  deprecation *log* already shipped in
  [#367](https://github.com/rise-deploy/rise/pull/367)
  (`middleware.rs`: `"deprecated: accepting raw external token …"`). What
  remains: emit a real deprecation *metric* (a counter keyed by issuer) so
  operators can see who still needs migrating; commit to a target version at
  which `auth.allow_raw_external_tokens` flips to `false`; and update
  [`docs/engineering/src/content/docs/authentication.md`](docs/engineering/src/content/docs/authentication.md):
  add Access to the at-a-glance table, replace the "Forthcoming" section
  with the real exchange endpoint (request/response, relation to the inner
  OIDC token, the `header.typ` discriminator), and document
  `auth.allow_raw_external_tokens` including the deprecation guidance.
  Cross-link the design rationale — do not duplicate the
  `crates/rise-backend-auth/README.md` disambiguation matrix.

## Phase 2 — CLI auto-exchange

- [~] **PR 2A — `ExchangingTokenSource` decorator.** Pure CLI change in
  `src/cli/token_source.rs`. Wraps the resolved provider: on `token()` it
  calls the exchange endpoint with the inner OIDC token + project name and
  caches the returned Rise access token, reusing the existing
  `CachedToken`/`is_fresh` machinery. Nested freshness: re-mint the inner
  OIDC token only when stale; re-exchange the outer Rise token only when
  stale (a fresh access token is returned without consulting the inner
  source). `rise deploy` (which knows the project) constructs it around the
  resolved base provider; the server already supports it from Phase 1.
  **Pass-through (no exchange) for tokens the endpoint cannot/must not
  exchange:** a Rise-issued session token (detected by `iss` == backend URL —
  interactive `rise login`) and an opaque non-JWT bearer both pass through
  unchanged, so behavior is fully backward-compatible. The legacy raw-token
  path stays accepted server-side through the cutover release
  (`auth.allow_raw_external_tokens`, default `true`) so customers can migrate
  before Phase 3 flips the default. **Shipped for `rise deploy`; remaining:**
  extend the wrap to the other project-scoped CLI commands
  (`service_account`, `environment`, `extension`, `encrypt`, deployment
  list/show via `resolve_token_with_retry`) and add an E2E test that
  authenticates with a service account and asserts the exchange path
  succeeds.

## Phase 3 — Remove the legacy path

- [ ] **PR 3A — Flip the default + delete the external branch.** Flip
  `auth.allow_raw_external_tokens` default to `false`. Delete the middleware
  external branch and the legacy raw-token resolution: the
  `AuthContext::ExternalToken` variant, `VerifiedExternalToken`, and the
  `ExternalToken` arm of `resolve_for_project` (the SA-matching now reachable
  only via the exchange endpoint's shared `sa_match.rs`). The
  `AuthContext::Access` consumption path stays — that is how handlers accept
  exchanged tokens. This is also where the long-deferred handler
  simplification lands: with the legacy path gone, the per-handler
  `resolve_for_project` + 404-masking boilerplate collapses to a snap
  decision on the resolved access principal. Move `platform::routes()`
  from `public_routes` to `auth_only_routes`. Remove `ControllerAuthContext` /
  `VerifiedControllerToken` from `src/server/auth/controller.rs`.
  (`match_controller_identity` / `build_controller_indexes` /
  `ControllerIdentity` stay — they relocated to `rise-backend-auth` in
  Phase 0 and are re-exported.) Tracked in
  [#374](https://github.com/rise-deploy/rise/issues/374).

## Deferred follow-ups

Listed so they don't fall out of the plan; not currently sequenced.

- [ ] **Per-SA configurable scopes.** Needs a DB column + migration +
  `src/db/service_accounts.rs` helper + `mise run sqlx:prepare`.
- [ ] **Unify user OIDC login on `AccessClaims::User`.** Touches cookies,
  ingress, `verify_jwt_skip_aud`.
- [ ] **`jti` deny-list for hard revocation.** Reintroduces a DB lookup,
  partially defeating the no-DB-in-middleware goal. Only if the short-TTL
  window proves insufficient.
- [ ] **De-duplicate the controller-id validator across
  `rise-backend-auth` and `rise-resource-api`.** Both crates currently
  carry their own validation logic; consolidate into one shared validator
  to prevent drift.
- [ ] **Overlapping previous-key verification window for HS256 rotation.**
  Today the symmetric secret is shared by sessions and access tokens, so
  rotating invalidates everything live at once. Preferred posture is
  `verify against {current, previous}, sign with current` so rotation
  drains within one TTL; alternative is hard dual cutover. Sessions are
  longer-lived than the ≤10-min access-token TTL, so the window is the
  better fit.

## Open questions

- **Access-token TTL.** *Resolved:* shipped as `auth_token_max_ttl_seconds`,
  default 600s (10 minutes) — the revocation-window bound, since an exchanged
  token can't be revoked mid-life. `jti` field leaves room for a future
  deny-list.
- **Capabilities endpoint timing.** Plan keeps it public through Phase 1
  and flips to auth-only in Phase 3. Could flip earlier (any time after
  Phase 2 ships the CLI auto-exchange so internal callers always bring an
  access token).
- **Exchange-endpoint SLO.** Once Phase 3 lands, the exchange is a hard
  dependency on the deploy critical path and a public DoS target
  (unauthenticated by design). Treat as tier-1 with its own SLO/alerting;
  CLI should retry on 5xx via existing `token_with_retry`. Decide where
  the SLO lives (operator docs vs. SRE-internal).

---

# Maintenance

When a PR merges:
1. Flip its `[ ]` to `[x]` in the relevant section.
2. Add the merged PR number in parentheses (e.g. `(PR #341)`).
3. Move "Deferred to follow-ups" notes onto the actual follow-up PRs as
   they become concrete.
4. Update [`upgrade-notes.md`](docs/engineering/src/content/docs/upgrade-notes.md)
   if there's operator-visible impact.
5. The
   [Rise Rollout Tracker GitHub Project](https://github.com/orgs/rise-deploy/projects/1)
   owns *live status* (CI, review state, release target); this file owns
   *plan and rationale*. Don't duplicate per-PR fields here.
