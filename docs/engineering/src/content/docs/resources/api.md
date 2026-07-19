---
title: "HTTP API"
description: "Generic resource API path grammar, auth model, request/response envelopes, and status/finalizer semantics."
---

All generic resource operations are dispatched from a single wildcard route:

```
GET|POST|PUT|DELETE /api/v1/resources/{*path}
```

The path describes a collection, an item, a subresource, or the operational `pending-deletion` listing. The handler classifies the path, resolves the leaf kind's parent chain via the resource store, and routes the request to the matching store operation.

## Path grammar

A path always names the **leaf** collection first as `{group}/{version}/{plural}`, then carries ancestor names (root-most first), then optionally a leaf name and a subresource keyword. Let `D` be the leaf kind's parent-chain depth (`0` for a root-scoped collection like `Organization`, `1` for a kind whose declared parent is `Organization`, etc.).

| Path | Methods | Operation |
|---|---|---|
| `{group}/{version}/{plural}/{ancestor}…` (`D` names) | GET, POST | List / create in the collection |
| `{group}/{version}/{plural}/{ancestor}…/{name}` (`D+1`) | GET, PUT, DELETE | Get / update / delete an item |
| `{group}/{version}/{plural}/{ancestor}…/{name}/status` (`D+2`) | PUT | Status subresource update |
| `{group}/{version}/{plural}/{ancestor}…/{name}/finalizers` (`D+2`) | PUT | Finalizer subresource update |
| `{group}/{version}/{plural}/uid:{uuid}` | GET, PUT, DELETE | Item by UID |
| `{group}/{version}/{plural}/uid:{uuid}/{sub}` | PUT | `status` or `finalizers` by UID |
| `pending-deletion` | GET | List tombstoned resources awaiting GC |

Ancestor segments are bare resource *names*; the ancestor *kinds* are derived from the leaf's `ResourceDefinition` parent chain and never appear in the URL. `pending-deletion` is only valid as the sole path segment, so a resource may be named `pending-deletion` without ambiguity.

Unversioned paths are not supported — `{group}/{version}` always names the leaf collection.

### UID addressing

The leaf identifier may be given as `uid:{uuid}` instead of a name. A UID is globally unique, so the ancestor names are redundant: the `uid:` form is valid **only** as the sole identifier segment, immediately after `{group}/{version}/{plural}`, with no ancestor names. A `uid:` token anywhere else in the path is a `400`.

UID addressing skips the ancestor-chain resolution entirely — it works even if an ancestor's `ResourceDefinition` has been removed. This is the intended disaster-recovery path: if a RD is deleted while instances still exist, named addressing fails, but operators and controllers can still reach the orphaned rows by UID.

When a UID-prefixed identifier is used in a PUT URL, the body's `metadata.name` is not validated against the URL — but it still must match the stored resource name (resources cannot be renamed via PUT).

## Auth tiers

| Tier | Credentials | Permitted operations |
|---|---|---|
| Operator | Listed in `auth.operator_users` | GET, POST, item PUT, DELETE, PUT `status` and `finalizers` |
| Controller | Configured `auth.controllers` JWT | PUT `status` and `finalizers`, gated by `allowed_status_controller_ids` |

Controller tokens cannot perform item-level PUT or any other operator operation. A non-operator user receives `403` on every endpoint — including paths that would resolve to a non-existent collection, so collection existence is not probeable by non-operators.

Operators have unrestricted access to subresources. When an operator writes finalizers, the per-controller ownership check is bypassed (the operator path is the deadlock-break for stuck cascade deletions). The `system.rise.dev/*` reserved-prefix guard still applies — operators cannot manipulate the cascade-deletion finalizer.

`auth.admin_users` are admins within the default Organization for typed APIs only; they do **not** receive Operator and cannot access the generic API unless also listed in `auth.operator_users`.

All write operations are audit-logged on the `rise::audit` target. Events: `resource.created`, `resource.updated`, `resource.deleted`, `resource.controller_status_updated`, `resource.controller_finalizers_updated`, `resource.operator_status_updated`, `resource.operator_finalizers_updated`, `resource.pending_deletion_listed`.

### Controller authorization

A controller JWT is verified by `auth_middleware` against the configured `ControllerIdentity` (issuer + JWKS + claim constraints) and yields a `ControllerAuthContext` carrying the controller's `identity_id` (the stable string written under `status.controllers`).

For a status/finalizer write, the handler additionally checks that the controller's `identity_id` appears in the resolved collection's `allowedStatusControllerIds`. An empty allowlist is **default-deny** — built-in collections (Organization, ResourceDefinition) currently have an empty allowlist, so controllers cannot write their status to built-ins until a future phase wires controller ownership for them.

## Request and response envelopes

All bodies follow the Kubernetes-style envelope: `apiVersion`, `kind`, and `metadata` are top-level fields alongside `spec` and `status`. The `/api/v1/` URL prefix is the Rise HTTP API namespace and is unrelated to `apiVersion` in the body.

### Create (POST)

```http
POST /api/v1/resources/example.dev/v1/widgets/acme
Content-Type: application/json
Authorization: Bearer <operator-jwt>

{
  "apiVersion": "example.dev/v1",
  "kind": "Widget",
  "metadata": {
    "name": "my-widget",
    "annotations": {"team": "platform"},
    "finalizers": [],
    "ownerReferences": [{
      "apiVersion": "rise.dev/v1alpha1",
      "kind": "Organization",
      "name": "acme",
      "uid": "6e999cac-9c0b-4a94-a844-546ce8d508fb"
    }]
  },
  "spec": {"color": "blue"}
}
```

- The body's `apiVersion` must be a *served* version of the collection.
- The body's `kind` must match the collection's kind.
- `metadata.name` is the resource's name within its scope.
- `metadata.ownerReferences` is optional lifecycle metadata. Every entry must
  identify the same live resource by API group/kind, name, and UID. References
  do not change the resource URL or grant authorization.
- Server-controlled fields (`uid`, `revision`, `discriminator`, `deletionTimestamp`) are rejected on create.
- `status` is rejected on create.
- Response: `201 Created` with the created resource (envelope projected to the URL's served version).

### Update (PUT)

```http
PUT /api/v1/resources/example.dev/v1/widgets/acme/my-widget
Content-Type: application/json
Authorization: Bearer <operator-jwt>

{
  "apiVersion": "example.dev/v1",
  "kind": "Widget",
  "metadata": {
    "name": "my-widget",
    "revision": 7,
    "annotations": {"team": "platform"},
    "finalizers": ["controller.example.com/cleanup"],
    "ownerReferences": []
  },
  "spec": {"color": "red"}
}
```

- `metadata.revision` is required; omitting it is `400`.
- A revision mismatch is `409 Conflict`.
- `metadata.name` must equal the URL name (or the stored row's name when addressed by UID) — resources cannot be renamed.
- `metadata.ownerReferences` replaces the complete owner-reference set. Omitting
  it is equivalent to an empty set.
- `status` is rejected on update (use the `status` subresource).
- Reads (GET/LIST) work for any *served* version. Writes (POST/PUT) must use the *storage* version — a write targeting a served non-storage version is rejected with `422 Unprocessable Entity` (version conversion is not yet implemented).

### Status subresource (controllers and operators)

```http
PUT /api/v1/resources/example.dev/v1/widgets/acme/my-widget/status
Content-Type: application/json
Authorization: Bearer <controller-jwt>

{"status": {"phase": "Ready", "message": "all good"}}
```

The body's `status` is stored under `status.controllers[<id>]` where `<id>` is the caller's `identity_id` (for controller tokens) or `operator:<email>` (for operator-user writes). Other controller slots in `status.controllers` are unaffected. A controller can only write its own slot; operators can write any slot via the operator path.

The status update applies unconditionally to the latest row (no revision needed) and increments `revision`.

### Finalizer subresource (controllers and operators)

```http
PUT /api/v1/resources/example.dev/v1/widgets/acme/my-widget/finalizers
Content-Type: application/json
Authorization: Bearer <controller-jwt>

{"add": ["controller.example.com/cleanup"], "remove": []}
```

`add` and `remove` are applied in a single transaction. Both lists may be empty. Adding or removing a `system.rise.dev/*` finalizer is rejected with `400`. Controllers can only add or remove finalizers whose name corresponds to a controller-owned token; operators bypass the controller-ownership check (but the reserved-prefix guard still applies).

### Delete (cascade)

```http
DELETE /api/v1/resources/rise.dev/v1alpha1/organizations/acme
Authorization: Bearer <operator-jwt>
```

`DELETE` always cascades through structural children and owner-reference
dependents (see [Storage Model](./storage)). Two response shapes:

- `200 OK` with `{"deleted": true, "uid": "..."}` — row had no finalizers and no children, hard-deleted in place.
- `202 Accepted` with `{"deleted": false, "markedForDeletion": true, "resource": ...}` — row tombstoned, cascade in progress. The returned envelope carries the row at its post-stamp state (`metadata.deletionTimestamp` set, `metadata.finalizers` may include `system.rise.dev/cascade-deletion`).

### Pending-deletion listing

```http
GET /api/v1/resources/pending-deletion?limit=100
Authorization: Bearer <operator-jwt>
```

Returns tombstoned rows oldest-first, up to `limit` (1–1000, default 100). Useful for spotting deletions stuck on a finalizer.

## Status codes

| Code | Meaning |
|---|---|
| 200 / 201 / 202 | Operation succeeded (200 read/update, 201 create, 202 cascade tombstoned) |
| 400 | Malformed path, body validation, reserved finalizer prefix, wrong version |
| 401 | No credentials or JWT verification failed |
| 403 | Authenticated, but not an operator (or controller not in the allowlist) |
| 404 | Unknown collection, unknown resource, kind/version mismatch on `uid:` |
| 405 | Method not valid for the addressed path (e.g. GET on `/status`) |
| 409 | Name conflict on create, revision conflict on update |
| 422 | Write targeting a served non-storage version (no conversion yet) |
| 503 | Discriminator generator exhausted (10 retries) |

## Versioning behavior

- Only versions with `served: true` are addressable in the URL; at least one version must be served.
- Exactly one version must have `storage: true`.
- Reads and lists return rows stored at any declared version of the same group/kind — including a non-served storage version. The response `apiVersion` is projected to the requested version.
- Writes (POST/PUT) must use the storage version. Conversion webhooks are not yet implemented.

## Example: Organization-scoped custom resource

Given a `ResourceDefinition` for `Widget` (`group: example.dev`, `kind: Widget`, `plural: widgets`, `parent: {apiVersion: rise.dev/v1alpha1, kind: Organization}`), the URL `D = 1` and paths look like:

```bash
# List widgets under the 'acme' organization.
curl -H "Authorization: Bearer $TOKEN" \
  https://rise.example.com/api/v1/resources/example.dev/v1/widgets/acme

# Get one widget.
curl -H "Authorization: Bearer $TOKEN" \
  https://rise.example.com/api/v1/resources/example.dev/v1/widgets/acme/my-widget

# Get the same widget by UID (skips the org-chain resolution).
curl -H "Authorization: Bearer $TOKEN" \
  https://rise.example.com/api/v1/resources/example.dev/v1/widgets/uid:0c0ffee0-dead-beef-cafe-000000000000

# Update its controller status.
curl -X PUT \
  -H "Authorization: Bearer $CONTROLLER_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"status": {"phase": "Ready"}}' \
  https://rise.example.com/api/v1/resources/example.dev/v1/widgets/acme/my-widget/status
```
