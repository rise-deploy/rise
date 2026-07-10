---
title: "ADR-0001: Unified Permission Model"
---

## Status

**Proposed** (under review). Date: 2026-07-10.

Scope: the generic resource API (`/api/v1/resources/...`) and
ServiceAccount/Controller token issuance (`POST /api/v1/auth/token`). It does
not change how `rise project create`, `rise deployment create`, or other
typed-table-backed CLI commands work; those converge onto this model
automatically once their tables migrate onto the generic resource store, which
is separate, already-planned work (ROADMAP.md, Workstream 1 Phase B).

## Context

Rise today has several disjoint authorization mechanisms, each with its own
code path. The generic resource API is operator-only: access is gated on
membership in the `auth.operator_users` config allowlist, with no finer
granularity. The typed APIs (projects, teams, deployments, …) each carry their
own per-endpoint ownership and membership checks, with `auth.admin_users`
bypassing them wholesale. Controllers authenticate separately and are
authorized for status/finalizer writes through each `ResourceDefinition`'s
`allowed_status_controller_ids` allowlist. ServiceAccounts get tokens through
trust-policy-gated token exchange. Five subject populations, five ways of
deciding "may this caller do this."

Almost none of this surface is load-bearing yet. The generic resource API's
only production use today is seeding the default `Organization` resource from
config, and controller authentication is entirely unused. This design
therefore carries no backwards-compatibility constraints on the resource-store
API or on controller authentication: where the model below conflicts with the
current surface, the surface changes — breaking changes are acceptable and
preferred over compatibility shims that would bake in tech debt before there
is anything to be compatible with.

The requirements come from the multi-tenancy split: three distinguishable
tiers — platform operator, org admins, org users — with per-org *asymmetric*
restrictions. The operator must be able to impose a ceiling on one specific
org (a compliance-restricted customer), and an org must be able to impose a
tighter ceiling on itself, with both enforced simultaneously. Org admins must
be able to delegate access further, but never beyond their own boundaries. And
it should be *one* runtime-configurable mechanism covering Users, Teams,
ServiceAccounts, Controllers, and Operators alike — not five code paths that
happen to agree.

The design below was converged through multiple independent adversarial-review
rounds; its wording is deliberate, particularly around the security-sensitive
edges (label-write gating, token minting, operator bootstrap).

## Decision

Every actor in Rise — a person, a team, a CI service account, a controller
process, or a platform operator — is a **subject**. Every subject's access to
every resource is decided the same way, by the same evaluator, regardless of
what kind of subject it is.

Resources live in a tree — an Organization contains Projects, which contain
Environments, which contain Deployments, and so on (e.g. an Environment
`env-prod` under the org `acme-corp`).
Access is granted by binding a **Role** (a named bundle of permissions — "can
update Deployments, can read Environments," built from `verbs` like
`get`/`update`/`delete`) to a subject, placed at some point in that tree. A
binding's grant applies to everything at or below where it's placed, and —
optionally — can be narrowed further to only the resources carrying a specific
label.

A subject's effective access on a resource is the combination of everything
its applicable bindings grant, **minus** anything any of them explicitly
denies. Denial always wins over allowance, wherever in the tree it comes from
— there's no "more specific wins" precedence between two ordinary bindings;
it's purely "if anything applicable denies it, it's denied." This is what lets
an organization say "everyone on my team can edit Deployments, except nobody
may ever delete one in `prod`" (by placing a Deny specifically for `prod`),
but it's not a general rule that narrower automatically overrides broader — a
Deny placed at the org root would just as effectively block a narrower Allow
underneath it. That combined result is then capped by however tightly the
platform operator and that resource's own organization have chosen to
constrain things.

The cap is a stack, checked top to bottom, most-restrictive-wins:

1. **The platform operator's global limit** — applies to every organization, always.
2. **A limit the operator has imposed on one specific organization** — optional, e.g. for a compliance-restricted customer.
3. **A limit that organization has chosen for itself** — optional self-governance.
4. **The actual grant** — a Role bound to a subject, or a Role's own definition being edited. Valid only if it fits inside whatever the first three layers resolve to, *and* the person making the write already holds what they're handing out themselves. This second check happens once, at the moment of the write — unlike the three ceiling layers, it is not re-checked on every later use of an already-existing grant.

All four layers' *ceiling* comparison is re-checked live on every single
request against the database — nothing is cached or baked into a token, so
tightening a ceiling takes effect immediately for everyone relying on it. This
is what makes revoking a Role exactly as effective as revoking a token before
it expires — but only for the identity the token belongs to: narrowing what a
ServiceAccount itself can do immediately narrows every outstanding token for
it. Revoking the separate grant that let someone *mint* that token in the
first place does not reach back and affect a token already issued (§7).

Ownership works through this same mechanism, not a separate one. A resource
can carry a label — `rise.dev/owner: platform` — naming the team or person it
belongs to. The platform ships one built-in rule: whoever that label names
automatically gets an owner-level Role on the resource. Nothing about that
rule is hardcoded to any specific label key — it's ordinary Role/binding data,
and any organization can replace it with their own version using the exact
same tools they'd use for any other grant. Because relabeling a resource can
silently redirect who holds that owner-level Role, writing such a label goes
through the identical check as writing a binding directly: you can only
redirect access you already hold.

The numbered sections below (§1–§10) are the concrete design this
plain-language model compiles down to.

### 1. Subjects

Five kinds of subject exist, in two groups.

**Org-agnostic — User, Controller.** A single identity can legitimately hold different roles in multiple different organizations: a person is a member of two customers' orgs with different access in each; a single Controller process reconciles resources across many orgs. Nothing about the subject's identifier ties it to one organization. A binding for one of these kinds names an explicit `Scope` (§4) — either one specific org, or a wildcard (`"*"`) meaning "the default for every org this identity touches, unless a more specific binding exists for that org."

**Org-native — Team, ServiceAccount.** These exist within exactly one organization by construction: a Team has one owning org, and a ServiceAccount is created and lives directly under an org (`acme-corp/serviceaccount:ci-bot`) — a sibling of Project in the resource tree, not nested under any one Project. This is deliberate: a ServiceAccount's reach comes entirely from what it's bound to via ordinary RoleBindings (§4), which can span any number of Projects/Environments in its org; tying its identity to a single "home" Project would suggest a relationship that has no bearing on what it can actually do, and would couple its `effectiveLabels`-inherited attribution (§6.1) to whichever Project happened to parent it. The org is baked into the identifier itself. A **static** binding (a literal, fixed subject — §4) for these kinds either omits an explicit `Scope` (inferred from the subject's own org) or, if one is supplied, it must match the subject's own org. A **dynamic** binding (a subject template — §4, §6.3) has no concrete subject to infer an org from until it is evaluated against a specific resource; §6.3 states how its resolved subject's org is determined in that case.

**Operator** is a platform-wide root identity, bootstrapped from configuration (an email allowlist) exactly as today — that doesn't change. What changes is how the status is *expressed*: rather than a hardcoded bypass branch in the evaluator, operator status is membership in one reserved subject, `system:operators` (a `system:`-prefixed name is reserved for platform-recognized pseudo-subjects, never an ordinary User/Team/ServiceAccount/Controller row). The platform seeds exactly one binding for it:

```
Subject: system:operators
Scope:   "*"
Role:    system-admin = { Allow: * on * }
```

An operator's request runs through the *same* evaluation algorithm as anyone else's (§4 steps 1–3) — no separate code path. The one thing that still has to be special-cased is step 4, ceiling intersection: Layer 1 (the instance ceiling) is itself operator-authored, so checking `system:operators`' own grant against it would let an operator accidentally lock themselves — and everyone else — out by narrowing Layer 1, with no one above an operator able to fix it. Step 4 is therefore skipped for `system:operators` specifically; every other subject, including org-admins, is fully subject to it. The granter-subset half of Layer 4 needs no special-casing at all: since `system:operators` always holds `Allow: * on *`, any grant an operator hands out trivially satisfies `⊆` their own effective permissions.

**Membership never replaces a caller's own identity.** When `niklas@example.net`, listed in the operator allowlist, makes a request, his subject is `user:niklas` — exactly what it would be if he weren't an operator at all. What differs is **membership expansion** (§4 step 1): evaluating a subject `S`'s own request considers not only bindings that target `S` directly, but also any binding targeting a group `S` currently belongs to, checked live on every request. Team membership and `system:operators` allowlist membership are both instances of this one rule, no separate code path per group kind. Since the only binding targeting `system:operators` grants `system-admin`, niklas's combined policy for that request includes `Allow: * on *`, unioned with whatever he separately holds as `user:niklas`. Remove his email from the allowlist, and his very next request no longer draws on it — nothing to revoke, no propagation delay, the same live-recheck property as everything else in this model (§5). This is also what makes the default ownership binding (§6.2) actually reach a human in the first place: it targets a resolved Team, and a member of that Team benefits from it only through this same expansion.

**Group subjects.** Two reserved group forms exist beyond Team: `system:authenticated` — every authenticated subject, of any kind — and `org:<name>` — every subject belonging to that organization: its org-native subjects (Teams, ServiceAccounts) and its user members alike. Both resolve through the same membership expansion as Teams and `system:operators`; there is no separate code path per group kind. ServiceAccount inclusion in `org:<name>` is deliberate and load-bearing: grants addressed to a whole organization (e.g. `use` on a platform-provided resource, §9) must reach the org's CI identities, or every machine-driven flow fails exactly where a human's would succeed.

**The binding is data; the membership is not — deliberately.** `system:operators`'s grant (the binding above) is a stored row, same table as every other binding. Whether a given identity is currently *in* `system:operators` is never stored anywhere in this model — it's synthesized at evaluation time from the config allowlist, which lives outside the resource store entirely. This is forced by the same bootstrap problem the Operator concept exists to solve: if membership were itself an ordinary RBAC record, granting the first one would require an already-privileged actor to write it, and nothing could ever create that first record. The config allowlist is the one piece of trust in this model that has to originate from outside the system Rise itself governs.

The binding has no equivalent forcing problem — it's never granted by anyone at runtime, only seeded once at bootstrap — so it can safely be data, with one refinement. Being immutable through the ordinary write path (§5's **seeded** Role-ownership tier: no write path can ever modify it, not even an operator) only protects against mutation through this model's own API — it says nothing about a bad migration, a restore from an old backup, or direct database access losing the row entirely, outside any write path this model governs. That residual risk is unacceptable for the one subject with no recovery authority above it, so `system:operators` resolving to `system-admin = { Allow: * on * }` is a fact the evaluator **guarantees unconditionally** — the same hardcoded way step 4 is already skipped for it, above — not something solely read from, and therefore losable with, a table row. The row is still materialized alongside that guarantee, purely so the same explain/audit tooling that inspects everyone else's access can inspect this one too without a special case; if it's ever found missing or altered outside the write path, that's healed by re-materializing it, not a live authorization dependency.

This mirrors how Kubernetes actually handles `system:masters`: a hardcoded superuser check in the authorizer grants it full access with no ClusterRole or ClusterRoleBinding required at all, *and*, redundantly, an ordinary `cluster-admin` ClusterRoleBinding also binds the same group to the same power as a stored object — kept self-healing (missing permissions/subjects on default, `kubernetes.io/bootstrapping=rbac-defaults`-labeled objects are restored automatically) rather than merely immutable. Every other `system:`-prefixed built-in role (`system:node`, `system:kube-scheduler`, etc.) gets only the self-healing-data half, no hardcoded bypass, because losing one of those is recoverable by whoever holds `system:masters` — the same distinction already drawn above between `system-admin` (nothing above it, needs the hardcoded guarantee) and `org-admin` (recoverable by an operator, doesn't). Kubernetes' authorization decisions are live on every request in both cases, same as this model's throughout (§5); what's actually startup-scoped there is narrower — only the drift-repair of default objects' stored contents, not authorization itself.

**Wildcard resolution.** When two bindings target the same `(Subject, LabelSelector-key-if-any)` pair — one with `Scope: "*"` and one with a more specific `Scope` — the more specific one **replaces the wildcard outright, for that scope** — it does not merge with it. "Same subject" for this comparison means the same literal subject, or the same subject *template* text; a dynamic binding on `LabelSelector: {key: rise.dev/owner}` never collides with one on `LabelSelector: {key: rise.dev/squad}`, even if both use the identical template `team:${ref.name}` — they are different rules. This comparison is always performed on the binding's *authored* Subject field exactly as written — literal string against literal string, or raw template string against raw template string — never on a resolved value: a literal binding (`Subject: team:platform`) and a dynamic one (`Subject: team:${ref.name}`) never collide with each other, even for a resource where the template happens to resolve to `platform`, so a platform-wide dynamic default is never silently discarded just because one particular resource's resolved subject happens to match some unrelated static binding. Where a `LabelSelector`'s optional `value` also differs between two otherwise-colliding bindings, replacement is evaluated per-resource, at the same point §4 step 1 collects applicable bindings, not as a blanket scope-wide swap — a `value`-narrowed selector only matches (and so only competes with and replaces a broader same-key selector for) resources whose label actually equals that value; resources carrying any other value never collect the narrowed binding in step 1, so the broader selector continues to govern them, undiminished. This replacement rule applies to any subject (not only Controller) whenever a wildcard `Scope` is in play, including the dynamic ownership bindings in §6. It exists to keep "what does this rule resolve to, in this org" a single, unambiguous answer instead of an additive combination of whatever bindings happen to apply — the one place bindings do not simply combine (§4 covers the ordinary, additive case).

**Accepted risk.** Because replacement is outright rather than merged, an org-specific binding can unintentionally discard everything a wildcard binding provided beyond what the org-specific one restates — e.g. narrowing a shared Controller's wildcard grant in one org for one reason can silently strip that Controller of unrelated capabilities it still needed there. There is no dry-run/impact-preview step (§5's ceiling-tightening risk acceptance applies equally here) — this is an accepted, unmitigated footgun for both ceilings and wildcard replacement.

### 2. Verbs

`get`, `list`, `create`, `update`, `delete` — Rise's existing action vocabulary — plus:

- **`updateStatus`** and **`updateFinalizers`**, splitting what was one coarse subresource verb. Finalizers gate deletion and are more sensitive than routine status updates; a controller can be granted one without the other.
- **`mintToken`** — applies to a ServiceAccount or Controller identity *as a resource in its own right*, not to whatever that identity subsequently acts on. See §7.
- **`use`** — the right to *reference* a resource from another resource's fields, distinct from reading or editing it. Checked at write time of the *referencing* resource, against its writer, wherever a `ResourceDefinition` declares a reference (§9). Granting `get` without `use` makes a catalog browsable but not selectable; `use` without `get` allows selection by name without exposing the referenced object's contents. (Precedent: the Kubernetes `use` verb on PodSecurityPolicies.)

### 3. Roles and the Allow/Deny evaluator

A **Role** is a named, reusable **policy**: an ordered-irrelevant list of statements,

```
{ effect: Allow | Deny, kinds: ["Deployment"] | "*", verbs: ["update", "delete"] | "*" }
```

A subject's access on `(verb, kind)` under a given policy is permitted iff at least one `Allow` statement matches **and** no `Deny` statement matches — Deny wins. This exists because pure-additive, union-only permission sets cannot express subtraction from a wildcard: "everything except `delete` on `Environment`" has no faithful positive encoding when the set of resource kinds is open-ended (operators register new kinds at runtime via `ResourceDefinition`, Rise's existing mechanism for registering a new resource kind's schema) — enumerating every other kind explicitly would silently exclude any kind registered later. A `Deny` statement expresses it directly:

```
Allow: * on *
Deny:  delete on Environment
```

Roles and RoleBindings are data (rows), not compiled match arms — operators and org-admins configure who can do what at runtime, no redeploy required. Ceilings (§5) are the same policy shape, reused rather than reinvented. A Role's own statement list is, like any other resource, ordinarily writable by whoever holds `update` on kind `Role` — but because editing a widely-bound Role changes what every subject bound to it can do, that write is gated the same way a RoleBinding write is (§5).

### 4. RoleBindings — targeting a subject to a slice of the resource tree

A **RoleBinding** attaches a Role to a subject, at a `Scope`, optionally narrowed by a `LabelSelector`:

```
Subject:        <literal subject, e.g. team:platform>  |  <subject template, e.g. team:${ref.name}>
Scope:          <path, e.g. Environment/acme-corp/env-prod>  |  "*"   # always present (defaults to "*")
LabelSelector?: { key: <label key>, value?: <fixed value> }      # optional narrowing filter
Role:           <Role name>
```

`Scope` is always present (it defaults to `"*"`, the whole tree, if omitted) and establishes where the binding is placed — it applies to the named node and everything beneath it. `LabelSelector`, when present, doesn't replace `Scope` as a separate targeting mode — it narrows the grant to only the resources *within* that scope whose `effectiveLabels` (§6.1) match. A binding with no `LabelSelector` grants over its entire scoped subtree; a binding with one grants only over the subset of that subtree carrying the matching label.

A `Scope` path is written exactly like a resource URL with the `{group}/{version}` prefix dropped (§8): the target's **kind** first, then its ancestor names root-first, then its own name — `Environment/acme-corp/env-prod` is the Environment `env-prod` under org `acme-corp`; `Organization/acme-corp` is the org itself; `RuntimeClass/standard` is a root-scoped instance (§9). Ancestor kinds are derived from the leaf kind's declared parent chain, the same resolution the URL grammar already performs — one path grammar, not two. (A distinct separator between the kind and the path was considered for visual clarity and rejected to keep `Scope` byte-identical to the URL form — see Alternatives considered.)

**Static** targeting — a fixed subject:

```
Subject: team:platform
Scope:   Environment/acme-corp/env-prod
Role:    deployment-editor
```

```
Subject:       team:platform
LabelSelector: { key: rise.dev/team, value: "platform" }
Role:          project-editor
```

(Role names other than `resource-owner`, §6.2, are illustrative throughout this document — `project-editor`, `deployment-editor`, etc. are examples of Roles an operator or org would define, not literal platform-shipped defaults.)

A `LabelSelector` carrying a `value` pairs with a static Subject — an equality filter on an already-fixed grant, no extraction needed. One without a `value` pairs with a dynamic Subject — an existence match whose matched value feeds `${ref.name}` (below). A static Subject combined with a value-less `LabelSelector` is **rejected at write time**: it would grant a fixed subject access to any resource carrying *any* value for that key, regardless of what it says, which is never the intent of a literal, non-templated binding. A dynamic Subject combined with a value-carrying `LabelSelector` is accepted but redundant — the template can only ever resolve to that one fixed string, so the same policy is more simply written as a static binding naming that value directly.

**Dynamic** targeting — the subject is resolved from the matched label's own value at evaluation time, via string-template interpolation:

```
Subject:       team:${ref.name}
LabelSelector: { key: rise.dev/owner }
Role:          resource-owner
```

Evaluating a dynamic binding against a resource is two independent steps: resolve the `LabelSelector` against the resource's `effectiveLabels` to get a raw string value, then substitute that string for `${ref.name}` in the subject template and hand the resulting string to the same subject-resolution code any static binding uses. The template step needs no per-kind knowledge of what the substituted value "means" — it is plain string interpolation, decoupled from subject parsing. §6.3 covers how the resolved subject's organization is determined for org-native kinds.

**Evaluation algorithm**, for subject `S` requesting `(verb, kind)` on resource `r`:

1. Collect every binding targeting `S`, or a group `S` currently belongs to (§1's membership expansion), or a template that resolves to `S` against `r`, whose `Scope` covers `r` (ancestor-chain membership; `"*"` covers everything) and whose `LabelSelector`, if any, matches `r.effectiveLabels`.
2. Apply wildcard-replacement (§1): where both a wildcard-`Scope` and a more-specific-`Scope` binding apply for the same `(Subject, LabelSelector key)` pair, drop the wildcard one.
3. Union the surviving bindings' Role policies into one combined policy; evaluate `(verb, kind)` against it (Allow-with-Deny-wins, §3) to get `S`'s raw grant on `r`. If no binding survives step 2, the combined policy is empty, no `Allow` statement can match, and the result is **denied** — there is no implicit grant.
4. Intersect the raw grant against the resolved ceiling for `r`'s organization (§5) — this check runs on every request, live.

A **worked trace**: `team:platform` requests `delete` on `Deployment/acme-corp/env-prod/foo`, which carries `rise.dev/owner: platform`.

- Step 1 collects two bindings: (a) a scope binding at `Environment/acme-corp/env-prod` granting `deployment-editor` = `{Allow: * on Deployment}`; (b) the seeded dynamic ownership binding (§6.2), which resolves to `team:platform` via the `rise.dev/owner` label and grants `resource-owner`.
- Step 2: no wildcard/specific collision between these two.
- Step 3: union = `{Allow: * on Deployment} ∪ resource-owner's statements`; no `Deny` present → `delete` is in the raw grant.
- Step 4: `acme-corp` has no Layer 2/3 ceiling narrower than the default → the raw grant passes through unchanged.
- **Result: allowed.**

Now suppose the org has separately authored, at `Environment/acme-corp/env-prod` specifically, a binding for `team:platform` with Role `{Deny: delete on Environment}` (the org's own "nobody deletes an Environment here" rule, §5). That binding's Role statement is unioned into the same combined policy in step 3 for any *Environment*-kind resource under that scope — for the Environment itself, `Deny` wins and `delete` is denied, even though the broader `deployment-editor` binding would otherwise have allowed it. This is the narrower-binding-subtracts-from-a-broader-one behavior the opening primer describes: it only takes effect where a binding's Role actually carries a matching `Deny`, not merely by virtue of being placed at a narrower scope.

### 5. Ceilings — the four-layer, most-restrictive-wins stack

Applied uniformly across every subject kind, with no asymmetry by *kind* — the same rules apply the same way whether the subject is a person or a machine. The sole exception is `system:operators` (§1), which sits above the stack by construction, not by subject kind — an org-admin and a Controller are treated identically to each other, and both are fully subject to all four layers.

| Layer | Set by | Scope | Default if unset |
|---|---|---|---|
| 1. Instance | Operator | Every org, always | — (root of the stack) |
| 2. Per-org, operator-imposed | Operator | One named org | Layer 1 |
| 3. Per-org, org-declared | That org's own admins (subjects holding the `org-admin` Role — see the scope-only rule in §6.7) | Their own org only | Layer 1 ∩ Layer 2 |
| 4. The actual write | Whoever is making the write | — | must fit inside 1∩2∩3, **and** inside the writer's own current effective permissions |

`org-admin` is a literal platform-shipped Role, scoped to one org (§6.7's scope-only rule). Its exact verb list is deployment-dependent — the same platform-owned-Role mechanism that lets an operator define `resource-owner` (§6.2) also decides how much of an org's own bookkeeping its admins can see, and this is expected to differ by how a given instance is run, not something the architecture fixes:

- **Multi-tenant SaaS default:** `{ Allow: * on *; Deny: [updateStatus, updateFinalizers] on *; Deny: [create, update, delete] on OrganizationPolicy }` — full CRUD within the org, same as today, but never touching platform bookkeeping or the operator-imposed ceiling document (below), both of which org-admins have no operational exposure to and no reason to edit. This mirrors `resource-owner`'s own shipped default, which already excludes `updateStatus`/`updateFinalizers`.
- **Self-hosted, single-team default:** `{ Allow: * on * }`, unrestricted — if the same people run the platform and use it, walling `status`/`finalizers` off from org-admins protects them from a mistake nobody there is actually shielded from anyway.

Both are the identical mechanism, chosen once at deployment time by whoever authors the Role, not two different architectures — and the same lever widens `resource-owner` if a deployment wants owners themselves to touch their own resources' bookkeeping. Layers 1–2 stay available as a hard backstop regardless of what any org-owned Role or §6.5 override later grants: a SaaS operator can Deny these verbs at the instance ceiling so the restriction holds even if some org tries to route around its own `org-admin` definition.

Its own definition, like any Role, is edit-gated by Layer 4 (below) — but because it's a platform-shipped Role rather than an org-authored one (see the Role-ownership rule below), only an operator may edit its statement list; an org-admin holding it cannot widen it for themselves. `org-admin` is a reserved Role name, recognized specially by the platform the same way `rise.dev/owner` is a reserved label key — every organization is provisioned with exactly one `org-admin`-Role binding, `Scope`-targeted to that org, at org-creation time (§10); Layer 3's write authority and §6.7's structural constraint both key off that reserved name directly, not off any separate "is this an admin" flag.

**Roles are owned, and only their owner may edit them — in one of three tiers.** A Role is **seeded** (baked in at platform bootstrap, immutable — no write path can ever modify it, not even an operator; today only `system-admin`, §1, which needs this because there is no authority above an operator left to recover a self-inflicted lockout), **platform-owned** (authored by an operator, usable and bindable by any org, e.g. `resource-owner`, `org-admin` — ordinarily editable, just operator-only, since an operator botching one of these stays recoverable by another operator), or **org-owned** (authored by one org's admins, usable only within bindings scoped to that org). Only an operator may edit a platform-owned Role; only that org's admins may edit an org-owned Role; nobody may edit a seeded one. This closes a case the write-time subset check alone doesn't: without ownership, a Role referenced by bindings across multiple orgs with different ceilings would have no single well-defined "which org's ceiling applies" answer when its body is edited. With ownership fixed, editing an org-owned Role is checked against that one org's ceiling and the editor's own permissions in it (§5's general Layer 4 rule); editing a platform-owned Role requires being an operator, who is still, as for any subject, held to holding what they hand out.

**Set composition.** Each layer is itself a policy (§3's Allow/Deny statement shape). "Layer 1 ∩ Layer 2 ∩ Layer 3" is computed *pointwise*, per `(verb, kind)`: a `(verb, kind)` pair is in the resolved ceiling iff every set layer independently permits it under §3's Allow-and-no-Deny rule. This is never computed by merging statement lists as data — the kind space is open-ended (new kinds are registered at runtime), so only a pointwise, per-request evaluation is well-defined. Numeric policy values (e.g. max token TTL, §7) compose via `min()` across whichever layers set a value — the same four-layer mechanism generalizing from "sets of verbs" to any orderable quantity without a second bespoke mechanism.

**Layer 4 — the general write-time gate.** This check is not limited to authoring a new RoleBinding. It applies to **every write that changes what any subject is effectively granted**: authoring or editing a RoleBinding (its `Subject`, `Scope`, `LabelSelector`, or `Role` reference), editing an existing Role's own statement list, and writing a value to a label that some binding's `LabelSelector` selects on (§6.6) — whether that write happens at resource creation or on an existing resource. Each such write is valid only if the newly-implied grant is `⊆` the resolved ceiling for the affected org **and** `⊆` the writer's own current effective permissions at the moment of the write. For a Role edit, "the affected org" is unambiguous because Roles are owned (above) — an org-owned Role's edits are checked against its one owning org; a platform-owned Role can only be edited by an operator in the first place. Editing a RoleBinding's `Scope` to move it across an org boundary is checked against both orgs — equivalent to independently validating a delete at the old scope and a create at the new one. This is the single canonical statement of the rule; §4's binding examples, §6.6's label-write gate, and §7's token issuance all point back to it rather than restating it.

Unlike the ceiling intersection (which is live, re-checked on every request), this granter-subset half of Layer 4 is checked once, at write time, and not re-evaluated afterward — if the writer's own permissions later shrink, grants they already made are unaffected. (Their permissions shrinking does not retroactively invalidate what they already gave away — only the live ceiling check can later revoke what a grant provides, by tightening Layers 1–3.)

**A concrete Role-write example.** Bob holds only `update` on kind `Role` — a narrow grant for maintaining Role definitions — and holds no other binding. Bob edits the `resource-owner` Role, appending `{Allow: mintToken on *}`. Because Role edits go through the same Layer 4 gate as everything else, this write is checked against Bob's own effective permissions: Bob does not himself hold `mintToken` on anything, so the newly-implied grant is not `⊆` his effective permissions, and the write is **rejected** — even though hundreds of subjects are bound to `resource-owner` via the platform-wide default (§6.2) and would otherwise all have been silently escalated by one edit.

**Layer 2's home.** Layer 2 lives in an ordinary resource, `OrganizationPolicy` — one per Organization, created alongside its `org-admin` binding at org-creation bootstrap (§10) — rather than a bespoke subresource on `Organization` itself. This needs no envelope-level machinery: an org's admins are granted ordinary `get`/`list` on it (a denial is diagnosable, not opaque, without ever needing write access), and by default nobody holds `create`/`update`/`delete` on it except `system:operators` (§1) — an ordinary consequence of no binding granting those verbs to anyone else, not a hardcoded rule. A self-hosted operator who wants their own org-admins to write it directly can grant that like any other verb; a SaaS operator leaves the default in place and, per the `org-admin` definition above, denies it explicitly so the restriction survives even a broad `Allow: * on *` elsewhere in that Role. Layer 3 lives in `Organization.spec`, writable by an org's admins through their ordinary grant on the Organization resource — no bespoke home needed for it either.

**Live, uncached ceiling enforcement.** All four layers' ceiling comparison is resolved fresh on every request. Tightening a ceiling takes effect immediately for every subject currently relying on it, with nothing to rewrite — this is also what makes "revoke the role" exactly as effective as "revoke the token" (§7).

### 6. Ownership and attribution

Ownership is not a bespoke field or a bespoke code path — it is expressed entirely through §3/§4's primitives. (A dedicated single-subject `ownerRef` field was considered and rejected — see Alternatives considered.)

#### 6.1 — Attribution is one governed label

A single reserved key, `rise.dev/owner`, holds a bare name (`platform`, `niklas`) — never a `kind:name` string. The label stores minimal, display-friendly data; the binding that selects on it (§6.2) declares how to interpret that data. Nested resources without their own value inherit one through `effectiveLabels` — a computed field, always resolved live (never stored or cached, consistent with §5's live-evaluation philosophy — both the read-path display value and the authorization-path match in §4 are the same computation), resolved by walking the already-fetched ancestor chain leaf-to-root, **nearest value wins per key**:

```
Project "secret-app"      rise.dev/owner: platform
  └─ Environment "prod"   rise.dev/owner: devops     # more specific, set later

effectiveLabels for "prod":  { "rise.dev/owner": "devops" }
```

A more specific descendant's label shadows its ancestor's; it does not additionally union with it. Restoring broader access on a shadowed resource is always possible — bind another Role at the broader scope — it is simply not automatic. `effectiveLabels` is the one ancestor-inheritance mechanism in the system; ownership reuses it rather than maintaining a parallel one.

#### 6.2 — The default ownership rule

One platform-seeded dynamic binding replaces any implicit "you can act on what you own" logic:

```
Subject:       team:${ref.name}
LabelSelector: { key: rise.dev/owner }
Role:          resource-owner
Scope:         "*"
```

`resource-owner` is a literal platform-shipped Role, defined as:

```
resource-owner = { Allow: [get, list, update, delete] on * }
```

— deliberately excluding `create`, `updateStatus`, `updateFinalizers`, and `mintToken`. Ownership alone never grants the ability to touch finalizers, mint a token for an owned ServiceAccount, or create new child resources; those require a separately-granted Role, same as for any non-owner subject. This is the multi-tenant SaaS default; like `org-admin` (§5), it's platform-owned Role data, and a self-hosted operator who wants owners to see their own resources' bookkeeping widens it the same way `org-admin` is widened — no separate mechanism, same lever. (An organization that wants ownership to imply more, within whatever the deployment's own default already allows, can grant it explicitly via its own override binding, §6.5 — subject to the same Layer 4 gate as any other grant.)

When the resolved subject happens to be the caller themselves, that is simply the self-ownership case falling out for free — no separate condition type is needed for it.

#### 6.3 — Resolving a dynamic subject's organization

A dynamic binding's `Subject` template has no concrete identity until it's evaluated against a specific resource. For an org-native kind (Team, ServiceAccount), the resolved subject's organization is taken to be **the matched resource's own organization** — consistent with those kinds' org-native identity (§1): a `rise.dev/owner: platform` label on a resource under `acme-corp` resolves to `acme-corp/team:platform`, never a `platform`-named team in some other org. The binding's own `Scope` therefore governs a different thing: which resources' evaluations consider the rule at all (its applicability domain — `"*"` for the platform default, one org for an override), not the resolved subject's organization, which is always derived per-resource. This is why §6.2's binding can validly carry both a `LabelSelector` and a `Scope: "*"` without contradicting §1's org-matching rule for static Team bindings — that rule constrains literal subjects; a template's resolved-subject org is constrained separately, per match, as stated here.

#### 6.4 — Individual ownership and organization-specific grouping need no new subject kind

Subject kind stays closed (User, Team, ServiceAccount, Controller — each carries real membership-resolution machinery, not worth making pluggable). Label *keys* are open — any organization can introduce one:

```
# individual ownership — same mechanism, a different kind and key
Subject:       user:${ref.name}
LabelSelector: { key: rise.dev/assignee }
Role:          resource-owner

# an org's own grouping concept — reuses Team, never registers a new kind
Subject:       team:${ref.name}
LabelSelector: { key: rise.dev/squad }
Role:          project-editor
```

A "squad" never exists as a subject kind — it is a Team, targeted via a label key the organization chose to call `rise.dev/squad`. This covers grouping concepts whose *membership* is ordinary Team membership; it does not provide a way to define a group with genuinely different membership semantics (externally-synced, rotation-based, non-exclusive overlapping groups, etc.) — that would require a real pluggable subject-kind registry, which is deliberately out of scope (Alternatives considered).

#### 6.5 — Organizations can override the default

The seeded ownership binding is ordinary `Scope: "*"` data. §1's wildcard-replace rule governs overrides the same way it governs any other wildcard — an org-specific binding for the same `(Subject, LabelSelector key)` pair replaces the platform default outright for that org:

```
Subject:       team:${ref.name}
LabelSelector: { key: rise.dev/owner }
Role:          project-viewer     # ownership implies read-only here
Scope:         Organization/acme-corp
```

The override write still passes the ordinary Layer 4 gate (§5) — no override-specific mechanism.

This "override" works specifically because the default binding uses a wildcard `Scope`, and §1's replace-outright rule applies only to a wildcard-vs-specific collision — it is not a general "narrower always overrides broader" mechanism. Two non-wildcard bindings for the same subject at different tree depths union additively (§4) rather than replace; narrowing access below what a non-wildcard ancestor binding grants requires an explicit `Deny` statement in the narrower binding's Role (§4's worked trace), not simply placing a binding at a deeper scope.

#### 6.6 — Label writes that retarget access are gated by Layer 4 itself

There is no hardcoded list of protected fields. On any write — creation or update — that sets or changes `metadata.labels[K]`:

1. If the value for `K` is unchanged from the resource's current effective value, no gate — ordinary `update` permission suffices.
2. If no binding *anywhere applicable to this location in the tree* (by `Scope` and kind, regardless of whether it currently matches this resource's present labels) selects on `K` via its `LabelSelector`, no gate. This check is evaluated against binding *applicability*, not the resource's pre-write label state — a resource that has never carried key `K` before is still gated on its first write, since a binding selecting on `K` could apply to it the moment the value is set.
3. Otherwise, resolve effective permissions before and after the proposed value (simulated, computed atomically with the write so a concurrent binding/ceiling change cannot open a window between simulation and commit) and diff them. Any newly-implied grant must be `⊆` the writer's own current effective permissions and `⊆` the resolved ceiling — §5's general Layer 4 rule, applied here.

A key becomes gated the moment some binding's `LabelSelector` references it, and stays ungated otherwise: protection is a consequence of binding existence, never a hardcoded field name.

**A narrow, explicit exception applies at creation.** A subject holding `create` on a kind may, in that same creation request, set an owner-selecting label to name only *themselves*, or a Team they are currently a member of (itself an ordinary Layer-4-gated fact — joining a team is its own gated write, not something a creator can manufacture on the fly to widen this exception) — without that specific write needing to independently pass the general subset check. This is not "handing out access you don't hold": there is no prior owner being displaced, only a first claim, and the claim is restricted to identities the creator can already act as. Naming a team the creator does *not* belong to is not covered by this exception and falls back to the general rule: checked `⊆` the creator's own effective permissions like any other grant, and rejected unless they hold some independent basis for it (e.g. being an org-admin).

"Creation" here means bringing a genuinely new, previously-nonexistent resource identity into being — never a write that targets an identity that already exists in the store, even one currently soft-deleted or otherwise inactive. Restoring a soft-deleted resource, or an upsert-style write that would create-or-update depending on whether the target already exists, is **not** creation for this exception's purposes and is unconditionally subject to the general rule instead: an implementer must resolve "does this identity already exist" before deciding whether the exception can apply, exactly because the exception's own safety rests on there being no prior owner to displace — which is only true for a genuinely new identity. The exception applies exactly once, under that definition — every later write to the same label, including the very next `update`, is unconditionally subject to the general rule above.

The check is a genuine subset comparison, not merely "does this write avoid dropping access to zero." An editor with no independent claim to `resource-owner` could relabel `rise.dev/owner: platform → their-own-team` without ever dropping the resource's access to zero — they would simply redirect it to themselves. The subset check blocks this; a caller who currently holds the role being handed off (the resource's actual current owner, or an org-admin whose access is independent of any label, §6.7) passes trivially, so legitimate transfers are unaffected.

Referential-integrity validation (§6.7) runs only *after* this gate passes — a caller who would be denied by this check never learns whether the value they attempted resolves to a real Team/User, avoiding turning the validation step into an unauthenticated existence oracle.

#### 6.7 — Orphan prevention is separate from escalation prevention

*Escalation* — an unauthorized party redirecting access to themselves — is §6.6's job. *Orphaning* — a legitimate write accidentally locking everyone out, typically a typo — needs two different mechanisms:

- **Referential-integrity validation at write time.** A value written to a label some binding selects on must resolve to a real Team/User, checked synchronously, rejected with a fuzzy-match suggestion (`Team 'platfrom' does not exist. Did you mean 'platform'?`) rather than silently stored.
- **Admin access stays independently derived, enforced structurally.** The `org-admin` Role (§5) may only ever be granted via a `Scope`-targeted binding, never a `LabelSelector`-targeted one — a platform-level constraint on binding authorship, not merely a convention. This makes "no resource is reachable *only* via a dynamic ownership binding" a checkable rule rather than an assumption: since `org-admin` access can never be routed through a mutable label by construction, even a validly-transferred-but-wrong reassignment stays recoverable by an org-admin.

### 7. Token issuance for ServiceAccount/Controller identities

Authentication — proving a caller is allowed to assume an identity at all (issuer/JWKS/claims trust-policy match) — is unchanged and is a distinct concern from authorization; trust-policy matching is never folded into the RBAC model.

Minting a token is *additionally* gated by `mintToken` (§2), held on the specific ServiceAccount/Controller being assumed — analogous to AWS STS `AssumeRole` requiring both a trust policy on the role and an identity-based `sts:AssumeRole` grant on the caller. This is deliberately a privilege-elevation-capable pattern: the resulting token resolves the **target** identity's own effective permissions, live, on every subsequent request — not the calling subject's. A caller who holds only `mintToken` on a ServiceAccount, and nothing else, can still mint it a token wielding that ServiceAccount's full, broader grant; the minting caller's own permission ceiling is irrelevant to what the minted token can do once issued.

**Chaining is bounded to one hop.** A token obtained via token exchange cannot itself be used as the calling identity for a further token-exchange request. Only a directly-authenticated caller — a User session, or a ServiceAccount/Controller presenting its own source-issuer credentials, never an already-minted Rise token — may mint a token for a target identity. This is enforced structurally, not by convention: every Rise-minted token carries a `minted: true` claim the token-exchange endpoint checks on the presented bearer credential before evaluating the request; a credential carrying that claim is rejected as a caller identity for minting, regardless of what `mintToken` grants it would otherwise satisfy. A User session token and a directly-issued source JWT (signed by the identity's own configured trust-policy issuer, not by Rise) never carry this claim, so legitimate first-hop minting is unaffected. This bound would be defeated if a trust policy could be configured to accept Rise's own token issuer as a valid source-issuer — a caller could then present an already-minted token to the *authentication* layer (not the token-exchange endpoint) as if it were independent source-issuer credentials for a second identity, sidestepping the `minted` claim check entirely by re-entering as a "directly-authenticated caller" a second time. Trust policies may therefore never name Rise's own issuer/audience as an accepted source — this closes the direct, degenerate case, enforced at trust-policy write time.

It does **not**, by itself, close the harder case where the round trip goes through a legitimately-trusted *external* system: minting a token with a non-Rise `requested_audience` (below), presenting it to that external system, and receiving back a genuinely externally-issued credential (no `minted` claim, a different issuer entirely) — which can then be presented to authenticate as a second Rise identity whose trust policy legitimately trusts that same external issuer, an entirely ordinary configuration for workload-identity federation. Rise cannot generally distinguish such a credential from one issued through an unrelated path, since provenance isn't preserved across an external system it doesn't control. This is accepted as a structurally harder, unclosed risk rather than papered over: authentication and trust-policy configuration are explicitly a separate concern from this model (opening paragraph, this section), and a full closure would require either the external system preserving and exposing mint provenance (out of Rise's control) or forbidding federation to any audience whose issued credentials could plausibly be trusted back by another Rise identity (which would defeat the purpose of federation). Operators configuring trust policies for federated identities should treat this the same way they'd treat any cross-system credential-laundering risk in a multi-hop trust chain. Without the one-hop bound at all, a caller holding `mintToken` on identity A, where A itself holds `mintToken` on identity B, could traverse an arbitrarily long chain to reach whatever B (or C, or D...) is entitled to, with no single grant along the way reflecting the actual resulting reach. The one-hop bound trades away legitimate multi-level minting automation (an orchestrator minting tokens for workers that themselves mint tokens for sub-workers) for a locally-reasonable blast radius: whoever grants `mintToken` on some identity X can evaluate the risk from X's own grants alone, without needing to trace X's own `mintToken` grants transitively.

The token carries identity only, never baked-in permissions — every request re-resolves the **target identity's** live role/ceiling, exactly as for User sessions. Revoking or narrowing the target's own Role is therefore exactly as effective as revoking the token itself, before its TTL naturally expires. This does **not** extend to the `mintToken` grant that authorized issuing the token in the first place: revoking a caller's `mintToken` grant on identity A stops them from minting a *new* token for A, but has no effect on a token they already minted — it continues to resolve A's own live permissions for the rest of its TTL, same as any other token for A. Responding to a suspected compromise of the *minting caller* (rather than the target identity itself) means acting on the target's own grants, or waiting out the TTL — there is no separate token-revocation list. This is why TTLs are kept short and ceiling-bounded (below) rather than treated as a formality.

A token-exchange request may ask for **less** than the target identity's full effective grant — a narrower `requested_scope` (encoded in the token as a fixed cap, layered on top of the target's live-resolved grant rather than replacing it — the cap can only ever narrow further what the live resolution would otherwise allow, never substitute for it), and/or a different `requested_audience` (native RFC 8693 concepts: federating a token out to an external system such as AWS STS versus Rise's own API). A request may never ask for more than the target identity itself holds.

Max token TTL is governed by the same four-layer stack (§5), composed via `min()`: an SA-specific setting, bounded by the org's own declared TTL ceiling, bounded by the operator's per-org imposed ceiling, bounded by the instance-wide default. Setting an SA-specific TTL that exceeds the resolved ceiling is **rejected at write time**, consistent with how Layer 4 treats permission-set grants (§5) — it is never silently accepted and clamped later; a write that would be meaningless once capped is refused up front rather than stored as a misleading value. As with permission-set grants, the writer must also currently hold an equal-or-greater TTL entitlement themselves — the numeric case of the same writer-subset check.

### 8. One canonical kind token — no plural forms

A kind has exactly one name: the `kind` itself (`Deployment`, `RuntimeClass`). Role statements (`kinds:`), `Scope` paths (§4), reference declarations (§9), and the resource API's URL grammar all use that same token — the URL grammar becomes `{group}/{version}/{Kind}/{ancestor}…/{name}`, and `ResourceDefinition` no longer declares a plural at all. Kubernetes maintains a parallel plural vocabulary for REST-style collection URLs, at the cost of every RBAC rule (`resources: ["deployments"]`) naming things differently from every manifest (`kind: Deployment`), with a lookup command (`kubectl api-resources`) existing largely to map between the two. A naming scheme that needs a lookup table is a tax, and collection-URL aesthetics don't pay for it. This changes the shipped URL grammar, which is sanctioned: the surface carries no compatibility constraints (Context).

### 9. References to platform-provided resources

Some resources exist to be *referenced* rather than contained: a platform-level `RuntimeClass` (root-scoped, operator-managed) describes how project deployments are reconciled, and organizations select one rather than own one. Some classes are for every org; others are provisioned for one specific customer. The interesting permission is not CRUD on the class — that stays operator-only by ordinary default-deny — but who may *select* it.

**Reference declarations.** A `ResourceDefinition` may declare that a field (or label key) of its kind references another kind:

```
references:
  - at:   spec.runtimeClass          # a field path or a label key
    kind: RuntimeClass
    verb: use
```

Declared once at kind registration, as data — the same family as `ResourceDefinition`-declared subresource verbs (Alternatives considered), never per-field engine code. Any write that sets or changes a declared reference additionally requires the writer to hold `use` (§2) on the *referenced instance*, evaluated by the ordinary algorithm (§4). An unchanged value on a later write is not re-checked (same rule as §6.6 step 1), and the check runs before existence disclosure (same ordering as §6.6/§6.7): a writer without `use` cannot probe whether a class exists.

**Availability is instance-targeted bindings.** A root-scoped instance is a node in the tree, so §4's `Scope` targets it with nothing new:

```
# everyone may use the standard class
Subject: system:authenticated
Scope:   RuntimeClass/standard
Role:    rc-user = { Allow: use on RuntimeClass }

# gpu-b is provisioned for acme-corp only
Subject: org:acme-corp
Scope:   RuntimeClass/gpu-b
Role:    rc-user
```

Multiple orgs → one binding each: explicit and auditable. "Org A cannot select org B's class" is not a rule anyone writes — it is the *absence of a grant*: org A's subjects hold no `use` binding on `gpu-b`, default-deny (§4 step 3) rejects the write without confirming the class exists, and org A cannot self-serve the grant — authoring a binding scoped at `RuntimeClass/gpu-b` requires binding-write access at root scope (operator territory), and Layer 4's subset check independently blocks handing out `use` they don't hold.

**Defaults are product data, not permission data.** `OrganizationPolicy` stays purely a ceiling document (§5); nothing product-specific accretes onto the RBAC core resources. The global default is a label on the class itself — `runtimeclass.rise.dev/is-default: "true"`, operator-writable because the class is operator-owned (the same pattern as Kubernetes' `storageclass.kubernetes.io/is-default-class`). Org- and Project-level overrides are a label on the Organization or Project (`runtimeclass.rise.dev/default: gpu-b`), and the override cascade — Deployment-explicit → Project → Organization → global — is `effectiveLabels`' nearest-wins walk (§6.1), with no new inheritance machinery. The default label key is itself covered by a reference declaration, so an org-admin setting their org's default is `use`-checked like anyone else — an org cannot default itself onto a class it was never granted.

**Materialization at deployment creation.** When a deployment is created, the effective class is resolved once and written onto the Deployment as its own concrete value; that materializing write is a reference write, `use`-checked against **the deployer** — the User or ServiceAccount driving the deployment. This is why `org:<name>` includes ServiceAccounts (§1): CI-driven deploys must pass exactly where a human's would. The reconciler then reads only the materialized field and never evaluates `use` at all — every `use` check in the system has a well-defined, present subject. (Precedent: Kubernetes' DefaultStorageClass admission stamps the default `storageClassName` onto a PVC at create time.)

This deliberately gives the reference *snapshot* semantics, not §6.1's live semantics: the never-store rule exists for access-driving labels, where staleness is a security bug, whereas here the recorded value is the *output* of a decision made at a specific moment by a specific subject, and reproducibility is the point. The org's default label remains live as an *input* to the next deployment. Revoking an org's `use` grant therefore stops the *next* deployment, never a running one — consistent with Layer 4 grants being write-time everywhere else (§5), and the right availability call: a revoked class ages out at the org's next deploy or rollback (which creates a new deployment and re-resolves against current grants).

**Boundary.** Org-admins cannot sub-delegate or per-instance-restrict `use` of platform-provided resources inside their org — those grants live at root scope. Their levers are the org default label and a kind-level Layer 3 ceiling denial (`Deny: use on RuntimeClass`); per-instance, org-side restriction would need resource admission policies, which are out of scope (§10).

### 10. Explicitly out of scope

- Org-registrable Controllers/ResourceDefinitions — falls out for free once registration is just another ceiling-governed verb, not designed now.
- Migrating today's typed-table-backed APIs (`Project`, `Team`, `Deployment`, …) onto this model — happens automatically as a consequence of their separate, already-planned migration onto the generic resource store.
- Ingress-level authentication for a deployed application's own end users — a different problem domain entirely.
- How a brand-new organization's first `org-admin` binding is created (the org-creation bootstrap) — necessarily an operator action, the same way the very first Role/RoleBinding on the whole instance must be, but the org-creation workflow itself is not designed here.
- A pluggable subject-kind registry letting organizations define groups with custom membership semantics (§6.4) — organization-specific *naming* of a grouping concept is supported today by pairing an existing kind with an organization-chosen label key; genuinely custom membership resolution is not, and would need a larger extension to the closed subject-kind list.
- Resource admission policies — org- or operator-authored rules constraining what may be written below a given scope (e.g. required labels, or per-instance restriction of which platform resources an org's own subjects may reference, §9). A future mechanism; nothing here forecloses it.

## Consequences

**Positive.**

- One evaluator decides access for every subject kind — Users, Teams,
  ServiceAccounts, Controllers, and Operators run the same algorithm, replacing
  five disjoint authorization code paths.
- Operator access becomes inspectable and auditable as data: the seeded
  `system:operators` binding is a stored row the same explain/audit tooling
  can read, instead of an invisible bypass branch (§1).
- Who-can-do-what is runtime-configurable per deployment: Roles, RoleBindings,
  and ceilings are rows, and platform-shipped defaults (`org-admin`,
  `resource-owner`) are operator-authored data, so a SaaS and a self-hosted
  instance get different postures from the same architecture (§5, §6.2).
- Revocation is live: ceilings and memberships are re-resolved on every
  request, so tightening a ceiling or narrowing an identity's Role takes
  effect immediately — including for every outstanding token of that identity
  (§5, §7).
- Reference authorization — who may *select* a platform-provided resource
  like a `RuntimeClass` — reuses the same evaluator, bindings, default-deny,
  and existence-masking as everything else; making a class available to an
  org is one auditable binding (§9).

**Negative / accepted risks.**

- Wildcard replacement is outright, not merged: an org-specific binding
  silently discards everything the wildcard binding provided beyond what it
  restates (§1).
- Ceiling tightening has no dry-run/impact-preview — an operator or org-admin
  can strand subjects (including their own controllers) with no warning before
  committing the write (§5, and Alternatives considered).
- There is no token-revocation list. Responding to a compromised *minting
  caller* means acting on the target identity's own grants or waiting out the
  TTL (§7).
- The federation round-trip laundering path — minting a token for an external
  audience and re-entering via an externally-issued credential a second
  identity's trust policy accepts — is left structurally open; only the
  direct case (trusting Rise's own issuer) is closed (§7).
- The creation-time ownership exception depends on a precise "genuinely new
  identity" definition: implementations must correctly distinguish creation
  from restore/upsert, or the exception becomes an ownership-displacement hole
  (§6.6).
- The transparency the model is designed around (§1's explain/audit tooling —
  "why can this subject do this?") is only practical once the implementation
  builds a policy explain/simulator; that is additional, eventual work the
  model assumes.
- `use` revocation takes effect at the next deployment — references are
  materialized and checked at write time, so running workloads are never
  retroactively broken, which also means a revoked class lingers until the
  org's next deploy or rollback (§9).
- Org-admins cannot per-instance restrict or sub-delegate `use` of
  platform-provided resources inside their org; their levers are the org
  default label and a kind-level Layer 3 denial, until admission policies
  exist (§9, §10).
- The resource-API RBAC items in `ROADMAP.md` (and everything sequenced on
  them) are to be planned against this model.

## Alternatives considered

- **Pure-additive, union-only permission sets (Kubernetes Role/RoleBinding-style), no Deny.** Cannot express subtraction from a wildcard — "everything except `delete` on `Environment`" has no faithful positive encoding against an open-ended, runtime-extensible set of resource kinds. Rejected; §3's Deny-capable evaluator, combined via union-then-evaluate in §4 step 3, is adopted specifically to make this expressible — including letting a narrower binding's Role genuinely subtract from what a broader one grants (§4's worked trace), not merely add to it.
- **Folding ownership into a wildcard `Allow * *` statement**, rather than a distinct owner Role. Would silently over-grant: an owner would automatically gain `mintToken` and `updateFinalizers` alongside ordinary access, defeating the deliberate verb separation in §2. Rejected in favor of the named `resource-owner` Role with the explicit verb list pinned down in §6.2 (`get`/`list`/`update`/`delete` only).
- **A dedicated single-subject `ownerRef` field as a separate ownership mechanism alongside the ceiling/Role model**, inherited down the ancestor chain as a union. Two independent inheritance and authorization mechanisms doing near-identical jobs complicates reasoning about why a subject has access to something, and union-across-the-whole-ancestor-chain semantics would mean a descendant could never fully exclude an ancestor's owner. Rejected; §6 subsumes ownership into the same Role/RoleBinding/label primitives as everything else — and unlike `ownerRef`, an ordinary Deny-bearing binding at a narrower scope genuinely can exclude a broader ancestor's grant (§4), so the exclusion capability `ownerRef` lacked is available for every kind of grant, not just reintroduced for ownership specifically.
- **Labels driving RBAC directly, with no write-gate on the label itself.** Ordinary `update` access on a resource would let any editor silently redirect which subject holds a derived Role — an ungated escalation path. Rejected in favor of §6.6's binding-triggered Layer 4 gate, which in turn is one instance of §5's general rule that *every* write changing effective access — including RoleBinding and Role edits, not only labels — passes through the same check.
- **A dedicated bespoke verb per protected field** (e.g. `setTeamLabel`), rather than a generic mechanism. Every newly-sensitive field would need a new verb and new engine code. Rejected in favor of §6.6, where protection is a consequence of a `LabelSelector` binding's existence, not a hardcoded field name — and generalized further in §5 so the same reasoning covers Role/RoleBinding writes, not only labels.
- **Gating label writes on "does not drop access to zero"** rather than the standard subset check. Defends availability only — it never checks *who* gains access, only that the total doesn't hit zero — so it would still permit an unauthorized party to redirect access to themselves. Rejected in favor of the genuine subset comparison in §6.6.
- **Exempting machine identities (Controller, ServiceAccount) from an org's own declared ceiling**, to avoid an org accidentally stranding its own controller. Rejected — an exemption would require every ceiling check to first determine whether the binding being evaluated targets a machine or human subject, adding a second evaluation path everywhere ceilings apply for a narrow footgun-avoidance benefit; ceilings apply uniformly instead, with no asymmetry by subject kind, and the resulting footgun is accepted as-is (§1, §5).
- **A ceiling-tightening (or wildcard-replacement) dry-run/impact-preview warning.** Would require simulating the write's effect across every subject with a live binding under the tightened rule before committing it — expensive and stateful in a way the rest of the write path deliberately isn't, and it doesn't integrate cleanly into a generic REST write path. Deferred; the footgun is accepted, not solved, for now (§1, §5).
- **An open, pluggable subject-kind registry**, to let organizations define arbitrary group types (e.g. "squad") with their own membership resolution. Subject kind carries real infrastructure (membership resolution, org-native-vs-agnostic encoding, `mintToken` semantics) not worth making pluggable. Rejected; §6.4 shows organization-specific *naming* of a grouping concept is expressible by pairing an existing kind (Team) with an organization-chosen label key — genuinely custom membership resolution is a separate, larger ask this does not address, and remains out of scope (§10).
- **Clamping a minted token's scope to the calling subject's own permissions.** Forecloses a legitimate privilege-elevation pattern — a low-privilege, long-lived caller minting a token for a higher-privilege, short-lived ServiceAccount, the same shape as AWS STS `AssumeRole`. Rejected; §7 gates *who may mint*, not what the minted token may then do.
- **A single global namespace for Role names, with no ownership.** Any org editing any Role by name would make cross-org ceiling attribution ambiguous the moment a Role is bound in more than one org — whose ceiling governs an edit? Rejected in favor of Role ownership (§5): a Role is platform-owned (operator-editable, any org may bind it) or org-owned (editable, and bindable, only within one org) — editing a Role always has exactly one unambiguous ceiling to check against.
- **Unbounded `mintToken` chaining** (letting a minted token itself be used to mint a further token). Lets a caller holding `mintToken` on one identity reach arbitrarily far through a chain of that identity's own `mintToken` grants, with no single grant reflecting the actual resulting reach. Rejected in favor of the one-hop bound in §7 — only directly-authenticated callers may mint.
- **Applying the general subset check to owner-label writes at creation with no exception.** Would mean a subject holding only `create` on a kind could never become the resulting resource's owner, since ownership (`resource-owner`) is strictly more than `create` alone implies — breaking the single most common operation the model exists to support. Rejected in favor of the narrow, membership-bounded creation-time exception in §6.6, which only ever lets a creator name themselves or a team they already belong to, never an arbitrary third party.
- **Permitting a ServiceAccount/Controller's trust policy to accept Rise's own token issuer as a valid source.** Would let a caller launder an already-minted token back into the authentication layer as if it were independent source-issuer credentials for a second identity, defeating the `mintToken` one-hop bound by re-entering as a "directly-authenticated caller." Rejected; trust policies may never name Rise's own issuer/audience (§7).
- **A general `fields:` include/exclude axis on Role statements**, replacing `updateStatus`/`updateFinalizers` (and potentially §6.6's label-write gate) with field-path matching on the ordinary `update` verb. Rejected on both counts. Folding in §6.6 doesn't work at all: its gate depends on whether *some other, unrelated binding* currently exists (a live property of the whole binding table, not data a Role statement can carry) and on whether a *value* changed, not merely which *path* was touched — properties no static field syntax can express, and the diff computation such a fold would require is vacuous everywhere except labels, since nothing else in this model resolves a Subject off a field value. Collapsing just `updateStatus`/`updateFinalizers` doesn't work either: it's a lateral move, not a reduction (field-matching and verb-matching are the same computational shape), it destroys `resource-owner`'s secure-by-default-via-omission property (§6.2), and it introduces a genuinely ambiguous, security-critical primitive with no analogue today — path containment feeding Layer 4's `⊆` check, where something as simple as `status.*` has two equally plausible readings (single-segment vs. recursive wildcard) that produce *opposite* security outcomes, and a natural-looking `fields: ["metadata.*"]` grant would silently also cover `metadata.finalizers`, something structurally impossible under separate verbs. If a genuinely new kind ever needs a third disjoint-population write region beyond spec/status/finalizers, the answer is to extend the same pattern that already works — a `ResourceDefinition`-declared, opaquely-named subresource verb, resolved once at kind-registration time — not a general path-glob language evaluated on every write.
- **A bespoke `policy` subresource on `Organization`** for Layer 2, gated by envelope-level machinery distinct from ordinary verbs. Requires inventing a third write-tier (alongside spec and status/finalizers) purely for this one field. Rejected in favor of `OrganizationPolicy` as an ordinary resource kind (§5): read access for org-admins and write access restricted to `system:operators` both fall out of the existing verb/binding machinery with zero new envelope concepts — org-admins simply are never bound to `create`/`update`/`delete` it, the same way anyone is denied anything else they hold no grant for.
- **A single fixed definition for `org-admin`/`resource-owner`, uniform across every deployment.** A multi-tenant SaaS operator and a self-hosted single-team operator want genuinely different answers to "should ordinary users/org-admins see platform bookkeeping (`status`/`finalizers`) or the operator-imposed ceiling document" — the former wants it locked to `system:operators`/Controllers, the latter may reasonably not care, since the same people already have full infrastructure visibility. Rejected in favor of treating both Roles' verb lists as ordinary, operator-authored, deployment-time configuration (§5, §6.2) rather than an architectural constant — the identical mechanism already used for everything else Role-shaped, requiring no new primitive to support either deployment model or anything in between.
- **Nesting a ServiceAccount under a single owning Project**, as its tree position. Access reach is granted entirely through bindings (§4), independent of tree position, so a "home" Project does no real work — it only couples the SA's inherited attribution (§6.1) to whichever Project happened to parent it, and requires re-parenting (or duplicating) the SA to give it first-class standing against a second Project it's equally bound against. Rejected in favor of parenting ServiceAccount directly under its org, a sibling of Project (§1) — matching how Team is already positioned.
- **Operator status as a hardcoded bypass branch in the evaluator**, checked before Role/binding resolution rather than expressed as data. Makes operator access the one thing the model's own explain/audit tooling can't account for, and duplicates logic the ordinary evaluator already has (union bindings, evaluate Allow/Deny). Rejected in favor of `system:operators` (§1): a reserved subject, resolved via the same live config-allowlist check as today, granted access through one seeded, immutable binding — operators run the same algorithm as everyone else, with only ceiling intersection (step 4) still skipped, and only for that one reserved subject.
- **Treating the seeded `system:operators` binding as immutable data only, with no evaluator-level guarantee behind it.** Immutability through the ordinary write path (§5) protects only against mutation via this model's own API — not a bad migration, a restore from an old backup, or direct database access losing the row entirely. That residual risk is unacceptable for the one subject with no recovery authority above it. Rejected in favor of a hardcoded, evaluator-guaranteed grant for `system:operators` specifically, mirrored as a healable data row for audit/tooling parity — matching how Kubernetes redundantly hardcodes `system:masters` alongside its ordinary, self-healing `cluster-admin` ClusterRoleBinding, rather than relying on either mechanism alone.
- **Making the `system:operators` binding fully virtual too, with no stored row at all** (matching how membership itself is virtual). Would remove operator access from the same explain/audit tooling that inspects everyone else's — exactly the gap `system:operators` was introduced to close by replacing a hardcoded bypass branch in the first place (above). Rejected; the binding stays data, mirrored and healable — only the evaluator's guarantee of its *effect* is hardcoded, not its existence as an inspectable object.
- **Making the seeded `system-admin` Role and its binding platform-owned (operator-editable) rather than immutable.** Would let an operator edit or delete their own bootstrap grant through the ordinary write path — trivially passing the subset check, since they hold everything — with no higher authority left to recover from it, unlike every other documented risk in this ADR. Rejected in favor of a third, **seeded** Role-ownership tier (§5) that no write path can modify, editable by no one.
- **Allowing a static Subject to pair with a value-less `LabelSelector`.** Would grant a fixed subject access to any resource carrying *any* value for that label key, regardless of what it actually says — access disconnected from the value the selector nominally matches on. Rejected; value-less selectors are reserved for dynamic (templated) subjects, where the matched value is actually used (§4).
- **Kubernetes-style plural resource names** (a `plural` declared per kind, used in collection URLs and grants). Creates a permanent dual vocabulary — rules and URLs naming `deployments` while every object says `kind: Deployment` — with a lookup step (`kubectl api-resources`) as the ongoing price of collection-URL aesthetics. Rejected; the `kind` token is the single canonical name everywhere (§8) and `ResourceDefinition` declares no plural.
- **A distinct separator between the kind and the path in `Scope`** (e.g. a `RuntimeClass:gpu-b`-style form), for visual clarity. Rejected to keep `Scope` byte-identical to the URL path form (§4, §8) — one grammar to learn, one parser to trust.
- **`get` as the reference gate** ("if you can read it, you can select it"), instead of a distinct `use` verb. Couples two independent decisions: a catalog may be browsable without being selectable (visible-but-gated offerings), and selectable without being readable (a class's internals — node selectors, cost plumbing — are not the selector's business). Rejected in favor of `use` (§2, §9), mirroring the Kubernetes `use` verb on PodSecurityPolicies.
- **An `allowedOrgs` list on the referenced resource's spec** as the availability mechanism. Moves an authorization decision out of the one system built to answer authorization questions, needs its own evaluation and audit path, and caps out at org granularity. Rejected; availability is ordinary instance-targeted `use` bindings (§9), which also express team- or ServiceAccount-narrow grants with no extra machinery.
- **Encoding product defaults (e.g. the default `RuntimeClass`) in `OrganizationPolicy`.** Would accrete product-specific settings onto the RBAC core's ceiling document. Rejected; the core stays agnostic — defaults live on the product resources themselves as labels, and the override cascade is `effectiveLabels` (§6.1, §9).
- **Live `use` re-evaluation at reconcile time**, instead of materializing the resolved class onto the Deployment at creation. Leaves the check with no well-defined subject (a reconciler acts for nobody in particular) and turns a grant revocation into retroactive breakage of running workloads. Rejected; the effective class is materialized at deployment creation and `use`-checked against the deployer (§9), matching Kubernetes' DefaultStorageClass admission behavior — revocation applies from the next deployment.

## References

- `ROADMAP.md`, Workstream 1 ("Multi-Tenancy & Generic Resource API") — owns
  live status for the resource-API RBAC items this model informs.
- [Generic Resource API](/operator-docs/generic-resource-api/) — the shipped,
  operator-only surface this model will govern.
