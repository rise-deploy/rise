# Generic Resource Storage And Organizations

## Summary

Introduce Rise's generic resource storage layer, with `Organization` and `ResourceDefinition` as the first built-in resource types. Organizations become the foundation for multi-organization support. ResourceDefinitions allow external controllers to register custom resources that use Rise for generic resource/state storage while owning their own schemas and migrations.

This phase intentionally keeps normal generic resource CRUD operator-only. Controller JWTs may use controller-specific status/finalizer operations. Organization-level end-user RBAC is out of scope for v1 and will be added later without exposing partially implemented multi-tenant access controls.

This phase is still a single-default-Organization compatibility phase for the existing typed APIs. It creates the Organization model, backfills existing typed rows to the default Organization, and prevents new typed rows from being unlinked, but it does not expose real multi-Organization user isolation through the existing typed project/team/deployment APIs yet.

## Design Rationale

Rise needs a Kubernetes-like external controller pattern for resource reconciliation, but Rise must not make Kubernetes itself the control-plane dependency. Kubernetes is one deployment target; future targets such as Snowflake SPCS need the same controller/resource lifecycle model without depending on Kubernetes CRDs. A small generic resource substrate in Rise provides that common control-plane model: external controllers can register resource types, own schemas and migrations, update status/finalizers, and reconcile against Rise state regardless of the deployment backend.

Planning Organizations and generic resources together is intentional. Organization is the first tenant-boundary resource and should be born in the target resource model instead of being introduced as another bespoke typed table that later has to migrate into the generic API. The discipline for this phase is scope: build the narrow resource substrate, Organizations, ResourceDefinitions, operator-only CRUD, controller status/finalizers, and default-Organization linkage, but do not present this as complete multi-Organization isolation for the existing typed APIs.

## Crate Structure

Add only the crates needed for the new resource layer. Do not move the whole backend yet.

- Convert the repository to a Cargo workspace so the new crates can be built and tested locally:
  - keep the existing `rise` binary/package as the only published/released artifact for now
  - do not publish the new resource crates in this phase
  - wire feature flags so `cargo check --workspace --all-features` covers the backend/resource crates
  - use `[workspace.dependencies]` to pin shared dependency versions (axum, sqlx, serde, tokio, etc.) and avoid cross-crate version conflicts
  - update repository build/release plumbing for the workspace layout, including Docker/cargo-chef inputs, cargo-dist workspace metadata, SQLx offline metadata generation/checks, and CI commands
- Add `crates/rise-resource-api`:
  - shared resource envelope types
  - object metadata/status types
  - `Organization` and `ResourceDefinition` API/spec/status types
  - request/response DTOs for generic resource operations
  - name/discriminator validation helpers
  - JSON Schema derivations for public resource API types
  - no Axum, sqlx, kube, or backend settings dependencies
- Add `crates/rise-resource-store`:
  - backend-only resource store trait and DB-backed implementation
  - generic resource CRUD/list/get/update/delete behavior
  - optimistic concurrency enforcement
  - finalizer and deletion behavior
  - ResourceDefinition registry resolution
  - spec/status validation orchestration
  - `migrations/` folder for generic resource infrastructure
  - migration runner, e.g. `rise_resource_store::run_migrations(&PgPool)`
- Defer `crates/rise-resource-client` until the HTTP API is implemented enough for external controllers to exercise:
  - async HTTP SDK for external controllers
  - controller auth token provider abstraction
  - helpers for ResourceDefinition registration and resource/status/finalizer operations
- Keep root/backend ownership of:
  - existing typed tables and their migrations
  - adding organization links to users, teams, projects
  - wiring typed APIs to the default Organization
  - backend startup ordering

Backend startup migration order:

1. Run existing root migrations.
2. Run `rise-resource-store` migrations.
3. Run default Organization bootstrap/backfill under an advisory lock.

## Resource Storage

- Add a generic `resources` table:
  - `uid UUID PRIMARY KEY`
  - `api_version TEXT NOT NULL`
  - `kind TEXT NOT NULL`
  - `parent_uid UUID NULL REFERENCES resources(uid)`
  - `name TEXT NOT NULL`
  - `discriminator VARCHAR(8) NOT NULL`
  - `metadata JSONB NOT NULL DEFAULT '{}'`
  - `spec JSONB NOT NULL DEFAULT '{}'`
  - `status JSONB NOT NULL DEFAULT '{}'`
  - `revision BIGINT NOT NULL DEFAULT 1`
  - timestamps
- Add a dedicated `resource_definitions` table for ResourceDefinition identity and registry lookup:
  - `uid UUID PRIMARY KEY REFERENCES resources(uid) ON DELETE RESTRICT`
  - `group_name TEXT NOT NULL`
  - `kind TEXT NOT NULL`
  - `plural TEXT NOT NULL`
  - `scope JSONB NOT NULL`
  - `versions JSONB NOT NULL`
  - `allowed_status_controller_ids TEXT[] NOT NULL DEFAULT '{}'`
  - timestamps
  - unique indexes for `plural` and ResourceDefinition identity tuples
- Store the ResourceDefinition envelope/spec/status in `resources`, but treat `resource_definitions` as the canonical indexed projection used for uniqueness, routing, registry resolution, and immutable identity checks.
- Add database-level checks:
  - `discriminator` is exactly 8 lowercase DNS-safe characters matching `^[a-z0-9][a-z0-9-]{6}[a-z0-9]$` (no leading or trailing hyphens)
  - `name` follows the resource name format accepted by the API
  - `metadata`, `spec`, and `status` are JSON objects
- Enforce same-level uniqueness:
  - `(parent_uid, kind, name)`
  - `(parent_uid, discriminator)`
  - matching partial unique indexes for root resources where `parent_uid IS NULL`
- Generate immutable 8-character lowercase DNS-safe discriminators, retrying on same-parent conflicts; cap at 10 attempts and return a 503 if all candidates are exhausted.
- Standard metadata includes:
  - `name`
  - `uid`
  - `revision`
  - `discriminator`
  - `annotations`
  - `finalizers`
  - `deletionTimestamp`
- Standard status defaults to `{}`; the `controllers` key is optional and populated only when a controller first writes its status update. Resources with no controllers have `status: {}` as a valid state.
- Treat SQL identity columns as canonical. The API must reject client-supplied identity metadata (`uid`, `revision`, `discriminator`, `deletionTimestamp`) on create/update/status paths.
- Delete behavior (Kubernetes-style cascade via finalizers):
  - `delete` always cascades to the subtree. The store collapses Kubernetes' Foreground/Background distinction into a single mode — tombstones are always visible, so client-observable ordering and server-side gating are the same mechanism. There is no `Orphan` policy: a non-root resource can never become parentless, so `parent_uid IS NULL` always means a root-scoped resource.
  - stamp `deletionTimestamp` on the target; if children exist, append a `system.rise.dev/cascade-deletion` system finalizer and stamp `deletionTimestamp` on **immediate children only** in the same transaction. A background GC worker fans out down the tree as each subtree's controllers drain their finalizers. The parent is hard-deleted only once all children are gone and finalizers are clear.
  - no finalizers and no children: hard-delete the row immediately
  - finalizers present: set `deletionTimestamp` and wait for finalizers to clear
  - controllers remove their controller-scoped finalizers after cleanup; the `system.rise.dev/*` prefix is reserved for store-internal finalizers and is rejected by `update_controller_finalizers` regardless of `controller_id`
  - GC sweep operates via `try_collect(uid)` (hard-deletes a tombstoned row when collectable, else stamps the next layer of children) and `list_pending_collection()` (enumerates candidates)
  - do not use `ON DELETE CASCADE` for resource parent/child cleanup because it would bypass child finalizers; the cascade-stamping + GC sweep is the substitute
  - tombstoned rows (with `deletionTimestamp` set) remain visible from `get`, `list`, `resolve_path`, and the HTTP API by default; filtering them out is opt-in via an explicit query param (deferred until a concrete use case appears)
  - additionally block deletion of an Organization that has typed children (teams or projects linked via `organization_resource_uid`); those rows are not `resources` table children so the generic child-detection check does not cover them — this guard must be explicit at the application layer until those typed APIs migrate to the generic resource model

## Built-In Resources

- Add strongly typed built-in resource registry in Rust.
- Built-in registrations define:
  - REST collection name, e.g. `organizations`
  - `apiVersion`, e.g. `rise.dev/v1alpha1`
  - `kind`, e.g. `Organization`
  - scope/root or parent resource kind
  - typed spec/status validators
  - schema generation hooks
- Add built-in `Organization`:
  - `apiVersion: rise.dev/v1alpha1`
  - `kind: Organization`
  - collection: `organizations`
  - root-scoped
  - `spec.displayName`
  - `spec.deploymentControllerClass` — ID of the controller responsible for reconciling the Organization's projects and deployments; matches a configured controller identity; optional, unset means no controller manages this org's deployments
  - `status.controllers`
- Add built-in `ResourceDefinition`:
  - root-scoped
  - operator-admin managed
  - describes external resource group/version/kind/plural/scope/schema/status controller ownership
  - persisted as both a normal built-in resource row and a row in the dedicated `resource_definitions` registry table
- Reserved collection names (built-ins implemented now or planned for a future phase): `organizations`, `projects`, `users`, `teams`, `environments`, `deployments`, `serviceaccounts`. Reject any ResourceDefinition whose plural matches one of these names.

## External Custom Resources

- External controllers register custom resources by creating `ResourceDefinition` records.
- A ResourceDefinition includes:
  - API group
  - versions with `served`, `storage`, and JSON Schema
  - kind
  - plural collection name
  - scope, initially root or child of Organization
  - allowed status controller IDs
- Reject ResourceDefinitions whose plural collection name collides with built-ins or reserved future names.
- Enforce database-level uniqueness for ResourceDefinitions through the dedicated `resource_definitions` table:
  - plural collection names are globally unique across external ResourceDefinitions
  - resource identity tuples, e.g. `(group, version, kind)`, are unique across external ResourceDefinitions
  - immutable identity fields cannot be changed while resources for that definition exist; enforced via application-layer pre-update check (Postgres has no conditional immutability)
- External resources are stored in the same `resources` table.
- Rise validates generic invariants for all resources:
  - identity, parent scope, name, discriminator, annotations, finalizers, deletion semantics, revision
- Built-in resources validate `spec/status` using Rust structs.
- External resources validate `spec` using the registered JSON Schema.
- External controller status is written under:
  - `status.controllers["controller-id"]`
- Controllers may only update their own status key and their own finalizers.
- External resource migrations are controller-owned:
  - v1 supports controller-driven migrations by listing old-version records and rewriting them
  - no conversion webhooks initially
  - Rise rejects unsupported or unserved `apiVersion/kind`
  - stored old-version resources must remain listable to their owning controller until the ResourceDefinition marks the version as neither `served` nor `storage`; at that point Rise treats migration complete and may enforce the new schema

## Generic API

- Add generic API routes under `/api/v1/resources`.
- Initial root routes:
  - `GET /api/v1/resources/{collection}`
  - `POST /api/v1/resources/{collection}`
  - `GET /api/v1/resources/{collection}/{name}`
  - `PUT /api/v1/resources/{collection}/{name}`
  - `DELETE /api/v1/resources/{collection}/{name}`
- Initial organization child routes for future external resources:
  - `GET /api/v1/resources/organizations/{org}/{collection}`
  - `POST /api/v1/resources/organizations/{org}/{collection}`
  - `GET /api/v1/resources/organizations/{org}/{collection}/{name}`
  - `PUT /api/v1/resources/organizations/{org}/{collection}/{name}`
  - `DELETE /api/v1/resources/organizations/{org}/{collection}/{name}`
- Path grammar is uniform `<kind>/<identifier>` per segment. The identifier is either a name or a UID-prefixed token (`uid:<uuid>`), e.g. `/api/v1/resources/organizations/acme/projects/uid:a1b2c3d4-...`. The kind is always present, so response shape is statically determinable from the URL and the store can verify that the UID's row actually has the expected kind (mismatches return 404). The HTTP layer resolves the path through the store's `resolve_path(&[PathSegment])` in a single round-trip.
- `DELETE` always cascades to the subtree.
- Break-glass / discovery endpoints:
  - `GET /api/v1/resources/pending-deletion` — resources tombstoned and awaiting GC (`list_pending_collection`)
  - `POST /api/v1/resources/{collection}/{name}/reparent` — atomic move to a new parent, admin-only; rejects cycles and uniqueness conflicts at the destination
- Route `{collection}` through the resource registry:
  - built-in registrations first
  - external ResourceDefinitions second
  - reject unknown collections
- Request bodies follow the Kubernetes-style envelope: `apiVersion`, `kind`, and `metadata` are top-level fields alongside `spec` and `status`. The `/api/v1/` prefix is the Rise HTTP API namespace and is unrelated to `apiVersion` in the body.
- Require body `apiVersion`, `kind`, and `metadata.name` to match the resolved registration and URL.
- Use `revision` for optimistic concurrency on updates. PUT must include `metadata.revision`; omitting it is rejected (same semantic as Kubernetes requiring `resourceVersion` on updates).
- Regular clients cannot write `status`.
- Status updates use a separate controller-oriented path/helper that writes only the caller's controller key.
- Restrict all generic resource API access to Rise Operators in v1:
  - non-operators cannot list, read, create, update, delete, or watch resources
  - this includes Organization-scoped child resource routes
  - existing typed project/team/deployment APIs keep their current access behavior
- Generic resource API status/finalizer operations also accept controller JWTs through a separate controller auth context:
  - controller JWTs are never treated as user JWTs or existing project service-account JWTs
  - controller-authenticated callers can use only controller-specific status/finalizer paths
  - controller-authenticated callers cannot use normal operator CRUD paths unless separately granted as Operators

## Operator Role

- Add a first-class `Operator` role for Rise installation operators.
- Operators have full access to generic resource storage and built-in resource management.
- Add `auth.operator_users`, a configured list of email addresses that receive Operator permissions.
- `auth.operator_users` matching is case-insensitive, matching the existing `auth.admin_users` behavior.
- Update local development Dex config to grant `ops@example.com` the Operator role.
- `auth.admin_users` are treated as admins within the default Organization only. They do NOT receive the Operator role and cannot access Organization resources or resources scoped to other Organizations. Operators are a separate, explicitly configured role.
- Organization-level users/admins are out of scope for this phase.
- Future work can replace operator-only generic access with org-scoped RBAC once the model is explicit.

## Controller Identity

- Add configured trusted controller identities in backend settings.
- A controller identity includes:
  - stable controller ID in Kubernetes label/annotation name format: a DNS subdomain with an optional `/path` suffix (e.g. `controller.example.com` or `controller.example.com/my-controller`); this string becomes the key under `status.controllers`
  - trusted JWT issuer/JWKS or equivalent verification settings
  - expected audience
  - optional subject/claim constraints
  - resource ownership grants, either explicit or derived from ResourceDefinitions the controller is allowed to manage
- External controllers authenticate with JWTs trusted by this backend configuration.
- Controller status/finalizer endpoints authorize by controller identity, not by user identity.
- Add a dedicated controller auth extractor/context separate from the existing user and project service-account auth context.
- Generic API controller endpoints accept this controller auth context directly.
- The existing external JWT service-account path remains project-scoped and must not be reused for controller authorization.
- Internal same-process controllers should use the same resource domain SDK/service interface without making HTTP requests back into the backend.
  - Define a resource store/service trait used by both HTTP handlers and internal controllers.
  - Shared validation, revision, finalizer, ownership, and deletion invariants must live below both call paths.
  - Internal callers may bypass transport/auth middleware, but must still pass an explicit internal controller identity for status/finalizer ownership checks.

## Multi-Organization Integration

- Link existing typed data to Organization resources:
  - user-to-organization membership relation
  - `organization_resource_uid` on teams
  - `organization_resource_uid` on projects
- Existing typed APIs remain externally unchanged and automatically use the configured default Organization.
- New authenticated users are added to the default Organization.
  - user find-or-create and default Organization membership creation must happen in one idempotent transaction
  - auth middleware must not create a user row that lacks default Organization membership
- New teams/projects are created in the default Organization.
- Team-owned project creation verifies the team belongs to the same Organization.
- Keep project and team names globally unique for now.
- Normal generic resource CRUD remains operator-only even though default Organization membership is backfilled.

## Kubernetes Namespace Prefix

- Store namespace prefix as Organization annotation:
  - `metadata.annotations["kubernetes.rise.dev/namespace-prefix"]`
- Add default Organization backend config:
  - `name`
  - `display_name`
  - optional `annotations`
  - optional `kubernetes_namespace_prefix`
- Add `controller_class_name` to the Kubernetes deployment controller backend config.
  - This value is the controller class identifier written to the default Organization's `spec.deploymentControllerClass`.
  - The Kubernetes controller reconciles only Organizations whose `spec.deploymentControllerClass` matches its configured `controller_class_name`.
  - Default to a stable value for existing installs if omitted, e.g. `kubernetes.rise.dev/default`.
- Startup bootstrap after migrations:
  - upsert default Organization
  - populate namespace-prefix annotation from config
  - set `spec.deploymentControllerClass` from the configured Kubernetes `controller_class_name`
  - backfill existing users, teams, and projects to default Organization
- Kubernetes controller namespace prefix resolution:
  - use the annotation when present
  - otherwise use `org-{metadata.discriminator}-`
- The Kubernetes controller only reconciles projects that belong to an Organization whose `spec.deploymentControllerClass` matches the controller's configured `controller_class_name`; projects in unmatched or unset orgs are ignored.
- Preserve existing installs:
  - the existing default Organization must resolve to the same namespace names as before migration
  - the current default `rise-{project_name}` maps to default Organization namespace prefix `rise-`, preserving `rise-myapp`
  - known existing installs use the default `rise-` namespace prefix; arbitrary namespace templates are not migrated in this phase
- The controller should know the default Organization for existing typed projects and use its namespace configuration when resolving namespaces.
- The Kubernetes controller must not process any typed projects until the default Organization record exists. If the Organization is absent at controller startup, the controller must error and refuse to proceed. Because bootstrap creates the Organization (with namespace prefix annotation) as its first action before backfilling typed rows, and bootstrap completes before the controller begins processing, this is guaranteed by startup ordering.

## Bootstrap And Migrations

- `rise-resource-store` migrations own only generic resource infrastructure:
  - `resources` table
  - `resource_definitions` table
  - generic resource indexes/checks
  - ResourceDefinition storage mechanics
- Root migrations own changes to existing typed tables:
  - user-to-organization membership relation
  - `organization_resource_uid` on teams
  - `organization_resource_uid` on projects
- Bootstrap/backfill must be idempotent and concurrency-safe:
  - run after all migrations
  - acquire a PostgreSQL advisory lock before mutating default Organization/linkage state
  - create or update exactly one configured default Organization (including namespace prefix annotation) as the first bootstrap action, before backfilling any typed rows
  - backfill all existing users, teams, and projects to the default Organization
  - preserve unrelated Organization annotations when applying namespace-prefix config
  - validate that all required typed rows have Organization linkage before startup proceeds
  - a process crash mid-backfill leaves some rows unlinked; on restart, idempotent backfill re-runs and completes before the validation check fires
  - fail server startup only if the validation check fails after a full backfill pass — not on a transient mid-backfill crash
- Prefer a two-phase migration for existing typed tables:
  - add nullable Organization linkage columns/relations
  - backfill under bootstrap/advisory lock
  - add stricter constraints only after validation is reliable

## Schema And Docs

- Derive `JsonSchema` for:
  - generic resource envelope
  - object metadata
  - controller status map
  - Organization spec/status
  - ResourceDefinition spec/status
  - backend default Organization config
- Add backend command:
  - `rise backend schemas generate`
- Generate deterministic schema files into operator docs for:
  - backend settings
  - `rise.toml`
  - generic resource envelope
  - Organization
  - ResourceDefinition
- Add a dedicated operator docs section:
  - `docs/engineering/src/content/docs/resources/index.mdx`
  - `docs/engineering/src/content/docs/resources/storage.md`
  - `docs/engineering/src/content/docs/resources/api.md`
  - `docs/engineering/src/content/docs/resources/custom-resources.md`
  - `docs/engineering/src/content/docs/resources/schemas.mdx`
- Update the Starlight nav config (`astro.config.mjs` or equivalent) to include the new resources section so pages are reachable.
- Add an Astro component to render generated JSON schemas as browsable reference docs.
- Wire `rise backend schemas generate` into a `mise run resource:schema:check` task (parallel to `mise run config:schema:check`) and add it to the CI lint pipeline.
- Document:
  - generic storage model
  - lifecycle, finalizers, deletion
  - revision/concurrency semantics
  - built-in vs external resources
  - custom resource registration
  - controller-owned status
  - external controller migration responsibilities
  - Organization namespace prefix behavior

## Test Plan

- Storage:
  - create/list/get/update/delete generic resources
  - same-level uniqueness for names and discriminators
  - database checks reject invalid names/discriminators and non-object JSON fields
  - discriminator rejects leading/trailing hyphens
  - discriminator retry ceiling (10 attempts) returns 503
  - discriminator format and immutability
  - conflicting client-supplied identity metadata (`uid`, `revision`, `discriminator`, `deletionTimestamp`) is rejected
  - PUT without `metadata.revision` is rejected
  - revision increments
  - finalizer deletion flow
  - delete cascades: stamps `deletionTimestamp` on parent and immediate children and injects `system.rise.dev/cascade-deletion` on the parent
  - `try_collect` hard-deletes a tombstoned row only when finalizers are clear AND no children remain; otherwise stamps the next layer of children
  - cascade subtree drains bottom-up via repeated `try_collect` as controllers shed their finalizers
  - controllers cannot add or remove `system.rise.dev/*` finalizers via `update_controller_finalizers`
  - `resolve_path` walks `Name` and `Uid` segments, returns the full ancestor chain (including tombstoned rows), and rejects kind mismatches on UID segments
  - `list_pending_collection` enumerates tombstoned rows for the GC worker
  - `reparent` moves a resource, rejects cycles, and surfaces destination uniqueness conflicts as `NameConflict`
  - Organization with linked teams or projects (via `organization_resource_uid`) is blocked from deletion at the application layer
- Built-ins:
  - Organization validation via Rust structs
  - ResourceDefinition validation via Rust structs
  - ResourceDefinition plural cannot collide with built-ins/reserved names
  - ResourceDefinition plural and group/version/kind uniqueness is enforced across external definitions
  - ResourceDefinition identity fields cannot be changed while resources for that definition exist
  - schema generation includes built-ins
- External resources:
  - ResourceDefinition registration adds runtime collection resolution
  - external spec JSON Schema validation works
  - unknown/unserved API versions are rejected
  - old stored versions remain available to owning controller for migration
  - controller status updates are limited to the caller's controller key
  - controller finalizer updates are limited to the caller's finalizers
- API:
  - Operators can manage generic resources, Organizations, and ResourceDefinitions
  - configured `auth.operator_users` receive Operator access
  - `auth.admin_users` do not receive Operator access unless also listed in `auth.operator_users`
  - non-operator users cannot list/read/write any generic resources
  - URL collection maps to expected `apiVersion/kind`
  - clients cannot write status through normal create/update
  - update revision conflicts are rejected
- Controller auth:
  - trusted external controller JWT can update only owned status/finalizers
  - trusted external controller JWT can call controller-specific generic API status/finalizer endpoints
  - trusted external controller JWT is rejected on normal operator CRUD routes unless separately authorized as an Operator
  - controller JWTs are not accepted by the existing project service-account auth path
  - untrusted JWT is rejected
  - normal user JWT cannot use controller status/finalizer endpoints
  - internal controller SDK path enforces the same ownership invariants
- Bootstrap:
  - default Organization is idempotently created
  - concurrent startup creates one default Organization and one complete backfill
  - concurrent first login creates one user and exactly one default Organization membership
  - namespace-prefix annotation is populated without clobbering unrelated annotations
  - existing users, teams, and projects are backfilled
  - startup fails on partial Organization linkage
- Namespace:
  - namespace prefix round-trip: bootstrap writes annotation → controller reads annotation → namespace resolves correctly
  - default org resolves `rise-myapp`
  - missing annotation resolves `org-{discriminator}-myapp`
  - controller ignores projects whose Organization has a non-matching or absent `deploymentControllerClass`
  - bootstrap sets `spec.deploymentControllerClass` on the default Organization from the configured Kubernetes `controller_class_name`
- Docs:
  - `rise backend schemas generate` is deterministic
  - operator docs build with the new resources section

## Implementation Increments

Deliver in the following order. PRs 1–4 avoid existing data model changes, but PR 1 and PR 2 still touch build/release and backend startup plumbing and must be verified through the normal container, CI, and SQLx offline paths. PR 5 is the only increment that touches existing typed data. PR 6 can be drafted in parallel with PR 5.

**PR 1 — Workspace + type crate (`rise-resource-api`)**
- Convert to Cargo workspace with `[workspace.dependencies]`
- Update Docker/cargo-chef, cargo-dist workspace metadata, CI commands, and SQLx offline preparation/checks for the workspace layout
- Scaffold `crates/rise-resource-api`: resource envelope, object metadata, Organization and ResourceDefinition spec/status types, request/response DTOs, name/discriminator validation helpers, JSON Schema derivations
- No DB, no HTTP, no behavior changes to existing code
- Critical path: everything else depends on this

**PR 2 — Resource store (`rise-resource-store`)**
- `resources` and `resource_definitions` table migrations (owned by `rise-resource-store`)
- Full store implementation: CRUD, optimistic concurrency, discriminator generation with retry cap, finalizer/deletion semantics, ResourceDefinition registry resolution, spec/status validation
- Wire the resource-store migration runner into backend startup immediately after root migrations, before any bootstrap/controller work
- Integration tests against a real DB
- Depends on: PR 1

**PR 3 — Auth: Operator role + controller identity**
- Add `auth.operator_users` and Operator role checks to settings, middleware, and `platform_access.rs`
- Add dedicated controller JWT extractor as a separate auth context (does not reuse service-account path)
- Clarify `auth.admin_users` as default-org-admin only in settings and access checks
- Update local Dex config to grant `ops@example.com` the Operator role
- No resource store wiring yet — auth contexts exist but controller one is unused
- Can be developed in parallel with PR 2

**PR 4 — Generic HTTP API**
- Routes under `/api/v1/resources` (root and organization-scoped)
- Uniform `<kind>/<identifier>` path grammar with `uid:` token support; backed by store's `resolve_path`
- Resource registry: built-in resolution first, external ResourceDefinitions second
- Request body validation, operator-only enforcement, controller status/finalizer endpoints
- `DELETE` always cascades to the subtree
- Break-glass `POST .../{name}/reparent` (admin-only) and a `GET /api/v1/resources/pending-deletion` diagnostics listing
- Audit logging for delete and reparent operations
- Purely additive — no existing routes change
- Depends on: PR 2, PR 3

**PR 4b — Resource GC worker**
- Background task that periodically polls `list_pending_collection()` and drives `try_collect()` per row
- Fans cascade down the tree as each subtree's controllers shed their finalizers
- Until this lands, cascade-marked subtrees do not actually drain
- Quota / fan-out rate limits and observability (metrics for backlog, time-to-collect, stuck finalizers) belong here
- Depends on: PR 2

**PR 5 — Multi-org linkage, bootstrap, and controller**
- Root migrations: nullable `organization_resource_uid` on users, teams, projects (two-phase: add nullable, backfill, constrain later)
- Add Kubernetes deployment backend `controller_class_name` config with a stable default for existing installs
- Bootstrap logic: advisory lock, default-org upsert with `displayName`, namespace prefix annotation, and `deploymentControllerClass` from `controller_class_name`; idempotent typed-row backfill; startup validation
- Wire new-user/team/project creation to default Organization
- Update Kubernetes controller: filter by `deploymentControllerClass`, read namespace prefix annotation, error on missing default org at startup
- Structure commits within this PR as: migrations → bootstrap → controller → typed API wiring, so reviewers can read each piece independently
- Depends on: PR 1, PR 2

**PR 6 — Schema generation + docs**
- `rise backend schemas generate` subcommand
- `mise run resource:schema:check` task wired into CI lint pipeline
- Starlight nav config updated for new resources section
- Five new operator doc pages
- Astro component for JSON schema rendering
- Can be drafted in parallel with PR 5; depends on PR 1 for schema types

## Assumptions

- Only Organizations and ResourceDefinitions are built into generic storage in this phase.
- Existing Projects, Teams, Environments, Deployments, and Extensions stay in current typed tables.
- Existing typed APIs are default-Organization compatibility APIs in this phase, not general multi-Organization APIs.
- Normal generic resource CRUD is operator-only in v1; controller JWTs may use controller-specific status/finalizer operations.
- `auth.admin_users` are org-level admins within the default Organization only; they do not receive Operator access and cannot manage Organization resources.
- Organization-level admins/users will be modeled later and will eventually replace current platform-level access concepts.
- Delete always cascades to the subtree. Reparent is the supported way to relocate a resource; to delete a parent but keep its children, reparent them elsewhere first.
- Tombstoned rows are always visible to the API; controllers and operators rely on observing in-progress teardown to do their work. An `exclude_deleted` filter can be added later when there is a concrete need.
- Reparent permission model (owner-of-source, owner-of-destination, both, admin-only) is deferred to the API layer and will be decided when reparent is exposed.
- Recursive-CTE optimization for `resolve_path` is deferred; the initial implementation is a per-segment loop in a single transaction, which is fine for typical hierarchy depths.
