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
   ┌─────────────────────────────┐             ┌────────────────────┐
   │ A1 Typed built-in registry  │────────────▶│ B1 Extensions →    │
   │ A2 End-user / org RBAC      │──┐          │    one RD per type │
   │ A3 Pagination + selectors   │  │          │ B2 Project + Env   │──┐
   │ A4 PATCH (merge+RFC6902)    │  │          │ B3 ServiceAccount  │  │
   │ A5 Server-side Apply        │  ├─────────▶│ B4 Deployment      │──┤
   │ A6 Secret-marked fields     │  │          │ B5 Drop typed tbls │◀─┘
   │ A7 Schemas served via HTTP  │  │          └────────────────────┘
   │ A8 Watch API + LISTEN/NOTIFY│  │          C. Multi-controller
   │ A9 rise-resource-client     │  │             ┌─────────────────┐
   └─────────────────────────────┘  └────────────▶│ C1 Per-org      │
                                                  │    controller   │
                                                  │    routing      │
                                                  │ C2 External K8s │
                                                  │    controller   │
                                                  │    reference    │
                                                  │    impl         │
                                                  └─────────────────┘
```

Hard ordering rules:

- **A1 (typed built-in registry) blocks B-anything.** Adding Project as a
  built-in means adding *the second* built-in beyond Organization/RD; doing it
  on top of the current ad-hoc match in `pg_store::builtin_collection_info`
  will compound technical debt.
- **A2 (org-scoped RBAC) blocks B2/B3/B4.** Project/Environment/Deployment/SA
  are end-user-owned; until the generic API can authorize end users by
  Organization membership + per-resource ownership, those kinds cannot move
  off their typed APIs without losing access control.
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

**PR A1 — Typed built-in resource registry**
- Files: `crates/rise-resource-store/src/{registry.rs,pg_store.rs,validation.rs}`,
  `crates/rise-resource-api/src/lib.rs`.
- Introduce `BuiltInRegistration { collection, api_version, kind, parent,
  spec_validator, status_writers, schema_fn }` and a `BuiltInRegistry` built
  at process start. Replace the hardcoded match in
  `builtin_collection_info()` with registry lookups. Organization +
  ResourceDefinition become two `BuiltInRegistration::for::<…>()` constructor
  calls. Generated JSON schemas list every registered built-in.
- Removes blocker: subsequent built-in kinds (Project, Env, Deployment, SA)
  become one-call registrations rather than copies of the match arm.
- Risk: registry must be the only writer of `CollectionInfo`; ensure
  `resolve_collection_by_kind` walks it before external `ResourceDefinitions`.

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

### Migration (depends on A1, A2, plus per-kind subset)

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

**PR B5 — Drop typed tables**
- After two releases of dual-read, drop `project_extensions`, then
  `environments`, `service_accounts`, `deployments`, `projects`, `teams`.
- Remove the corresponding typed routes (or thin them down to
  read-throughs against the resource API for the CLI compatibility window).

### Multi-controller routing

**PR C1 — Per-Organization controller registry**
- Each `Organization.spec.deploymentControllerClass` already chooses a
  controller class. Add a backend-side controller-registry table (or
  ResourceDefinition `Controller`) that records which controller identities
  are registered against which class, plus per-Org config blobs (KubeConfig
  ref, AWS account, default region). RDS/S3/etc. providers can register the
  same way per class.
- A controller-class string moves from "the one in-process K8s controller"
  to "a queryable list of registered controllers."
- Required so a customer can BYO an external K8s controller and we route
  *their* Org's Projects to it.

**PR C2 — External K8s controller reference implementation**
- A separate `rise-k8s-controller` binary that uses `rise-resource-client`
  (A9), authenticates with a controller JWT, watches Projects + Deployments
  scoped to its controller class, and reconciles into the configured
  cluster.
- Validates the whole stack end-to-end: a second copy can run against a
  second cluster for a second Organization, proving the multi-tenant story.

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
