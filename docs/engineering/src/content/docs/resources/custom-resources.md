---
title: "Custom Resources"
description: "Registering external resource kinds via ResourceDefinition: schema validation, version lifecycle, and controller migration responsibilities."
---

External controllers register a new resource kind by creating a `ResourceDefinition`. Once registered, the kind is addressable via the [generic API](./api) just like a built-in. Rise stores `spec` and `metadata`; the controller owns reconciliation and writes its `status` slot.

## Built-in vs external

The store treats both the same way at the row level — same `resources` table, same uniqueness rules, same finalizer/deletion semantics. They differ in two ways:

| | Built-in | External |
|---|---|---|
| Registration | Strongly typed in Rust, compiled into the binary (`Organization`, `ResourceDefinition`) | `ResourceDefinition` row at runtime |
| Spec validation | Rust struct deserialization + a typed validator | JSON Schema declared in the `ResourceDefinition` |
| Resolution order | Tried first | Tried second |

A `ResourceDefinition`'s plural collection name cannot collide with a built-in or a reserved name (`organizations`, `projects`, `users`, `teams`, `environments`, `deployments`, `serviceaccounts`, `resourcedefinitions`). Reserved names that are not yet built-in are reserved for future built-ins so external controllers cannot squat on them.

## Registering a ResourceDefinition

Submit a `ResourceDefinition` via the operator API:

```bash
curl -X POST \
  -H "Authorization: Bearer $OPERATOR_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "apiVersion": "rise.dev/v1alpha1",
    "kind": "ResourceDefinition",
    "metadata": {"name": "widgets.example.dev"},
    "spec": {
      "group": "example.dev",
      "kind": "Widget",
      "plural": "widgets",
      "parent": {"apiVersion": "rise.dev/v1alpha1", "kind": "Organization"},
      "versions": [
        {
          "name": "v1",
          "served": true,
          "storage": true,
          "schema": {
            "type": "object",
            "required": ["color"],
            "properties": {
              "color": {"type": "string"},
              "size": {"type": "string", "enum": ["S", "M", "L"]}
            }
          }
        }
      ],
      "allowedStatusControllerIds": ["controller.example.com"]
    }
  }' \
  https://rise.example.com/api/v1/resources/rise.dev/v1alpha1/resourcedefinitions
```

Naming convention: `ResourceDefinition.metadata.name` is `<plural>.<group>` (e.g. `widgets.example.dev`) — matching Kubernetes CRD convention. This is enforced by the store's same-level uniqueness, plus the partial unique indexes on `(spec.plural)` and `(spec.group, spec.kind)`.

### Spec fields

- `group` — DNS subdomain, e.g. `example.dev`.
- `kind` — UpperCamelCase ASCII (`Widget`).
- `plural` — DNS-label collection name (`widgets`). Globally unique across external definitions.
- `parent` — optional `{apiVersion, kind}` of the parent collection. Omit for root-scoped kinds. The parent's `ResourceDefinition` must already exist; cyclic chains are rejected at registration.
- `versions` — at least one entry; exactly one must have `storage: true`; at least one must have `served: true`. Each version may declare a JSON Schema for spec validation.
- `allowedStatusControllerIds` — list of controller IDs allowed to write under `status.controllers` and `finalizers` for this collection. Empty list is default-deny; populate it with the controller IDs that should own status writes.

### Identity immutability

Once a `ResourceDefinition` has any resources stored against it, its identity fields (`group`, `kind`, `plural`, `parent`) cannot be changed. This is enforced at the application layer (Postgres has no conditional immutability). Versions can still be added, marked unserved, or removed — see "Version lifecycle" below.

### Schema validation

Each version declares an optional `schema`: a JSON Schema (Draft 2020-12) applied to `spec` on every create and update. Bad schemas fail at registration time (the store rejects invalid JSON Schema documents); spec validation failures during create/update return `400`.

The status payload is not schema-validated — the controller owns its slot.

## Controller-owned status

Status writes use a separate URL (`.../<name>/status`) and a separate body shape:

```json
{"status": {"phase": "Ready", "lastReconciledAt": "2026-05-26T00:00:00Z"}}
```

The body's `status` value is stored verbatim under `status.controllers[<id>]`, where `<id>` is the controller's `identity_id` (the stable string from its `ControllerIdentity`). Other controller slots are unaffected. The update applies unconditionally to the latest row and bumps `revision`.

Operator status writes use the same endpoint and store under `status.controllers["operator:<email>"]`. This is the recovery path when a controller has been deprovisioned and its status slot needs to be cleared or overridden.

## Controller-owned finalizers

A finalizer is a string token in `metadata.finalizers` that blocks hard-delete while present. The convention is `<controller-id>/<reason>` (e.g. `controller.example.com/cleanup`). Controllers add their finalizer when they take ownership, do their cleanup on observing `metadata.deletionTimestamp`, then remove it.

```http
PUT /api/v1/resources/example.dev/v1/widgets/acme/my-widget/finalizers
{"add": ["controller.example.com/cleanup"], "remove": []}
```

Controllers can only manipulate finalizers whose name corresponds to a controller-owned token. The reserved `system.rise.dev/*` prefix is rejected by the controller-finalizer path regardless of `controller_id` — this includes the cascade-deletion finalizer the store manages itself.

## Version lifecycle and controller-driven migration

There is no conversion webhook. Reads/lists project the response `apiVersion` to the URL-requested version but do not transform `spec`. Writes must use the storage version.

:::note[Possible future extension: conversion webhooks]
A Kubernetes-style conversion webhook could be added later: the store would call out to the owning controller on every read/list whose URL version differs from the row's stored version, and the controller would return a transformed `spec`. This would let clients pin to an older `apiVersion` indefinitely without the controller rewriting every row. Until that exists, the controller-driven migration steps below are the only supported path between versions.
:::

A controller that introduces a new version is therefore responsible for migration:

1. Add the new version to the `ResourceDefinition` with `served: true`. Keep the old version `served: true` so existing clients can read it; keep the old version's row data intact.
2. Promote one version's `storage: true` only after all rows have been rewritten at that version. The store keeps rows at every declared version of the kind addressable via the read paths.
3. List all rows at the old version (the store's `list_versions` accepts a list of api_versions), rewrite each via PUT at the new storage version. Each rewritten row carries the new `spec` shape.
4. Once no rows remain at the old version, mark the old version `served: false`. The collection's `declared_api_versions` continues to include it so any orphaned rows remain visible to the controller.
5. Remove the old version from the `ResourceDefinition` once it is neither `served` nor `storage`. Rise treats the migration complete at that point and may enforce the new schema strictly.

Rise rejects writes targeting a version that is not declared, and reads to URL-versions that are not `served`. A row stored at a declared-but-unserved version is still findable by the owning controller via the store's `list_versions` API.

## ResourceDefinition deletion

A `ResourceDefinition` cannot be deleted while any resources of its kind exist in any declared version. The store enforces this; the delete returns `409` if instances remain.

If a `ResourceDefinition` is deleted by mistake while rows exist (e.g. due to a controller bug), affected rows are not destroyed — they become unreachable by name (the path classifier can no longer derive the leaf kind's parent chain). They are still reachable by `uid:` for status/finalizer recovery; the supported repair is to re-register the `ResourceDefinition` with the same identity.

## Internal controllers

In-process controllers (those running in the same backend process as the resource store, like the Kubernetes deployment controller eventually will) should call the resource store trait directly rather than making HTTP requests back into the local backend. The store's `update_controller_status` / `update_controller_finalizers` enforce the same ownership invariants regardless of call path; an internal caller still passes an explicit `controller_id`.

The HTTP middleware can be bypassed for in-process callers, but the store-level invariants (ownership, reserved-prefix, kind-matching on UID paths, version validity) cannot.
