---
title: "ADR-0004: Control-Plane Process Topology"
---

## Status

**Proposed** (under review). Date: 2026-08-21.

ADR-0001 fixes the authorization model and the crate structure that realizes
it; ADR-0002 fixes the subresource execution seam. This decision changes
neither. It decides *which process* each of them runs in, what protocol the
components between them speak, and in what order the split happens.

## Context

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
  args:
    - "backend"
    - "server"
```

`rise backend server` hosts, in one `tokio` runtime sharing one `PgPool`:

- the whole HTTP API, including the generic resource API at
  `/api/v1/resources`;
- the project controller, the ECR controller, the resource GC worker, the
  workload-identity refresh controller, and the Entra active-sync loop, each
  `tokio::spawn`ed in `src/server/mod.rs` and gated by a `rise-runtime-sync`
  leader election;
- the Docker reconciler (`rise-backend-docker`), spawned from
  `src/server/state.rs` under the same leader election;
- the Metacontroller sync/finalize webhook (`src/server/deployment/webhook.rs`).

One detail is easy to miss and matters to the ordering below: the Kubernetes
reconcile *loop* is already out of process. Metacontroller runs as its own
Deployment and calls Rise's webhook. What lives in `rise backend server` is the
decision function — and it reads typed tables directly, through
`state.deployment_store`, not through any resource API.

### What the boundary already gets right

Schema ownership is real, not aspirational:

- `resource_store` belongs to `rise-resource-store-postgres`, which owns its
  migrations and its own SQLX offline cache. `runtime_sync` belongs to
  `rise-runtime-sync` on the same terms.
- No `rise-deploy` production query reaches into `resource_store`; `grep -rn
  resource_store src/db/*.rs` is empty. One test fixture writes the schema
  directly (`src/server/resources/gc.rs:741`, under `#[cfg(test)]`).
- The typed-table linkage migration explicitly refuses a foreign key across the
  line:

  ```sql
  -- migrations/20260526000002_add_organization_linkage.sql
  -- The columns intentionally do NOT declare a FOREIGN KEY to
  -- `resource_store.resources(uid)`: that schema is owned by the
  -- rise-resource-store crate's migrations […]
  -- Treat these columns as soft references to `resource_store.resources(uid)`.
  ```

- The store contract itself is dependency-light and lives in
  `rise-resource-api`, not in the server crate.

So the failure mode usually feared — a second component quietly growing SQL
against someone else's tables — has not happened.

### What it does not get right

**1. `ResourceStore` is a storage contract, not an API contract.** Four of its
methods take the acting identity as a string parameter
(`crates/rise-resource-api/src/store.rs:293-325`):

```rust
async fn update_controller_status(&self, uid: Uuid, controller_id: &str, …);
async fn update_controller_finalizers(&self, uid: Uuid, controller_id: &str, …);
async fn operator_update_status(&self, uid: Uuid, operator: &str, …);
async fn operator_update_finalizers(&self, uid: Uuid, operator: &str, …);
```

The trust boundary above them is already correct: the HTTP handlers pass
`controller.0.identity_id` from the authenticated Controller
(`src/server/resources/handlers.rs:1197`) and `user.email` after
`require_operator` (`:1245`). The problem is not a live vulnerability — it is
that the trait cannot express where identity comes from, so it cannot be the
contract a remote client codes against. Other methods — `try_collect`,
`list_pending_collection`, `resolve_path`, `ancestors`, `resolve_collection*`,
`register_resource_definition` — are not API operations at all; they are the
internals of serving one.

**2. Every in-tree consumer takes the whole surface.** `AppState` holds a single
`Arc<dyn ResourceStore>` (`src/server/state.rs:75`) handed to the resource
handlers, the GC worker, the deployment webhook, the organization helpers,
bootstrap, and the Docker reconciler (`src/server/state.rs:1215` →
`crates/rise-backend-docker/src/reconciler.rs:186`) alike. That last one is a
*deployment controller* — the category §1 places outside the boundary — already
holding a direct store handle. Nothing distinguishes a caller that could be remote from one
that could not, so nothing tells us today which code the split would break. The field's own
documentation states the intent plainly:

```rust
// src/server/state.rs
/// Generic-resource store used by the `/api/v1/resources` HTTP API and
/// (later) by internal controllers wanting to reconcile against Rise state
/// without a network round-trip.
pub resource_store: Arc<dyn rise_resource_api::ResourceStore>,
```

That is the second protocol, named in advance: a privileged in-process path for
components that are also expected to exist as external controllers. Whatever it
is used for must be re-derivable from the API, or those components can never
leave the process.

**3. The consistency model is still being decided.** ADR-0001's write-time grant
gate requires serializable transactions spanning policy reads and policy writes;
normalization and admission are specified as transaction-scoped;
`ResourceStore::ancestors` exists precisely so `effectiveLabels` resolution
(ADR-0001 §6.1) can be a pure function over a chain the store returns. All of
that is inside-the-server work — and `rise-authz` is not yet a dependency of
`rise-deploy` at all. Drawing a process boundary now means fixing the
transaction boundary before the thing that needs it exists.

**4. The HTTP API cannot yet be the only way in.** Pagination, label/field
selectors, Watch, JSON Merge Patch, Server-Side Apply, and discovery are all
open items under `ROADMAP.md` §1 "Resource API maturation". A component demoted
to client-only today would poll unfiltered lists.

**5. There is live cross-boundary state.** Typed rows carry
`organization_resource_uid` soft references, and the guard that protects them
documents its own race:

```rust
// src/server/resources/organization.rs
// TODO(multi-org): the count and the delete run in separate statements
// with no surrounding transaction […] serialize delete vs. typed insert by
// taking `pg_advisory_xact_lock` keyed on the Org UID in this function
// *and* in every […] call site.
```

That fix costs one advisory lock while both sides share a pool. Split the
process first and the same bug needs a distributed answer instead.

### Forces

- **Ownership.** One process holding credentials for every schema makes "who may
  write this table" a code-review question rather than an enforced one.
- **Multi-org isolation** (`ROADMAP.md` §5) needs controllers that reconcile one
  Organization into one cluster with no access to anything else. A controller
  linked into the product backend cannot be constrained that way.
- **Third-party and extension controllers** need a supported contract, which
  means a network API with credentials, not a Rust trait.
- **Protocol lock-in**, the motivating concern: every month the in-process path
  is the only path is a month of code written against storage semantics that a
  network boundary cannot preserve.

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
| Subresource execution and forwarding (ADR-0002, §4 below) | Web UI |
| Garbage collection | |
| Watch fan-out | |

The apiserver is the **sole holder of credentials for `resource_store`** and its
only writer. Everything in the right-hand column is a client with a Rise-issued
identity, addressing resources by path.

This is deliberately the kube-apiserver shape: admission and authorization sit
*inside* the process that owns storage, because both need to read and write in
one transaction. Rise's storage engine happens to be PostgreSQL rather than
etcd, which buys real transactions and costs nothing here.

**The typed product API is a shrinking shim, not a fixture.** As
`ROADMAP.md` §4 lands each kind, the typed routes stop owning storage and become
a translation layer over the resource API — and `ROADMAP.md` §4's closing item
already says typed tables are dropped "after [the] resource-backed path has
baked and compatibility reads are no longer needed." The end state is a shim
that exists only for clients not yet speaking the resource API, and quite
possibly no typed surface at all. Three things follow, and this ADR reads
differently without them:

- **The right-hand column is not a peer architecture.** Deployment and extension
  controllers are permanent; the typed API is a compatibility artifact with a
  planned end. Do not invest in it as though it were durable, and do not add a
  typed route for something the resource API can already express.
- **The shim holds no storage and no credentials.** Once translation is all it
  does, it is an ordinary client — so *where* it runs stops being an
  architectural question and becomes a deployment one. It stays in `rise`
  because that is where the CLI already is, not because it needs to be its own
  tier.
- **Several problems below are transitional, and should be solved
  transitionally.** The cross-boundary Organization lock (§6), the bootstrap
  split (§6), and the typed rows that soft-reference Organization UIDs all exist
  *because* typed tables exist. They do not need permanent architecture — they
  need a mechanism that carries the migration and is then deleted with the
  tables it served. Building an enduring distributed-consistency layer for them
  would outlive its problem.

### 2. The API contract is the only protocol across the boundary

Normative, and enforceable long before any process splits:

1. No component other than the apiserver opens a connection to `resource_store`.
2. No SQL join, view, or foreign key crosses the boundary. Soft UID references
   are permitted during the `ROADMAP.md` §4 migration; they carry no
   referential guarantee.
3. **No transaction spans the boundary.** Anything requiring atomicity with a
   resource write is implemented inside the apiserver — as admission, as a
   subresource, or as a store operation — never as a caller-side transaction.
4. Every operation a client performs is expressible as one request against one
   resource path, with the acting identity taken from its credential.
5. The only cross-boundary consistency primitives are the resource-level ones:
   `resourceVersion` compare-and-swap, finalizers, owner references, and watch.
   Advisory locks, `SELECT … FOR UPDATE`, and `rise-runtime-sync` leases are
   legitimate *within* one component and illegitimate *between* two.

Rule 5 is the one that bites. `rise-runtime-sync` is the right tool for
coordinating replicas of a single component and the wrong tool for coordinating
two components; using it across the line reintroduces exactly the shared-database
coupling this decision removes. §6 states how far that can actually be enforced.

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
| `list_deletion_blockers` | Backs the served `deletion-blockers` subresource (`src/server/resources/handlers.rs:665`); the subresource is remotable, this primitive is its implementation |
| `resolve_path`, `ancestors` | Path resolution and `effectiveLabels` primitives feeding admission and authorization |
| `resolve_collection`, `resolve_collection_version`, `resolve_collection_by_kind` | Registry lookups behind every request |
| `register_resource_definition`, `update_resource_definition` | The atomic storage projection behind ordinary `ResourceDefinition` writes |
| `operator_update_status`, `operator_update_finalizers` | Transitional operator bypass; dissolves into RBAC |

The four `controller_id: &str` / `operator: &str` parameters disappear when
ADR-0001's choke point lands — `ROADMAP.md` §1 already commits to removing
`ResourceDefinition.allowedStatusControllerIds` and authorizing `status` and
`finalizers` through RBAC alone.

In-tree code outside `src/server/resources/` and the GC worker uses only the API
surface. That single rule turns the whole question into a compile-time check: a
caller that cannot be expressed on the API surface is a future distributed-systems
bug, found today, for free.

`ResourceApi` has two implementations, but only for the duration of the
migration: a direct in-process one, and an HTTP one in `rise-resource-client`.
Because there is no embedded mode (§8), the direct implementation is never a
product topology — it is how callers are held to the API surface while the
apiserver is still hosted in `rise backend server`, and it retires at G5. The
resource-store contract tests run against both while both exist.

### 4. Product subresources execute where their capability lives

The generic seam runs in the apiserver; the *capability* a product subresource
needs often does not. `deployment-logs` is the case that decides the rule: the
runtime log backend holds a live `kube::Client` or `bollard::Docker`
(`src/server/deployment/logs.rs:189-190`, `:253`, `:312`). Compiling that
handler into the apiserver would hand the control plane cluster and daemon
credentials — exactly the merged blast radius this decision exists to separate —
and would require it to know which cluster serves which Organization, which is
controller state.

So the apiserver **authorizes and forwards**. ADR-0002 §2's pipeline is
unchanged — route, resolve the parent, evaluate RBAC, open the audit record —
and only the leaf differs: instead of invoking a local handler, a platform-known
forwarding strategy streams from the Controller that owns the resource. The
client sees one path grammar and one authorization pipeline; the apiserver never
holds a runtime credential.

**This amends ADR-0002, deliberately.** That ADR requires a handler identifier
to "exist in the process's code-backed registry" and rejects handlers naming "a
URL, executable, dynamic library, or arbitrary Rust type" — but its §1 reserves
the boundary for "a later ADR [to] deliberately open", and its §3 shape table
already contemplates a constrained reverse-proxy exchange. The rejection it
makes is of *arbitrary operator-defined* remote handlers, and that rejection
stands: the forwarding strategy is platform-known and compiled in, the upstream
is a registered `Controller` rather than an operator-supplied URL, and no
`ResourceDefinition` can name a network endpoint. What changes is that a
platform handler's implementation may terminate at a Controller instead of in
local code.

Consequences to design when the first one ships:

- Streaming passthrough must carry cancellation and backpressure across two
  hops, and ADR-0002 §6's connection, idle, and duration limits now apply at
  the apiserver, not at the process holding the log source.
- Its two-phase audit record spans processes: the start record is the
  apiserver's after authorization, the completion record needs the outcome,
  duration, and byte count the forwarding leg observed.
- Controller endpoint discovery becomes apiserver state — a Controller must
  publish a reachable address, and an unreachable one is a distinct failure
  from an unauthorized request.
- A product subresource whose backend needs no runtime credential (the Loki log
  backend is an HTTP call to a log store) *could* execute locally. Allowing two
  execution shapes for one subresource is not worth the divergence; forwarding
  is the rule.

`status`, `finalizers`, and `token` are unaffected. They need no capability
beyond the store and ADR-0001's signing keys, and execute in the apiserver.

### 5. Identity and transport

Clients authenticate with **Rise-issued tokens over TLS**. There is no second
trust root: principals are ADR-0001's `User`, `ServiceAccount`, and `Controller`
identities carrying `rise_uid`, and every decision is live RBAC intersected with
the token's `authorization_details` cap. mTLS is not part of the contract — a
client certificate would model identity a second time, in a system that cannot
express Rise's scopes, delegation, or revocation.

Two consequences follow directly:

- The apiserver verifies TLS wherever it is reachable. A plaintext listener is a
  configuration error, not a deployment option.
- Token lifetime becomes an availability parameter. A controller that cannot
  refresh stops reconciling, so the platform-global maximum TTL
  (`ROADMAP.md` §2) is now a control-plane tuning decision, not only a security
  one.

**Browsers are ordinary clients.** Once the typed-object migration
(`ROADMAP.md` §4) completes, the web UI addresses the apiserver directly rather than
through the product backend. This is not a new identity kind: the UI already
holds a Rise-issued JWT session cookie — the same credential the ingress-auth
subrequest validates. It does move browser-facing concerns onto the apiserver
that the product backend owns today: CORS, cookie scope and `SameSite`, CSRF on
non-idempotent verbs, and per-item list filtering fast enough to be interactive.
Until each kind migrates, the UI keeps reading it through the typed shim; the
switch happens per surface, not as one cutover. Every client makes the same
move eventually — the UI is just the one whose migration we control end to end,
which is why it goes first.

### 6. Availability and coordination

**N stateless replicas.** Every replica serves reads and writes; PostgreSQL
holds the consistency, so no replica is privileged for request handling. Only
background sweeps are singleton — the GC worker runs under a `rise-runtime-sync`
leader lease exactly as it does today.

**Each component owns its own lease schema.** `rise-runtime-sync` gains a schema
parameter: the apiserver migrates and holds credentials for its own lease
tables, while the product backend keeps `runtime_sync` for its controllers and
extension provisioners. That is not free — the crate's queries are compile-time
`sqlx::query!` macros with `runtime_sync.` written into the SQL
(`crates/rise-runtime-sync/src/leader_leases.rs:377`) against a crate-local
offline cache, so a runtime schema parameter means either dynamic queries or a
`search_path` scheme, and the crate already treats `search_path` mutation as
hazardous enough to sacrifice a connection over (`lib.rs`, `run_migrations`).

**This isolates leases, not locks.** `LeaderElection` and `GlobalSchedule` are
table-backed, so separate schemas separate them. `GlobalLock` is not: it hashes
a name to an `i64` and takes `pg_advisory_lock` over a keyspace that is
database-wide (`crates/rise-runtime-sync/src/global_lock.rs:47`), and the crate
warns that "collisions would silently serialize unrelated callers". Two
components against one database can therefore still collide. For `GlobalLock`,
invariant 5 remains a convention, and the honest mitigation is that it has no
cross-component use after the split — not that the schema split prevents one.

That matters because the codebase's *documented* fix for the live
Organization-delete race is a cross-boundary advisory lock:
`src/server/resources/organization.rs:42-51` prescribes `pg_advisory_xact_lock`
keyed on the Org UID, taken in the resource-delete path *and* in every
`set_team_organization` / `set_project_organization` / `ensure_user_membership`
call site. Invariant 3 forbids exactly that once those call sites are in a
different process. The replacement is a finalizer: the product backend registers
a finalizer on Organization and clears it only when no typed row references that
UID, so the apiserver tombstones and waits instead of counting rows it cannot
see. That has to land before an Organization can be deleted across the boundary
— it is a G5 obligation, not an implementation detail. It is also a
*transitional* one: the finalizer exists to guard typed rows, so it is deleted
with them (§1). Build it to be removable, not to endure.

Bootstrap does not simply move. Its `GlobalLock` serializes default-Organization
creation, but the work it guards writes typed tables too
(`backfill_user_organization_memberships`, `backfill_teams_organization`,
`backfill_projects_organization` in `src/server/bootstrap.rs`). Moving it into
the apiserver would hand the apiserver typed-table credentials. It splits
instead: the apiserver creates the Organization resource, and the product
backend does its own linkage pass afterwards, converging rather than
transacting. Convergence is the right shape precisely because the problem is
temporary — the backfills disappear entirely once those kinds are
resource-backed, and nothing should be built here that would be missed.

### 7. The API proves itself before the process splits

Gates, in order. G1, G2, and G4 are independently valuable; G3 is a library
with no consumer until G4, and is listed separately only because it is the
seam's first real client.

- **G1 — Authorization inside the boundary.** `rise-authz` wired into the
  request path, `require_operator` replaced by the centralized choke point,
  admission and the write-time grant gate transaction-scoped.
- **G2 — API completeness.** Pagination and selectors, Watch, Patch, discovery.
  Until these exist, "client-only" means "polls everything".
- **G3 — `rise-resource-client`** with Rise-issued credential providers, watch
  resume, and finalizer/subresource helpers.
- **G4 — One controller runs entirely on the API.** No typed-table reads, no
  in-process store handle, a Controller identity and RBAC grants of its own.
  Depends on resource families (ADR-0003) and the extension-kind migration if
  the first controller is an extension provisioner, as recommended below.
- **G5 — Typed-object migration far enough** that (a) no remaining
  cross-boundary write needs a single transaction, *and* (b) the authorization
  engine's reads are inside the boundary. (b) is the one it is tempting to
  forget: ADR-0001's transitional `MembershipResolver` may read legacy
  `team_members`, and admin/operator classification resolves against
  IdP-managed teams in typed tables (`src/server/auth/roles.rs`). An apiserver
  that owns authorization cannot reach either, so `User`, `Group`,
  `GroupMembership`, and `UserIdentity` must be resource-backed before cutover.

**G5 is most of `ROADMAP.md` §4, and this decision should not pretend
otherwise.** Between the identity kinds above, the Organization finalizer in
§6, and the typed rows that still soft-reference Organization UIDs, the honest
statement is that the process split lands near the *end* of the typed-object
migration rather than alongside it. That is a reason to sequence the split
last, not a reason to doubt it — and the coupling runs the useful way: every
kind that migrates deletes a straddle rather than relocating it, so the gate
gets cheaper as §4 proceeds instead of more expensive.

Only then does the apiserver move into its own process — at which point it is a
packaging change, because every caller already speaks the API.

**The first external controller should be an extension provisioner, not
`rise-k8s-controller`.** It has the smallest blast radius, its kinds are already
scheduled to become `ResourceDefinition`s under the `Extension` family
(ADR-0003, `ROADMAP.md` §4), and it exercises the full seam — Watch, Controller
tokens, `status` and `finalizers` subresources, RBAC — without putting production
deployment reconciliation on an unproven path. `ROADMAP.md` §5 currently lists
`rise-k8s-controller` as the first item of that workstream; accepting this ADR
means reordering it, which the roadmap should be edited to reflect.

**Controllers externalize before the store does, not after.** A controller can
be a separate process while the apiserver is still hosted inside
`rise backend server`; it just speaks HTTP to it. Ordering it this way makes the
API's completeness a precondition of a change we want anyway (`ROADMAP.md` §5,
multi-org),
rather than a precondition of a change that is invisible to users.

### 8. Packaging, embedded mode, and version skew

**A separate `rise-apiserver` binary**, built from this workspace and shipped in
the same image on the same release cadence. `rise` keeps the CLI and whatever
remains of the typed API; `rise-apiserver` links neither. The asymmetry is
deliberate and grows over time: the apiserver's surface is the one that lasts,
so it is the one kept clean. One extra build target buys a control
plane that does not carry code it never runs, and a `--help` that means
something. Each process gets its own Helm Deployment, its own scaling, and its
own database role.

**There is no embedded mode.** No supported configuration has the product
backend hosting the apiserver in-process — not in production, not in
`tests/e2e`, not in local development. Before G5 the apiserver is still hosted
in `rise backend server`, but that is a transitional state rather than a
configuration, and it ends at the cutover. Small installs therefore run two
processes; the Compose development setup and the e2e harness gain a second one.
This is the strict choice, taken because a supported dual topology means every
feature must work in both, forever, and the cheaper one silently becomes the
tested one.

**Version skew: apiserver *n*, clients *n-1*.** The window opens at the
cutover, not today — the API it would cover does not yet have pagination,
watch, patch, or discovery, and the project is pre-1.0 (`Cargo.toml`,
`0.23.0-rc8`). From that point: upgrade the apiserver first.
Clients up to one minor version behind keep working, and every client tolerates
a newer apiserver. Rolling upgrades then need no cross-component ordering beyond
that first step, and a third-party controller has a defined window. CI holds the
window honest by running the previous release's client against the current
apiserver.

## Consequences

- Authorization and admission become the apiserver's job by construction, which
  is the only place ADR-0001's transactional requirements can be met.
- A controller's blast radius becomes its RBAC grants. Multi-org isolation
  (`ROADMAP.md` §5) becomes demonstrable rather than asserted.
- Third-party controllers get a supported contract with no Rust linkage, and a
  version window they can build against.
- Every resource read behind a typed route gains a network hop, with no
  mitigation claimed. Request-local `AuthorizationSnapshot` memoization is not
  one: it lives in `rise-authz`, which `rise-deploy` does not yet depend on, and
  once authorization runs inside the apiserver it cannot absorb a cost paid on
  the other side of the boundary. The cost is bounded in time as well as size —
  it is paid on the compatibility path, and it retires with the shim (§1). That
  is a reason to measure it at G4 rather than to engineer against it now.
- A product subresource costs two hops and couples its availability to both
  processes (§4 above).
- The apiserver becomes browser-facing after the typed-object migration,
  inheriting CORS, cookie, and CSRF handling that the typed API owns today. The
  web UI is simply the first client to stop needing the shim, not a special
  case (§5).
- Operators run, scale, and upgrade more processes, with no single-process
  escape hatch for small installs. This is an operator-impact change: it needs
  Upgrade Notes and a Rollout Tracker item when it lands.
- Local development and the e2e harness run two processes from G5 onward.
- Debugging crosses a process boundary; request correlation IDs through the
  client become mandatory rather than nice.

## Alternatives considered

**Split the resource store into its own process now**, ahead of the gates. This
is the proposal as first stated and its motivation is sound. Rejected on
sequencing, not direction: the transaction boundary it would fix is still being
designed (G1), the API cannot yet serve a client-only backend (G2), and the `ROADMAP.md` §4
migration window still contains cross-boundary writes whose only cheap fix is a
shared pool (G5). Splitting first converts each of those from a local problem
into a distributed one with no compensating mechanism.

**Keep the store an in-process library indefinitely.** Cheapest, and honest for
a single-tenant install. Rejected because multi-org isolation and third-party
controllers both require a network boundary with independent credentials, and
because "the trait is the contract" gets less true every month it goes untested
against a remote implementation.

**Externalize controllers but never split the store.** Captures most of the
multi-org benefit at a fraction of the cost, and is a legitimate stopping point.
Not adopted as the target because the process holding `resource_store`
credentials would still be the process holding every typed table's credentials —
so the apiserver's blast radius stays merged with the product backend's, and the
strongest reason to have a boundary at all goes unrealized. It is, however,
exactly where G4 leaves us, and stopping there for a while is safe.

**A supported embedded mode** for small installs, with the split as an opt-in.
Attractive: one process for single-tenant users, and both `ResourceApi`
implementations stay continuously exercised. Rejected because two supported
topologies means every feature, every failure mode, and every upgrade path must
work in both — and the in-process path, being faster and easier to debug, would
become the one that is really tested while the split path accumulates
divergence.

**mTLS for client identity**, either as the primary credential or alongside
tokens. Rejected because it models identity a second time in a system that
cannot express Rise's scopes, delegation chains, or revocation, and it makes
certificate issuance and rotation an operator responsibility in every install.
TLS still protects the transport; it just does not name the principal.

**A shared `runtime_sync` schema with per-component key namespaces.** Zero code
change, one migration path. Rejected because it leaves "no lease crosses the
boundary" as a convention that a future PR can break silently, in a decision
whose whole premise is making such rules enforced rather than conventional.

**Lockstep versioning** — every component from one release, upgraded together.
Simplest possible contract. Rejected because it forbids partial rollout, implies
control-plane unavailability during upgrades, and leaves third-party controllers
unable to version independently, which undercuts a main reason for the boundary.

**Linking the product handler code into the apiserver** so ADR-0002's
code-backed registry stands unamended. Rejected because `deployment-logs` needs
a `kube::Client` or `bollard::Docker` (`src/server/deployment/logs.rs:189-190`),
so the apiserver would hold runtime credentials and per-Organization cluster
configuration — the merged blast radius the boundary exists to end.

**Serving product subresources from the product backend or the controller
directly**, leaving only `status`, `finalizers`, and `token` on the apiserver.
Honest about where the capability lives and needs no forwarding machinery.
Rejected because it splits one resource's path grammar across hosts and gives up
the single authorization and audit pipeline that ADR-0002 exists to protect —
`.../deployments/x` and `.../deployments/x/logs` would answer from different
places under different enforcement.

**Deferring product-subresource execution entirely** and scoping this ADR to
generic subresources. Tempting, and it would have left §8's packaging answer
untouched. Rejected because the hole is not hypothetical: it is reached at G5,
it constrains packaging, and leaving it open would mean the ADR silently
contradicts ADR-0002 in the meantime.

**gRPC or a purpose-built protocol between components.** Faster on paper.
Rejected for now: the HTTP resource API is the contract clients already have and
that discovery and OpenAPI already describe, and adding a second protocol is
precisely the untanglement debt this decision exists to avoid. Revisit only with
a measurement, and only as an additional transport for the same contract.

**An etcd-style key-value substrate under the apiserver.** Faithful to the
Kubernetes design. Rejected because PostgreSQL already provides stronger
primitives than the ones that shape would give up — real transactions, partial
indexes, and the serializable semantics ADR-0001's grant gate needs.

## Deferred pending measurement

The first two are numbers this decision needs and cannot guess. The third is
sequencing that follows from work already scheduled elsewhere.

- **Watch fan-out capacity and connection limits** set the apiserver's practical
  replica count. Measured when Watch lands (`ROADMAP.md` §1), before G5.
- **Whether the product backend needs read caching** once it is a client.
  `ROADMAP.md` §1 already refuses to decide cross-request authorization caching
  ahead of measurement; the same discipline applies here, measured at G4.
- **The per-surface order in which the web UI switches** to direct apiserver
  access, which follows the typed-object migration kind by kind.

## References

- [ADR-0001: Unified Permission Model](./0001-unified-permission-model.md) —
  the authorization engine, transaction-scoped admission, and the
  `effectiveLabels` resolution this decision places inside the boundary.
- [ADR-0002: Generic Resource Subresource Execution Model](./0002-generic-resource-subresource-execution-model.md)
  — the execution seam that `status`, `finalizers`, and `token` run on, and
  whose code-backed handler boundary §4 above deliberately opens for forwarding
  to a registered Controller.
- [ADR-0003: Resource Families](./0003-resource-families.md) — the extension-kind
  migration that makes an extension provisioner the natural first external
  controller.
- `ROADMAP.md` §1 (resource API maturation), §2 (token issuance and TTL), §4
  (typed-object migration), §5 (external controllers and multi-org routing), §6
  (codebase decomposition).
- [Generic resource API](../generic-resource-api.md) — path grammar, parent
  chains, and discriminators.
- [Deployment backends](../deployment-backends.md) — the controller topology
  this decision changes.
