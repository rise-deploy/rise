# rise-resource-store

Backend-only DB layer for generic resource storage. Implements the `ResourceStore` trait
backed by PostgreSQL.

## Storage model

The `resources` table holds every resource (Organizations, ResourceDefinitions, and any
user-defined kind). Hierarchy is encoded by a single FK:

```sql
parent_uid UUID NULL REFERENCES resources(uid)
```

Uniqueness of `(api_version, kind, name)` within a parent scope is enforced by partial
unique indexes — one for `parent_uid IS NULL` (root) and one for `parent_uid IS NOT NULL`.

`resource_definitions` is a projection table holding the indexed/queryable identity fields
(group, kind, plural, scope) of `ResourceDefinition` rows; it stays in sync via
`register_resource_definition` and `update_resource_definition`.

## Deletion model

Inspired by Kubernetes finalizers, but adapted to a hierarchical store with a hard FK.

### Lifecycle

1. `delete(uid, policy)` marks the row (`deletion_timestamp = NOW()`) and reacts to children
   per `policy`.
2. Controllers observe `deletion_timestamp`, do their teardown work, and clear their own
   finalizers via `update_controller_finalizers`.
3. A GC worker iterates `list_pending_collection()` and calls `try_collect(uid)` on each
   tombstoned row. When the row has no children and no remaining finalizers, `try_collect`
   hard-deletes it.

### Visibility contract

Tombstoned rows are **visible by default** in `get`, `get_by_name`, `list`, and
`resolve_path`. Filtering them out is the caller's responsibility — controllers, operators,
and resolution paths all need to observe in-progress teardown.

### Propagation policy

`PropagationPolicy` controls what happens to children when a parent is deleted.

- **`Cascade`** (default). Stamps `deletion_timestamp` on the parent and its **immediate
  children**, and attaches the store-managed finalizer
  `system.rise.dev/cascade-deletion` to the parent (if children exist). Subsequent GC
  sweeps via `try_collect` fan out down the tree as each level drains. The parent stays
  observable until the entire subtree has been collected.

- **`Orphan`**. Detaches immediate children (`UPDATE resources SET parent_uid = NULL WHERE
  parent_uid = $1`) and then deletes the parent normally. Children continue as root-level
  resources. This is an admin/break-glass operation; **admin gating is the caller's
  responsibility** (the store accepts the policy unconditionally).

### Finalizer ownership

Two kinds of finalizers live in the `finalizers` array:

- **Controller finalizers** — anything matching `<controller_id>` or
  `<controller_id>/<path>`. Added and removed by the owning controller via
  `update_controller_finalizers`.
- **System finalizers** — prefix `system.rise.dev/`. Reserved for the store itself.
  `update_controller_finalizers` rejects any attempt by a controller to add or remove
  them with `StoreError::ReservedFinalizer`.

## Path resolution

`resolve_path(&[PathSegment])` walks a path of `(api_version, kind, identifier)` pairs in a
single transaction and returns the full ancestor chain (leaf is the last element). Two segment
forms:

```rust
PathSegment::Name { api_version, kind, name }    // "widgets/foo"
PathSegment::Uid  { api_version, kind, uid }     // "widgets/uid:aaaa-bbbb"
```

`api_version` and `kind` are always required, even for UID-addressed segments. This lets
the API layer:

- Determine the response shape from the URL alone (no resolve-before-route round-trip).
- Surface `KindMismatch` when a UUID was copy-pasted into the wrong slot.
- Catch cross-subtree references (`ParentNotFound`) — a UID whose `parent_uid` doesn't
  match the resolved ancestor chain.

## Orphan discovery

`list_orphans(parent_uid)` returns rows whose parent has `deletion_timestamp IS NOT NULL`
(i.e. children of an in-progress teardown). Scoped optionally to a single subtree. Useful
for admin tooling that needs to inspect or repair in-flight cascade operations.

Note: the FK on `parent_uid` makes "dangling" orphans (parent row gone) impossible. Orphan
mode explicitly nulls `parent_uid` instead.

## Reparenting

`reparent(uid, new_parent_uid)` atomically moves a resource. Rejects:

- `ReparentCycle` — self-loop or moving a node under one of its own descendants.
- `NameConflict` — destination scope already has a row with the same
  `(api_version, kind, name)` or
  discriminator.

Use this in preference to delete-then-recreate, which loses the UID and revision history.
