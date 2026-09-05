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

Every authenticated request — User or Controller alike — is authorized by the
ADR-0001 engine: one `(verb, ResourceKind, subresource?)` decision per
resource, evaluated against that resource's own ancestry and effective labels.
There is no operator gate in front of the API any more. An operator reaches
everything because the seeded `PlatformRoleBinding/system-admin` says so and
because an operator's request ignores every `Deny`; anyone else reaches
exactly what stored policy grants them.

| Caller | How it is authorized |
|---|---|
| Operator (`auth.operator_users`, `auth.operator_idp_groups`) | Expands to `system:operators`, whose seeded binding allows every verb on every kind and subresource |
| Any other authenticated user | Live RBAC: bindings that name them, one of their Groups, `org:<name>`, or `system:authenticated`, plus dynamic ownership bindings resolved from `rise.dev/owner` |
| Controller (a live `Controller` resource) | Live RBAC like any other principal: bindings naming its `controller:<name>` subject. See [Controller authorization](#controller-authorization) |

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

Every masked answer is byte-identical to the genuine one: status `404` and the
body `{"error": "resource not found"}`, whether the resource is absent, an
ancestor is absent, the row belongs to another collection, or the caller simply
may not read it. Authorization runs before the request body is inspected, so a
malformed body against an invisible resource is masked too rather than answered
with a `400`. On the create path the body checks run *first* instead — they read
only the request and the collection registry, so the same `400` comes back
whether or not the parent exists, and the parent is reached only once the answer
can no longer depend on the body.

The masking is on status and body, not on timing: an existing-but-invisible
resource costs an ancestry walk and a policy evaluation that an absent one does
not. Treat the difference as observable by anyone who can measure it.

Items the caller cannot `list` are omitted, and their existence is masked: a
caller with no applicable grant receives an empty collection, never a `403`
confirming the scope is populated. Addressing one resource by name is masked the
same way — a caller who holds no `get` on it receives the `404` a name that does
not exist receives, on every verb, so the item path cannot hand back one name at
a time what the listing withholds. A caller who *can* read the resource is told
which verb they are short instead, with a `403`: they already know it exists, and
masking would only make the refusal harder to act on. Which *collections* exist
stays visible to every caller who reaches the API at all, matching discovery
being a property of the registry rather than of any one resource. Who reaches it is the platform-access policy's question rather than
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

Changing the `metadata.ownerReferences` set is authorized separately, because
the edge is not inert: deleting the owner starts deletion of the dependent.
Attaching or removing a reference both require `use` on the owner — §2's verb
for referencing a resource from another resource's fields. Attaching
additionally requires `delete` on the dependent when it already exists, since
the edge is what makes it deletable through someone else's resource; and
`blockOwnerDeletion: true` requires `delete` on the *owner*, because that flag
is not a reference but a hold on the owner's own deletion. Removing a reference
whose owner is already gone or draining is ungated — the store will not accept
such a reference back, so gating its removal would leave the dependent with no
legal write at all. Re-sending references that are already stored is an ordinary
read-modify-write.

A write that changes who can do what — a `Role` or `PlatformRole` body, a
binding, a `GroupMembership`, an identity or trust mapping, a `User` create or
activation, or a label some binding selects on — additionally passes ADR-0001
§5's **grant gate**: the
authority the change would confer must already be held by the writer, over the
same domain. Holding `create` on `PlatformRoleBinding` therefore does not let a
caller bind a Role granting more than they have; the refusal is a `403`.

How much that refusal says depends on where its parts came from. A recipient or a
domain the caller supplied in this request is named; one read out of stored policy
is not, because a refusal that echoes it is a read of policy the caller may hold
nothing on. So a refused binding create names the subject and scope it asked for,
while a refused `Role` edit or label write names neither. What the recipient would
have *gained* is never named: the gate compares their whole effective policy over
the domain, so a witness tuple can come from any binding delivering policy to them
— including ones the caller has never seen. The full comparison is in the
`rise::audit` `resource.grant_gate` record.

The check and the mutation are one `SERIALIZABLE` transaction with bounded
retry, so a concurrent revocation either precedes the check or forces the write
to be replayed against fresh facts.

Two writes reach the gate because of what they *schedule* rather than what they
say. Attaching an owner reference to a policy resource is a scheduled delete of
it — the cascade tombstones the dependent, and a tombstoned binding stops
applying at once — so it is diffed as that delete. Deleting an Organization is
diffed as the deletion of every `Role` and `RoleBinding` beneath it, for the
same reason. In both cases removing a `Deny` is a grant, and routing it through
a cascade must not be cheaper than asking for it directly. A write that keeps losing that race returns
`503` with a retryable message rather than committing on stale assumptions.

Three consequences worth knowing:

- An unbound `Role` body confers nothing, so authoring one is ungated. Binding it
  is what the gate weighs.
- Deleting a binding or narrowing a `Role` is also a grant when it removes a
  `Deny`, and passes the same check.
- Removing a membership is not gated — ADR-0001 §4 puts it outside — and a
  platform-tier `Deny` subjected to `org:<name>` or `group:<org>/<name>` stops
  matching a caller who drops that affiliation. So granting `delete` on
  `GroupMembership` lets those users shed such a cap along with the membership.
  Shipped policy grants it to nobody: the seeded owner binding is label-selected,
  so leaving is an explicit per-organization grant. Grant it where self-service
  exit is wanted, not where a subject-scoped platform `Deny` is load-bearing.
  Re-entry stays gated, so a caller who leaves cannot readmit themselves to
  recover what the affiliation carried.

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

A controller JWT is matched against live `ControllerTrustPolicy` resources
beneath a live root `Controller` and resolves to the ordinary principal
`controller:<name>`, where `<name>` is the Controller resource's name. From
there a Controller is authorized exactly like a User: the choke point
evaluates `(verb, ResourceKind, subresource?)` against stored policy, with no
separate controller-specific gate. An org `RoleBinding` never reaches a
Controller — it belongs to no organization (ADR-0001 §3) — so a Controller's
grants are always `PlatformRoleBinding`s.

To let a controller reconcile a kind, create its identity and grant it access:

```jsonc
// 1. The Controller identity — root-scoped, one per controller process.
POST /api/v1/resources/rise.dev/v1alpha1/controllers
{ "apiVersion": "rise.dev/v1alpha1", "kind": "Controller",
  "metadata": { "name": "widget-controller" }, "spec": {} }

// 2. What issuer/claims a token must present to authenticate as it.
POST /api/v1/resources/rise.dev/v1alpha1/controllertrustpolicies/widget-controller
{ "apiVersion": "rise.dev/v1alpha1", "kind": "ControllerTrustPolicy",
  "metadata": { "name": "github-actions" },
  "spec": { "issuer": "https://token.actions.githubusercontent.com",
            "claims": { "aud": "rise-controller", "sub": "repo:acme/widget-controller:*" } } }

// 3. The permission to grant.
POST /api/v1/resources/rise.dev/v1alpha1/platformroles
{ "apiVersion": "rise.dev/v1alpha1", "kind": "PlatformRole",
  "metadata": { "name": "widget-controller-role" },
  "spec": { "statements": [{ "effect": "Allow", "kinds": ["example.dev/Widget"],
                              "verbs": ["get", "list", "update"],
                              "subresources": ["status", "finalizers"] }] } }

// 4. Bind it to the controller subject.
POST /api/v1/resources/rise.dev/v1alpha1/platformrolebindings
{ "apiVersion": "rise.dev/v1alpha1", "kind": "PlatformRoleBinding",
  "metadata": { "name": "widget-controller-binding" },
  "spec": { "subject": "controller:widget-controller",
            "roleRef": { "kind": "PlatformRole", "name": "widget-controller-role" } } }
```

Several trust policies matching the *same* Controller are an ordinary match — a
Controller may accept more than one issuer or claim shape. Policies matching
*different* Controllers make a token ambiguous and the request is refused with
`409`. Deleting or tombstoning a Controller resource invalidates every token
minted for it on the next request; a controller that still holds finalizers
when its `PlatformRoleBinding` is removed can no longer clear them itself — a
user with the same subresource grant can, through the same finalizers
endpoint (see below).

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
    "finalizers": ["widget-controller/cleanup"],
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

The body's `status` is stored under `status.controllers[<key>]` where `<key>` is the calling Controller's resource name (for a controller token) or `operator:<email>` (for a user write, authorized by `(update, Kind, status)`). Other slots in `status.controllers` are unaffected. A controller can only write its own slot; a user write lands in its own writer-keyed slot and never overwrites a controller's.

The status update applies unconditionally to the latest row (no revision needed) and increments `revision`.

### Finalizer subresource (controllers and users)

```http
PUT /api/v1/resources/example.dev/v1/widgets/acme/my-widget/finalizers
Content-Type: application/json
Authorization: Bearer <controller-jwt>

{"add": ["widget-controller/cleanup"], "remove": []}
```

`add` and `remove` are applied in a single transaction. Both lists may be empty. Adding or removing a `system.rise.dev/*` finalizer is rejected with `400`. A controller can only add or remove finalizers named `<its own resource name>` or `<its own resource name>/<reason>`; a user write authorized by `(update, Kind, finalizers)` bypasses that ownership check — that path is the deadlock-break for stuck cascade deletions — but the reserved-prefix guard still applies.

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
other collection, on `get` rather than `list` because each item carries more
than list granularity projects (its UID and its finalizers): one the caller
cannot `get` is counted in `hiddenBlockers` rather than named, so the report
never reads as "nothing is blocking this" while something is.

`hiddenBlockers` is itself a deliberate disclosure: it is a live count of
resources the caller may not read, so a principal holding only
`(get, Kind, deletion-blockers)` can watch it move as others create and delete
children. That is accepted rather than overlooked — a blocker report that
silently omits blockers is worse than one that says how many it withheld — but
grant the subresource on that basis, not on the assumption that it reveals
nothing about a subtree the caller cannot list.

## Status codes

| Code | Meaning |
|---|---|
| 200 / 201 / 202 | Operation succeeded (200 read/update, 201 create, 202 cascade tombstoned) |
| 400 | Malformed path, body validation, reserved finalizer prefix, wrong version |
| 401 | No credentials or JWT verification failed |
| 403 | Authorized to read the resource but not to perform this verb, or refused by the grant gate |
| 404 | Unknown collection, unknown resource, kind/version mismatch on `uid:`, or a resource the caller may not read |
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
