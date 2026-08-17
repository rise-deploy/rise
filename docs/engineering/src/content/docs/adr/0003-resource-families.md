---
title: "ADR-0003: Resource Families"
---

## Status

**Draft.** Date: 2026-08-17.

This is an exploratory design, not yet a proposed decision. It may change
substantially before promotion to **Proposed**. ADR-0001 fixes the
authorization shape `(verb, ResourceKind, subresource?)` and the collection-read
semantics this draft builds on; nothing here changes them.

## Context

The generic resource store keys name uniqueness per kind, per parent:

```sql
-- crates/rise-resource-store-postgres/migrations/20260519000000_create_resource_store.sql
CREATE UNIQUE INDEX resources_child_kind_name_unique
    ON resource_store.resources (parent_uid, split_part(api_version, '/', 1), kind, name)
    WHERE parent_uid IS NOT NULL;
```

The pool is `(parent, group, kind)`. An API group namespaces kind *identity*
(`resource_definitions_group_kind_unique`) but is not itself a name pool: two
kinds in one group may hold the same name under the same parent. Nothing in
`ResourceDefinitionSpec` lets a set of kinds opt into a shared pool.

Two product requirements do not fit that model.

**Polymorphic listing.** Extensions are presented to users as one concept with
several providers. Today that is one table and one query: `project_extensions`
is keyed `PRIMARY KEY (project_id, extension)` with `extension_type` as an
ordinary column, and the HTTP surface addresses an instance as
`(project, name)` with the provider type nowhere in the path. `ROADMAP.md` §4
migrates extension kinds to external `ResourceDefinition`s, at which point
"list this project's extensions" becomes "discover which registered kinds are
extensions, then issue one list call per kind" — and the resource API has no
way to express which kinds *are* extensions. This is the primary motivation.

**A shared allocatable name pool.** The same migration would let
`AwsRdsInstance/db` and `SnowflakeOAuth/db` coexist under one project. That is
legal in the store and a silent regression from the guarantee users have
today, where a name identifies at most one extension in a project.

The store already enforces a cross-kind, per-scope allocation pool for
`discriminator` (`resources_child_discriminator_unique`), so the mechanism is
not novel — only its exposure as a user-chosen, kind-spanning identity is.

## Draft decision

Introduce a **resource family**: a named set of kinds that share one name pool
within a parent scope and can be listed and addressed as a unit.

A family is *only* those two things — a name pool and a discovery grouping. It
is deliberately not a schema, a controller boundary, or an authorization
grouping.

### Declaration

A family is a registry resource, `ResourceFamily`, and `ResourceDefinitionSpec`
gains an optional `family` field naming the family the kind joins. The family
must exist before a kind joins it.

A family could instead have been implicit — the set of RDs naming a string —
but three things need a declaration site that a bare string does not have: the
family's own `plural`/`singular`/`shortNames` (which a CLI resolves against),
its printer-column contract (below), and eventually its membership governance.

A family carries both spellings a kind carries, for the same two jobs:
`{group}/{Family}` is its canonical identity, in the same namespace
`ResourceKind` occupies, while `plural`/`singular`/`shortNames` are what URLs
and CLI arguments resolve against.

```yaml
apiVersion: rise.dev/v1alpha1
kind: ResourceFamily
metadata:
  name: extensions.extensions.rise.dev
spec:
  group: extensions.rise.dev
  family: Extension
  plural: extensions
  singular: extension
  shortNames: [ext]
  parent:
    apiVersion: rise.dev/v1alpha1
    kind: Project
```

Registration of a member validates:

1. **Same API group.** A member kind's `group` equals the family's `group`.
   Without this, any RD author could inject a kind into another group's family,
   consuming names in its pool and appearing in its listings. RD creation is
   operator-gated today, so this is latent — but ADR-0001 delegates creation,
   so the rule is encoded now rather than retrofitted.
2. **Same parent kind.** An RD declares exactly one parent, and it must equal
   the family's `parent`. Members parented under different kinds have no common
   scope, leaving "unique within the family under this scope" undefined and
   family-scoped listing unaddressable.
3. **Immutable after RD creation.** `family` may be set only at RD create.
   Adding one to a kind with live instances would require reconciling existing
   collisions; removing one would silently widen the pool under resources that
   were admitted under the narrower rule.
4. **Distinct identity.** `{group}/{Family}` must not collide with any
   `ResourceKind` in that group, and a family's `plural`, `singular`, and
   `shortNames` must not collide with any kind's or any other family's. Both
   levels matter: the first keeps canonical identities unambiguous, the second
   keeps a URL segment or CLI argument resolving to exactly one family or one
   kind, never both. Family collection names follow the existing
   collection-name grammar and reserved-name list.
5. **`use` on the family.** Registering a member sets a declared reference to
   the `ResourceFamily`, so ADR-0001's reference rule applies unchanged: the
   writer must hold `use` on the referenced instance. A family owner controls
   who may join by granting `use`; no bespoke allowlist is introduced. This is
   inert while RD creation is operator-gated and becomes the operative gate
   when ADR-0001 delegates it.

Nothing here requires a family to know its members: membership is declared
outward-in by each RD, so registering a kind never mutates the family object.

Nothing else is required of members. Family kinds need not share spec fields, a
status shape, a controller, or a served version set. A family is not an
inheritance mechanism, and no part of the engine may read a member's spec
through a family-level schema.

Built-in `rise.dev` kinds may declare a family. Because the reserved group
rejects `ResourceDefinition`s, a built-in family is registered in the built-in
registry rather than created through the API: `BuiltInKindDefinition` gains a
`family` field and its `ResourceFamily` is seeded alongside the kinds. No
built-in family is declared by this decision — see the note under Consequences
on why the policy kinds are not one.

### Addressing

A family collection route carries **no version segment**. Diverse schemas imply
diverse versioning: members version independently, a result is heterogeneous,
and a single collection-wide version would either exclude members that do not
serve it or assert a uniformity that does not exist. Each item is served at its
own kind's preferred served version and self-describes through its
`apiVersion`, exactly as a mixed result must.

That makes the family route a second, shorter grammar beside the kind route,
distinguished by a leading keyword rather than by parsing:

```
{group}/{version}/{plural}/{ancestor}…        # kind collection (existing)
families/{group}/{plural}/{ancestor}…         # family collection
families/{group}/{plural}/{ancestor}…/{name}  # family item
```

List versus get follows from depth exactly as it does for kinds: `D` ancestor
segments list, `D+1` gets, where `D` is the family's parent-chain depth — well
defined because every member shares the family's parent.

### Lifecycle

Deleting a `ResourceFamily` is **rejected while any member RD names it**. A
family is a namespace its members and their instances depend on; cascading
would let one delete of a registry object destroy customer data, and
tombstoning would leave instances in a pool with no contract. Deleting the
member RDs first is the deliberate, visible path.

`columns` are **append-only**. Adding one is safe by the unbound-renders-empty
rule. Removing or retyping one breaks existing member bindings and any client
rendering the table itself (columns are public API surface — see below), so
both are rejected rather than left to operator discipline.

### Storage and uniqueness

`family` is denormalized onto `resource_store.resources` at write time — the
store already resolves the RD on every write, and a unique index cannot reach
across tables. Two partial unique indexes mirror the existing kind-scoped pair:

```sql
CREATE UNIQUE INDEX resources_child_family_name_unique
    ON resource_store.resources (parent_uid, family, name)
    WHERE parent_uid IS NOT NULL AND family IS NOT NULL;

CREATE UNIQUE INDEX resources_root_family_name_unique
    ON resource_store.resources (family, name)
    WHERE parent_uid IS NULL AND family IS NOT NULL;
```

Concurrent creates of two different kinds claiming one name conflict in a
single index on a single table, so no new locking is required. The index also
backs the family-scoped `(parent_uid, family, name)` lookup that named
addressing needs.

### Ordering

Family lists sort by `name`, which is unique within a family scope by
construction. Two implementation constraints:

- Sort `ORDER BY name COLLATE "C"`. Resource names admit `-` and `.`, and
  non-`C` collations treat punctuation as ignorable at the primary level, so
  ordering would otherwise be locale-dependent and unstable across databases —
  unacceptable if a cursor is ever a name value.
- Keep `uid` as a tiebreak. It costs nothing and degrades a uniqueness bug into
  a stable-but-odd order rather than a pagination cursor that skips or repeats
  rows.

### Printer columns

A family list is heterogeneous, so a column cannot be a single path evaluated
against every row: members share no spec shape, and requiring one would make a
family the inheritance mechanism this decision refuses to make it. The
declaration therefore splits in two.

**The family declares the contract** — an ordered list of columns, each with a
`name` (the header, and the identifier members bind against), a `type`
(`string`, `integer`, `number`, `boolean`, `date`), an optional `description`,
and an optional `priority` (`0`, the default, always renders; higher values
render only under `-o wide`).

**Each member binds the data**, per version, alongside that version's `schema`
— the place where shape already varies:

```yaml
kind: ResourceFamily
spec:
  columns:
    - {name: Status,   type: string, description: Provisioning state}
    - {name: Endpoint, type: string, priority: 1}
---
kind: ResourceDefinition
spec:
  kind: AwsRdsInstance
  family: extensions
  versions:
    - name: v1
      familyColumns:
        Status: .status.phase
        Endpoint: .status.endpoint
```

Rules:

- **An unbound column renders empty**, and is never a registration error.
  Otherwise adding a column to a family breaks every member RD already
  registered against it.
- **Bindings match by column `name`**, exactly. A binding naming a column the
  family does not declare is rejected at RD registration.
- **Paths use a restricted JSONPath subset** — field traversal and array
  indexing only, no filters, recursion, or wildcards. Evaluation stays bounded
  and deterministic, which matters once RD creation is delegated beyond
  operators.
- **Types are advisory at registration.** A version's `schema` is optional, so
  the store generally cannot prove a path yields the declared type; a value
  that does not match renders empty rather than failing the request. Where a
  version does declare a schema, registration may validate the binding against
  it.
- **`NAME` and `KIND` are implicit and always present**, first and second;
  family columns follow in declared order; `AGE` renders last.

**Tables are rendered server-side**, through content negotiation on the
ordinary collection GET rather than a subresource. A client sends one request
and receives cells rather than whole objects, and no client reimplements path
evaluation.

This is a convenience and a transfer optimization, never a security boundary.
`ResourceFamily` and `ResourceDefinition` are ordinary resources, so a client
holding `get` on them reads the column contract and the members' bindings and
may render the same table itself, from the same objects the server would have
read. Column definitions are therefore public API surface, not private
configuration.

The transfer saving is on the wire and in client parsing; the store still reads
whole rows to evaluate paths. A database-side saving is available but not
automatic: the bound paths can be extracted in SQL, selecting scalars instead
of whole `spec`/`status` documents. That extraction must run *after* the
per-item read decision, which is decidable from metadata and ancestry alone —
so the shape is: read metadata for the scope, authorize per item, extract
columns only for items that cleared `get`. Worth doing when a family list is
measurably slow, not before.

The per-item read decision is the notable consequence: column *values* follow
ADR-0001's per-item read granularity exactly. `KIND` is free — the list
projector's base-field allowlist includes it — but a column pathing into `spec`
or `status` has nothing to read for an item the caller can `list` but not
`get`. Those cells render empty, so a table is ragged across rows of differing
access. Widening the projection because a column was declared would let an RD
author make fields readable under `list` alone, which is a policy hole, not a
formatting choice.

### Authorization

A family confers no permissions. ADR-0001 keys policy on `ResourceKind` and
admits only `kinds: "*"` as a wildcard; a family that implicitly granted across
its members would be an escalation surface.

Consequently a family-scoped list is an ordinary ADR-0001 collection read,
evaluated per item against its own kind, with non-listable items omitted and
masked rather than refused — otherwise `risectl get extensions` becomes a probe
for which extension kinds exist under a project. A family-scoped *get* resolves
a name to a kind and then evaluates `get` on that kind; a caller without it
receives not-found, never a 403 that would disclose which kind holds the name.

A family-scoped list is a collection route, not a subresource. It is outside
ADR-0002's registration seam entirely.

## Derived requirements on the resource API

The intended consumer is `risectl`, a future CLI speaking only to the generic
resource API, kubectl-shaped but optimized for Rise's hierarchical resource
tree. Two commands drive the design:

```console
$ risectl get extensions acme-corp/web-app          # every kind in the family
NAME        KIND               STATUS      AGE
analytics   SnowflakeOAuth     Ready       12d
db          AwsRdsInstance     Available   30d
uploads     AwsS3Bucket        Ready       5d

$ risectl get extension acme-corp/web-app/db        # one named item
```

They force four things the API does not have today:

**Singular and short names.** `ResourceDefinitionSpec` carries only `plural`.
`get extension` versus `get extensions` needs `singular`, and kubectl parity
needs `shortNames` — on kinds and on families alike, all in one identifier
namespace.

**A discovery endpoint.** None exists. risectl cannot resolve `extensions` to a
family, enumerate its member kinds, learn their parent-chain depth, or pick a
served version without hardcoding. Family-aware discovery is therefore a
prerequisite for the CLI, not an enhancement, and must report families and
their members alongside per-kind aliases, parent chain, and served versions.
Column definitions need not be duplicated there: a client that wants them reads
the `ResourceFamily` and the member RDs directly.

**The positional path is the scope.** `<org>/<project>[/<name>]` is the
existing URL grammar minus `{group}/{version}`, whose ancestor *types* are
already derived from the leaf's parent chain. List versus get follows from
depth: `D` ancestor segments list the collection, `D+1` gets an item. This is
the deliberate divergence from kubectl — the hierarchy is the argument, not an
`-n namespace` flag.

**Heterogeneous table output.** One table, not one table per kind, with `KIND`
as an ordinary column and family-declared columns beside it — the printer-column
split above exists to serve this.

## Open questions before Proposed

- **Per-kind printer columns.** A family list is covered above, but a
  single-kind list (`risectl get awsrdsinstances …`) still has no columns
  beyond the base ones. Presumably the same binding mechanism with a per-RD
  contract, but it is a separate decision and not required by this one.
- **Cross-kind pagination and Watch.** Both are already prerequisites for the
  typed-object migration (`ROADMAP.md` §4); a family list inherits them and may
  add constraints of its own — notably a cursor that stays stable while a
  member kind is registered or removed mid-page.

## Consequences if adopted

- The extension name guarantee users have today survives the move to per-kind
  `ResourceDefinition`s, and `rise extension list` keeps its single query.
- The store carries a denormalized `family` column that must be written from
  the RD on every create — a new write-time coupling between the RD registry
  and the resource row, in the same position as existing kind resolution.
- Adopting a family for kinds that already have instances is not possible
  without a collision-reconciling migration. Families must be declared when a
  kind is first registered — for extensions, that means this decision lands
  *before* the `ROADMAP.md` §4 extension migration, not after.
- `ResourceDefinitionSpec` grows `family`, `singular`, `shortNames`, and
  per-version `familyColumns`; `BuiltInKindDefinition` grows `family`; and
  `ResourceFamily` arrives as a new built-in kind. The schemas under
  `docs/engineering/public/schemas/` regenerate with them.
- The API gains server-side table rendering — a restricted JSONPath evaluator
  and a table content type — which no current endpoint needs.
- A family's `columns` become public API surface, readable by any client that
  can `get` the `ResourceFamily`. Hence append-only.
- The API gains a second collection grammar under a `families/` keyword,
  unversioned where the kind grammar is versioned. Discovery, client libraries,
  and any path-aware middleware must handle both.
- **A family asserts that its members' names must not collide — a semantic
  claim, not just a listing convenience — and for some obvious groupings that
  claim is false.** The org-parented policy kinds are the sharp case: `Role` and
  `RoleBinding` share a parent and a group, so they *could* form a family, but
  naming a binding after the role it binds is a common and useful pattern that a
  shared pool would forbid. Built-ins may declare families; that is not a reason
  for any particular set of them to be one. Plausible first candidates are the
  org-parented subject kinds (`Group`, `ServiceAccount`) and the root-scoped
  identities (`User`, `Controller`), each deferred to its own decision.
- Discovery becomes a hard dependency of the CLI workstream.

## Alternatives considered

**One `Extension` kind with a discriminated `spec.type`.** Mirrors today's
table exactly, needs no store change, and preserves both the name pool and
single-query listing for free. Rejected because it puts per-provider schema
validation back inside one union spec, which is much of what moving to per-kind
`ResourceDefinition`s was meant to buy, and because it solves the problem only
for extensions — any later set of sibling kinds faces it again.

**Make the API group the name pool.** Requires no new field: uniqueness would
key on `(parent, group, name)`. Rejected because a group is a namespace for
kind identity, not a product grouping — it would force unrelated kinds sharing
a vendor's group into one pool, and would still leave polymorphic listing
without a way to name which kinds belong together.

**One table per member kind, each with its own columns** (`kubectl get all`).
Sidesteps the heterogeneous-column problem entirely and needs no family-level
column contract. Rejected because the result is not one sortable, scannable
list — the thing that makes `KIND` a column rather than a section heading — and
because it scales badly as a family grows: three providers become three
stanzas to read.

**A single family-level path per column.** One JSONPath evaluated against every
member, no per-member binding. Rejected because it only reaches fields every
member happens to share, so either the columns stay trivial or the family
acquires a de-facto common schema — exactly the inheritance this decision
refuses.

**Client-side aggregation.** risectl could fan out one list per kind and merge.
Rejected because it needs the same discovery data anyway, gives no name-pool
guarantee, multiplies round trips by member count, and pushes cross-kind
ordering and pagination into every client.

**Do nothing.** Accept cross-kind name collisions and per-kind listing.
Rejected on the listing requirement above, and because the resulting collision
is a silent user-visible regression rather than a deferred feature.

## References

- ADR-0001 §collection authorization — per-item `list` filtering, the response
  projector's base-field allowlist, and existence masking.
- ADR-0002 — the subresource execution seam a family list is *not* part of.
- `ROADMAP.md` §4, Typed-object migration — the extension-kind migration this
  decision must precede.
- [Generic resource API](../generic-resource-api.md) — path grammar, parent
  chains, and discriminators.
