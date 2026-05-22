# Resource API Versioning — Notes

**Status: parked.** This captures where the resource-store versioning design
stands and the open questions, so the work can be resumed later. The
"orphans cannot exist by design" change is being implemented first.

## Current behaviour

The generic resource store supports versioned collections:

- A `ResourceDefinition` declares one or more versions; each can be `served`,
  and exactly one is the `storage` version.
- Every `resources` row stores its own `api_version` (`group/version`) — the
  version it was last *written* at.
- **Reads** (`get`, `list`) span *all declared versions* of a `group`+`kind`
  — including a non-served storage version, so a row is never orphaned by
  the version it happens to be stored at. The response `apiVersion` is
  projected to the version the caller requested.
- **Writes** (`create`, `update`) persist at the current `storage` version. A
  row therefore migrates lazily: it keeps its old `api_version` until its next
  `update`, which rewrites it to the storage version (the `COALESCE` in the
  update SQL).
- The `{version}` in a request is a **view selector**, not a storage filter:
  asking for `v2` returns the same objects as `v1`, just relabelled. It must be
  a *served* version.

## The limitation

Conversion is **no-op only** (`None`-strategy, in Kubernetes terms): the API
relabels `apiVersion` but never transforms `spec`. So multi-version is only
*correct* when the versions are structurally identical — two genuinely
different schemas served side by side would hand back mislabelled data.

## Why version without a schema change

`None`-strategy versioning still has real uses:

- **API maturity signalling** — `v1alpha1 -> v1beta1 -> v1` is a stability
  contract escalation, independent of schema.
- **Deprecation windows** — serve the old and new version together for a
  migration period, then drop the old one.

These — not schema evolution — are the only things our versioning supports today.

## How Kubernetes does conversion (reference)

- Objects are persisted at one **storage version**. The apiserver converts
  **on the fly** for every read (storage->requested) and write
  (requested->storage). Stored bytes are not rewritten by a read.
- Changing the storage version does **not** rewrite existing rows; they stay at
  their old version and convert on read. `CustomResourceDefinition.status.storedVersions`
  tracks which versions still have objects persisted.
- **Storage migration** is a separate, deliberate bulk operation: rewrite every
  object to the new storage version (a manual read-write loop, or the
  storage-version-migrator). Required before an old version can be retired.
- **Mechanism** (CRDs): a **conversion webhook** — the apiserver calls an HTTP
  endpoint the type owner provides (`strategy: None` vs `Webhook`). Both the
  on-the-fly path and storage migration go through the same webhook.
- **Code organization**: hub-and-spoke — one hub version (usually storage);
  every other version implements only `ConvertTo(hub)` / `ConvertFrom(hub)`, so
  N versions need 2N converters, not N^2. Built-in k8s types use a never-served
  internal version as the hub.
- **Gotcha**: down-conversion is lossy. A v1 client reading a v2-stored object
  (dropping v2-only fields), editing it, and writing it back loses those fields.
  Conversions must be lossless or stash the remainder in annotations.

## What we would need for real maturation

- We already have the lazy half — a row's `api_version` jumps to the storage
  version on its next `update`.
- Missing: (1) actual conversion logic — a conversion webhook fits well, since
  we already have external `ControllerIdentity` endpoints; (2) tracking of which
  versions still have rows stored (a `storedVersions` equivalent), so we know
  when an old version is safe to stop serving.

## Open decisions

- Is multi-version worth its machinery (served/storage, `{version}` in the
  route, projection, `list_versions`) given it currently only relabels a frozen
  schema? Current lean: **keep it** — maturing an API's shape is considered
  likely.
- If kept: build conversion via webhook, or via in-process conversion functions?
