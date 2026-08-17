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

`ResourceDefinitionSpec` gains an optional `family` field naming the family the
kind joins. A family has no separate registry object; it exists as the set of
RDs that name it.

Registration validates:

1. **Same API group.** Every member kind's `group` equals the family's group.
   Without this, any RD author could inject a kind into another group's family,
   consuming names in its pool and appearing in its listings. RD creation is
   operator-gated today, so this is latent — but ADR-0001 delegates creation,
   so the rule is encoded now rather than retrofitted.
2. **Same parent kind.** An RD declares exactly one parent. Members that parent
   under different kinds have no common scope, leaving "unique within the
   family under this scope" undefined and family-scoped listing unaddressable.
3. **Immutable after RD creation.** `family` may be set only at RD create.
   Adding one to a kind with live instances would require reconciling existing
   collisions; removing one would silently widen the pool under resources that
   were admitted under the narrower rule.
4. **Shared identifier namespace.** Family names are unique against each other
   *and* against every kind's `plural`, `singular`, and `shortNames`
   (below), and follow the existing collection-name grammar and reserved-name
   list. A CLI argument resolves to exactly one family or one kind, never both.

Nothing else is required of members. Family kinds need not share spec fields, a
status shape, a controller, or a served version set. A family is not an
inheritance mechanism, and no part of the engine may read a member's spec
through a family-level schema.

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
NAME        KIND               AGE
analytics   SnowflakeOAuth     12d
db          AwsRdsInstance     30d
uploads     AwsS3Bucket        5d

$ risectl get extension acme-corp/web-app/db        # one named item
```

They force four things the API does not have today:

**Singular and short names.** `ResourceDefinitionSpec` carries only `plural`.
`get extension` versus `get extensions` needs `singular`, and kubectl parity
needs `shortNames`. Both join the shared identifier namespace above.

**A discovery endpoint.** None exists. risectl cannot resolve `extensions` to a
family, enumerate its member kinds, learn their parent-chain depth, or pick a
served version without hardcoding. Family-aware discovery is therefore a
prerequisite for the CLI, not an enhancement, and must report families and
their members alongside per-kind aliases, parent chain, and served versions.

**The positional path is the scope.** `<org>/<project>[/<name>]` is the
existing URL grammar minus `{group}/{version}`, whose ancestor *types* are
already derived from the leaf's parent chain. List versus get follows from
depth: `D` ancestor segments list the collection, `D+1` gets an item. This is
the deliberate divergence from kubectl — the hierarchy is the argument, not an
`-n namespace` flag.

**Heterogeneous table output.** `KIND` as a column is free: ADR-0001's list
projector allowlists `apiVersion`, `kind`, and `metadata`, so it renders even
for items the caller can `list` but not `get`. Beyond NAME/KIND/AGE, a
cross-kind table can only show columns every member carries — see open
questions.

## Open questions before Proposed

- **Printer columns.** Either family lists stay at base metadata columns, or a
  family declares its own printer columns resolved by JSONPath into each member
  object (with kinds declaring their own for single-kind lists). The latter is
  more useful and more machinery; undecided.
- **Family membership governance.** Once RD creation is delegable beyond
  operators, same-group is the only gate on joining an existing family, which
  reduces to "who governs an API group" — a concept Rise does not have yet.
- **Route shape for family collections.** Whether a family occupies the same
  URL position as a `plural` (relying on the shared identifier namespace) or a
  distinct segment.
- **Cross-kind pagination and Watch.** Both are already prerequisites for the
  typed-object migration (`ROADMAP.md` §4); a family list inherits them and may
  add constraints of its own.
- **Whether built-in kinds may declare families.** Nothing here requires it,
  and admitting it early risks a family becoming a de-facto type hierarchy.

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
- `ResourceDefinitionSpec` grows `family`, `singular`, and `shortNames`, and
  the schema under `docs/engineering/public/schemas/` regenerates with it.
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
