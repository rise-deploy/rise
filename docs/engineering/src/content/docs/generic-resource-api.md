---
title: "Generic Resource API"
---

The generic resource API (`/api/v1/resources/…`) is an operator-only HTTP surface for managing arbitrary resources in the resource store without needing typed endpoints for each kind. It is intentionally low-level — think of it as direct store access over HTTP.

## URL Grammar

Every path under `/api/v1/resources/` follows an alternating `collection/identifier` pattern. The leaf determines which operation is dispatched.

| Path | HTTP Methods | Operation |
|---|---|---|
| `col` | GET, POST | List / Create in root-scoped collection |
| `col/id` | GET, PUT, DELETE | Get / Update / Delete root-scoped item |
| `col/id/status` | PUT | Controller status update |
| `col/id/finalizers` | PUT | Controller finalizer update |
| `col/id/reparent` | POST | Break-glass reparent |
| `colA/idA/colB` | GET, POST | List / Create in org-scoped collection under `idA` |
| `colA/idA/colB/idB` | GET, PUT, DELETE | Get / Update / Delete at depth 1 |
| `colA/idA/colB/idB/status` | PUT | Controller status at depth 1 |
| `…` | … | Arbitrary depth follows the same pattern |
| `orphans` | GET | Break-glass orphan listing |

## Reserved Path Segments

The following names cannot be used as collection plural names or resource identifiers:

- `orphans`
- `status`
- `finalizers`
- `reparent`

## UID Addressing

Any identifier segment can be prefixed with `uid:` to address by UUID instead of name:

```
GET /api/v1/resources/organizations/uid:123e4567-e89b-12d3-a456-426614174000
```

When a UID-prefixed identifier is used in a PUT URL, the body's `metadata.name` is not validated against the URL (it still must match the stored name).

## Authentication and Authorization

Three auth tiers gate the API:

| Tier | Credentials | Permitted operations |
|---|---|---|
| **Operator** | Listed in `auth.operator_users` | GET, POST (create), PUT (item), DELETE |
| **Controller** | Configured `ControllerIdentity` token | PUT `status` and `finalizers` only, further gated by the collection's `allowed_status_controller_ids` (default-deny when the list is empty) |
| **Admin + operator** | Listed in both `auth.admin_users` and `auth.operator_users` | `reparent`, orphan listing, DELETE with `?propagationPolicy=Orphan` |

Controller tokens cannot perform item-level PUT or any other operator operation.

## Scope Enforcement

Collections are registered as either **root-scoped** or **organization-scoped**:

- Root-scoped collections appear at the top level (`col` or `col/id`).
- Organization-scoped collections must always appear under a parent (`colA/idA/colB`).

Placing a collection at the wrong depth returns `400 Bad Request`.

## Propagation Policy

DELETE accepts a `propagationPolicy` query parameter:

| Value | Behaviour | Requirement |
|---|---|---|
| `Cascade` (default) | Deletes the resource and all descendants | Operator |
| `Orphan` | Deletes only the resource; children become orphans | Admin + operator |

```
DELETE /api/v1/resources/organizations/acme?propagationPolicy=Orphan
```

## Examples

### List organizations (depth 0)

```bash
curl -H "Authorization: Bearer $TOKEN" \
  https://rise.example.com/api/v1/resources/organizations
```

### Get a specific organization

```bash
curl -H "Authorization: Bearer $TOKEN" \
  https://rise.example.com/api/v1/resources/organizations/acme
```

### List widgets under an organization (depth 1)

```bash
curl -H "Authorization: Bearer $TOKEN" \
  https://rise.example.com/api/v1/resources/organizations/acme/widgets
```

### Create a widget

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
  https://rise.example.com/api/v1/resources/organizations/acme/widgets
```

### Update controller status (controller token)

```bash
curl -X PUT \
  -H "Authorization: Bearer $CONTROLLER_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{ "status": { "phase": "Ready", "message": "all good" } }' \
  https://rise.example.com/api/v1/resources/organizations/acme/widgets/my-widget/status
```

The status payload is stored under `status.controllers[<controller-id>]` — the controller's own slot. Other controllers' slots are unaffected.
