# Roadmap — Finish the Multi-Tenancy Migration and the Generic Resource API

> Follow-on to [`MULTI_TENANCY_PLAN.md`](./MULTI_TENANCY_PLAN.md). That plan's
> PR 1–6 have all landed; this document is the next-phase roadmap that takes
> the resource API from "operator-only substrate that backs Organizations and
> ResourceDefinitions" to "the canonical, productionizable storage layer for
> nearly everything in Rise, with per-Organization external controllers."

## Context

MULTI_TENANCY_PLAN.md's PR 1–6 have all landed: the workspace conversion,
`rise-resource-api` + `rise-resource-store` crates, the operator role and
controller-JWT auth contexts, the generic HTTP API under `/api/v1/resources`
with GC + cascade-finalizer semantics, the default-Organization bootstrap that
backfills `organization_resource_uid` onto users/teams/projects, and the
schema-generation tooling + operator docs. Today the K8s deployment controller
already reads its Organization's `spec.deploymentControllerClass` to decide
whether to reconcile a `RiseProject` (`src/server/deployment/webhook.rs`).

What remains is the rest of the original ambition: make the resource store the
single, productionizable storage layer for almost everything in Rise; let
external controllers — including per-Organization controllers in different
clouds or clusters — reconcile against it; and decommission the bespoke typed
tables that today still own projects, teams, deployments, environments,
service accounts, and extensions. That requires:

1. A **mature** resource-store substrate (typed built-in registry, end-user
   RBAC, pagination + selectors, watch, PATCH/server-side-apply,
   secret-marked fields, schemas served via HTTP, a client SDK).
2. **Migrating** Rise's typed objects on top of that substrate — one
   ResourceDefinition per extension type, plus first-class built-in
   Project/Environment/Deployment/ServiceAccount kinds.
3. **Operational** routing of multiple controllers per Organization so two
   customers can use Rise to deploy to two different clusters/AWS accounts,
   each with their own RDS controller, all configured through the resource
   API.

## Workstreams and Dependency Graph

```
A. Substrate maturation                     B. Migration
   ┌─────────────────────────────────┐         ┌──────────────────────────┐
   │ A1 Typed built-in registry      │         │ B1 Extensions →          │
   │ A2 End-user / org RBAC          │──┐      │    one RD per type       │
   │ A3 Pagination + selectors       │  │      │ B2 Project + Env         │──┐
   │ A4 PATCH (merge+RFC6902)        │  ├─────▶│ B3 ServiceAccount        │  │
   │ A5 Server-side Apply            │  │ (A1+ │ B4 Deployment            │──┤
   │ A6 Secret-marked fields         │  │  A2) │ B5{a..f} Drop typed tbls │◀─┘
   │ A7 Schemas served via HTTP      │  │      └──────────────────────────┘
   │ A8 Watch API + LISTEN/NOTIFY    │  │      C. Multi-controller
   │ A9 rise-resource-client         │  │         ┌─────────────────────┐
   │ A10 Built-in version evolution  │  │         │ C1 External K8s     │
   │ A11 RD spec evolution           │  │         │    controller       │
   │ A12 Audit on PATCH/RBAC paths   │  │         │    reference impl   │
   │ A13 Reserve (group,kind) for    │  │         │ C2 Controller as a  │
   │     built-ins (shadowing fix)   │  │         │    resource kind +  │
   │ A14 Watch backpressure / limits │  └────────▶│    per-Org registry │
   │ (design note for A2:            │            └─────────────────────┘
   │   admin vs operator overlap)    │
   └─────────────────────────────────┘
```

Hard ordering rules:

- **A1 (typed built-in registry) blocks B2/B3/B4 — but not B1.** Built-ins
  like Project/Environment/Deployment/ServiceAccount route through the
  registry; adding a new built-in to the pre-A1 hardcoded match in two
  places was the tax A1 removes. B1, by contrast, ships extensions as
  *external* `ResourceDefinitions` validated by JSON Schema with rows in the
  `resource_definitions` projection — that path never touches the built-in
  registry, so B1 can land before A1 if scheduling demands.
- **A2 (org-scoped RBAC) gates the *API-surface* migration of B2/B3/B4, not
  the *data-plane* move.** The migration can run in two stages: first land
  the data-plane (typed APIs become facades that write through the resource
  store with system credentials — no RBAC change), then later flip the
  user-facing routes onto the generic resource API (requires A2). B2 can
  therefore begin once A1 is in.
- **A3 (pagination + label selectors) blocks B4 (Deployment).** Deployments
  are high-cardinality (≫ projects). Unpaginated `list` is acceptable for
  Projects but not for Deployments.
- **A6 (secret-marked fields) blocks B1 for OauthClient and SnowflakeOauth.**
  Those extensions store client secrets and refresh tokens; we can't migrate
  them onto a `spec` JSON blob that reads back to operators.
- **A8 (watch) blocks C1/C2 in practice.** Cross-cluster external controllers
  polling `list` is a non-starter; they need a change feed.
- **A9 (rise-resource-client) is the artifact C2 ships against.** It depends
  on A8 for the Watch helpers but can stub out HTTP semantics earlier so
  in-tree controllers consume it first.

## Recommended PR Sequence

The sequence below is what I recommend; each PR is independently
reviewable/shippable and the dependencies are the minimum to keep each PR
green.

### Substrate maturation (does not touch existing typed APIs)

**PR A1 — Typed built-in resource registry** *(shipped)*
- Files: `crates/rise-resource-store/src/{builtin.rs,lib.rs,pg_store.rs}`.
- `BuiltInRegistration { collection, api_version, kind, parent,
  spec_validator }` and a `BuiltInRegistry` indexed by both plural and
  `(group, kind)`. `PgResourceStore::new(pool)` keeps using
  `BuiltInRegistry::defaults()`; `with_builtin_registry()` is exposed for
  tests and future feature-flagged built-ins. Both hardcoded matches in
  `pg_store.rs` (`builtin_collection_info`, `resolve_collection_by_kind`)
  now consult the registry.
- **Deferred to follow-ups:** `schema_fn` on the registration (lands with A7,
  which needs per-collection schemas at runtime for the `_schema`/`_discovery`
  endpoints — without it, A7 would re-create the hardcoded match A1 just
  removed); `status_writers` (lands when the first built-in actually has a
  controller writing its status — for now `allowed_status_controller_ids`
  stays empty for built-ins and the field is omitted from the registration
  struct entirely); driving `schemas.rs::generate_schemas` off the registry
  (re-evaluate when there are >2 built-ins).
- Removes blocker: subsequent built-in kinds (Project, Env, Deployment, SA)
  become one-call registrations rather than copies of the match arm.

**PR A2 — Org-scoped RBAC on the generic resource API**
- Add `OrganizationRole` (member, admin) sourced from
  `user_organization_memberships`; cache per-request in `AuthContext`.
- Add a per-resource ownership model: `metadata.ownerRef = { kind, name }`
  pointing at a `User` or `Team` (eventually a built-in kind itself; for now
  a typed reference looked up against existing tables).
- **Ownership propagates down the parent chain (additive).** A descendant
  resource (Deployment, Environment, ServiceAccount, env var, …) does *not*
  need its own `ownerRef` — the authz check walks the parent chain looking
  for the nearest ancestor that declares one. The descendant can also carry
  its own `ownerRef` to *extend* the access set (additive union with
  inherited owners), never to *restrict* it. Conceptually: an owner of a
  Project owns everything underneath it; adding an owner on a Deployment
  grants that principal extra access without revoking Project owners.
- Replace the global `require_operator` gate in
  `src/server/resources/handlers.rs` with: operators bypass; org-admins have
  CRUD on their org's subtree; org-members get CRUD on resources they own
  by inherited or direct `ownerRef`.
- Files: `src/server/auth/context.rs`, `src/server/resources/handlers.rs`,
  new `src/server/resources/authz.rs`.
- Removes blocker: Project/Team/Deployment migration becomes possible without
  forking the API for end users.
- Risk: the existing typed Project/Team APIs already enforce
  ownership/team-membership; we must define the precise relationship between
  *typed-API checks* (used today) and *generic-API checks* (used after
  migration) so a request never gets both half-applied. Walking the parent
  chain on every authz check must reuse the already-resolved ancestor row
  set (`resolve_path` returns the chain) — no second DB walk per request.

**PR A3 — Pagination + label/field selectors on `list`**
- Add `metadata.labels BTreeMap<String,String>` to the resource envelope
  (`crates/rise-resource-api`) and a GIN index on
  `resources.metadata->'labels'`.
- Extend `ResourceStore::list*` with `ListOptions { limit, continue_token,
  label_selector, field_selector }`. Adopt Kubernetes-style opaque
  `continue` tokens (encode `(updated_at, uid)`).
- HTTP: parse `?limit=&continue=&labelSelector=&fieldSelector=`.
- Removes blocker: Deployment + Env vars migration.

**PR A4 — JSON Merge Patch + RFC 6902 PATCH**
- Wire `PATCH /api/v1/resources/{*path}` into the dispatcher.
  `Content-Type: application/merge-patch+json` → RFC 7396; `…/json-patch+json`
  → RFC 6902. Both go through the same revision-checked update path as PUT.
- Files: `src/server/resources/handlers.rs`, new `…/patch.rs`.
- Risk: PATCH on `/status` must remain controller-only; PATCH on `spec` must
  reject changes to identity fields.

**PR A5 — Server-Side Apply (`PATCH … application/apply-patch+yaml`)**
- Field-manager tracking (`metadata.managedFields`) so multiple actors (e.g.,
  the CLI, a controller, and a Helm-style operator) can co-own different
  fields without clobbering each other. This is what "declarative" tooling
  for the resource API will want.
- Optional in the first cut; can land after the initial migration if PATCH
  in A4 is enough for CLI ergonomics.

**PR A6 — Secret-marked fields with encryption-provider integration**
- Two schema annotations on a leaf string property:
  - `x-rise-secret: true` — value is encrypted at rest and handled as a
    `SecretString` in-process (no `Debug`/`Display` leak; reuses the same
    pattern as `extensions::InjectedEnvVarValue::Secret`). Any caller
    authorized to read the parent resource also gets the *decrypted* value
    in the response (no extra round-trip). This matches today's "secret"
    env-var behavior.
  - `x-rise-secret: protected` — same encryption-at-rest, but the API
    response masks the value (the existing `Protected` env-var pattern from
    `src/server/extensions/mod.rs`). Controllers that need plaintext fetch
    it through a dedicated decrypt endpoint scoped to their controller
    identity.
- The single hard invariant: secret values are **never** persisted in
  cleartext in the `resources` table. The visibility tier (returned vs.
  masked) is a separate, configurable property of the field.
- Store layer: on write, encrypt with the configured encryption provider
  (`src/server/encryption/`); on read, decrypt for callers authorized for
  the parent and either inline the plaintext (`secret`) or return a sentinel
  like `{ "encrypted": true }` (`protected`). Add a `SecretString` wrapper
  in `rise-resource-api` so even internal log/`Debug` output cannot
  accidentally leak the value.
- Removes blocker: migrating OauthClient + SnowflakeOauth extensions
  (client secret, refresh tokens) and protected env vars.

**PR A7 — Serve JSON Schemas + OpenAPI discovery via HTTP**
- `GET /api/v1/resources/_discovery` — lists every collection (built-in +
  ResourceDefinition) with served versions.
- `GET /api/v1/resources/{group}/{version}/{plural}/_schema` — returns the
  spec JSON Schema for that version.
- `GET /api/v1/resources/_openapi` — combined OpenAPI 3.1 document for
  language servers / `kubectl explain`-style CLI commands.
- Reuses `generate_schemas()` in `src/server/resources/schemas.rs`; the
  on-disk artifacts in `docs/engineering/public/schemas/` remain the
  CI-checked copy.

**PR A8 — Watch API (LISTEN/NOTIFY-backed change feed)**
- Postgres trigger on `resources` publishes `resource_change` events; backend
  fans them out via an in-process broadcaster.
- `GET /api/v1/resources/{*path}?watch=true` returns chunked NDJSON of
  `{type: Added|Modified|Deleted, object: …, revision}` events; resumable
  from a client-supplied `resourceVersion`. Reuse `updated_at + revision` as
  the cursor.
- The GC worker keeps its existing `list_pending_collection` poll; watch is
  for end-user/controller reconcile loops.

**PR A9 — `crates/rise-resource-client`**
- Async HTTP client that consumes A2/A3/A4/A8: typed `Resource<TSpec,
  TStatus>` accessors per built-in/RD, controller-JWT auth provider,
  watch-with-resume helpers, finalizer-add/remove helpers.
- First consumer is the in-tree K8s controller (it goes through the SDK even
  though it shares the process); same trait powers external controllers in
  C2.

**PR A10 — Built-in version evolution**
- A1 collapses `storage`/`served`/`declared` to a single api_version per
  built-in ("version evolution happens through code"). That's fine for
  Organization/ResourceDefinition forever; it breaks the moment Project moves
  from `v1alpha1` to `v1` post-B2.
- Extend `BuiltInRegistration` with multiple typed `versions[]` (storage,
  served lists) and a `convert_spec` hook between adjacent versions; reuse
  the registry's existing `(group, kind)` indexing.
- Lands before any built-in needs a second version, not on a fixed
  calendar. Pulled in from "the future" to make the dependency visible.

**PR A11 — ResourceDefinition spec evolution for existing rows**
- Today, registering a new RD version is structurally supported but the
  store does not constrain stored rows on an old version when the new
  schema tightens. The plan's "old stored versions remain available to
  owning controller until marked neither served nor storage" needs a
  concrete validation policy: do tightened schemas reject reads of valid-
  by-old-schema rows? Block updates that don't migrate?
- Lands alongside (or just before) the first external controller that
  actually rolls a schema.

**PR A12 — Audit logging on PATCH / RBAC paths and discovery completeness**
- The existing audit trail covers POST/PUT/DELETE + controller and operator
  finalizer/status writes (`src/server/resources/handlers.rs` `rise::audit`
  events). PATCH (A4/A5), end-user RBAC writes (A2), and the discovery
  endpoints (A7) need the same coverage — particularly attribution for
  end-user actors once A2 lands, since "operator did X" no longer covers
  every mutation.
- Slot ordering: this stays unblocked through A4/A5/A2/A7 and lands as one
  consolidated cleanup PR after them.

**PR A13 — Reserve `(group, kind)` for built-ins; close the shadowing gap**
- Pre-existing: `validate_resource_group` is DNS-shape-only and
  `RESERVED_COLLECTION_NAMES` covers plurals only, so an external
  `ResourceDefinition` *can* declare `(rise.dev, Organization)` under a
  non-`organizations` plural and be silently shadowed in by-kind resolution
  (the registry's `lookup_by_group_kind` already enforces the right
  behaviour for the `rise.dev` group, but the RD validator does not stop
  the registration in the first place).
- Add a `register_resource_definition`-side check: reject any external RD
  whose `(group, kind)` collides with a registered built-in, regardless of
  plural. The built-in registry is the natural authority here — pass
  `Arc<BuiltInRegistry>` into the RD validation path.
- Small, self-contained; can land anywhere after A1.

**Design note for A2 — `auth.admin_users` vs `auth.operator_users` overlap**
- *Not a separate PR — must be resolved during A2's design phase.* Recorded
  here so it doesn't fall out of the plan.
- CLAUDE.md flags this as intentionally deferred: admins are default-Org
  admins, operators have generic-API access, the two roles do not overlap
  by configuration. Once A2 introduces end-user RBAC, the question becomes
  user-visible (an admin who is also an operator should see consistent
  semantics in both API paths). Pick one of: (a) make admin a strict subset
  of org-admin under A2's model, (b) keep them disjoint and document
  explicitly in the operator docs, (c) merge into a single role.

**PR A14 — Watch backpressure, connection limits, and observability**
- A8 ships the change feed; this PR adds per-connection rate limits,
  max-concurrent-watch caps per principal, NDJSON chunk timeouts, and
  metrics (active-watch gauge, events-fanned-out counter, drop counter for
  slow consumers). Without it, a single misbehaving controller can exhaust
  connections.
- Self-contained follow-up to A8; not on B/C's critical path.

### Migration

B1 is independent of A1 (extensions ride external `ResourceDefinitions`). B2/B3/B4
each have two stages: the **data-plane move** (typed APIs become facades over
the resource store; needs A1 only) and the **API-surface flip** (user-facing
routes go to `/api/v1/resources/…`; needs A2 for end-user RBAC). Per-PR
dependencies in each section below.

**PR B1 — Extensions: one ResourceDefinition per type**
- Add four ResourceDefinitions registered at startup (`AwsRdsPostgres`,
  `AwsS3Bucket`, `OauthClient`, `SnowflakeOauth`), each parented to
  `Project`. Move provider validation onto each kind's JSON Schema.
- Background reconcilers in `src/server/extensions/providers/` switch from
  the typed `project_extensions` table to listing/watching resources via the
  resource store (still in-process; A9 lets them go external later).
- Migration writes a row into `resources` for each existing
  `project_extensions` row, then dual-reads for one release before B5.
- OauthClient/SnowflakeOauth need A6 first; AwsRdsPostgres / AwsS3Bucket can
  ship as soon as A1 is in.

**PR B2 — Project + Environment as built-in resources**
- `Project` built-in: parent Organization, spec covers visibility/access
  class/owner ref/primary domain; status carries current lifecycle state +
  active deployment ref.
- `Environment` built-in: parent Project.
- Typed `projects` and `environments` tables remain authoritative behind a
  feature flag for one release; new writes go to resources, reads union
  both. The K8s webhook keeps using the typed projection until B4 fully
  rewires reconciliation.

**PR B3 — ServiceAccount as a built-in resource**
- Parent: Project. Spec: claims + trust policy. Status: last-used.
- Smaller and self-contained — good warm-up for the B4 migration patterns.

**PR B4 — Deployment as a built-in resource**
- Parent: Project (subject to A3 pagination). The K8s controller reads
  Deployments via Watch (A8) instead of the typed table. Finalizers gate
  K8s-resource teardown; the existing `RiseProject` CRD becomes a
  controller-managed projection of the resource-store row.
- This is the biggest migration: end-to-end test plan must cover
  deploy/rollback/stop/expire flows + CLI compatibility.

**PR B5 — Drop typed tables (one PR per table, gated on per-kind bake time)**
- B1–B4 land at different times, so "after two releases of dual-read" is a
  per-kind clock, not a global one. Split into six independent PRs, each
  gated on its own kind's bake time and on the dashboard/CLI having moved
  off the typed read path:
  - **B5a** — drop `project_extensions` (depends on B1 bake)
  - **B5b** — drop `service_accounts` (depends on B3 bake)
  - **B5c** — drop `environments` (depends on B2 bake)
  - **B5d** — drop `deployments` (depends on B4 bake)
  - **B5e** — drop `projects`. Blocked on the full FK fan-in to
    `projects(id)` clearing, not just the parent-chain children. As of this
    writing those FKs are: `project_extensions` (→ B5a), `service_accounts`
    (→ B5b), `environments` (→ B5c), `deployments` (→ B5d), `env_vars`,
    `custom_domains`, and `project_app_users`. The last three are not yet
    on the migration roadmap and gate B5e independently — see follow-ups
    below.
  - **B5f** — drop `teams` (depends on the typed Team API moving onto a
    Team built-in, which is on the post-roadmap backlog)
- Each PR removes the corresponding typed routes (or thins them to
  read-throughs against the resource API for the CLI compatibility window).
- **Follow-up migrations (not yet sequenced)**: `env_vars` (high-cardinality
  + secret-bearing → pulls in A3 + A6, currently flagged in Open Question 3);
  `custom_domains` (parent: Project or Environment, depending on the env-
  scoped-domain work); `project_app_users` (end-user identity rows under a
  Project — likely belongs under a Team or Project user-membership kind, not
  its own root collection). Treat as B5e prerequisites; add concrete PRs
  once the typed-table dependencies above are resolved.

### Multi-controller routing

C1 and C2 were originally sequenced "registry first, then controller."
On reflection that's backwards: building a controller-registry abstraction
before any external controller exists risks designing for hypothetical
needs. Ship the reference controller first against the existing static
`deploymentControllerClass` mechanism — what it teaches will inform the
registry's shape (and, given the rest of this plan, the answer to
"controller-registry table vs `Controller` resource kind" is almost
certainly "resource kind").

**PR C1 — External K8s controller reference implementation** *(was C2)*
- A separate `rise-k8s-controller` binary that uses `rise-resource-client`
  (A9), authenticates with a controller JWT, watches Projects + Deployments
  scoped to its controller class, and reconciles into the configured
  cluster. Runs against today's static `deploymentControllerClass`.
- Validates the whole stack end-to-end: a second copy can run against a
  second cluster for a second Organization, proving the multi-tenant story.

**PR C2 — `Controller` as a resource kind + per-Org controller registry** *(was C1)*
- Promote "controller identity" from a config-file entry to a first-class
  resource kind. Each `Controller` row carries: class name, trusted JWT
  issuer/JWKS, per-class config blobs (KubeConfig ref, AWS account, default
  region). RDS/S3/etc. providers register the same way per class.
- An Organization's `spec.deploymentControllerClass` now points at a row
  rather than a config string; provisioning a customer with their own
  cluster becomes a Controller create + Org update, not a backend redeploy.
- Lands after C1 so the abstraction is shaped by a real consumer.

## Things to Decide Before Coding Each PR

- **A2 RBAC model:** does `ownerRef` carry a user/team *name* or *uid*? Cross-Org
  references must be illegal — enforce at validation time.
- **A3 labels storage:** labels in `metadata.labels` JSONB with a GIN index, or
  a separate `resource_labels` table for selector performance? JSONB is simpler;
  measure before adding the extra table.
- **A4 PATCH on `/status`:** PATCH must scope the controller's write to its
  own `status.controllers.<id>` key, mirroring `update_controller_status`.
- **A6 secrets:** `secret` fields are returned decrypted to any caller who
  can read the parent (no extra endpoint); `protected` fields are masked
  with a sentinel (`{ encrypted: true }`). All on-the-wire and at-rest
  values are encrypted; in-process they're wrapped in `SecretString` so a
  stray `Debug` log can't leak them. Open: do we mask the *length* of a
  protected value too (return a constant sentinel) or expose `len`?
  Recommend constant sentinel.
- **A8 watch:** resourceVersion granularity. The store's `revision` is
  per-row; use `(updated_at, uid)` as the cursor. Postgres LISTEN/NOTIFY has
  an 8 KB payload limit — publish only the UID and let the broadcaster
  re-fetch.
- **B1 extensions migration:** dual-write or one-shot? Dual-write needs a
  per-table view that flips per release; one-shot is risky if a reconciler
  is mid-flight. Recommend a one-shot copy under an advisory lock plus a
  read-only fallback to the typed table for one release.
- **B4 Deployment cardinality:** project the resource-store row into the
  existing typed-summary view for the dashboard, or have the dashboard call
  the resource API directly with pagination + cursor? Probably both: dashboard
  paginates the resource API; the K8s controller watches.

## Verification

Per-PR plus phase-level smoke tests:

- **Per-PR:** every PR ships `cargo test --all-features`,
  `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo fmt --all`, and where DB shape changes,
  `mise run sqlx:prepare` + `mise run resource:schema:check`.
- **Phase A done:** an operator can `POST` + `PATCH` + `WATCH` a sample
  ResourceDefinition + custom resource end-to-end using only the resource
  API; client SDK demo connects from a small test binary.
- **Phase B done:** existing `rise project create / deployment create /
  service-account create` CLI flows execute against the resource API with
  *zero* user-visible behavior change; typed-table writes are no-ops and
  reads are deprecated.
- **Phase C done:** two `rise-k8s-controller` instances running against two
  separate kubeconfigs reconcile two Organizations' Projects independently;
  flipping `deploymentControllerClass` migrates Projects between them with
  no in-flight Deployment loss.

## Open Questions to Resolve Before Starting

1. Do we want the operator API + end-user RBAC to share one URL space
   (`/api/v1/resources/…` with authz inferred from token) or split into
   `/api/v1/org/{name}/resources/…`?
2. Should `Project` *replace* the typed Project DTO returned by today's
   typed API, or should the typed API keep its richer projection (with
   computed URL, status summary, etc.) as a view over the resource?
3. Are there other typed objects worth migrating that aren't on the list
   (custom domains, env vars, OAuth tokens)? Env vars in particular are a
   prime candidate but also high-cardinality + secret-bearing, so they pull
   in A3 + A6 together.
