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
| `orphans` | GET | Break-glass orphan listing |
| `orphans/apis/{group}/{version}/{plural}` | GET | List parentless resources of a non-root-scoped type |
| `orphans/apis/{group}/{version}/{plural}/{id}` | GET, DELETE | Get / delete a parentless resource of a non-root-scoped type |
| `orphans/apis/{group}/{version}/{plural}/{id}/reparent` | POST | Reparent a parentless resource of a non-root-scoped type |

`orphans` is only valid as the first path segment, so a resource may be named `orphans`.

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
| Admin + operator | Listed in both `auth.admin_users` and `auth.operator_users` | `reparent`, orphan listing, DELETE with `?propagationPolicy=Orphan` |

Controller tokens cannot perform item-level PUT or other operator operations.

## Resource Definitions

`ResourceDefinition` declares the group, kind, plural, versions, storage version, and parent type for a collection.

- Root-scoped resources set `scope: "root"` and do not declare `parent`.
- Non-root resources must declare `parent: { "apiVersion": "...", "kind": "..." }`.
- Children may only exist directly under a parent whose stored `apiVersion` and `kind` exactly match the declared parent.
- The type decides scope. A non-root-scoped resource with no current `parent_uid` is a parentless scoped resource, not a root-scoped resource.
- A `ResourceDefinition` cannot be deleted while any related resources exist in any declared version.

Version behavior follows Kubernetes-style semantics:

- Only versions with `served: true` can be addressed by the HTTP API.
- Exactly one version must have `storage: true`.
- Creates and updates through any served version are stored at the current storage version.
- Reads and lists can return rows stored in any served version of the same group/kind; the response `apiVersion` is projected to the requested version.
- Conversion is currently no-op only: the API projects `apiVersion` but does not transform `spec`.

## Propagation Policy

DELETE accepts `propagationPolicy`:

| Value | Behaviour | Requirement |
|---|---|---|
| `Cascade` (default) | Deletes the resource and descendants | Operator |
| `Orphan` | Deletes only the resource; children are detached and become discoverable through type orphan paths | Admin + operator |

```bash
curl -X DELETE \
  -H "Authorization: Bearer $TOKEN" \
  "https://rise.example.com/api/v1/resources/apis/rise.dev/v1alpha1/organizations/acme?propagationPolicy=Orphan"
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
