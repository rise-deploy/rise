---
title: "ADR-0004: Control-Plane Process Topology"
---

## Status

**Draft** (pre-decision working material). Date: 2026-08-21.

ADR-0001 fixes the authorization model and the crate structure that realizes
it; ADR-0002 fixes the subresource execution seam. This decision changes
neither. It decides *which process* each of them runs in, what protocol the
components between them speak, and in what order the split happens.

The open questions that must close before this becomes Proposed are listed at
the end.

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
- No `rise-deploy` query reaches into `resource_store`. `grep -rn resource_store
  src/db/*.rs` is empty.
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

**1. `ResourceStore` is a storage contract, not an API contract.** Two of its
methods take the caller's identity as a string parameter:

```rust
// crates/rise-resource-api/src/store.rs
async fn update_controller_status(&self, uid: Uuid, controller_id: &str, …);
async fn operator_update_status(&self, uid: Uuid, operator: &str, …);
```

In one process, a caller asserting `controller_id` is merely trusted. Across a
network it is a forgery primitive: the identity has to come from the
authenticated principal, never from the request body. Other methods —
`try_collect`, `list_pending_collection`, `resolve_path`, `ancestors`,
`resolve_collection*`, `register_resource_definition` — are not API operations
at all; they are the internals of serving one, and no remote client should ever
name them.

**2. Every in-tree consumer takes the whole surface.** `AppState` holds a single
`Arc<dyn ResourceStore>` (`src/server/state.rs:75`) handed to the resource
handlers, the GC worker, the deployment webhook, the organization helpers, and
bootstrap alike. Nothing distinguishes a caller that could be remote from one
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

## Draft decision

### 1. The target is an apiserver, not a database service

A dedicated control-plane process — working name `rise-apiserver` — owns the
generic resource API end to end:

| Inside the apiserver | Outside |
|---|---|
| Resource store and its Postgres schema | Product backend (projects, deployments, env vars, domains) |
| Kind registry and `ResourceDefinition` projection | Deployment controllers (Kubernetes, Docker) |
| Normalization and admission | Extension controllers (RDS, S3, Snowflake OAuth) |
| Authorization (`rise-authz` engine, ADR-0001) | CLI and web UI |
| Subresource execution (ADR-0002) | |
| Garbage collection | |
| Watch fan-out | |

The apiserver is the **sole holder of credentials for `resource_store`** and its
only writer. Everything in the right-hand column is a client with a Rise-issued
identity, addressing resources by path.

This is deliberately the kube-apiserver shape: admission and authorization sit
*inside* the process that owns storage, because both need to read and write in
one transaction. Rise's storage engine happens to be PostgreSQL rather than
etcd, which buys real transactions and costs nothing here.

### 2. The API contract is the only protocol across the boundary

Normative, and enforceable long before any process splits:

1. No component other than the apiserver opens a connection to `resource_store`.
2. No SQL join, view, or foreign key crosses the boundary. Soft UID references
   are permitted during the §4 migration; they carry no referential guarantee.
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
coupling this decision removes.

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
| `try_collect`, `list_pending_collection`, `list_deletion_blockers` | GC worker internals over tombstoned rows |
| `resolve_path`, `ancestors` | Path resolution and `effectiveLabels` primitives feeding admission and authorization |
| `resolve_collection`, `resolve_collection_version`, `resolve_collection_by_kind` | Registry lookups behind every request |
| `register_resource_definition`, `update_resource_definition` | The atomic storage projection behind ordinary `ResourceDefinition` writes |
| `operator_update_status`, `operator_update_finalizers` | Transitional operator bypass; dissolves into RBAC |

The `controller_id: &str` and `operator: &str` parameters disappear when
ADR-0001's choke point lands — `ROADMAP.md` §1 already commits to removing
`ResourceDefinition.allowedStatusControllerIds` and authorizing `status` and
`finalizers` through RBAC alone.

In-tree code outside `src/server/resources/` and the GC worker uses only the API
surface. That single rule turns the whole question into a compile-time check: a
caller that cannot be expressed on the API surface is a future distributed-systems
bug, found today, for free. `ResourceApi` gets two implementations — a direct
in-process one and an HTTP one in `rise-resource-client` — so moving a component
across the boundary is a wiring change, not a rewrite.

### 4. The API proves itself before the process splits

Gates, in order. Each is independently valuable; none exists solely to unblock
the next.

- **G1 — Authorization inside the boundary.** `rise-authz` wired into the
  request path, `require_operator` replaced by the centralized choke point,
  admission and the write-time grant gate transaction-scoped.
- **G2 — API completeness.** Pagination and selectors, Watch, Patch, discovery.
  Until these exist, "client-only" means "polls everything".
- **G3 — `rise-resource-client`** with Rise-issued credential providers, watch
  resume, and finalizer/subresource helpers.
- **G4 — One controller runs entirely on the API.** No typed-table reads, no
  in-process store handle, a Controller identity and RBAC grants of its own.
- **G5 — Typed-object migration far enough** that no remaining cross-boundary
  write needs a single transaction.

Only then does the apiserver move into its own process — at which point it is a
packaging change, because every caller already speaks the API.

**The first external controller should be an extension provisioner, not
`rise-k8s-controller`.** It has the smallest blast radius, its kinds are already
scheduled to become `ResourceDefinition`s under the `Extension` family
(ADR-0003, `ROADMAP.md` §4), and it exercises the full seam — Watch, Controller
tokens, `status` and `finalizers` subresources, RBAC — without putting production
deployment reconciliation on an unproven path.

**Controllers externalize before the store does, not after.** A controller can
be a separate process while the apiserver is still hosted inside
`rise backend server`; it just speaks HTTP to it. Ordering it this way makes the
API's completeness a precondition of a change we want anyway (§5, multi-org),
rather than a precondition of a change that is invisible to users.

### 5. Packaging: one binary, several entrypoints

Process separation does not require crate separation. Ship one image and one
binary with distinct entrypoints:

```
rise backend server       # product API
rise backend apiserver    # resource API, admission, authz, GC, watch
rise backend controller   # one controller class per process
```

Each gets its own Helm Deployment, its own scaling, its own database role, and
its own `rise-runtime-sync` lease namespace. This buys the isolation and the
ownership clarity without a release-version matrix or skew between separately
versioned crates. A crate split happens only where a client genuinely must not
link the server — `rise-resource-client` is the planned instance.

An **interim enforcement step available immediately**: give the resource store
its own `PgPool` under a database role that can reach only the `resource_store`
schema, and revoke that schema from the backend's role. Since no cross-schema
SQL exists today, this costs a configuration change and converts invariant 1
from a convention into something Postgres enforces.

## Consequences if adopted

- Authorization and admission become the apiserver's job by construction, which
  is the only place ADR-0001's transactional requirements can be met.
- A controller's blast radius becomes its RBAC grants. Multi-org isolation
  (§5) becomes demonstrable rather than asserted.
- Third-party controllers get a supported contract with no Rust linkage.
- Every resource read from the product backend gains a network hop.
  Request-local `AuthorizationSnapshot` memoization (already shipped) absorbs
  much of it inside one request; the rest is the price of the boundary and
  should be measured at G4, not estimated now.
- Two implementations of `ResourceApi` must stay semantically identical. The
  resource-store contract tests (`crates/rise-resource-api/tests/`) run against
  both, or the boundary rots silently.
- Operators gain processes to run, scale, and upgrade. This is an operator-impact
  change: it needs Upgrade Notes and a Rollout Tracker item when it lands, and a
  supported single-process mode for small installs (see open questions).
- Debugging crosses a process boundary; request correlation IDs through the
  client become mandatory rather than nice.

## Alternatives considered

**Split the resource store into its own process now**, ahead of the gates. This
is the proposal as first stated and its motivation is sound. Rejected on
sequencing, not direction: the transaction boundary it would fix is still being
designed (G1), the API cannot yet serve a client-only backend (G2), and the §4
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

**gRPC or a purpose-built protocol between components.** Faster on paper.
Rejected for now: the HTTP resource API is the contract clients already have and
that discovery and OpenAPI already describe, and adding a second protocol is
precisely the untanglement debt this decision exists to avoid. Revisit only with
a measurement, and only as an additional transport for the same contract.

**An etcd-style key-value substrate under the apiserver.** Faithful to the
Kubernetes design. Rejected because PostgreSQL already provides stronger
primitives than the ones that shape would give up — real transactions,
partial indexes, and the serializable semantics ADR-0001's grant gate needs.

## Open questions before Proposed

1. **Transport and client authentication.** Rise-issued Controller tokens alone,
   or mTLS as well for in-cluster clients? Does the apiserver terminate TLS
   itself?
2. **Does the web UI talk to the apiserver directly**, or only through the
   product backend? The answer decides whether browser-originated CORS and
   session handling become apiserver concerns.
3. **Apiserver HA shape.** N stateless replicas with GC leader-elected as it is
   today, or a single-writer design? Watch fan-out capacity and connection
   limits (a `ROADMAP.md` §1 item) determine this.
4. **`runtime_sync` ownership.** Does the apiserver own that schema too, or does
   each process get its own lease namespace within it? Invariant 5 forbids
   sharing a lease across components but not sharing the table.
5. **Embedded mode.** Is a single-process deployment (`rise backend server`
   hosting the apiserver in-process) a supported configuration or a development
   convenience? A supported one keeps small installs simple and keeps both
   `ResourceApi` implementations exercised — at the cost of two topologies to
   test.
6. **Version skew policy.** What compatibility window must the apiserver hold
   for older clients and controllers during a rolling upgrade, and how is it
   tested?
7. **Naming.** `rise-apiserver` vs. `rise-resource-api` as the process name, and
   whether the entrypoint is `rise backend apiserver` or a separate binary.

## References

- [ADR-0001: Unified Permission Model](./0001-unified-permission-model.md) —
  the authorization engine, transaction-scoped admission, and the
  `effectiveLabels` resolution this decision places inside the boundary.
- [ADR-0002: Generic Resource Subresource Execution Model](./0002-generic-resource-subresource-execution-model.md)
  — the execution seam that `status`, `finalizers`, and `token` run on.
- [ADR-0003: Resource Families](./0003-resource-families.md) — the extension-kind
  migration that makes an extension provisioner the natural first external
  controller.
- `ROADMAP.md` §1 (resource API maturation), §4 (typed-object migration), §5
  (external controllers and multi-org routing), §6 (codebase decomposition).
- [Generic resource API](../generic-resource-api.md) — path grammar, parent
  chains, and discriminators.
- [Deployment backends](../deployment-backends.md) — the controller topology
  this decision changes.
