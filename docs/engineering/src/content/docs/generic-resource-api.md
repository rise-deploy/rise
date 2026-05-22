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
| `{group}/{version}/{plural}/uid:{uuid}` | GET, PUT, DELETE | Get / update / delete an item by UID |
| `{group}/{version}/{plural}/uid:{uuid}/{sub}` | PUT | `status` / `finalizers` on an item by UID |
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

## Discriminator

Every resource carries a system-generated 8-character `discriminator`. It is unique among all resources that share the same parent (its siblings), regardless of kind — but **not** unique across different parents or globally.

Like `name`, the discriminator identifies a resource within its sibling scope. Unlike `name` (chosen by the user and potentially reconstructable from external inputs), it is random — so controllers can use it as a collision-free token when constructing derived identifiers in external systems while reconciling a resource.

## Authentication and Authorization

| Tier | Credentials | Permitted operations |
|---|---|---|
| Operator | Listed in `auth.operator_users` | GET, POST, item PUT, DELETE |
| Controller | Configured `ControllerIdentity` token | PUT `status` and `finalizers`, gated by `allowed_status_controller_ids` |

Controller tokens cannot perform item-level PUT or other operator operations.

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

## Deletion

`DELETE` always cascades: the resource and its entire subtree are removed. The resource is tombstoned, its subtree drains as finalizers clear, and rows are collected bottom-up.

`GET /api/v1/resources/pending-deletion` lists resources that are tombstoned and still draining — useful for spotting a deletion stuck on a finalizer.

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
