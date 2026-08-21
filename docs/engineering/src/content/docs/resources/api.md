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
| `{group}/{version}/{plural}/{ancestor}…/{name}/deletion-blockers` (`D+2`) | GET | Deletion-blocker diagnostics |
| `{group}/{version}/{plural}/uid:{uuid}` | GET, PUT, DELETE | Item by UID |
| `{group}/{version}/{plural}/uid:{uuid}/{sub}` | PUT | `status` or `finalizers` by UID |
| `{group}/{version}/{plural}/uid:{uuid}/deletion-blockers` | GET | Deletion-blocker diagnostics by UID |
| `pending-deletion` | GET | List tombstoned resources awaiting GC |

Ancestor segments are bare resource *names*; the ancestor *kinds* are derived from the leaf's `ResourceDefinition` parent chain and never appear in the URL. `pending-deletion` is only valid as the sole path segment, so a resource may be named `pending-deletion` without ambiguity.

Unversioned paths are not supported — `{group}/{version}` always names the leaf collection.

### UID addressing

The leaf identifier may be given as `uid:{uuid}` instead of a name. A UID is globally unique, so the ancestor names are redundant: the `uid:` form is valid **only** as the sole identifier segment, immediately after `{group}/{version}/{plural}`, with no ancestor names. A `uid:` token anywhere else in the path is a `400`.

UID addressing skips the ancestor-chain resolution entirely — it works even if an ancestor's `ResourceDefinition` has been removed. This is the intended disaster-recovery path: if a RD is deleted while instances still exist, named addressing fails, but operators and controllers can still reach the orphaned rows by UID.

When a UID-prefixed identifier is used in a PUT URL, the body's `metadata.name` is not validated against the URL — but it still must match the stored resource name. Resource names are immutable.

## Authorization

Every user-authenticated request is authorized by the ADR-0001 engine: one
`(verb, ResourceKind, subresource?)` decision per resource, evaluated against
that resource's own ancestry and effective labels. There is no operator gate in
front of the API any more. An operator reaches everything because the seeded
`PlatformRoleBinding/system-admin` says so and because an operator's request
ignores every `Deny`; anyone else reaches exactly what stored policy grants them.

| Caller | How it is authorized |
|---|---|
| Operator (`auth.operator_users`, `auth.operator_idp_groups`) | Expands to `system:operators`, whose seeded binding allows every verb on every kind and subresource |
| Any other authenticated user | Live RBAC: bindings that name them, one of their Groups, `org:<name>`, or `system:authenticated`, plus dynamic ownership bindings resolved from `rise.dev/owner` |
| Controller (`auth.controllers` JWT) | PUT `status` and `finalizers` only, gated by the collection's `allowedStatusControllerIds` |

The verbs map onto the HTTP surface directly: `list` and `get` for reads,
`create` for POST, `update` for item PUT and for the `status`/`finalizers`
subresources, `delete` for DELETE. Subresource permissions are separate from the
main resource — a statement with no `subresources` field permits the main
resource only.

Two read granularities exist, and they are independent grants:

- **`list` without `get`** returns each item projected onto an allowlist —
  `apiVersion`, `kind`, and the `metadata` fields `name`, `labels`,
  `effectiveLabels`, and `deletionTimestamp`. `spec`, `status`, and any other
  top-level field are absent, and so are `uid`, `revision`, and `discriminator`.
- **`list` and `get`** returns the full stored object for that item.

Items the caller cannot `list` are omitted, and their existence is masked: a
caller with no applicable grant receives an empty collection, never a `403`
confirming the scope is populated. Addressing one resource by name is different —
a `get`, `update`, or `delete` the caller does not hold is a `403`. Which
*collections* exist stays visible to every caller who reaches the API at all,
matching discovery being a property of the registry rather than of any one
resource. Who reaches it is the platform-access policy's question rather than
authorization's: these routes sit behind `auth.platform_access`, which under its
default `allow_all` admits every authenticated user.

`metadata.effectiveLabels` is on every response, resolved live by walking the
ancestor chain nearest-wins: a child with no value of its own reports the one it
inherits, and a child that sets the key shadows its ancestor rather than unioning
with it. It is the same walk a binding's `labelSelector` matches against, so the
value a client sees and the value authorization used can never disagree. Note the
corollary: granting broad `list` beneath an ancestor exposes that ancestor's
inherited label values on every listed child.

`auth.admin_users` are admins within the default Organization for typed APIs
only; they do **not** receive Operator and are not implicitly granted anything
here. Access for anyone other than an operator comes from a binding.

### Authorization-changing writes

Attaching a `metadata.ownerReferences` entry is authorized separately, because
the edge is not inert: deleting the owner starts deletion of the dependent. A
*new* reference requires `use` on the owner — §2's verb for referencing a
resource from another resource's fields — and, when the dependent already
exists, `delete` on the dependent, since the edge is what makes it deletable
through someone else's resource. Re-sending references that are already stored
is an ordinary read-modify-write.

A write that changes who can do what — a `Role` or `PlatformRole` body, a
binding, a `GroupMembership`, an identity or trust mapping, or a label some
binding selects on — additionally passes ADR-0001 §5's **grant gate**: the
authority the change would confer must already be held by the writer, over the
same domain. Holding `create` on `PlatformRoleBinding` therefore does not let a
caller bind a Role granting more than they have; the refusal is a `403` naming
the recipient, the domain, and the missing authority.

The check and the mutation are one `SERIALIZABLE` transaction with bounded
retry, so a concurrent revocation either precedes the check or forces the write
to be replayed against fresh facts. A write that keeps losing that race returns
`503` with a retryable message rather than committing on stale assumptions.

Two consequences worth knowing:

- An unbound `Role` body confers nothing, so authoring one is ungated. Binding it
  is what the gate weighs.
- Deleting a binding or narrowing a `Role` is also a grant when it removes a
  `Deny`, and passes the same check.

### Seeded baseline policy

Startup seeds five root policy resources described by [ADR-0001](../adr/0001-unified-permission-model). These are the whole of the shipped policy: an install that adds no bindings of its own grants nothing to anyone but operators and the subjects a `rise.dev/owner` label names.

| Resource | Grants | Mutability |
|---|---|---|
| `PlatformRole/system-admin` | every verb on every kind and subresource | immutable |
| `PlatformRoleBinding/system-admin` | `system-admin` to `system:operators` | immutable |
| `PlatformRole/org-admin` | the global org-admin baseline | operator-editable |
| `PlatformRole/resource-owner` | `get`, `list`, `update`, `delete` — no `create`, no subresources | operator-editable |
| `PlatformRoleBinding/resource-owner` | `resource-owner` to whoever `rise.dev/owner` names | operator-editable |

The two immutable rows are not the source of operator authority — the evaluator hardcodes that, so the guarantee survives a bad restore or a direct database write. They are its only inspectable record, which is why the store rejects `PUT` and `DELETE` on both, and why a startup finding either diverged from its shipped definition fails with the row named and remediation instructions rather than silently rewriting it. Seeding never overwrites the three editable rows, so an operator edit survives every restart and a deleted one is re-created on the next.

Editing `org-admin` changes what administrators may do in every organization, never *who* they are: admin standing comes from an exact org-root, scope-only `RoleBinding`, so no label or Group name can confer it. Editing `resource-owner` changes what ownership means platform-wide; an organization can instead override the default for itself with its own binding on the same `(subject, label key)` pair.

`system:operators` is reserved: no binding of either kind may name it except the seeded root `PlatformRoleBinding/system-admin`, and only carrying its shipped body.

An org `RoleBinding`'s subject must belong to the Organization the binding sits in. A subject that names its own organization — `group:`, `serviceaccount:`, `org:` — is checked at write time and refused on a mismatch: both organizations are fixed once the row exists, so such a binding would read as a cross-org grant while granting nothing, forever. `user:` and `system:authenticated` subjects are accepted, because their affiliation is a live membership question rather than a property of the identifier.

Because the Organization is implied by placement, that subject also accepts the relative form `group:<name>`, expanded against the parent before the row is stored:

```json
{ "subject": "group:platform", "roleRef": { "kind": "Role", "name": "viewer" } }
```

under `acme` stores `group:acme/platform`. `PlatformRoleBinding` has no parent Organization and so takes absolute subjects only.

Resource lifecycle operations are audit-logged on the `rise::audit` target. Records include `resource.created`, `resource.updated`, `resource.deleted`, `resource.deletion_cascaded`, `resource.controller_status_updated`, `resource.controller_finalizers_updated`, `resource.user_status_updated`, `resource.user_finalizers_updated`, `resource.pending_deletion_listed`, `resource.deletion_blockers_listed`, `resource.access_denied` (a refused authorization decision), and `resource.grant_gate` (what the grant gate compared, including the operator short-circuit that produces no claims). Cascade records are best-effort after commit; durable delivery would require a transactional outbox or Event resource.

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
      "uid": "6e999cac-9c0b-4a94-a844-546ce8d508fb",
      "blockOwnerDeletion": false
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
  do not change the resource URL or grant authorization. Deleting an owner
  always starts dependent deletion. Optional `blockOwnerDeletion` defaults to
  `false`; when `true`, that dependent also keeps the owner visible until the
  dependent is collected.
- A built-in `Organization` or `ResourceDefinition` cannot currently be the
  dependent side of an owner reference: requests that put
  `metadata.ownerReferences` on either kind are rejected. Owner-driven deletion
  tombstones dependents through the generic garbage collector, which would
  bypass the additional deletion safety checks for legacy Organization-owned
  records and resources that still use a ResourceDefinition. Both kinds may
  still be referenced as owners. A custom `Organization` kind in another API
  group is unaffected.
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
- `metadata.name` must equal the URL name (or the stored row's name when addressed by UID); resource names are immutable.
- `metadata.ownerReferences` replaces the complete owner-reference set. Omitting
  it is equivalent to an empty set.
- `status` is rejected on update (use the `status` subresource).
- Reads (GET/LIST) work for any *served* version. Writes (POST/PUT) must use the *storage* version — a write targeting a served non-storage version is rejected with `422 Unprocessable Entity` (version conversion is not yet implemented).

### Status subresource (controllers and users)

```http
PUT /api/v1/resources/example.dev/v1/widgets/acme/my-widget/status
Content-Type: application/json
Authorization: Bearer <controller-jwt>

{"status": {"phase": "Ready", "message": "all good"}}
```

The body's `status` is stored under `status.controllers[<id>]` where `<id>` is the caller's `identity_id` (for controller tokens) or `operator:<email>` (for user writes, authorized by `(update, Kind, status)`). Other controller slots in `status.controllers` are unaffected. A controller can only write its own slot; a user write lands in its own writer-keyed slot and never overwrites a controller's.

The status update applies unconditionally to the latest row (no revision needed) and increments `revision`.

### Finalizer subresource (controllers and users)

```http
PUT /api/v1/resources/example.dev/v1/widgets/acme/my-widget/finalizers
Content-Type: application/json
Authorization: Bearer <controller-jwt>

{"add": ["controller.example.com/cleanup"], "remove": []}
```

`add` and `remove` are applied in a single transaction. Both lists may be empty. Adding or removing a `system.rise.dev/*` finalizer is rejected with `400`. Controllers can only add or remove finalizers whose name corresponds to a controller-owned token; a user write authorized by `(update, Kind, finalizers)` bypasses the controller-ownership check — that path is the deadlock-break for stuck cascade deletions — but the reserved-prefix guard still applies.

### Delete (cascade)

```http
DELETE /api/v1/resources/rise.dev/v1alpha1/organizations/acme
Authorization: Bearer <operator-jwt>
```

`DELETE` always cascades through structural children and owner-reference
dependents (see [Storage Model](./storage)). Two response shapes:

- `200 OK` with `{"deleted": true, "uid": "..."}` — row had no finalizers or blocking dependents, hard-deleted in place. Non-blocking owner-reference dependents have already been tombstoned and continue draining asynchronously.
- `202 Accepted` with `{"deleted": false, "markedForDeletion": true, "resource": ...}` — row tombstoned, cascade in progress. The returned envelope carries the row at its post-stamp state (`metadata.deletionTimestamp` set, `metadata.finalizers` may include `system.rise.dev/cascade-deletion`).

### Pending-deletion listing

```http
GET /api/v1/resources/pending-deletion?limit=100
Authorization: Bearer <operator-jwt>
```

Returns tombstoned rows oldest-first, up to `limit` (1–1000, default 100). Useful for spotting deletions stuck on a finalizer.

### Deletion-blockers subresource

```http
GET /api/v1/resources/rise.dev/v1alpha1/organizations/acme/deletion-blockers
Authorization: Bearer <operator-jwt>
```

Returns the concrete resources currently preventing the addressed resource
from being collected. Structural children are always blockers;
owner-reference dependents appear only when their matching reference carries
`blockOwnerDeletion: true`. Each item identifies the relationship and resource,
including its deletion timestamp and finalizers. The response also reports
whether `system.rise.dev/cascade-deletion` is currently present. This
subresource is authorized by `(get, Kind, deletion-blockers)` on the resource
being blocked, and computed from the canonical resource rows; it does not
maintain a separate blocker table. The blockers are filtered per item like any
other collection: one the caller cannot `list` is counted in `hiddenBlockers`
rather than named, so the report never reads as "nothing is blocking this" while
something is.

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
