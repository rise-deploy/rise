---
title: "Generic Resource API"
---

The generic resource API (`/api/v1/resources/...`) is an operator-only HTTP surface for managing arbitrary resources in the resource store without typed endpoints for every kind.

## URL Grammar

Resource collection paths are versioned, following the same group/version shape as the Kubernetes API:

| Path | HTTP Methods | Operation |
|---|---|---|
| `apis/{group}/{version}/{plural}` | GET, POST | List / create in a root-scoped collection |
| `apis/{group}/{version}/{plural}/{id}` | GET, PUT, DELETE | Get / update / delete an item |
| `apis/{group}/{version}/{plural}/{id}/status` | PUT | Controller status update |
| `apis/{group}/{version}/{plural}/{id}/finalizers` | PUT | Controller finalizer update |
| `apis/{group}/{version}/{plural}/{id}/reparent` | POST | Break-glass reparent |
| `apis/{groupA}/{versionA}/{pluralA}/{idA}/apis/{groupB}/{versionB}/{pluralB}` | GET, POST | List / create children under a typed parent |
| `pending-deletion` | GET | List resources tombstoned and awaiting garbage collection |

`pending-deletion` is only valid as the sole path segment, so a resource may be named `pending-deletion`.

Unversioned resource paths are not supported.

## UID Addressing

Any identifier segment can be prefixed with `uid:` to address by UUID instead of name:

```bash
curl -H "Authorization: Bearer $TOKEN" \
  https://rise.example.com/api/v1/resources/apis/rise.dev/v1alpha1/organizations/uid:123e4567-e89b-12d3-a456-426614174000
```

When a UID-prefixed identifier is used in a PUT URL, the body's `metadata.name` is not validated against the URL. It still must match the stored resource name.

## Authentication and Authorization

| Tier | Credentials | Permitted operations |
|---|---|---|
| Operator | Listed in `auth.operator_users` | GET, POST, item PUT, DELETE |
| Controller | Configured `ControllerIdentity` token | PUT `status` and `finalizers`, gated by `allowed_status_controller_ids` |
| Admin + operator | Listed in both `auth.admin_users` and `auth.operator_users` | `reparent` |

Controller tokens cannot perform item-level PUT or other operator operations.

## Resource Definitions

`ResourceDefinition` declares the group, kind, plural, versions, storage version, and parent type for a collection.

- Root-scoped resources set `scope: "root"` and do not declare `parent`.
- Non-root resources must declare `parent: { "apiVersion": "...", "kind": "..." }`.
- Children may only exist directly under a parent whose stored `apiVersion` and `kind` exactly match the declared parent.
- A `ResourceDefinition` cannot be deleted while any related resources exist in any declared version.

Version behavior follows Kubernetes-style semantics:

- Only versions with `served: true` can be addressed by the HTTP API; at least one version must be served.
- Exactly one version must have `storage: true`.
- Creates and updates through any served version are stored at the current storage version.
- Reads and lists return rows stored at any declared version of the same group/kind — including a non-served storage version; the response `apiVersion` is projected to the requested version.
- A version cannot be removed from a `ResourceDefinition` while resources are still stored at it.
- Conversion is currently no-op only: the API projects `apiVersion` but does not transform `spec`.

## Deletion

`DELETE` always cascades: the resource and its entire subtree are removed. The resource is tombstoned, its subtree drains as finalizers clear, and rows are collected bottom-up. To delete a parent while keeping its children, `reparent` the children to another valid parent first, then delete the now-childless parent.

`GET /api/v1/resources/pending-deletion` lists resources that are tombstoned and still draining — useful for spotting a deletion stuck on a finalizer.

```bash
curl -X DELETE \
  -H "Authorization: Bearer $TOKEN" \
  "https://rise.example.com/api/v1/resources/apis/rise.dev/v1alpha1/organizations/acme"
```

## Examples

### List Organizations

```bash
curl -H "Authorization: Bearer $TOKEN" \
  https://rise.example.com/api/v1/resources/apis/rise.dev/v1alpha1/organizations
```

### Get An Organization

```bash
curl -H "Authorization: Bearer $TOKEN" \
  https://rise.example.com/api/v1/resources/apis/rise.dev/v1alpha1/organizations/acme
```

### List Widgets Under An Organization

```bash
curl -H "Authorization: Bearer $TOKEN" \
  https://rise.example.com/api/v1/resources/apis/rise.dev/v1alpha1/organizations/acme/apis/example.dev/v1/widgets
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
  https://rise.example.com/api/v1/resources/apis/rise.dev/v1alpha1/organizations/acme/apis/example.dev/v1/widgets
```

### Update Controller Status

```bash
curl -X PUT \
  -H "Authorization: Bearer $CONTROLLER_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{ "status": { "phase": "Ready", "message": "all good" } }' \
  https://rise.example.com/api/v1/resources/apis/rise.dev/v1alpha1/organizations/acme/apis/example.dev/v1/widgets/my-widget/status
```

The status payload is stored under `status.controllers[<controller-id>]`. Other controller slots are unaffected.
