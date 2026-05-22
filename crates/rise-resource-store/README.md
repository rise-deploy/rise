# rise-resource-store

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

## Deletion model

Inspired by Kubernetes finalizers, but adapted to a hierarchical store with a hard FK.

### Lifecycle

1. `delete(uid)` marks the row (`deletion_timestamp = NOW()`) and stamps its immediate
   children.
2. Controllers observe `deletion_timestamp`, do their teardown work, and clear their own
   finalizers via `update_controller_finalizers`.
3. A GC worker iterates `list_pending_collection()` and calls `try_collect(uid)` on each
   tombstoned row. When the row has no children and no remaining finalizers, `try_collect`
   hard-deletes it.

### Visibility contract

Tombstoned rows are **visible by default** in `get`, `get_by_name`, `list`, and
`resolve_path`. Filtering them out is the caller's responsibility — controllers, operators,
and resolution paths all need to observe in-progress teardown.

### Cascade

Deletion always cascades. `delete` stamps `deletion_timestamp` on the parent and its
**immediate children**, and attaches the store-managed finalizer
`system.rise.dev/cascade-deletion` to the parent (if children exist). Subsequent GC sweeps
via `try_collect` fan out down the tree as each level drains. The parent stays observable
until the entire subtree has been collected; because the `parent_uid` FK forbids deleting a
referenced row, collection is necessarily bottom-up.

To delete a parent but keep its children, `reparent` the children elsewhere first, then
delete the now-childless parent. There is no detach/orphan operation — a non-root resource
can never become parentless.

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

## Reparenting

`reparent(uid, new_parent_uid)` atomically moves a resource. The destination is validated
against the resource's *own* declared parent type — `reparent` self-determines this from the
resource's definition, so the caller passes no scope:

- Built-in kinds (`Organization`, `ResourceDefinition`) and any `ResourceDefinition` that
  declares no `parent` are root-scoped: they have no declared parent and must move to root
  (`new_parent_uid = None`).
- A resource whose `ResourceDefinition` declares a `parent` must move under a row matching
  the declared parent's API **group** and **kind**. The version is ignored, so a parent
  stored at an older served version of the same group is still accepted. Arbitrary nesting
  depth is allowed — the parent need not be root-level.

Rejects:

- `Validation` — destination does not match the resource's declared parent type (e.g. a
  root-scoped resource given a parent, a non-root resource sent to root, or a wrongly-typed
  new parent).
- `ReparentCycle` — self-loop or moving a node under one of its own descendants.
- `NameConflict` — destination scope already has a row with the same `(group, kind, name)`
  or discriminator.

Use this in preference to delete-then-recreate, which loses the UID and revision history.
