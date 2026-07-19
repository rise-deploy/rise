---
title: "Storage Model"
description: "How the generic resource store persists, scopes, deletes, and garbage-collects resources."
---

The resource store is one Postgres table (`resource_store.resources`) plus a view (`resource_store.resource_definitions`) that projects ResourceDefinition identity fields for indexed lookup. All built-in and custom resource rows live in the same physical table; the model is uniform regardless of which kind a row carries.

## Row shape

Every resource row carries these fields:

| Column | Type | Purpose |
|---|---|---|
| `uid` | UUID PK | Globally unique. Stable for the row's lifetime. |
| `api_version` | TEXT | `<group>/<version>`, e.g. `rise.dev/v1alpha1`. |
| `kind` | TEXT | Resource kind, e.g. `Organization`. |
| `parent_uid` | UUID NULL | Parent row's `uid`. NULL means root-scoped. |
| `name` | TEXT | User-chosen name. Scope-unique. |
| `discriminator` | VARCHAR(8) | System-generated. Scope-unique. |
| `metadata` | JSONB | Resource annotations. |
| `spec` | JSONB | Caller-controlled desired state. |
| `status` | JSONB | Controller-owned observed state (`status.controllers[<id>]`). |
| `revision` | BIGINT | Monotonic version counter. Increments on every write. |
| `finalizers` | TEXT[] | Cleanup tokens that block hard-delete. |
| `owner_references` | JSONB | Optional UID-authoritative lifecycle owners. |
| `deletion_timestamp` | TIMESTAMPTZ NULL | Tombstone marker. |
| `created_at`, `updated_at` | TIMESTAMPTZ | Standard audit timestamps. |

The store enforces these invariants at the database level:

- `discriminator` matches `^[a-z0-9][a-z0-9-]{6}[a-z0-9]$` (8 lowercase DNS-safe chars, no leading or trailing hyphen).
- `name` matches a DNS-label (`my-app`) or DNS-subdomain (`widgets.example.dev`) format, max 253 chars.
- `metadata`, `spec`, `status` must each be a JSON object.
- `owner_references` must be a JSON array.

And these uniqueness invariants:

- `(parent_uid, split_part(api_version, '/', 1), kind, name)` — same-level `(group, kind, name)` uniqueness for non-root rows.
- `(split_part(api_version, '/', 1), kind, name)` — same uniqueness scoped to root rows (`parent_uid IS NULL`).
- `(parent_uid, discriminator)` and `(discriminator)` for root — same-level discriminator uniqueness.

Same-level uniqueness keys on the API *group* (the substring of `api_version` before `/`), not the full `apiVersion`. This is so a single logical resource cannot exist twice under two declared versions of one group.

For ResourceDefinitions there are two additional partial unique indexes on the `resources` table itself:

- `((spec->>'plural'))` where `kind = 'ResourceDefinition'` — globally unique plural collection name across external definitions.
- `((spec->>'group'), (spec->>'kind'))` where `kind = 'ResourceDefinition'` — unique `(group, kind)` tuple.

## Identity fields are server-controlled

The API rejects client-supplied values for `uid`, `revision`, `discriminator`, and `deletionTimestamp` on create. On update, only `revision` may (and must) be supplied — the server uses it for optimistic concurrency, then increments it.

## Discriminator

Every resource gets an 8-character lowercase DNS-safe discriminator at create time. It is unique among siblings under the same parent (regardless of kind), but **not** unique across different parents or globally.

The generator retries up to 10 times on a same-parent conflict; if the random space is exhausted at that scope the create returns `503 Service Unavailable`. The discriminator is immutable once set.

Discriminators give controllers a stable random token to use in external systems when constructing derived identifiers (e.g. namespace names, IAM role names). `name` is user-chosen and may be reconstructable from external inputs; the discriminator is not.

## Revision and concurrency

Every write increments `revision`. Updates use it for optimistic concurrency:

- `PUT` requests must include `metadata.revision` (omitting it is rejected, mirroring Kubernetes' requirement for `resourceVersion`).
- The update succeeds only if the in-database revision matches; otherwise the call returns `409 Conflict`.
- Controller status and finalizer updates do not require a caller-supplied revision — they apply unconditionally to the latest row and bump the revision themselves.

Read-modify-write loops in controllers must therefore re-fetch the row after a 409, merge their changes onto the new revision, and retry.

## Hierarchy and parent scope

Each ResourceDefinition declares exactly one `parent` (or none, for root-scoped kinds). The parent chain is fixed by kind, not by row — a `Widget`'s URL only carries ancestor *names*; the ancestor *kinds* are derived from the registered chain.

`parent_uid IS NULL` always means root-scoped. There is no `Orphan` policy: a non-root resource can never become parentless, so the cascade-deletion machinery does not need to consider that case.

The store caps parent-chain depth (and the `resolve_path` ancestor walk) at `MAX_PARENT_CHAIN_DEPTH = 32`. Registration also rejects cyclic parent graphs.

## Owner references

`metadata.ownerReferences` adds lifecycle edges without changing structural
parentage or URL scope. Each typed reference carries `apiVersion`, `kind`,
`name`, and `uid`; a write succeeds only when those fields identify the same
live row. The UID is authoritative after admission, so deleting and recreating
the same name cannot transfer lifecycle ownership.

The `resources.owner_references` JSONB column is the sole persisted source of
truth. A `jsonb_path_ops` GIN index accelerates reverse `@>` containment lookup
when the collector needs all resources that reference an owner UID. There is no
edge table, trigger-maintained projection, or application dual-write to drift
from the resource envelope.

Edge-creating owner-reference mutations and collection take a
transaction-scoped graph lock so graph checks and row locks have one ordering.
Admission locks referenced owners, rejects duplicate UIDs and tombstoned or
mismatched owners, and runs a recursive cycle check over both structural
`parent_uid` edges and owner-reference edges. Resource names are immutable, so
the inspectable name descriptor cannot drift after admission. Multiple owners
are allowed; deletion of any one owner starts deletion of the dependent. Owner
references are lifecycle-only and confer no authorization.

## Lifecycle: create

The create path validates the spec (via the typed validator for built-ins, JSON Schema for external custom resources), generates a discriminator, and inserts the row at `revision = 1`. Same-level name and discriminator conflicts surface as `409 Conflict`.

## Lifecycle: update

`PUT` replaces the resource's user-controlled fields (`spec`, plus
`metadata.annotations`, `metadata.finalizers`, and
`metadata.ownerReferences`) under optimistic concurrency on `revision`. The
body's `metadata.name` must equal the stored name — resources cannot be renamed.

`api_version` may be changed to a different declared version of the same group via the store's `UpdateResourceParams.api_version`; the HTTP API translates a served request version to the collection's storage version before calling the store.

Identity fields (`uid`, `discriminator`, `deletionTimestamp`) cannot be modified via update. Status and finalizers also cannot be set through `PUT` — they use dedicated subresources.

## Lifecycle: delete (cascade + finalizers)

`DELETE` always cascades through both structural children and owner-reference
dependents. Behavior depends on the row's state:

| Row state | Outcome |
|---|---|
| No finalizers, no dependents | Hard-deleted in one transaction. Returns `200 OK`. |
| Has finalizers and/or dependents | Tombstoned (`deletion_timestamp` set). Returns `202 Accepted`. |

When dependents exist at delete time, the store stamps `deletion_timestamp` on
immediate structural children and direct owner-reference dependents in the same
transaction, then attaches the system finalizer
`system.rise.dev/cascade-deletion` to the owner. A background garbage collector
(see below) drains the lifecycle DAG bottom-up by repeatedly calling
`try_collect` on tombstoned rows. Each call stamps the next layer and eventually
hard-deletes a row when all its finalizers and dependents are gone.

`ON DELETE CASCADE` is deliberately not used. That would bypass child finalizers; the cascade-stamping + GC sweep is the substitute.

### Finalizers

Finalizers are caller-managed cleanup tokens. While *any* finalizer is on a row, the row cannot be hard-deleted. Controllers add their own finalizer when they take ownership of an external resource, do their cleanup on observing a deletion timestamp, then remove the finalizer to unblock the hard-delete.

The `system.rise.dev/*` prefix is reserved for store-internal finalizers (currently `system.rise.dev/cascade-deletion`). Controllers cannot add or remove these via `update_controller_finalizers` regardless of `controller_id`. Operators can override stuck cascade finalizers via the operator-finalizer endpoint, but the same reserved-prefix guard still applies — the operator path is for breaking deadlocks on controller-owned finalizers, not for forcing through cascade state.

### Tombstones are visible

Tombstoned rows (those with `metadata.deletionTimestamp`) remain visible from `get`, `list`, `resolve_path`, and the HTTP API. This is intentional: controllers and operators rely on observing in-progress teardown to do their work. There is no automatic exclusion; an opt-in filter can be added later when a concrete use case appears.

## Garbage collection

A leader-elected background worker (`ResourceGcController`) periodically polls `list_pending_collection()` (oldest tombstones first) and calls `try_collect(uid)` on each row.

Per-row, `try_collect` is idempotent and does:

- Non-tombstoned row → no-op (returns `MarkedForDeletion(row)`); the GC worker logs and moves on.
- Tombstoned with children → stamps any still-unstamped children, ensures the cascade finalizer is set, returns `MarkedForDeletion`.
- Tombstoned with no children → removes `system.rise.dev/cascade-deletion` if present; if no other finalizers remain, hard-deletes and returns `Deleted`; otherwise returns `MarkedForDeletion` (waiting on a controller finalizer).

The sweep has a forward-progress guard so stuck rows (controllers whose finalizer never clears) cannot starve newer tombstones within one tick. Consecutive sweep failures back off exponentially up to 60s.

Two operational endpoints help diagnose stuck deletions:

- `GET /api/v1/resources/pending-deletion?limit=N` — list tombstoned rows oldest first (1 ≤ N ≤ 1000, default 100). Operator-only.
- Per-row `GET` returns the row with `metadata.deletionTimestamp` and the current `metadata.finalizers`, so you can see which finalizer is holding cleanup.

## Organization namespace prefix

Organizations carry an optional annotation `kubernetes.rise.dev/namespace-prefix` that the Kubernetes controller uses to build per-project namespace names. The bootstrap path is owned by the backend (not the resource store), but the annotation lives on the Organization resource:

- The Kubernetes controller reads `metadata.annotations["kubernetes.rise.dev/namespace-prefix"]` when present and uses it as the namespace prefix (e.g. `rise-myapp`).
- When absent, the controller falls back to `org-{metadata.discriminator}-` (e.g. `org-a1b2c3d4-myapp`), which is collision-safe by construction.

The bootstrap path also populates `spec.deploymentControllerClass` on the default Organization from the Kubernetes controller's configured `controller_class_name`. The Kubernetes controller only reconciles projects whose Organization carries a matching `deploymentControllerClass`; an unmatched or unset value means the org's projects are ignored, leaving room for alternate deployment backends per organization.

Bootstrap creates the configured default Organization only when no
Organizations exist. Otherwise an exact `default_organization.name` match is
required; a nonmatching existing Organization makes backend startup fail.
Bootstrap never renames an Organization or creates a second candidate default.

Deleting an Organization that still has typed children (teams or projects linked via `organization_resource_uid`) is blocked at the application layer. Those rows are not children in the generic `resources` table, so the generic same-parent child check does not cover them — the guard must be explicit at the bootstrap/HTTP layer until typed APIs migrate onto the generic store.
