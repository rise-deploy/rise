---
title: "Generic Resource API"
---

The generic resource API (`/api/v1/resources/...`) is an operator-only HTTP surface for managing arbitrary resources in the resource store without typed endpoints for every kind.

## URL Grammar

A resource path names the **leaf** collection as `{group}/{version}/{plural}`, followed by the resource hierarchy as bare names. Each `ResourceDefinition` declares exactly one `parent`, so the whole ancestor chain of *kinds* is derived from the leaf kind — the URL only names *which instances*. `D` is the leaf kind's parent-chain depth (`0` for a root-scoped collection).

| Path | HTTP Methods | Operation |
|---|---|---|
| `{group}/{version}/{plural}/{ancestor}…` (`D` names) | GET, POST | List / create in the collection |
| `{group}/{version}/{plural}/{ancestor}…/{name}` (`D+1`) | GET, PUT, DELETE | Get / update / delete an item |
| `{group}/{version}/{plural}/{ancestor}…/{name}/status` (`D+2`) | PUT | Controller status update |
| `{group}/{version}/{plural}/{ancestor}…/{name}/finalizers` (`D+2`) | PUT | Controller finalizer update |
| `{group}/{version}/{plural}/{ancestor}…/{name}/deletion-blockers` (`D+2`) | GET | Deletion-blocker diagnostics |
| `{group}/{version}/{plural}/uid:{uuid}` | GET, PUT, DELETE | Get / update / delete an item by UID |
| `{group}/{version}/{plural}/uid:{uuid}/{sub}` | PUT | `status` / `finalizers` on an item by UID |
| `{group}/{version}/{plural}/uid:{uuid}/deletion-blockers` | GET | Deletion-blocker diagnostics by UID |
| `pending-deletion` | GET | List resources tombstoned and awaiting garbage collection |

The `{ancestor}` segments are bare resource *names*, ordered root-most first; the ancestor *types* are derived from the leaf's `ResourceDefinition` parent chain, never supplied in the URL. `pending-deletion` is only valid as the sole path segment, so a resource may be named `pending-deletion`.

Unversioned resource paths are not supported — `{group}/{version}` always names the leaf collection.

## UID Addressing

The leaf identifier may be given as `uid:{uuid}` instead of a name. A UID is globally unique, so the ancestor names are redundant: the `uid:` form is valid **only** as the sole identifier segment, immediately after `{group}/{version}/{plural}`, with no ancestor names.

```bash
curl -H "Authorization: Bearer $TOKEN" \
  https://rise.example.com/api/v1/resources/rise.dev/v1alpha1/organizations/uid:123e4567-e89b-12d3-a456-426614174000
```

When a UID-prefixed identifier is used in a PUT URL, the body's `metadata.name` is not validated against the URL. It still must match the stored resource name.

The `uid:` form skips ancestor-chain resolution entirely — it does not walk or validate the parent hierarchy. This means it works even if an ancestor `ResourceDefinition` has been removed from the system. This is the intended recovery path in disaster scenarios: if an RD is deleted while instances still exist, named addressing will fail because the ancestor chain can no longer be resolved, but a controller can still write status or finalizers to a resource via `uid:`, and operators can reach any resource regardless of whether its parent hierarchy is intact.

## Discriminator

Every resource carries a system-generated 8-character `discriminator`. It is unique among all resources that share the same parent (its siblings), regardless of kind — but **not** unique across different parents or globally.

Like `name`, the discriminator identifies a resource within its sibling scope. Unlike `name` (chosen by the user and potentially reconstructable from external inputs), it is random — so controllers can use it as a collision-free token when constructing derived identifiers in external systems while reconciling a resource.

## Authentication and Authorization

| Tier | Credentials | Permitted operations |
|---|---|---|
| Operator | Listed in `auth.operator_users` | GET, POST, item PUT, DELETE, PUT `status` and `finalizers` |
| Controller | Configured `ControllerIdentity` token | PUT `status` and `finalizers`, gated by `allowed_status_controller_ids` |

Controller tokens cannot perform item-level PUT or other operator operations.

Operators have full, unrestricted access to all resource API endpoints including the `status` and `finalizers` subresources. When an operator updates finalizers, controller ownership checks are bypassed — this is the intended recovery path for stuck cascade deletions when a controller has been deprovisioned. All operator subresource writes are audit-logged.

### Seeded baseline policy

Startup seeds five root policy resources, described by [ADR-0001](/operator-docs/adr/0001-unified-permission-model/). They exist and are inspectable today; nothing consults them yet, because the API is still operator-gated.

| Resource | Grants | Mutability |
|---|---|---|
| `PlatformRole/system-admin` | every verb on every kind and subresource | immutable |
| `PlatformRoleBinding/system-admin` | `system-admin` to `system:operators` | immutable |
| `PlatformRole/org-admin` | the global org-admin baseline | operator-editable |
| `PlatformRole/resource-owner` | `get`, `list`, `update`, `delete` — no `create`, no subresources | operator-editable |
| `PlatformRoleBinding/resource-owner` | `resource-owner` to whoever `rise.dev/owner` names | operator-editable |

The two immutable rows are not the *source* of operator authority — the evaluator hardcodes that so the guarantee survives a bad restore or a direct database write. They are its only inspectable record, which is why the store refuses to update or delete them, and why a startup that finds one diverged fails with instructions rather than silently rewriting it. Deleting an editable default is safe: the next restart re-seeds it.

Editing `org-admin` changes what administrators may do in every organization, never *who* they are — admin standing comes from an exact org-root, scope-only `RoleBinding`, so no label or Group name can confer it. Editing `resource-owner` changes what ownership means platform-wide; an organization can instead override the default for itself with its own binding on the same `(subject, label key)` pair.

## Resource Definitions

`ResourceDefinition` declares the group, kind, plural, versions, storage version, and parent type for a collection.

- A resource is root-scoped when its `ResourceDefinition` declares no `parent`.
- A non-root resource declares `parent: { "apiVersion": "...", "kind": "..." }`; its `ResourceDefinition` parent chain must be acyclic (registration rejects cycles).
- The ancestor *types* in a URL are derived from this parent chain — the URL carries only ancestor *names*, so a child can never be addressed under a parent of the wrong type.
- A child may only exist directly under a parent of the declared parent kind.
- A `ResourceDefinition` cannot be deleted while any related resources exist in any declared version.

Version behavior follows Kubernetes-style semantics:

- Only versions with `served: true` can be addressed by the HTTP API; at least one version must be served.
- Exactly one version must have `storage: true`.
- Creates and updates through any served version are stored at the current storage version.
- Reads and lists return rows stored at any declared version of the same group/kind — including a non-served storage version; the response `apiVersion` is projected to the requested version.
- A version cannot be removed from a `ResourceDefinition` while resources are still stored at it.
- Conversion is currently no-op only: the API projects `apiVersion` but does not transform `spec`.

Reads (GET/LIST) work for any served version. Writes (POST/PUT) must use the storage version — there is currently no version conversion webhook support, so a write targeting a served non-storage version is rejected with `422 Unprocessable Entity`. Version conversion (allowing writes to any served version with automatic conversion to storage) is planned for a future release.

## Deletion

`DELETE` cascades through structural children and UID-bound
`metadata.ownerReferences`. Every direct dependent is tombstoned; structural
children and owner references with `blockOwnerDeletion: true` retain the owner
through `system.rise.dev/cascade-deletion`, while non-blocking cross-tree
dependents may continue draining after the owner disappears. Rows are collected
as their controller finalizers clear.

`GET /api/v1/resources/pending-deletion` lists resources that are tombstoned and still draining — useful for spotting a deletion stuck on a finalizer. Accepts a `limit` query parameter (1–1000, default 100).

`GET .../{name-or-uid}/deletion-blockers` is an operator-only, consistent
snapshot of the concrete structural and opted-in cross-tree dependents retaining
that resource, including their deletion timestamps and finalizers. Newly
tombstoned dependents also produce best-effort structured
`resource.deletion_cascaded` logs on the `rise::audit` target.

Tombstoned resources (those pending cascade deletion) also appear in normal list responses — `GET` collection endpoints return them alongside live resources. They are identifiable by the presence of `metadata.deletionTimestamp`. This differs from kubectl's default behavior, which hides terminating resources; the resource API surfaces them explicitly so consumers can react to in-progress deletions. Use `pending-deletion` for an operator-level view of all tombstoned resources across the entire system.

```bash
curl -X DELETE \
  -H "Authorization: Bearer $TOKEN" \
  "https://rise.example.com/api/v1/resources/rise.dev/v1alpha1/organizations/acme"
```

## Examples

These examples assume a `widgets` collection (group `example.dev`) whose declared parent is the built-in `Organization` — a parent-chain depth of `1`, so a widget path carries one ancestor name (the organization).

### List Organizations

```bash
curl -H "Authorization: Bearer $TOKEN" \
  https://rise.example.com/api/v1/resources/rise.dev/v1alpha1/organizations
```

### Get An Organization

```bash
curl -H "Authorization: Bearer $TOKEN" \
  https://rise.example.com/api/v1/resources/rise.dev/v1alpha1/organizations/acme
```

### List Widgets Under An Organization

```bash
curl -H "Authorization: Bearer $TOKEN" \
  https://rise.example.com/api/v1/resources/example.dev/v1/widgets/acme
```

### Create A Widget

```bash
curl -X POST \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "apiVersion": "example.dev/v1",
    "kind": "Widget",
    "metadata": { "name": "my-widget", "annotations": {} },
    "spec": { "color": "blue" }
  }' \
  https://rise.example.com/api/v1/resources/example.dev/v1/widgets/acme
```

### Update Controller Status

```bash
curl -X PUT \
  -H "Authorization: Bearer $CONTROLLER_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{ "status": { "phase": "Ready", "message": "all good" } }' \
  https://rise.example.com/api/v1/resources/example.dev/v1/widgets/acme/my-widget/status
```

The status payload is stored under `status.controllers[<controller-id>]`. Other controller slots are unaffected.
