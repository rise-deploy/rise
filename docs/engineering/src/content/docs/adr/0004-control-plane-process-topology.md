---
title: "ADR-0004: Control-Plane Process Topology"
---

## Status

**Proposed** (under review). Date: 2026-08-21.

ADR-0001 fixes the authorization model and the crate structure that realizes
it; ADR-0002 fixes the subresource execution seam. This decision changes
ADR-0001 not at all and ADR-0002 in one deliberate respect (§4). It decides
*which process* each of them runs in, what protocol the components between them
speak, and in what order the split happens.

## Context

Every month the in-process path is the only path is a month of code written
against storage semantics a network boundary cannot preserve. That is the
motivating concern; multi-org isolation (`ROADMAP.md` §5), third-party
controllers, and clear credential ownership are the reasons it is worth paying
to fix.

### Today's topology

One binary, one process, one Deployment.

```toml
# Cargo.toml
[[bin]]
name = "rise"
path = "src/main.rs"
```

```yaml
# helm/rise/templates/deployment.yaml
# Single server container with all controllers running in the same process
- name: server
  args: ["backend", "server"]
```

`rise backend server` hosts, in one `tokio` runtime sharing one `PgPool`: the
whole HTTP API including `/api/v1/resources`; the project, ECR, resource-GC,
workload-identity-refresh and Entra-sync loops, each `tokio::spawn`ed in
`src/server/mod.rs` under a `rise-runtime-sync` leader election; the Docker
reconciler (`src/server/state.rs`) under the same; and the Metacontroller
sync/finalize webhook (`src/server/deployment/webhook.rs`).

One detail matters to the ordering below: the Kubernetes reconcile *loop* is
already out of process — Metacontroller runs as its own Deployment and calls
Rise's webhook. What lives in `rise backend server` is the decision function,
and it reads typed tables directly through `state.deployment_store`, not
through any resource API.

### What the boundary already gets right

Schema ownership is real, not aspirational:

- `resource_store` belongs to `rise-resource-store-postgres`, which owns its
  migrations and SQLX offline cache. `runtime_sync` belongs to
  `rise-runtime-sync` on the same terms.
- No `rise-deploy` production query reaches into `resource_store`; `grep -rn
  resource_store src/db/*.rs` is empty. One test fixture writes it directly
  (`src/server/resources/gc.rs:741`, under `#[cfg(test)]`).
- The linkage migration explicitly refuses a foreign key across the line:

  ```sql
  -- migrations/20260526000002_add_organization_linkage.sql
  -- The columns intentionally do NOT declare a FOREIGN KEY to
  -- `resource_store.resources(uid)`: that schema is owned by the
  -- rise-resource-store crate's migrations […]
  -- Treat these columns as soft references to `resource_store.resources(uid)`.
  ```

- The store contract is dependency-light and lives in `rise-resource-api`, not
  in the server crate.

So the failure mode usually feared — a second component quietly growing SQL
against someone else's tables — has not happened.

### What it does not get right

**1. `ResourceStore` is a storage contract, not an API contract.** Four methods
take the acting identity as a string parameter
(`crates/rise-resource-api/src/store.rs:293-325`):

```rust
async fn update_controller_status(&self, uid: Uuid, controller_id: &str, …);
async fn update_controller_finalizers(&self, uid: Uuid, controller_id: &str, …);
async fn operator_update_status(&self, uid: Uuid, operator: &str, …);
async fn operator_update_finalizers(&self, uid: Uuid, operator: &str, …);
```

The trust boundary above them is already correct — handlers pass
`controller.0.identity_id` from the authenticated Controller
(`src/server/resources/handlers.rs:1820`) and `authz.subject()` from the
authorization choke point (`:1879`). The defect is expressive, not exploitable: the
trait cannot say where identity comes from, so it cannot be the contract a
remote client codes against. Others — `try_collect`, `list_pending_collection`,
`resolve_path`, `ancestors`, `resolve_collection*`,
`register_resource_definition` — are not API operations at all, but the
internals of serving one.

**2. Every in-tree consumer takes the whole surface.** One
`Arc<dyn ResourceStore>` (`src/server/state.rs:75`) goes to the resource
handlers, the GC worker, the deployment webhook, the organization helpers,
bootstrap, and the Docker reconciler (`state.rs:1215` →
`crates/rise-backend-docker/src/reconciler.rs:186`) alike — that last one a
*deployment controller*, the category §1 places outside the boundary, already
holding a direct handle. Nothing distinguishes a caller that could be remote
from one that could not. The field's own documentation names the second
protocol in advance:

```rust
// src/server/state.rs
/// Generic-resource store used by the `/api/v1/resources` HTTP API and
/// (later) by internal controllers wanting to reconcile against Rise state
/// without a network round-trip.
pub resource_store: Arc<dyn rise_resource_api::ResourceStore>,
```

A privileged in-process path, reserved for components that are also expected to
become external controllers. Whatever it is used for must be re-derivable from
the API, or those components can never leave the process.

**3. There is live cross-boundary state.** Typed rows carry
`organization_resource_uid` soft references, and the guard protecting them
documents its own race:

```rust
// src/server/resources/organization.rs
// TODO(multi-org): the count and the delete run in separate statements
// with no surrounding transaction […] serialize delete vs. typed insert by
// taking `pg_advisory_xact_lock` keyed on the Org UID in this function
// *and* in every […] call site.
```

That costs one advisory lock while both sides share a pool. §6 says what
replaces it when they do not.

**Half-ready.** The authorization side has landed: `rise-authz` is wired into
`rise-deploy` behind `src/server/authz/`, `require_operator` is gone in favour
of a centralized choke point, and writes replay under `SERIALIZABLE` with
`StoreError::Serialization` driving bounded retry. So there is now something
real to draw a boundary around. What is still open is reach: pagination,
selectors, Watch, Patch and discovery remain `ROADMAP.md` §1 items, so a
client-only backend today would poll unfiltered lists. §7 keeps both as gates,
G1 satisfied and G2 not.

## Decision

### 1. The target is an apiserver, not a database service

A dedicated control-plane process — `rise-apiserver` — owns the generic
resource API end to end:

| Inside the apiserver | Outside |
|---|---|
| Resource store and its Postgres schema | Typed product API — shrinking (below) |
| Kind registry and `ResourceDefinition` projection | Deployment controllers (Kubernetes, Docker) |
| Normalization and admission | Extension controllers (RDS, S3, Snowflake OAuth) |
| Authorization (`rise-authz` engine, ADR-0001) | CLI |
| Subresource execution and forwarding (ADR-0002, §4) | Web UI |
| Garbage collection · Watch fan-out | |

The apiserver is the **sole holder of credentials for `resource_store`** and its
only writer. Everything on the right is a client with a Rise-issued identity,
addressing resources by path.

This is deliberately the kube-apiserver shape: admission and authorization sit
*inside* the process that owns storage, because both must read and write in one
transaction. Rise's storage engine is PostgreSQL rather than etcd, which buys
real transactions and costs nothing here.

**The typed product API is a shrinking shim, not a fixture.** As `ROADMAP.md`
§4 lands each kind, the typed routes stop owning storage and become a
translation layer over the resource API; that section's closing item already
drops typed tables once "compatibility reads are no longer needed". The end
state is a shim serving only clients that do not yet speak the resource API,
and quite possibly no typed surface at all. Three things follow:

- **The right-hand column is not a peer architecture.** Controllers are
  permanent; the typed API is a compatibility artifact with a planned end.
  Do not add a typed route for anything the resource API can already express.
- **The shim holds no storage and no credentials**, so where it runs is a
  deployment question, not an architectural one. It stays in `rise` because the
  CLI is already there.
- **Several problems below are transitional and should be solved
  transitionally.** The Organization lock and the bootstrap split (§6) exist
  *because* typed tables exist. They need a mechanism that carries the migration
  and is then deleted with the tables it served, not permanent architecture.

### 2. The API contract is the only protocol clients speak

These rules govern the **inbound** direction. The apiserver's own outbound call
to a Controller when forwarding a subresource is a second contract, governed by
§4 — the only sanctioned exception.

Normative, and enforceable long before any process splits:

1. No component other than the apiserver opens a connection to `resource_store`.
2. No SQL join, view, or foreign key crosses the boundary. Soft UID references
   are permitted during the `ROADMAP.md` §4 migration and carry no referential
   guarantee.
3. **No transaction spans the boundary.** Anything needing atomicity with a
   resource write is implemented inside the apiserver — as admission, as a
   subresource, or as a store operation — never as a caller-side transaction.
4. Every operation a client performs is expressible as one request against one
   resource path, with the acting identity taken from its credential.
5. The only cross-boundary consistency primitives are resource-level ones:
   `resourceVersion` compare-and-swap, finalizers, owner references, watch.
   Advisory locks, `SELECT … FOR UPDATE`, and `rise-runtime-sync` leases are
   legitimate *within* one component and illegitimate *between* two.

Rule 5 is the one that bites: `rise-runtime-sync` coordinates replicas of one
component, and using it across the line reintroduces exactly the shared-database
coupling this decision removes. §6 states how far that is actually enforceable.

### 3. Split `ResourceStore` into an API surface and a store-internal surface

Before any process moves, divide today's trait by whether an operation could
ever be issued by a remote client.

**API surface** (`ResourceApi` — remotable, one HTTP request each):

| Method | Becomes |
|---|---|
| `create`, `update`, `delete` | `POST` / `PUT` / `DELETE` on a resource path |
| `get`, `get_by_name`, `list`, `list_versions` | `GET` on a resource or collection path |
| `update_controller_status`, `update_controller_finalizers` | `status` / `finalizers` subresource writes, **identity from the token** |

**Store-internal surface** (`ResourceStore` — never remotable):

| Method | Why it stays in |
|---|---|
| `try_collect`, `list_pending_collection` | GC worker internals over tombstoned rows |
| `resolve_path`, `ancestors`, `label_inheriting_descendants` | Path, ancestry and label-subtree primitives feeding admission and ADR-0001 §6.6's write gate |
| `resolve_collection`, `resolve_collection_version`, `resolve_collection_by_kind` | Registry lookups behind every request |
| `register_resource_definition`, `update_resource_definition` | The atomic storage projection behind ordinary `ResourceDefinition` writes |
| `list_deletion_blockers` | Implementation of a served subresource (`handlers.rs:772`) — the route is remotable, the primitive is not |
| `operator_update_status`, `operator_update_finalizers` | Same shape: the route is served today (`handlers.rs:1879`, taking `authz.subject()`), and only the identity-bypassing primitive is internal. The route converges on `status`/`finalizers` under RBAC; the primitive dissolves |

The four `controller_id` / `operator` parameters disappear when ADR-0001's
choke point lands — `ROADMAP.md` §1 already commits to removing
`ResourceDefinition.allowedStatusControllerIds` and authorizing `status` and
`finalizers` through RBAC alone.

Only code that runs *inside* the apiserver may name `ResourceStore`: the
resource handlers, the GC worker, the authorization engine (which needs
`ancestors` for `effectiveLabels`), and the composition root that constructs
it. Everything else takes `ResourceApi`. That one rule makes the whole question
a compile-time check: a caller that cannot be expressed on the API surface is a
future distributed-systems bug, found today, for free — and applying it moved
exactly the callers this ADR predicts sit outside, the Docker reconciler and the
Metacontroller webhook's Organization view among them.

`ResourceApi` has two implementations, but only for the migration's duration: a
direct in-process one and an HTTP one in `rise-resource-client`. Because there
is no embedded mode (§8), the direct implementation is never a product topology
— it is how callers are held to the API surface while the apiserver is still
hosted in `rise backend server`, and it retires at G5. Contract tests run
against both while both exist.

### 4. Product subresources execute where their capability lives

The generic seam runs in the apiserver; the *capability* a product subresource
needs often does not. `deployment-logs` decides the rule: the runtime log
backend holds a live `kube::Client` or `bollard::Docker`
(`src/server/deployment/logs.rs:189-190`, `:253`, `:312`). Compiling it into the
apiserver would hand the control plane cluster and daemon credentials — the
merged blast radius this decision exists to separate — and require it to know
which cluster serves which Organization, which is controller state.

So the apiserver **authorizes and forwards**. ADR-0002 §2's pipeline is
unchanged — route, resolve the parent, evaluate RBAC, open the audit record —
and only the leaf differs: instead of invoking a local handler, a platform-known
forwarding strategy streams from the Controller that owns the resource. One path
grammar, one authorization pipeline, no runtime credential in the control plane.

**This amends ADR-0002, and amends a prohibition rather than exercising a
reservation.** That ADR rejects handlers naming "a URL, executable, dynamic
library, or arbitrary Rust type" with no escape clause; its "unless a later ADR
deliberately opens that boundary" governs *which kinds* a product handler may be
registered for, not whether one may terminate remotely. So this is a change to
ADR-0002, not a door it left open — though its §3 shape table already
contemplates a constrained reverse-proxy exchange. The prohibition's *reason*
survives intact: arbitrary operator-defined remote handlers stay rejected. The
strategy is platform-known and compiled in, the upstream is a registered
`Controller`, and no `ResourceDefinition` may name an endpoint.

**The forwarding leg needs its own trust model**, because authorizing a request
is not the same as trusting the hop:

- *The endpoint is operator-set, never controller-set.* A Controller's address
  lives in its `spec`, written by an operator — never in `status`, which the
  controller writes itself. Otherwise a stolen controller credential steers
  where the apiserver opens connections and the control plane becomes an open
  proxy; "registered Controller rather than operator-supplied URL" is an empty
  distinction if the controller registers its own URL. HTTPS is required and
  redirects are not followed.
- *The apiserver forwards its own identity, never the client's.* A user's bearer
  token must not reach a controller. The apiserver presents a Rise-issued
  assertion audience-bound to that Controller, naming the target resource, the
  subresource, and — for the controller's audit trail, not its authorization —
  the acting principal; ADR-0001's `act` attribution already models the shape.
  The controller authorizes the apiserver, not the end user: a confused deputy
  by construction, contained by the audience binding and by naming one resource.
- *Controllers acquire a serving surface, and that is accepted.* One that serves
  subresources listens, terminates TLS, and has liveness the apiserver must
  treat as a failure distinct from an unauthorized request. That is the price of
  keeping runtime credentials out of the control plane.

**The forwarding contract is versioned separately, in the opposite direction.**
§8 covers clients calling the apiserver; here the apiserver is the client and a
Controller is the server, so a newer apiserver must keep working against a
controller one minor behind — the ordinary state mid-rollout and the permanent
state for a third-party controller on its own cadence. No legacy controllers
exist on this seam, so the contract can be defined cleanly from the first one
rather than retrofitted. CI covers this direction too: current apiserver against
the previous release's controller.

To design when the first one ships: streaming passthrough carrying cancellation
and backpressure across two hops, with ADR-0002 §6's connection, idle and
duration limits now applying at the apiserver rather than at the log source; a
two-phase audit record spanning processes, its completion half needing the
outcome, duration and byte count the forwarding leg observed; and endpoint
liveness as a distinct failure mode. A subresource needing no runtime credential
(the Loki log backend is an HTTP call to a log store) *could* execute locally —
two execution shapes for one subresource is not worth the divergence, so
forwarding is the rule.

`status`, `finalizers`, and `token` are unaffected: they need nothing beyond the
store and ADR-0001's signing keys, and execute in the apiserver.

### 5. Identity and transport

Clients authenticate with **Rise-issued tokens over TLS**. No second trust root:
principals are ADR-0001's `User`, `ServiceAccount`, and `Controller` identities
carrying `rise_uid`, and every decision is live RBAC intersected with the
token's `authorization_details` cap. mTLS is not part of the contract — a client
certificate models identity a second time, in a system that cannot express
Rise's scopes, delegation, or revocation. Two consequences follow: the apiserver
verifies TLS wherever it is reachable, so a plaintext listener is a
configuration error rather than a deployment option; and token lifetime becomes
an availability parameter, since a controller that cannot refresh stops
reconciling — making the platform-global maximum TTL (`ROADMAP.md` §2) a tuning
decision as well as a security one.

**Browsers are ordinary clients.** Once `ROADMAP.md` §4 completes, the web UI
addresses the apiserver directly. This is not a new identity kind — the UI
already holds a Rise-issued JWT session cookie, the same credential the
ingress-auth subrequest validates. It does move browser-facing concerns onto the
apiserver: CORS, cookie scope and `SameSite`, CSRF on non-idempotent verbs, and
per-item list filtering fast enough to be interactive. Until each kind migrates
the UI reads through the shim; the switch happens per surface, not as one
cutover. Every client makes the same move eventually — the UI is simply the one
whose migration we control end to end.

### 6. Availability and coordination

**N stateless replicas.** Every replica serves reads and writes; PostgreSQL
holds the consistency, so none is privileged for request handling. Only
background sweeps are singleton — the GC worker runs under a `rise-runtime-sync`
leader lease exactly as today.

**Each component owns its own lease schema.** `rise-runtime-sync` gains a schema
parameter: the apiserver migrates and holds credentials for its own lease
tables, the product backend keeps `runtime_sync`. Not free — the crate's queries
are compile-time `sqlx::query!` macros with `runtime_sync.` written into the SQL
(`crates/rise-runtime-sync/src/leader_leases.rs:377`) against a crate-local
offline cache, so a runtime parameter means dynamic queries or a `search_path`
scheme, and the crate already treats `search_path` mutation as hazardous enough
to sacrifice a connection over.

**This isolates leases, not locks.** `LeaderElection` and `GlobalSchedule` are
table-backed, so separate schemas separate them. `GlobalLock` is not: it hashes
a name to an `i64` and takes `pg_advisory_lock` over a database-wide keyspace
(`crates/rise-runtime-sync/src/global_lock.rs:47`), and the crate warns that
"collisions would silently serialize unrelated callers". For `GlobalLock`,
invariant 5 stays a convention; the honest mitigation is that it has no
cross-component use after the split, not that the schema split prevents one.

**The Organization delete race.** The count and the delete now share one
session (`organization.rs:40-42`), but that is not mutual exclusion against the
typed writers: PostgreSQL checks a serializable transaction's predicate reads
only against writers that are themselves serializable, and every typed link
write runs at `READ COMMITTED`, so an insert committing after the snapshot is
invisible to the count and aborts nothing. The file's remedy is
`pg_advisory_xact_lock` on the Org UID in the delete path *and* in every
`set_team_organization` / `set_project_organization` / `ensure_user_membership`
call site. Two transactions coordinating through a shared lock is not one
transaction spanning the boundary, so it breaks invariant 5, not 3 — and only
*half* of it is illegal, which is the whole fix. Invariant 5
forbids a lock shared *between* components and permits one *within* a component,
and both the count and every typed insert live in the product backend:

- **Cross-boundary half — a finalizer.** The backend registers a finalizer on
  Organization and clears it only when no typed row references that UID. The
  apiserver tombstones and waits rather than counting rows it cannot see.
- **Local half — the lock stays, inside the backend.** Every typed-insert path
  and the finalizer-clearing transaction take the same backend-local advisory
  lock keyed on the Org UID: what the TODO asks for, minus the participant that
  made it illegal.

That leaves one window: an insert whose liveness check passed before the
tombstone, committing after the finalizer cleared, producing a typed row naming
a deleted UID — in a column carrying no referential guarantee anyway
(invariant 2), for a rare admin action, in tables scheduled for deletion. **We
accept and detect that residue rather than closing it**, because closing it
needs the backend to hold authoritative Organization state of its own, and a
second copy of a resource the apiserver owns is the coupling this ADR removes.
Both mechanisms are G5 obligations and both are transitional: they guard typed
rows, so they are deleted with them (§1). Build them to be removable.

**Bootstrap splits rather than moves.** Its `GlobalLock` serializes
default-Organization creation, but the work it guards writes typed tables too
(`backfill_user_organization_memberships`, `backfill_teams_organization`,
`backfill_projects_organization` in `src/server/bootstrap.rs`), and moving it
inside would hand the apiserver typed-table credentials. Instead the apiserver
creates the Organization and the backend runs its own linkage pass afterwards —
converging rather than transacting. Convergence is right precisely because the
problem is temporary: the backfills vanish once those kinds are resource-backed.

### 7. The API proves itself before the process splits

Gates, in order. G1, G2 and G4 are independently valuable; G3 is a library with
no consumer until G4, listed separately only because it is the seam's first
real client.

- **G1 — Authorization inside the boundary.** *Landed.* `rise-authz` is wired
  into the request path, `require_operator` is replaced by the centralized
  choke point, and the write-time grant gate runs under `SERIALIZABLE` with
  bounded retry.
- **G2 — API completeness.** Pagination and selectors, Watch, Patch, discovery.
  Until these exist, "client-only" means "polls everything".
- **G3 — `rise-resource-client`** with Rise-issued credential providers, watch
  resume, and finalizer/subresource helpers.
- **G4 — One controller runs entirely on the API.** No typed-table reads, no
  in-process store handle, a Controller identity and RBAC grants of its own.
  Depends on resource families (ADR-0003) and the extension-kind migration if
  the first controller is an extension provisioner, as below.
- **G5 — Typed-object migration far enough** that (a) no remaining
  cross-boundary write needs a single transaction, *and* (b) the authorization
  engine's reads are inside the boundary. (b) is the one it is tempting to
  forget: ADR-0001's transitional `MembershipResolver` may read legacy
  `team_members`, and admin/operator classification resolves against IdP-managed
  teams in typed tables (`src/server/auth/roles.rs`). An apiserver owning
  authorization can reach neither, so `User`, `Group`, `GroupMembership` and
  `UserIdentity` must be resource-backed before cutover.

**G5 is most of `ROADMAP.md` §4, and this decision should not pretend
otherwise.** The split lands near the *end* of the typed-object migration rather
than alongside it. That is a reason to sequence it last, not to doubt it — and
the coupling runs the useful way: every kind that migrates deletes a straddle
rather than relocating it, so the gate gets cheaper as that migration
proceeds. Only then
does the apiserver move into its own process, at which point it is a packaging
change, because every caller already speaks the API.

**The first external controller should be an extension provisioner, not
`rise-k8s-controller`** — smallest blast radius, kinds already scheduled to
become `ResourceDefinition`s under the `Extension` family (ADR-0003), and it
exercises the full seam (Watch, Controller tokens, `status`/`finalizers`, RBAC)
without putting production deployment reconciliation on an unproven path.
`ROADMAP.md` §5 is ordered to match, with `rise-k8s-controller` second.

**Controllers externalize before the store does, not after.** A controller can
be a separate process while the apiserver is still hosted in `rise backend
server`; it speaks HTTP to it and — once it serves a product subresource — is
spoken to in return (§4). Ordering it this way makes the API's completeness a
precondition of a change we want anyway (`ROADMAP.md` §5, multi-org) rather than
of one invisible to users.

### 8. Packaging, embedded mode, and version skew

**A separate `rise-apiserver` binary**, from this workspace and image on the
same release cadence. `rise` keeps the CLI and whatever remains of the typed
API; `rise-apiserver` links neither. The asymmetry is deliberate and grows: the
apiserver's surface is the one that lasts, so it is the one kept clean. One
extra build target buys a control plane that carries no code it never runs.
Each process gets its own Helm Deployment, scaling, and database role.

**There is no embedded mode.** No supported configuration has the product
backend hosting the apiserver in-process — not in production, not in
`tests/e2e`, not in local development. Before G5 the apiserver is still hosted
in `rise backend server`, but that is a transitional state rather than a
configuration and it ends at the cutover. Small installs therefore run two
processes, as do Compose development and the e2e harness. This is the strict
choice, taken because a supported dual topology means every feature must work in
both forever — and the cheaper path silently becomes the only tested one.

**Version skew: apiserver *n*, clients *n-1*.** The window opens at the cutover,
not today: the API it would cover has no pagination, watch, patch or discovery
yet, and the project is pre-1.0 (`Cargo.toml`, `0.23.0-rc8`). From then: upgrade
the apiserver first; clients up to one minor behind keep working; every client
tolerates a newer apiserver. CI runs the previous release's client against the
current apiserver. This covers the inbound direction only — on the forwarding
leg the burden inverts, and §4 states its rule and its CI job.

## Consequences

- Authorization and admission become the apiserver's job by construction — the
  only place ADR-0001's transactional requirements can be met.
- A controller's blast radius becomes its RBAC grants, making multi-org
  isolation (`ROADMAP.md` §5) demonstrable rather than asserted.
- Third-party controllers get a supported contract with no Rust linkage and a
  version window to build against.
- Every resource read behind a typed route gains a network hop, with no
  mitigation claimed. Request-local `AuthorizationSnapshot` memoization is now
  live in the request path, but it is not a mitigation for this: once
  authorization runs inside the apiserver, it cannot absorb a cost paid on the
  other side of the boundary. The cost
  retires with the shim (§1), but the shim's window is set by how fast *clients*
  migrate, not by Rise's schedule: "bounded" means has an end, not has a date.
  Measure at G4.
- A product subresource costs two hops and couples its availability to both
  processes (§4).
- A controller serving a subresource stops being a pure client: it listens,
  terminates TLS, and needs an inbound identity check. Accepted — it is what
  keeps runtime credentials out of the control plane.
- Two versioned contracts run in opposite directions (§4, §8): two compatibility
  windows, two CI jobs.
- One orphan window stays open by choice — a typed row may outlive the
  Organization it names (§6), detected rather than prevented, closing when the
  typed tables do.
- The apiserver becomes browser-facing after `ROADMAP.md` §4, inheriting CORS,
  cookie and CSRF handling the typed API owns today.
- Operators run, scale and upgrade more processes with no single-process escape
  hatch. This is an operator-impact change: it needs Upgrade Notes and a Rollout
  Tracker item when it lands. Local development and the e2e harness run two
  processes from G5 onward.
- Debugging crosses a process boundary; request correlation IDs through the
  client become mandatory rather than nice.

## Alternatives considered

**Split the resource store into its own process now**, ahead of the gates — the
proposal as first stated, and its motivation is sound. Rejected on sequencing,
not direction: the transaction boundary it would fix is still being designed
(G1), the API cannot yet serve a client-only backend (G2), and the `ROADMAP.md`
§4 window still holds cross-boundary writes whose only cheap fix is a shared
pool (G5). Splitting first converts each from a local problem into a distributed
one with no compensating mechanism.

**Keep the store an in-process library indefinitely.** Cheapest, and honest for
a single-tenant install. Rejected because multi-org isolation and third-party
controllers both need a network boundary with independent credentials, and
"the trait is the contract" gets less true every month it goes untested against
a remote implementation.

**Externalize controllers but never split the store.** Captures most of the
multi-org benefit at a fraction of the cost, and is a legitimate stopping point
— it is exactly where G4 leaves us, and staying there a while is safe. Not
adopted as the *target* because the process holding `resource_store` credentials
would still hold every typed table's credentials, so the apiserver's blast
radius stays merged with the product backend's and the strongest reason for a
boundary goes unrealized.

**A supported embedded mode** for small installs, with the split opt-in.
Attractive: one process for single-tenant users, and both `ResourceApi`
implementations stay exercised. Rejected because two supported topologies means
every feature, failure mode and upgrade path must work in both — and the
in-process path, being faster and easier to debug, becomes the one really
tested while the split path accumulates divergence.

**mTLS for client identity**, primary or alongside tokens. Rejected because it
models identity a second time in a system that cannot express Rise's scopes,
delegation chains, or revocation, and makes certificate issuance and rotation an
operator responsibility in every install. TLS still protects the transport; it
just does not name the principal.

**A shared `runtime_sync` schema with per-component key namespaces.** Zero code
change, one migration path. Rejected because it leaves "no lease crosses the
boundary" a convention a future PR can break silently, in a decision whose
premise is making such rules enforced instead.

**Lockstep versioning** — every component from one release, upgraded together.
Rejected because it forbids partial rollout, implies control-plane unavailability
during upgrades, and leaves third-party controllers unable to version
independently, undercutting a main reason for the boundary.

**Three ways to place product subresources other than forwarding.** *Linking the
product handlers into the apiserver* leaves ADR-0002 unamended but hands the
control plane a `kube::Client` and per-Organization cluster configuration — the
merged blast radius the boundary exists to end. *Serving them from the product
backend or controller directly* is honest about where the capability lives and
needs no forwarding machinery, but splits one resource's path grammar across
hosts and gives up the single authorization and audit pipeline ADR-0002 exists
to protect. *Deferring the question* and scoping this ADR to generic
subresources would have left §8's packaging answer untouched, but the hole is
reached at G5, it constrains packaging, and leaving it open means silently
contradicting ADR-0002 in the meantime.

**gRPC or a purpose-built protocol between components.** Faster on paper.
Rejected for now: the HTTP resource API is the contract clients already have and
that discovery and OpenAPI already describe, and a second protocol is precisely
the untanglement debt this decision exists to avoid. Revisit only with a
measurement, and only as another transport for the same contract.

**An etcd-style key-value substrate under the apiserver.** Faithful to the
Kubernetes design. Rejected because PostgreSQL already provides stronger
primitives than that shape would give up — real transactions, partial indexes,
and the serializable semantics ADR-0001's grant gate needs.

## Deferred pending measurement

The first two are numbers this decision needs and cannot guess; the third is
sequencing that follows work scheduled elsewhere.

- **Watch fan-out capacity and connection limits** set the apiserver's practical
  replica count. Measured when Watch lands (`ROADMAP.md` §1), before G5.
- **Whether the product backend needs read caching** once it is a client.
  `ROADMAP.md` §1 already refuses to decide cross-request authorization caching
  ahead of measurement; same discipline, measured at G4.
- **The per-surface order in which the web UI switches** to direct apiserver
  access, following the typed-object migration kind by kind.

## References

- [ADR-0001: Unified Permission Model](./0001-unified-permission-model.md) —
  the authorization engine, transaction-scoped admission, and the
  `effectiveLabels` resolution this decision places inside the boundary.
- [ADR-0002: Generic Resource Subresource Execution Model](./0002-generic-resource-subresource-execution-model.md)
  — the execution seam `status`, `finalizers` and `token` run on, and whose
  code-backed handler boundary §4 deliberately opens for forwarding to a
  registered Controller.
- [ADR-0003: Resource Families](./0003-resource-families.md) — the extension-kind
  migration that makes an extension provisioner the natural first external
  controller.
- `ROADMAP.md` §1 (resource API maturation), §2 (token issuance and TTL), §4
  (typed-object migration), §5 (external controllers and multi-org routing), §6
  (codebase decomposition).
- [Generic resource API: HTTP API](../resources/api.md) — path grammar, parent
  chains, and discriminators.
- [Deployment backends](../deployment-backends.md) — the controller topology
  this decision changes.
