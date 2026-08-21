---
title: "Upgrade Notes & Breaking Changes"
---

This page is the canonical, per-release reference for changes that affect
operators running a Rise installation: breaking changes, required actions, and
new or changed configuration. Read the section for the version you are upgrading
**to** before upgrading.

:::note
This page is fed from the [**Rise Rollout Tracker**](https://github.com/orgs/rise-deploy/projects/1)
GitHub Project. Any tracked item whose `Operator impact` is not `None` must have a
matching entry here for its `Target release` before it is marked `Done`. The
project's "Operator impact" view is the worklist; this page is what operators read.
:::

## Impact legend

| Badge | Meaning |
|---|---|
| **Breaking** | Requires action or behavior changes incompatibly; upgrading without reading may break installs. |
| **Action required** | Upgrade succeeds, but an operator action is needed to get correct/expected behavior. |
| **Config change** | New or changed configuration is available; defaults preserve existing behavior. |

---

## Unreleased

_Changes merged to `develop` but not yet in a tagged release. Moved into a
version section at tag time._

Merged to `develop`:

- **Action required if you granted generic resource API access by adding people
  to `auth.operator_users` — the API is now authorized, not operator-gated.**
  `/api/v1/resources` no longer refuses everyone but operators. Every request is
  authorized by the ADR-0001 engine against the resource it names, and
  authorization-changing writes additionally pass the write-time grant gate
  inside a `SERIALIZABLE` transaction.

  **Operators are unaffected**: an operator expands to `system:operators`, whose
  seeded `system-admin` binding allows every verb on every kind and subresource,
  and an operator's request ignores every `Deny`. Nothing an operator could do
  before is refused now.

  What changed for everyone else is that they are *evaluated* rather than
  refused outright. Shipped policy grants a non-operator nothing, so an install
  that has authored no bindings sees no access change — but if you were relying
  on "non-operator ⇒ 403" as the whole authorization story, that is no longer
  what the code says. Who reaches the evaluation at all is unchanged: the
  resource routes still sit behind the platform-access middleware, so
  `auth.platform_access.policy: restrictive` keeps its allowlist in front of
  everything below. Under the default `allow_all` that is every authenticated
  user. Two visible differences even without any binding:

  - A collection listing by a caller with no `list` grant returns an **empty
    `200`**, not a `403`. This is deliberate existence masking: a `403` would
    confirm the scope is populated. Naming one resource exactly is masked the
    same way: a caller with no `get` on it receives a **`404`**, on every verb.
    A caller who *can* read the resource still gets a `403` naming the verb they
    are short.
  - Which *collections* exist is now visible to any authenticated caller, since
    the path is classified before any per-resource decision. What a collection
    contains is authorized per item.

  Behaviour changes on the request/response surface:

  - `metadata.effectiveLabels` is present on every resource response, resolved
    live from the ancestor chain (nearest value wins per key).
  - A write returns the object at the granularity the caller may *read*. A
    caller holding `(update, Kind, status)` but not `get` gets the projected
    metadata shape back, not the `spec` — a write verb is not a read grant.
  - A main-resource `PUT` may no longer change `metadata.finalizers`; carrying
    the stored list back unchanged is fine, and changing them is the
    `finalizers` subresource's separate grant. A create carrying a reserved
    `system.rise.dev/*` finalizer is rejected.
  - Changing a `ResourceDefinition`'s `allowedStatusControllerIds` now requires
    operator standing: it grants controllers status and finalizer writes
    outside the authorization model, so it is not something an ordinary
    `update` on the definition should confer.
  - A listing under an ancestor that does not exist returns an empty collection
    rather than `404`, so the ancestor path is not enumerable by name. Item
    paths and creates under a missing ancestor still fail.
  - Changing the `metadata.ownerReferences` set is now authorized. Attaching or
    removing a reference requires `use` on the owner; attaching additionally
    requires `delete` on the dependent when it already exists, and setting
    `blockOwnerDeletion: true` requires `delete` on the owner, since that flag
    holds the owner's own deletion open. Removing a reference whose owner is
    already gone or draining is ungated, so a dependent is never frozen behind
    an owner nobody can revive. Re-sending already-stored references is
    unaffected. Note that the shipped `resource-owner` role does **not** include
    `use`, so owning a resource does not by itself let you make it the owner of
    another — author a binding that grants `use` where you need that.
  - `deletion-blockers` filters its blockers per item on `get` and reports a
    `hiddenBlockers` count for those the caller cannot read. That count is a
    deliberate disclosure: it moves as resources the caller cannot see are
    created and deleted, so grant the subresource on that basis.
  - A refused authorization-changing write says less than it did. The recipient
    and the domain are named only when they came from the request; what the
    recipient would have gained is never named, because the gate compares their
    whole effective policy over the domain and a witness can come from any
    binding delivering policy to them. The full comparison is in the
    `rise::audit` `resource.grant_gate` record.
  - Every `404` from these routes now carries the same body,
    `{"error": "resource not found"}`. Clients that matched on the old wording
    ("resource 'x' not found", "parent path segment not found") need updating.
    Authorization also runs before the request body is inspected, so a `PUT`
    with a malformed body against a resource the caller cannot read returns
    `404` rather than `400`.
  - An item a caller can `list` but not `get` is returned projected onto
    `apiVersion`, `kind`, and the `metadata` fields `name`, `labels`,
    `effectiveLabels`, and `deletionTimestamp` — no `spec`, `status`, `uid`,
    `revision`, or `discriminator`. Operators always hold `get`, so their list
    responses are unchanged apart from `effectiveLabels`.
  - A write that loses a serialization race after three attempts returns `503`
    with a retryable message instead of committing on stale facts.

  **A restriction must name a subject that resolves today.** Group ties are read
  from `User` and `Group` resources, which no login path writes yet, so every
  principal currently has an empty tie set. A group-targeted binding therefore
  grants nothing — harmless — but a *cap* expressed as a group-targeted `Deny`
  is equally never collected, so it does not restrict anyone either. Until
  identity resolution lands, express a restriction against `system:authenticated`,
  `org:<name>`, or the principal itself.

  Two audit record names changed: `resource.operator_status_updated` and
  `resource.operator_finalizers_updated` are now `resource.user_status_updated`
  and `resource.user_finalizers_updated`, because the caller need not be an
  operator. Two records are new: `resource.access_denied` and
  `resource.grant_gate`. Update any log-based alerting that matches the old
  names.

  No migration and no backfill runs, and no configuration changes.
  `auth.operator_users` and `auth.operator_idp_groups` keep their meaning.

- **Action required if you author org `RoleBinding`s — subject bounded to its own
  Organization**. An org `RoleBinding` whose `subject` names a *different*
  organization — `group:<other>/x`, `serviceaccount:<other>/x`, `org:<other>` —
  is now refused at write time. Such a binding never granted anything: ADR-0001
  §1's recipient boundary already required the subject to belong to the binding's
  own Organization, so the row read as a cross-org grant while being permanently
  dead. Only the generic resource API is affected, and only for callers who may
  write policy resources there.

  Existing rows keep being readable and keep granting exactly what they granted
  before (nothing), but **an update to such a row now fails**. To find them:

  ```sql
  SELECT parent.name AS organization, r.name, r.spec->>'subject' AS subject
  FROM resource_store.resources r
  JOIN resource_store.resources parent ON parent.uid = r.parent_uid
  WHERE r.kind = 'RoleBinding'
    AND r.deletion_timestamp IS NULL
    AND r.spec->>'subject' ~ '^((group|serviceaccount):[a-z0-9-]+/[a-z0-9-]+|org:[a-z0-9-]+)$'
    AND CASE
          WHEN r.spec->>'subject' LIKE 'org:%'
            THEN split_part(r.spec->>'subject', ':', 2)
          ELSE split_part(split_part(r.spec->>'subject', ':', 2), '/', 1)
        END <> parent.name;
  ```

  Delete what it returns, or re-point each subject at a Group in the binding's
  own Organization. `user:` and `system:authenticated` subjects are unaffected —
  their affiliation is a live membership question, not a property of the
  identifier — as are `controller:` subjects.

  The same field now also accepts the relative form `group:<name>`, expanded
  against the parent Organization before storage, so `group:platform` under
  `acme` stores `group:acme/platform`. Absolute subjects are unchanged, and
  `PlatformRoleBinding` still takes absolute subjects only.

- **No action required — seeded baseline authorization policy**. Startup now
  creates five root policy resources described by ADR-0001:
  `PlatformRole/system-admin` with its `system:operators` binding, and the
  editable `PlatformRole/org-admin`, `PlatformRole/resource-owner`, and
  `PlatformRoleBinding/resource-owner` defaults. Seeding is idempotent and never
  overwrites the three editable rows, so an operator edit survives every restart
  and a deleted one is re-created on the next.

  These are the whole of the shipped policy: with the choke point live (above),
  an install that authors no bindings of its own grants nothing to anyone but
  operators and the subjects a `rise.dev/owner` label names.

  Two of the five are immutable through the API: the
  resource store refuses to update or delete `PlatformRole/system-admin` or its
  binding, because they are the inspectable record of operator authority rather
  than its source (the evaluator hardcodes that, so it survives a bad restore).
  If startup ever reports one of those two as diverging from its shipped
  definition, something wrote to `resource_store.resources` directly; the error
  names the row and the fix is to delete it and restart. No migration and no
  backfill runs.

- **No action required — generic resource labels**. Resources in the generic
  resource API carry `metadata.labels` alongside `metadata.annotations`. The
  migration adds a column with an empty default, so existing rows and clients
  are unaffected and no backfill runs. Label keys use the Kubernetes-shaped
  grammar that policy `labelSelector` keys already use; values are capped at 63
  bytes. A key becomes access-relevant only once a policy binding selects on it,
  and writing such a key then passes the write-time grant gate.
- **Action required if conflicts exist — identity resource activation** ([#421](https://github.com/rise-deploy/rise/pull/421)).
  Rise now activates the eight reserved `rise.dev/v1alpha1` identity resource
  kinds in the PostgreSQL resource store. Before upgrading, remove any legacy
  ResourceDefinitions that claim those reserved group/kind or collection
  identities, and migrate or remove any stored identity rows whose structural
  parents do not match the built-in hierarchy. Startup fails closed when such
  conflicts exist and reports the total plus a bounded sample; use the previous
  Rise version to remove the conflicting definitions and rows, then recreate
  custom resources under a non-reserved identity if needed. Installations with
  no reported conflicts require no action.

  Worth knowing before you start: the reservation is wider than the eight
  identity kinds. The whole `rise.dev` API group is now closed to external
  ResourceDefinitions, as are the eight identity collection names in *any*
  group (collection names have always been globally unique). The activation
  runs in one transaction, so an upgrade rejected by the audit leaves the
  database exactly as it was — clean up the conflicts it names under the
  previous Rise version and retry.
- **Config change — admin and Operator roles by IdP group** ([#429](https://github.com/rise-deploy/rise/pull/429)).
  `auth.admin_idp_groups` and `auth.operator_idp_groups` grant the admin and
  Operator roles to everyone in the listed IdP groups, so the IdP stays the source
  of truth instead of an email allowlist that has to be edited and redeployed.
  Both default to empty, so
  installs that grant roles by email alone are unaffected and pay no extra query.
  A user holds a role if their email is on the allowlist **or** they are in one of
  the groups; group names match case-insensitively.

  All group matching — including the existing `auth.platform_access.allowed_idp_groups`
  — now resolves against the **IdP-managed** teams Rise syncs from the configured
  group claim (`auth.idp_group_claim`, default `groups`), rather than against
  every team the user belongs to. **Action
  required only if** you granted platform access through `allowed_idp_groups`
  naming a team that Rise did not create from the IdP (i.e. `idp_managed = false`);
  those users lose platform access until the group comes from the IdP. This closes
  a privilege-escalation path where a user could create a team named after an
  allowed group and grant themselves access. Group membership refreshes at login,
  so revoking a group in the IdP takes effect on the user's next login (or the next
  Entra active sync).
- **Config change — custom IdP group claim**. `auth.idp_group_claim` selects the
  ID-token claim Rise uses for IdP-managed team synchronization. It defaults to
  `groups`, preserving existing behavior. AWS Cognito operators can set it to
  `cognito:groups` (or set `OIDC_GROUP_CLAIM=cognito:groups` in the standalone
  Docker deployment).
- **Action required if conflicts exist — policy resource activation** ([#430](https://github.com/rise-deploy/rise/pull/430)).
  Rise activates the four reserved `rise.dev/v1alpha1` policy resource kinds —
  `Role` and `RoleBinding` under an Organization, `PlatformRole` and
  `PlatformRoleBinding` at the root — in the PostgreSQL resource store. The
  same fail-closed pattern as the identity activation above applies: before
  upgrading, remove any stored rows in the `rise.dev` group using those four
  Kind names, and any ResourceDefinition claiming one of the four collection
  names (`roles`, `rolebindings`, `platformroles`, `platformrolebindings`) in
  any group. Startup reports the total plus a bounded sample and leaves the
  database unchanged, so clean up under the previous Rise version and retry.
  Installations with no reported conflicts require no action.

  These resources are what the choke point (above) evaluates: writing a
  `RoleBinding` now grants access. Bindings are validated at write time, so creating one requires its `roleRef` target, its
  `scope` target, and any literal `subject` it names to already exist — create
  the Role before the RoleBinding that references it.

- **Behavior change — workload identity on the Docker backend** ([#382](https://github.com/rise-deploy/rise/pull/382),
  closing [#378](https://github.com/rise-deploy/rise/issues/378)).
  The Docker controller now delivers the same workload-identity material as
  Kubernetes — the bootstrap credential and one token file per `[identity].audiences`
  entry — to `/var/run/secrets/rise/identity/` inside each app container (via the
  Docker archive API), and refreshes the token files before they expire. No new
  configuration; this closes a parity gap, so a Docker app that sets
  `[identity].audiences` now receives its tokens instead of nothing. Identity
  files are delivered when a container is created, and the controller also
  self-heals already-running containers that lack them on the next reconcile, so
  apps running before the upgrade pick up their identity material without a
  redeploy (mirroring the Kubernetes controller re-establishing it on each sync).
- **Behavior change — workload identity token refresh on Kubernetes** ([#390](https://github.com/rise-deploy/rise/pull/390)).
  The Kubernetes controller now runs a leader-elected loop that re-mints each
  deployment's pre-minted identity token files before they expire. The sync
  webhook records a per-deployment due time (~2/3 of
  `deployment_controller.identity_token_ttl_seconds` after each mint); the loop
  resyncs a `RiseProject` only when one of its deployments is due. Metacontroller
  does not resync a steady project on its own, so previously a long-lived pod's
  identity *file* token could expire without being refreshed (the on-demand
  token-exchange endpoint was unaffected). No new configuration and no action
  required; the only operational change is a `rise.dev/trigger` annotation write
  per *due* deployment (so projects are touched only when a refresh is needed, and
  the work is naturally staggered) and one more background lease
  (`rise-identity-refresh`). Docker already refreshed via its own reconcile loop,
  so this closes the gap on Kubernetes. The per-project re-mint due time is
  tracked on the `RiseProject` CR's `status.identityRefreshDueAt` (written by the
  sync webhook), so there is no deployments-table schema change.
- **Action required — raw external token deprecation signal** ([#394](https://github.com/rise-deploy/rise/pull/394)).
  While `auth.allow_raw_external_tokens` is `true`, each *accepted* raw-token
  request now emits one metric-shaped `tracing` event
  (`target=rise::deprecation`, `metric=raw_external_token`) carrying the
  validated `issuer`/`sub`. Aggregate it in your log pipeline (count, group by
  `issuer`/`sub`) to find which CI workload identities still present raw external
  tokens: the default flips to `false` in **0.25.0**, after which those callers
  must pre-exchange at `POST /api/v1/auth/token`. No config change; migrate CI
  before upgrading to 0.25.0.
- **Config change — auth token exchange (phase 1)** ([#367](https://github.com/rise-deploy/rise/pull/367)).
  Adds the RFC 8693 exchange endpoint and a Rise `Access` token kind. Purely
  additive; existing token flows are unchanged, legacy in-handler verification
  remains the fallback. See
  [`ROADMAP.md`](https://github.com/rise-deploy/rise/blob/develop/ROADMAP.md)
  § "Workstream 2 — Authentication & Token Exchange".
- **Config change — Docker deployment backend** ([#358](https://github.com/rise-deploy/rise/pull/358)).
  Selectable via `deployment_controller.type = "docker"`. Single-host; Kubernetes
  remains the default, so existing installs are unaffected unless they opt in.
  A new deployment rolls over via Traefik health checks, with old and new
  overlapping in one load-balanced service (a rolling update, vs. Kubernetes'
  atomic blue/green). Probing is **opt-in**: no `health_check` means
  ready-when-running; a set `health_check` is a **2xx–3xx** check.
  Operator-relevant settings on the Docker controller (env-driven; the shipped
  standalone compose sets working defaults):
  - `traefik_api_url` (default in-network `http://rise-traefik:8080`) — the rolling
    gate reads Traefik's `serverStatus`, the **authoritative** readiness signal for
    health-checked containers (no fallback). The standalone Traefik enables its API
    internally (`--api.insecure=true`, port **not** published). If you run your own
    Traefik and any project uses a `health_check`, you **must** expose its API to
    the backend over the internal network (optionally with basic-auth embedded in
    the URL); without it a health-checked deployment never becomes Healthy. It may
    be left unset only when no project uses health checks.
  - Replicas: the Docker config raises `deployment_constraints.max_replicas` to 10
    (`RISE_MAX_REPLICAS`); the controller additionally hard-caps at 50.
  The deployment-backend feature matrix, the Docker operator pages, and the
  cutover/health-check docs ship with this PR.
- **Action required — reserved `RISE_` env-var prefix** ([#355](https://github.com/rise-deploy/rise/pull/355)).
  User-supplied environment variable keys beginning with `RISE_` are rejected at
  the API and at deploy time (project env vars and per-container
  `[containers.X.env]`). If any of your users' apps set `RISE_*` keys, rename them
  before upgrading.

### Known release-candidate limitations

- **Docker drift-driven recreation can briefly interrupt a single replica.** A
  normal deployment cutover uses the documented health-driven rolling overlap,
  but replacing an already-running container whose runtime configuration drifted
  still removes the old container before creating its replacement. Multi-replica
  workloads recreate one healthy replica at a time; a single-replica workload
  has a short gap. Tracked in [#379](https://github.com/rise-deploy/rise/issues/379).
- **Docker HA status writes have a bounded stale-leader race.** Destructive Docker
  operations re-check leadership immediately before acting. The slower health
  reconciliation pass can, however, finish a status-only database write after
  its previously confirmed lease window has elapsed. A current leader corrects
  that status on a later tick; no stale leader performs a container create or
  removal through this path. Tracked in
  [#401](https://github.com/rise-deploy/rise/issues/401).
- **This release has no earlier Docker-enabled upgrade baseline.** The upgrade
  harness already supports the Docker backend, but every release before this
  one predates the standalone Docker stack. CI therefore runs the in-place
  upgrade suite on Kubernetes for this release. Publishing this version
  establishes the Docker baseline; enable the Docker upgrade job for the next
  release cycle so subsequent versions exercise image and database migrations
  from it.

---

## 0.23.0

First release of the generic resource substrate (compatibility phase). None of
these change behavior for existing installs by default; the items below are the
configuration knobs they introduce.

- **Config change — Operator role (`auth.operator_users`).** The generic resource
  API (`/api/v1/resources`) is gated to a new, separately configured Operator
  role. `auth.admin_users` do **not** receive Operator access. No action needed
  unless you want operators to manage generic resources. See
  [`ROADMAP.md`](https://github.com/rise-deploy/rise/blob/develop/ROADMAP.md).
- **Config change — default Organization / Kubernetes `controller_class_name`.**
  Backend startup bootstraps a single default Organization and backfills existing
  users, teams, and projects to it under an advisory lock. Existing installs
  resolve to the **same** namespace names as before (`rise-` prefix → `rise-myapp`).
  The Kubernetes controller's `controller_class_name` defaults to a stable value
  for existing installs if unset.

### Watch for later (not yet released)

These are tracked as finalization gates and will land in a **future** release —
listed here so operators can anticipate them:

- **Breaking (future) — multi-tenancy phase 2.** Tightening
  `organization_resource_uid` to `NOT NULL` after backfill, and migrating typed
  tables onto the generic resource model. Tracked in
  [#372](https://github.com/rise-deploy/rise/issues/372).
- **Breaking (0.25.0, behind operator toggle) — removal of the legacy auth path.**
  `auth.allow_raw_external_tokens` defaults to `false` starting in **0.25.0**,
  and auth token-exchange phase 3 removes the legacy in-handler verification
  path. The `rise::deprecation` raw-external-token metric (above) tells you when
  raw-token traffic has drained and it is safe to upgrade. Tracked in
  [#374](https://github.com/rise-deploy/rise/issues/374).
