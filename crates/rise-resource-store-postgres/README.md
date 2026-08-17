# rise-resource-store-postgres

PostgreSQL implementation of Rise's generic resource persistence boundary.

The storage-neutral contract lives in `rise-resource-api`: consumers should
import `ResourceStore`, `ResourceRow`, `StoreError`, request parameters,
validators, and shared constants from that crate. This crate intentionally
exports only PostgreSQL/migration/registry concerns and concrete validators:
`PgResourceStore`, `run_migrations`, `BuiltInRegistry`,
`OrganizationValidator`, `ResourceDefinitionValidator`, and
`JsonSchemaValidator`, plus narrow identity/trust/membership lookup adapters.

Keeping SQLX and JSON Schema compilation behind the implementation boundary
lets authorization and controller code use fake `ResourceStore`
implementations without pulling in PostgreSQL.

Backend-only DB layer for generic resource storage. Implements the `ResourceStore` trait
backed by PostgreSQL.

## Storage model

The `resources` table holds every resource (Organizations, ResourceDefinitions, and any
user-defined kind). Hierarchy is encoded by a single FK:

```sql
parent_uid UUID NULL REFERENCES resources(uid)
```

Resource identity is unique per `(group, kind, name)` within a parent scope, enforced by
partial unique indexes — one for `parent_uid IS NULL` (root) and one for
`parent_uid IS NOT NULL`. The group is the substring of `api_version` before `/`, so the
same logical resource cannot be created twice under two different versions of the same
group; resources from *different* groups may still share `(kind, name)`. `parent_uid IS NULL`
is an exact synonym for a root-scoped resource: a non-root resource always has a parent
(resources are only ever removed by cascading delete, never detached). A name-uniqueness
violation surfaces as `StoreError::NameConflict`, never a raw database error.

`resource_definitions` is a view over `resources` that projects the indexed/queryable
identity fields (group, kind, plural, versions, allowed status controller IDs) of
`ResourceDefinition` rows out of their `spec`. Being a view it cannot drift from the
backing row; identity uniqueness is enforced by partial unique indexes on `resources`.

Lifecycle owner references are stored once, in the resource row's
`owner_references` JSONB array. A `jsonb_path_ops` GIN index supports reverse
UID containment queries for garbage collection. No separate edge table is
maintained.

## Built-in identity and policy admission

The eight `rise.dev/v1alpha1` identity kinds and the four policy kinds
(`Role`, `RoleBinding`, `PlatformRole`, `PlatformRoleBinding`) are routed from
the immutable built-in registry. Pure typed validation and canonicalization run
before the mutation transaction; contextual admission then locks and verifies
the exact live built-in parent chain in the same transaction as persistence.
Every built-in registration's validator is authoritative even when the caller
omits or forges its optional `SpecValidator`. Identity admission also enforces
mapping immutability and GroupMembership's optional matching-User owner
reference. Custom same-named kinds in other API groups remain generic.

Policy admission adds the contextual half a JSON Schema cannot express. A
binding's `scope` is normalized against facts known only at write time — its
parent Organization for an org `RoleBinding`, its subject for a
`PlatformRoleBinding` — and every reference it carries is then resolved against
live rows under `FOR SHARE`: the `roleRef` at the exact placement its kind
implies, the `scope` path down a registry- or ResourceDefinition-derived parent
chain, and any literal `subject`. Containment is checked on the resolved rows,
so an org binding cannot leave its own organization's subtree and a static
org-native subject cannot reach outside its own organization. Updates run the
same path, which is idempotent for an already-normalized spec.

Nothing consults these resources yet: a stored binding grants no access until
the authorization engine lands.

Three partial expression indexes project only live built-in rows: global
UserIdentity issuer/subject uniqueness (including inactive mappings),
parent-and-issuer workload trust lookup, and reverse GroupMembership name
lookup. `IdentityLookup`, `TrustPolicyLookup`, and `MembershipLookup` expose
these fixed reads without adding JSON filters to `ResourceStore`.

Before these routes activate, each activation migration audits tombstoned and
live legacy ResourceDefinitions and resource rows that would be shadowed,
reporting a bounded count plus sample. It then installs a durable write
guard: the identity migration reserves the whole `rise.dev` group against
external ResourceDefinitions plus the eight identity collection names in any
group, and the policy migration adds the four policy collection names. Audit
and guard share one transaction, so an upgrade that reports a conflict leaves
the database untouched: remove every resource the diagnostic names using the
previously deployed Rise binary, then retry.

The policy activation adds no index. The identity indexes exist because
authentication resolves a login through them on every request; bindings are
read through ordinary tree and collection reads, so an index for them stays
speculative until the authorization engine makes its access patterns
measurable.

Every index here is built inside its migration's transaction, so the build and
SQLx's bookkeeping commit together. `CREATE INDEX CONCURRENTLY` would avoid the
brief write lock a plain build takes, but it cannot run in a transaction, and
splitting those two steps opens a crash window that can leave an unrecorded
index (wedging the next startup on "relation already exists") or an `INVALID`
one. An `INVALID` unique index enforces nothing while looking present, which
for the identity mappings means silently losing the uniqueness the login path
depends on. `resource_store.resources` is small, so the lock is the cheaper
cost. Revisit if the typed-object migration makes this table large.

A built-in `Organization` or `ResourceDefinition` cannot currently be the
dependent side of an owner reference: writes that put `owner_references` on
either kind are rejected. Owner-driven deletion tombstones dependents directly
through the generic collector, which would bypass the additional deletion
safety checks for legacy Organization-owned records and resources that still
use a ResourceDefinition. Both kinds remain valid owners, and a custom
`Organization` kind in another API group is unaffected. The restriction can be
removed once those guards move into the transaction-scoped lifecycle layer.

## Labels and ancestry

Resources carry `metadata.labels` in a dedicated `labels` column rather than
inside `metadata`, which stores exactly the annotations map and is read back as
a flat string-to-string map. Keys parse through the same `LabelKey` grammar a
binding's `labelSelector` uses, so any key that can be written can also be
selected on; values are bounded and single-line. A database CHECK keeps the
stored shape honest against direct SQL.

`ancestors(uid)` returns the root-first structural chain including the
resource itself, in one recursive query rather than the per-level walk that
placement and path resolution use. It exists for `effectiveLabels`: ADR-0001
resolves label inheritance nearest-wins over an already-fetched ancestor
chain, so the inheritance itself is a pure function on the caller's side and
never a query. Like `resolve_path`, the chain includes tombstoned rows for the
caller to interpret.

## Deletion model

Inspired by Kubernetes finalizers, but adapted to a hierarchical store with a hard FK.

### Lifecycle

1. `delete(uid)` stamps immediate structural children and direct owner-reference
   dependents, then tombstones the owner when finalizers or blocking dependents
   remain; otherwise it hard-deletes the owner.
2. Controllers observe `deletion_timestamp`, do their teardown work, and clear their own
   finalizers via `update_controller_finalizers`.
3. A GC worker iterates `list_pending_collection()` and calls `try_collect(uid)` on each
   tombstoned row. When the row has no blocking dependents and no remaining finalizers,
   `try_collect` hard-deletes it after stamping any non-blocking dependents.

### Visibility contract

Tombstoned rows are **visible by default** in `get`, `get_by_name`, `list`, and
`resolve_path`. Filtering them out is the caller's responsibility — controllers, operators,
and resolution paths all need to observe in-progress teardown.

### Cascade

Deletion always cascades. `delete` stamps `deletion_timestamp` on every
immediate structural child and owner-reference dependent. Structural children
always block collection; cross-tree dependents block only when the matching
reference has `blockOwnerDeletion: true` (default `false`). The store-managed
`system.rise.dev/cascade-deletion` finalizer represents the aggregate condition
that at least one blocking dependent remains. Non-blocking dependents drain in
the background after the owner disappears. Each newly tombstoned dependent
best-effort emits a structured `resource.deletion_cascaded` audit log after
commit. A transactional outbox or Event resource is required for durable
delivery.

There is no detach/orphan operation — a non-root resource can never become parentless.

### Finalizer ownership

Two kinds of finalizers live in the `finalizers` array:

- **Controller finalizers** — anything matching `<controller_id>` or
  `<controller_id>/<path>`. Added and removed by the owning controller via
  `update_controller_finalizers`.
- **System finalizers** — prefix `system.rise.dev/`. Reserved for the store itself.
  `update_controller_finalizers` rejects any attempt by a controller to add or remove
  them with `StoreError::ReservedFinalizer`.

## Path resolution

`resolve_path(&[PathSegment])` walks a path of `(api_versions, kind, identifier)` pairs in a
single transaction and returns the full ancestor chain (leaf is the last element). Two segment
forms:

```rust
PathSegment::Name { api_versions: vec!["widgets.example.com/v1".into()], kind: "Widget".into(), name: "foo".into() }
PathSegment::Uid  { api_versions: vec!["widgets.example.com/v1".into()], kind: "Widget".into(), uid }
```

`api_versions` is the set of API versions the caller accepts for that segment (typically the
served versions of a collection). The stored row must match one of them. `kind` is always
required, even for UID-addressed segments. Together they let the API layer:

- Determine the response shape from the URL alone (no resolve-before-route round-trip).
- Surface `KindMismatch` when a UUID was copy-pasted into the wrong slot.
- Catch cross-subtree references (`ParentNotFound`) — a UID whose `parent_uid` doesn't
  match the resolved ancestor chain.

## Pending-deletion discovery

`list_pending_collection(limit)` returns tombstoned rows (`deletion_timestamp IS NOT NULL`),
oldest first. The GC worker iterates it to drive `try_collect`; it also backs operator
tooling that needs to spot resources stuck mid-deletion.
