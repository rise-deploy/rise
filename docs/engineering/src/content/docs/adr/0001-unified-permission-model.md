---
title: "ADR-0001: Unified Permission Model"
---

## Status

**Proposed** (under review). Date: 2026-07-10.

Scope: the generic resource API (`/api/v1/resources/...`) and
ServiceAccount/Controller token issuance (the `token` subresource). It does
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
restrictions. The operator must be able to impose a restriction on one specific
org (a compliance-restricted customer), and an org must be able to impose a
tighter restriction on itself, with both enforced simultaneously. Org admins must
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
underneath it. That combined result is all there is: a restriction is just
one more `Deny` folded into the same combination, never a separate capping
step on top of it.

A **restriction** ("cap") the platform operator or a resource's own
organization imposes is itself just a `Deny` binding, folded into the same
Deny-wins combination as every other binding: an operator caps one org by
placing a `Deny` binding only they can author, an organization caps itself by
placing one on its own members, and both take effect at once because Deny
always wins. Who may impose or lift a cap is simply *where* the binding is
placed (§5), not a separate mechanism. There is one further rule, applied only
at the moment of a write: whoever authors a grant — binding a Role to a
subject, or editing a Role's own definition — must already hold everything they
are handing out. Because a cap `Deny` applies to that writer too, this single
check enforces every cap for free: you cannot hand out what a cap has already
taken away from you.

Every grant is re-read live from the database on every single
request — nothing is cached or baked into a token, so
tightening a cap or narrowing a Role takes effect immediately for everyone relying on it. This
is what makes revoking a Role exactly as effective as revoking a token before
it expires — but only for the identity the token belongs to: narrowing what a
ServiceAccount itself can do immediately narrows every outstanding token for
it. Revoking the separate grant that let someone *mint* that token in the
first place does not reach back and affect a token already issued (§7).

Ownership works through this same mechanism, not a separate one. A resource
can carry a label — `rise.dev/owner: platform` — naming the team or person it
belongs to. The platform ships one built-in rule: whoever that label names
automatically gets an owner-level Role on the resource. That rule is the
entirety of what "ownership" is — the engine itself has no ownership concept;
remove or replace the rule and the word means nothing, or means whatever the
replacement says. Nothing about that
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

**Org-agnostic — User, Controller.** A single identity can legitimately hold different roles in multiple different organizations: a person is a member of two customers' orgs with different access in each; a single Controller process reconciles resources across many orgs. Nothing about the subject's identifier ties it to one organization. A binding for one of these kinds has a normalized `Scope` (§4) — either one specific org, or a wildcard (`"*"`) meaning "the default for every org this identity touches, unless a more specific binding exists for that org."

**Org-native — Team, ServiceAccount.** These exist within exactly one organization by construction: a Team has one owning org, and a ServiceAccount is created and lives directly under an org (`acme-corp/serviceaccount:ci-bot`) — a sibling of Project in the resource tree, not nested under any one Project. This is deliberate: a ServiceAccount's reach comes entirely from what it's bound to via ordinary RoleBindings (§4), which can span any number of Projects/Environments in its org; tying its identity to a single "home" Project would suggest a relationship that has no bearing on what it can actually do, and would couple its `effectiveLabels`-inherited attribution (§6.1) to whichever Project happened to parent it. The org is baked into the identifier itself. A **static** binding (a literal, fixed subject — §4) for these kinds either omits an explicit `Scope` (normalized to the subject's own Organization scope) or, if one is supplied, it must lie within the subject's own org. In particular, explicit `Scope: "*"` is invalid for a static org-native subject: a wildcard in the bound Role's `kinds` still ranges only within the binding's Scope and does not make an org-scoped binding reach root-scoped kinds. A **dynamic** binding (a subject template — §4, §6.3) has no concrete subject to infer an org from until it is evaluated against a specific resource; §6.3 states how its resolved subject's org is determined in that case.

**Canonical subject identifiers.** Authorization never operates on an unparsed string. One shared `SubjectId` type accepts exactly these concrete forms:

```
user:<name>
controller:<name>
<org>/team:<name>
<org>/serviceaccount:<name>
org:<name>
system:authenticated
system:operators
```

`<org>` and `<name>` use the generic resource API's canonical resource-name grammar. Empty components, extra `/` or `:`, dot segments, query/fragment syntax, and non-canonical encodings are rejected; there is no permissive fallback interpretation. A literal User, Controller, Team, ServiceAccount, or `org:<name>` in a binding must resolve at write time to an existing Rise resource (including resources created in the same atomic transaction). The two `system:` forms are virtual and follow the additional authoring restrictions below. Only the exact dynamic forms declared by the closed subject-kind set — for example `team:${ref.name}` or `user:${ref.name}` — may contain a template marker; arbitrary interpolation syntax is rejected when the binding is written. Dynamic substitution produces a concrete canonical `SubjectId` and is parsed again at evaluation time, failing closed if invalid. For an org-native template the matched resource supplies `<org>` as defined in §6.3, so `team:${ref.name}` on an acme resource resolves to `acme-corp/team:<value>` rather than to an incomplete literal.

**Subject records and their relationships are built-in resources.** All persisted identity, membership, and authentication-policy objects use the existing built-in `rise.dev/v1alpha1` API group; there is no separate `authorization.rise.dev` group. Placement is fixed by kind:

| Kind | Parent | Purpose |
|---|---|---|
| `User` | root | Stable human identity and non-authoritative profile fields |
| `UserIdentity` | `User` | One external SSO `(issuer, subject)` mapping |
| `Controller` | root | Stable org-agnostic workload identity |
| `ControllerTrustPolicy` | `Controller` | One accepted external issuer/audience/claims policy |
| `Team` | `Organization` | Org-native group identity |
| `TeamMembership` | `Team` | One typed reference to a member `User` |
| `ServiceAccount` | `Organization` | Stable org-native workload identity |
| `ServiceAccountTrustPolicy` | `ServiceAccount` | One accepted external issuer/audience/claims policy |

The separate trust-policy kinds are intentional: a generic kind has exactly one declared parent, so one `TrustPolicy` kind cannot be parented under both Controller and ServiceAccount without violating the store's exact-parent invariant. Likewise, Team membership is one child resource per edge rather than an array in `Team.spec`: this avoids whole-object lost updates, gives each membership its own grant-gated lifecycle/audit identity, and supports lookups in both directions. `TeamMembership.spec.userRef` is an immutable, kind-qualified reference to an existing User UID; changing the member is a delete plus create, so adding the replacement User passes the ordinary membership grant gate. Membership itself is boolean for authorization — any descriptive membership role is ordinary metadata and never a second permission system. Deleting a User is blocked while memberships reference it (or an authorized cleanup deletes those membership resources in the same transaction), so no dangling membership can authenticate or expand access.

**User names are stable generated identifiers, not email addresses.** A User gets an immutable, collision-resistant DNS-safe resource name such as `u-<lowercase-ulid>` (with the store's ordinary uniqueness constraint as the final authority). The generic resource-name grammar remains unchanged and does not admit `@`; relaxing it for one identity kind would either weaken every resource path or require kind-specific metadata-name parsing in the core. `User.spec` may carry presentation fields such as `displayName` and `primaryEmail`, but email is mutable, case-sensitive in troublesome ways, and non-unique across issuers, so it is never a subject key and Rise never auto-links accounts by email. `UserIdentity.spec` carries the authoritative external `issuer` and `subject`; that pair is globally unique among live UserIdentity resources. UI/CLI surfaces resolve and display email/name while bindings, tokens, references, URLs, and audit records use the stable User name or UID.

Trust-policy resources contain public matching configuration (issuer, audience, required claim constraints), never private signing keys or bearer credentials. UserIdentity and workload trust-policy writes are ordinary governed resource writes, with their schemas and the authentication-specific validation in §7 applied before persistence. `org:<name>`, `system:authenticated`, and `system:operators` remain virtual predicates and have no corresponding identity row; an Operator remains a User plus live config-derived membership.

**Operator** is a platform-wide root identity, bootstrapped from configuration as today, but the target allowlist names canonical User resource names/UIDs rather than mutable email addresses. Existing email-based `auth.operator_users` entries are migration inputs resolved once to User/UserIdentity resources, not permanent authorization keys. What changes is how the status is *expressed*: rather than a hardcoded bypass branch in the evaluator, operator status is membership in one reserved subject, `system:operators` (a `system:`-prefixed name is reserved for platform-recognized pseudo-subjects, never an ordinary User/Team/ServiceAccount/Controller row). The platform seeds exactly one binding for it:

```
Subject: system:operators
Scope:   "*"
roleRef: { kind: PlatformRole, name: system-admin }      # a PlatformRoleBinding (§3)
```

where `PlatformRole/system-admin` allows every verb on every main resource and
every registered subresource (§2).

An operator's request runs through the *same* evaluation algorithm as anyone else's (§4 steps 1–3) — no separate code path. One thing is special-cased for any request whose membership expansion (§4 step 1) includes `system:operators` — i.e. any request by a current operator: the Deny-wins union of steps 1–3 is overridden, so no `Deny` collected in step 1 can reduce an operator's effective access. This is load-bearing because a cap is itself a `Deny` binding (§5): an operator caps every other subject, including org-admins, but an operator's *own* request ignores every `Deny` — otherwise an operator could accidentally lock themselves, and everyone else, out by placing an instance-wide cap that only they can author, with no one above an operator able to fix it. The granter-subset half of the write-time grant gate (§5) needs no special-casing at all: since `system:operators` always holds every main-resource and subresource permission, any grant an operator hands out trivially satisfies `⊆` their own effective permissions.

**`system:` names are reserved, and `system:operators` is never a binding target.** The `system:` prefix is reserved, enforced at both subject creation and binding-`Subject` authoring: an unrecognized `system:`-prefixed subject (anything but the platform-recognized `system:operators` and `system:authenticated`) is rejected wherever it appears. Among the recognized names, `system:operators` may *never* be named as the `Subject` of an ordinary binding — only the platform-seeded bootstrap binding above targets it; an ordinary `RoleBinding` or `PlatformRoleBinding` whose `Subject` is `system:operators` is rejected at write time. Otherwise an org could author a `{Deny: * on *}` binding catching operators and lock them out of an org with no in-model recovery — the exact state §1 exists to prevent. (`system:authenticated` and `org:<name>` remain ordinary group predicates a binding may target — §4's list authorization and general org-wide grants use them, independent of §9 — bounded by the recipient boundary below.)

**Membership never replaces a caller's own identity.** When the User `user:u-01jz…`, listed by canonical name/UID in the operator allowlist, makes a request, that remains the caller's subject — exactly what it would be if they weren't an operator at all. What differs is **membership expansion** (§4 step 1): evaluating a subject `S`'s own request considers not only bindings that target `S` directly, but also any binding targeting a group `S` currently belongs to, checked live on every request. Team membership and `system:operators` allowlist membership are both instances of this one rule, no separate code path per group kind. Since the only binding targeting `system:operators` grants `system-admin`, the User's combined policy includes every main-resource and registered-subresource permission, unioned with whatever they separately hold as their User subject. Remove that canonical User from the allowlist, and their very next request no longer draws on it — nothing to revoke, no propagation delay, the same live-recheck property as everything else in this model (§5). Changing their profile email has no effect. This is also what makes the default ownership binding (§6.2) actually reach a human in the first place: it targets a resolved Team, and a member of that Team benefits from it only through this same expansion.

**Group subjects.** Two reserved group forms exist beyond Team: `system:authenticated` — every authenticated subject, of any kind — and `org:<name>` — every subject belonging to that organization: its org-native subjects (Teams, ServiceAccounts) and its user members alike. A `User` is a member of `org:<name>` iff they are a current member of at least one Team owned by that org — org membership composes with, and is exactly as live as, the Team-membership expansion above, never a separately-stored roster; removing a user's last such Team tie removes them from `org:<name>` on their next request. This is Team-tie-only by design: an `org:<name>`-addressed grant never reaches a team-less individual, and an org `RoleBinding` targeting one silently confers nothing — to give a team-less individual org-private access, bind `user:` directly, use `system:authenticated`, or place them on a Team. Both resolve through the same membership expansion as Teams and `system:operators`; there is no separate code path per group kind. ServiceAccount inclusion in `org:<name>` is deliberate and load-bearing: an org-wide grant (e.g. `list` on a collection granted to `org:<name>`, or any CI-facing grant addressed to the whole org) must reach the org's CI identities, or every machine-driven flow fails exactly where a human's would succeed.

**Org bindings target only their own org.** An org-parented `RoleBinding`'s grant to any subject is intersected with live membership in that binding's *own* org — a subject receives the grant only while it is a current member of that org. An org-native subject (`Team`/`ServiceAccount`) whose baked-in org differs from the binding's org is provably foreign and rejected at write time; a `User` subject receives the grant only while a live member of the binding's org (so a user in `acme` and `beta` gets an `acme`-scoped binding's grant in `acme` alone, and loses it on leaving `acme`); `system:authenticated` inside an org binding auto-clamps to that org's authenticated members and cannot expose platform-wide. A `Controller` — org-agnostic, a member of no single org — therefore cannot be targeted by an org binding at all. `PlatformRoleBinding`s (operator-authored, root-placed) are exempt: they may target any subject, cross-org or platform-wide — that is how the seeded ownership binding (§6.2), `RuntimeClass` availability (§9), and any deliberate global grant reach their subjects. This bans cross-org sharing through org bindings by construction; a first-class cross-org sharing primitive is deferred (§10).

**The binding is data; the membership is not — deliberately.** `system:operators`'s grant (the binding above) is a stored row, same table as every other binding. Whether a given identity is currently *in* `system:operators` is never stored anywhere in this model — it's synthesized at evaluation time from the config allowlist, which lives outside the resource store entirely. This is forced by the same bootstrap problem the Operator concept exists to solve: if membership were itself an ordinary RBAC record, granting the first one would require an already-privileged actor to write it, and nothing could ever create that first record. The config allowlist is the one piece of trust in this model that has to originate from outside the system Rise itself governs.

The binding has no equivalent forcing problem — it's never granted by anyone at runtime, only seeded once at bootstrap — so it can safely be data, with one refinement. Being immutable through the ordinary write path (§5's **seeded** Role-ownership tier: no write path can ever modify it, not even an operator) only protects against mutation through this model's own API — it says nothing about a bad migration, a restore from an old backup, or direct database access losing the row entirely, outside any write path this model governs. That residual risk is unacceptable for the one subject with no recovery authority above it. Operator status is a property of the requesting caller, not of any one subject row. Whenever a request's live membership expansion (§4 step 1) includes `system:operators`, the evaluator yields the complete main-resource and registered-subresource policy for that request unconditionally — it ignores every `Deny` collected in step 1 from *any* subject, including a `Deny` targeting the caller's own `user:` identity or any cap binding. No binding can reduce an operator's effective access. The write-time rejection of bindings that target `system:operators` (above) remains as defence-in-depth but is not load-bearing on its own: operator allowlist membership is external config that can change after a binding is written, so the guarantee must hold at evaluation regardless of what `Deny` rows exist. This guarantee is hardcoded in the evaluator — not something solely read from, and therefore losable with, a table row. The row is still materialized alongside that guarantee, purely so the same explain/audit tooling that inspects everyone else's access can inspect this one too without a special case; if it's ever found missing or altered outside the write path, that's healed by re-materializing it, not a live authorization dependency.

This mirrors how Kubernetes actually handles `system:masters`: a hardcoded superuser check in the authorizer grants it full access with no ClusterRole or ClusterRoleBinding required at all, *and*, redundantly, an ordinary `cluster-admin` ClusterRoleBinding also binds the same group to the same power as a stored object — kept self-healing (missing permissions/subjects on default, `kubernetes.io/bootstrapping=rbac-defaults`-labeled objects are restored automatically) rather than merely immutable. Every other `system:`-prefixed built-in role (`system:node`, `system:kube-scheduler`, etc.) gets only the self-healing-data half, no hardcoded bypass, because losing one of those is recoverable by whoever holds `system:masters` — the same distinction already drawn above between `system-admin` (nothing above it, needs the hardcoded guarantee) and `org-admin` (recoverable by an operator, doesn't). Kubernetes' authorization decisions are live on every request in both cases, same as this model's throughout (§5); what's actually startup-scoped there is narrower — only the drift-repair of default objects' stored contents, not authorization itself.

**Wildcard resolution.** When two bindings target the same `(Subject, LabelSelector-key-if-any)` pair — one with `Scope: "*"` and one with a more specific `Scope` — the more specific one **replaces the wildcard outright, for that scope** — it does not merge with it. "Same subject" for this comparison means the same literal subject, or the same subject *template* text; a dynamic binding on `LabelSelector: {key: rise.dev/owner}` never collides with one on `LabelSelector: {key: rise.dev/squad}`, even if both use the identical template `team:${ref.name}` — they are different rules. This comparison is always performed on the binding's *authored* Subject field exactly as written — literal `SubjectId` against literal `SubjectId`, or raw template string against raw template string — never on a resolved value: a literal binding (`Subject: acme-corp/team:platform`) and a dynamic one (`Subject: team:${ref.name}`) never collide with each other, even where the template resolves to that same concrete Team, so a platform-wide dynamic default is never silently discarded just because one particular resource's resolved subject happens to match some unrelated static binding. Where a `LabelSelector`'s optional `value` also differs between two otherwise-colliding bindings, replacement is evaluated per-resource, at the same point §4 step 1 collects applicable bindings, not as a blanket scope-wide swap — a `value`-narrowed selector only matches (and so only competes with and replaces a broader same-key selector for) resources whose label actually equals that value; resources carrying any other value never collect the narrowed binding in step 1, so the broader selector continues to govern them, undiminished. This replacement rule applies to any subject (not only Controller) whenever a wildcard `Scope` is in play, including the dynamic ownership bindings in §6 — and crucially it applies **across placement tiers**: an org-parented `RoleBinding` may replace a root-parented `PlatformRoleBinding`, which is exactly what lets an org override the platform-seeded ownership default (§6.5), whose default *is* a `PlatformRoleBinding`. What replacement may never do is *subtract a `Deny`*: it preserves every `Deny` statement the superseded binding carried and may drop only the wildcard binding's *Allow* content. That single invariant — not a blanket placement prohibition — is what stops an org from escaping an operator's platform restriction: a restriction expressed as a `Deny` survives replacement regardless of who authored the superseding binding, while an all-`Allow` default (like `resource-owner`, §6.2) remains freely overridable. It exists to keep "what does this rule resolve to, in this org" a single, unambiguous answer instead of an additive combination of whatever bindings happen to apply — the one place bindings do not simply combine (§4 covers the ordinary, additive case).

**Accepted risk.** Because replacement is outright rather than merged, an org-specific binding can unintentionally discard everything a wildcard binding provided beyond what the org-specific one restates — e.g. narrowing a shared Controller's wildcard grant in one org for one reason can silently strip that Controller of unrelated capabilities it still needed there. There is no dry-run/impact-preview step (§5's cap-tightening risk acceptance applies equally here) — this is an accepted, unmitigated footgun for both cap tightening and wildcard replacement. The footgun is one of capability *loss* (dropped Allows), not escalation: were replacement to drop the superseded binding's `Deny` statements too, losing a `Deny` would be a privilege *gain* — the escalation direction, not merely lost capability — which is exactly why replacement preserves `Deny` (above). A platform-hard restriction is therefore durable whether an operator expresses it as an operator cap `Deny` (a `PlatformRoleBinding` no org can remove, and whose `Deny` survives replacement anyway) or as a `Deny` statement inside a wildcard binding (preserved through any override); the accepted, unmitigated part is only that an org override can silently drop an operator's wildcard *Allows* — a capability-loss footgun, never a restriction-bypass one.

### 2. Verbs and subresources

The ordinary verbs are `get`, `list`, `create`, `update`, and `delete` — Rise's
existing action vocabulary — plus **`use`**, the right to *reference* a
resource from another resource's fields. `use` is distinct from reading or
editing and is checked at write time of the *referencing* resource, against its
writer, wherever a `ResourceDefinition` declares a reference (§9). Granting
`get` without `use` makes a catalog browsable but not selectable; `use` without
`get` allows selection by name without exposing the referenced object's
contents. (Precedent: the Kubernetes `use` verb on PodSecurityPolicies.)

Like Kubernetes, Rise models a secondary operation as an orthogonal
**subresource**, not by inventing a compound verb. An authorization request is
therefore `(verb, kind, subresource?)`: `update` on a Deployment's `status` is
`(update, Deployment, status)`, while creating a ServiceAccount token is
`(create, ServiceAccount, token)`. The main resource has no subresource value.
Permissions never flow implicitly between the two: `update on Deployment`
does not authorize `update on Deployment/status`, and a status grant does not
authorize the main update endpoint. This keeps authorization aligned with the
API route and leaves room for Kubernetes-shaped endpoints such as `logs`,
`scale`, and `proxy` without expanding the verb vocabulary into `getLogs`,
`updateScale`, and so on.

A subresource is an API routing and authorization boundary over a parent
resource, not necessarily a second stored object. `status` is a field in the
parent kind's schema and is returned by an authorized `get` of the main object;
`/status` is the restricted mutation path for that field. `token` is instead a
create-only operation that returns a credential without persisting a Token
resource. `ResourceDefinition` declares which named subresources a kind
supports and which shared handler strategy each uses. Requests for undeclared
subresources fail before authorization, and declarations use canonical
lowercase names from a closed platform registry for the initial model:
`status`, `finalizers`, and `token`.
Each registered strategy also declares its supported HTTP/RBAC verbs; for
example, `token` accepts `create` but not `get` or `update`, while `status` and
`finalizers` accept their defined read/update operations. A Role may contain a
broader wildcard, but it can authorize only an operation the registered route
actually serves.

**`status` follows Kubernetes' split-object semantics.** A kind that declares
`status` still defines and stores `status` in the one resource envelope. An
authorized main-resource `get` returns the complete object, including status;
there is no separately stored `DeploymentStatus` kind. Main-endpoint
`POST`/`PUT`/`PATCH`/apply operations ignore proposed status changes and
preserve the stored value, while writes through `/status` ignore every change
except status. This makes manifests copied from a read safe to apply without
overwriting controller-owned observations. Status-only writes do not increment
`metadata.generation`, but they do use the normal schema validation, admission,
resource-version concurrency, persistence, and audit path. A kind without a
declared `status` subresource has no such field-level separation.

This separation is generic resource-API machinery, not code every handler
reimplements. For a replace, the main strategy restores old status and the
status strategy restores every non-status field; patch/apply exclude protected
fields while calculating the mutation so the caller neither changes nor
acquires field ownership for them. The shared layer then validates and stores
the resulting whole object. A `ResourceDefinition` may supply an optional
kind-specific validator for legal status transitions, but routing,
authorization, projection, merge behavior, concurrency, and persistence remain
shared. Rise applies the same machinery to `/finalizers` as a deliberate
hardening extension: finalizers remain part of `metadata`, but main writes
preserve them and only `(update, kind, finalizers)` may change them.

This ADR standardizes that shared authorization and handler seam, plus
the concrete `status`, `finalizers`, and `token` strategies. Streaming,
connection, proxy, and virtual-projection contracts needed by possible future
`logs`, `proxy`, or `scale` subresources are explored in
[ADR-0002](../0002-generic-resource-subresource-execution-model/). Adding one
later does not change the RBAC algebra: it registers a handler and is authorized
by the same `(verb, kind, subresource)` tuple.

### 3. Roles and the Allow/Deny evaluator

A **Role** is a named, reusable **policy**: an ordered-irrelevant list of statements,

```
{ effect: Allow | Deny, kinds: ["Deployment"] | "*", verbs: ["update", "delete"] | "*", subresources?: ["status"] | "*" }
```

A statement with omitted `subresources` matches only the main resource. A
statement with `subresources: ["status"]` matches only that subresource, and
`subresources: "*"` matches every subresource registered for the matching
kind, but still not the main resource. Consequently, granting `* on *` does
not silently grant a subresource registered in the future; a Role that truly
needs both writes two statements. The seeded `system-admin` policy does so:

```
{ effect: Allow, kinds: "*", verbs: "*" }
{ effect: Allow, kinds: "*", verbs: "*", subresources: "*" }
```

A subject's access on `(verb, kind, subresource?)` under a given policy is permitted iff at least one `Allow` statement matches **and** no `Deny` statement matches — Deny wins. This exists because pure-additive, union-only permission sets cannot express subtraction from a wildcard: "everything except `delete` on `Environment`" has no faithful positive encoding when the set of resource kinds is open-ended (operators register new kinds at runtime via `ResourceDefinition`, Rise's existing mechanism for registering a new resource kind's schema) — enumerating every other kind explicitly would silently exclude any kind registered later. A `Deny` statement expresses it directly:

```
Allow: * on *
Deny:  delete on Environment
```

Roles and RoleBindings are data (rows), not compiled match arms — operators and org-admins configure who can do what at runtime, no redeploy required. Restrictions ("caps", §5) are just `Deny`-bearing bindings of this same policy shape, not a separate construct. A Role's own statement list is, like any other resource, ordinarily writable by whoever holds `update` on its kind — but because editing a widely-bound Role changes what every subject bound to it can do, that write is gated the same way a RoleBinding write is (§5).

**Two kind pairs, one per placement level.** The resource store's parent model is exact — a kind declares one parent, not a choice of parents — so policy objects come as two same-shaped pairs, the same fork Kubernetes resolves with `ClusterRole`/`Role`: **`Role` and `RoleBinding`** are parented under an `Organization` (org-level policy, authored by whoever holds `create`/`update` there — org-admins by default, further delegable like anything else), while **`PlatformRole` and `PlatformRoleBinding`** are parented at the root (platform-level policy — operator-authored, not by a bespoke rule but because only `system:operators` holds `create` at root under ordinary default-deny). Where this document says "binding" or "Role" without qualification, the statement applies to both pairs alike.

### 4. RoleBindings — targeting a subject to a slice of the resource tree

A **RoleBinding** attaches a Role to a subject, at a `Scope`, optionally narrowed by a `LabelSelector`:

```
Subject:        <literal SubjectId, e.g. acme-corp/team:platform> | <subject template, e.g. team:${ref.name}>
Scope:          <path, e.g. Environment/acme-corp/env-prod>  |  "*"   # always present after normalization
LabelSelector?: { key: <label key>, value?: <fixed value> }      # optional narrowing filter
roleRef:        { kind: PlatformRole | Role, name: <name> }
```

`Subject` is deliberately singular in the initial model. Giving the same Role to unrelated subjects uses one binding per subject; giving it to a population uses a Team, `org:<name>`, or `system:authenticated`. A future `subjects:` convenience may normalize each entry into an independent virtual binding without changing policy semantics, but storing several subjects in one normative binding is rejected for now. In this model subject identity participates in wildcard replacement, dynamic-template resolution, recipient-boundary validation, grant-gating, and audit explanation; keeping it singular avoids partial replacement or partial mutation semantics that Kubernetes' additive-only bindings do not have.

`Scope` is always present after write-time normalization and establishes the binding's applicability domain — it applies to the named node and everything beneath it. If omitted on an org-parented `RoleBinding`, it defaults to that binding's parent `Organization/<name>`; if omitted on a root-parented `PlatformRoleBinding` with a static Team or ServiceAccount subject, it defaults to that subject's `Organization/<name>`; in every other `PlatformRoleBinding` case it defaults to `"*"`, the whole tree. These defaults are ordered deliberately: omission can never turn an org binding or an org-native literal subject into a platform-wide grant. An explicitly authored `Scope` remains subject to the containment and subject-org validation below. `LabelSelector`, when present, doesn't replace `Scope` as a separate targeting mode — it narrows the grant to only the resources *within* that scope whose `effectiveLabels` (§6.1) match. A binding with no `LabelSelector` grants over its entire scoped subtree; a binding with one grants only over the subset of that subtree carrying the matching label. A Role statement's `kinds: "*"` means every kind *inside this applicability domain*; only `Scope: "*"` reaches both org-contained and root-scoped resources.

A `Scope` path is written exactly like a resource URL with the `{group}/{version}` prefix dropped (§8): the target's **kind** first, then its ancestor names root-first, then its own name — `Environment/acme-corp/env-prod` is the Environment `env-prod` under org `acme-corp`; `Organization/acme-corp` is the org itself; `RuntimeClass/standard` is a root-scoped instance (§9). Ancestor kinds are derived from the leaf kind's declared parent chain, the same resolution the URL grammar already performs — one path grammar, not two. (A distinct separator between the kind and the path was considered for visual clarity and rejected to keep `Scope` byte-identical to the URL form — see Alternatives considered.)

`Scope` is likewise a shared parsed type, never an opaque string inside the evaluator. At binding write time the same canonical path parser used by the generic resource API accepts either exactly `"*"` or a canonical resource path: it rejects empty or extra components, dot segments, query/fragment syntax, embedded separators, and non-canonical encodings; resolves the leading Kind through `ResourceDefinition`; and verifies that the number and order of following names exactly match that kind's declared parent chain. The target node must already exist in the store or be created in the same atomic transaction. Only after syntax, kind-chain, existence, and normalization checks pass are the containment and subject-org rules below evaluated. The normalized `Scope` is what is persisted and compared, so HTTP routing, storage, subset checks, and authorization cannot assign different meanings to the same bytes.

Two write-time validation rules tie a binding's placement (§3) to its content. **Containment:** an org-parented `RoleBinding`'s `Scope` must lie within its own parent org's subtree; a root-parented `PlatformRoleBinding`'s `Scope` is unrestricted — `"*"`, any org path, or a root-scoped instance such as `RuntimeClass/gpu-b` (§9). This makes "org-admins cannot author platform-wide or cross-org grants" structural, not asserted. **Reference direction:** the structured `roleRef` names its target with separate `kind` and `name` fields — `{ kind: PlatformRole, name: resource-owner }` or `{ kind: Role, name: deploy-viewer }`. An org `RoleBinding` may reference its own org's `Role`s or any `PlatformRole` (how platform-shipped Roles are bound org-locally without duplication); a `PlatformRoleBinding` may reference only `PlatformRole`s — org-authored policy can never escape its org through a platform-wide binding. References are never resolved by bare-name fallback: an org creating a `Role` named `resource-owner` shadows nothing, because every existing `{ kind: PlatformRole, name: resource-owner }` reference keeps meaning exactly that.

**Static** targeting — a fixed subject:

```
Subject: acme-corp/team:platform
Scope:   Environment/acme-corp/env-prod
roleRef: { kind: Role, name: deployment-editor }
```

```
Subject:       acme-corp/team:platform
LabelSelector: { key: rise.dev/team, value: "platform" }
roleRef:       { kind: Role, name: project-editor }
```

(Role names other than `resource-owner`, §6.2, are illustrative throughout this document — `project-editor`, `deployment-editor`, etc. are examples of Roles an operator or org would define, not literal platform-shipped defaults.)

A `LabelSelector` carrying a `value` pairs with a static Subject — an equality filter on an already-fixed grant, no extraction needed. One without a `value` pairs with a dynamic Subject — an existence match whose matched value feeds `${ref.name}` (below). A static Subject combined with a value-less `LabelSelector` is **rejected at write time**: it would grant a fixed subject access to any resource carrying *any* value for that key, regardless of what it says, which is never the intent of a literal, non-templated binding. A dynamic Subject combined with a value-carrying `LabelSelector` is accepted but redundant — the template can only ever resolve to that one fixed string, so the same policy is more simply written as a static binding naming that value directly.

**Dynamic** targeting — the subject is resolved from the matched label's own value at evaluation time, via string-template interpolation:

```
Subject:       team:${ref.name}
LabelSelector: { key: rise.dev/owner }
roleRef:       { kind: PlatformRole, name: resource-owner }
```

Evaluating a dynamic binding against a resource is two independent steps: resolve the `LabelSelector` against the resource's `effectiveLabels` to get a raw string value, then substitute that string for `${ref.name}` in the subject template and hand the resulting string to the same subject-resolution code any static binding uses. The template step needs no per-kind knowledge of what the substituted value "means" — it is plain string interpolation, decoupled from subject parsing. §6.3 covers how the resolved subject's organization is determined for org-native kinds.

**Evaluation algorithm**, for subject `S` requesting `(verb, kind, subresource?)` on resource `r`:

1. Collect every binding targeting `S`, a group `S` belongs to (§1 membership expansion), or a template resolving to `S` against `r`, whose `Scope` covers `r` and whose `LabelSelector`, if any, matches `r.effectiveLabels`. This *includes* any **cap** bindings — `Deny`-bearing bindings targeting a broad subject such as `system:authenticated`, scoped to `r`'s org or the whole tree (§5) — because `S` is a member of those broad subjects too. Caps are ordinary bindings, not a separate layer.
2. Apply wildcard-replacement (§1): suppress a superseded wildcard binding's *Allow* content but carry its `Deny` statements forward (§1's Deny-preservation invariant — load-bearing here, since a cap *is* a `Deny`).
3. Union the surviving bindings' Role policies — grants and cap `Deny`s alike — into one combined policy and evaluate `(verb, kind, subresource?)` Allow-with-Deny-wins (§3). That is `S`'s effective decision on `r`: permitted iff some `Allow` matches and no `Deny` matches. Capping needs no separate step — it *is* Deny-wins over the same union.

A **worked trace**: `acme-corp/team:platform` requests `delete` on `Deployment/acme-corp/env-prod/foo`, which carries `rise.dev/owner: platform`.

- Step 1 collects two bindings: (a) a scope binding at `Environment/acme-corp/env-prod` granting `deployment-editor` = `{Allow: * on Deployment}`; (b) the seeded dynamic ownership binding (§6.2), which resolves to `acme-corp/team:platform` via the `rise.dev/owner` label and grants `resource-owner`. No cap binding for `acme-corp` applies.
- Step 2: no wildcard/specific collision between these two.
- Step 3: union = `{Allow: * on Deployment} ∪ resource-owner's statements`; no `Deny` present → `delete` is permitted.
- **Result: allowed.**

Now suppose the org has separately authored, at `Environment/acme-corp/env-prod` specifically, a binding for `acme-corp/team:platform` with Role `{Deny: delete on Environment}` (the org's own "nobody deletes an Environment here" rule, §5). That binding's Role statement is unioned into the same combined policy in step 3 for any *Environment*-kind resource under that scope — for the Environment itself, `Deny` wins and `delete` is denied, even though the broader `deployment-editor` binding would otherwise have allowed it. This is the narrower-binding-subtracts-from-a-broader-one behavior the opening primer describes: it only takes effect where a binding's Role actually carries a matching `Deny`, not merely by virtue of being placed at a narrower scope.

**Collection (`list`) authorization and read granularity.** `get` and `list` are two independently evaluated read granularities. A collection request is rooted at the requested scope node and a Kind; its result contains exactly the items of that Kind under the scope that the caller holds `list` on, each independently evaluated through the full §4 algorithm (per-item `effectiveLabels`, wildcard-replacement, Deny-wins union with any applicable cap). It is filtered per-item, never scope-level all-or-nothing. For each included item, the response projector constructs a fresh object from an explicit base-field allowlist: `apiVersion`, `kind`, and `metadata` (name, labels, `effectiveLabels`, timestamps). It must not implement metadata-only output by deleting known fields such as `spec` and `status`, because generic resources may carry arbitrary other top-level fields. If the caller also holds `get` on that individual item, the projector returns the full stored object, including any kind-specific top-level fields; otherwise it returns only the three allowlisted base fields. This permits the common list-and-inspect path to avoid follow-up `get` round trips without letting `list` alone disclose resource data. Items the caller cannot `list` are omitted and their existence is masked: a caller with no applicable `list` grant receives a masked-empty result, not a 403 that would confirm the scope is populated (consistent with §6.6/§9 existence-masking). Note the corollary of returning `effectiveLabels`: because §6.1 resolves *every* label key nearest-wins down the tree, a `list` grant exposes an ancestor's inherited label values (not just `rise.dev/owner`) on the listed children — org-wide by construction, and same-org only since inheritance never crosses the org boundary. An org that puts sensitive metadata in an ancestor label should not grant broad `list` beneath it.

This separates existence/owner visibility from data visibility, each grantable independently per RoleBinding: e.g. `list on Project` granted to `system:authenticated` at `Organization/acme` (auto-clamped to acme members by §1's recipient boundary) lets every acme member see all acme project names and owner labels — resolving name-conflict friction — while `get`/`update`/`delete` stay narrow to owned projects via the ownership binding. Cross-org isolation holds: with no `list` binding on another org's collection, that org's resources are masked entirely. Name-uniqueness is enforced per-parent (an org's Project names are unique within that org, not globally), so a create-conflict can only reveal a sibling's existence within a scope the creator can already `create` in — an intra-scope existence hint, never a cross-org leak (per-parent uniqueness, and no non-operator creates at the root where globally-named kinds live). `list` and `create` are independent verbs, so this hint does not depend on the creator holding `list`.

**Adding a subject to a group is itself a grant-gated write.** Group membership drives effective access (§1's expansion), so adding subject `M` to a group `G` — a Team membership, or a `User`'s org membership via a Team tie (§1) — is a write that introduces grants and is gated by the write-time grant gate (§5) exactly as authoring a binding is. The newly-implied grant of that write is the *full union* of the policies of every binding currently targeting `G`, resolved over `G`'s applicability domain — *including* any grant `M` gains transitively (adding `M` to a Team may newly make `M` a member of that Team's `org:<name>`, conferring every `org:<name>`-addressed binding too); that union must be `⊆` the writer's own current effective permissions. Otherwise a bare `update on Team` becomes org takeover — a writer could self-add to a powerfully-bound Team and inherit its grants without ever holding them.

Removing a subject from a group is not grant-gated: it removes every grant that flows through that Team and, on the last Team tie, every org-parented grant as well as the org's self-cap `Deny`s. A separately operator-authored `PlatformRoleBinding` may still authorize the now-non-member on that org's resources, but that access is deliberately outside the org's self-governance domain (§5's scope-faithfulness nuance) and can be restricted only by an operator-authored per-org cap. The model does not treat leaving an org as authoring a new grant merely because an org self-cap ceases to apply; doing so would let an org retain policy authority over non-members and contradict the membership boundary.

**Parents are immutable.** A resource cannot be re-parented through this API: a "move" is a `delete` at the old location plus a `create` at the new one, each independently gated (§5). There is thus no parent-change write for the grant gate to police — the model relies on the store's exact, immutable-parent property.

### 5. Restrictions, the write-time grant gate, and Role authority

Restrictions apply uniformly across every subject kind, with no asymmetry by *kind* — the same rules apply the same way whether the subject is a person or a machine. The sole exception is `system:operators` (§1), whose own request ignores every `Deny` by construction, not by subject kind — an org-admin and a Controller are treated identically to each other, and both are fully subject to every cap.

**Restrictions are `Deny` bindings, tiered by placement.** A restriction ("cap") is a `Deny` statement in a Role/RoleBinding targeting a broad subject — typically `system:authenticated`, so it reaches every actor in scope — and Deny-wins (§4) applies it. Nothing composes caps separately — a cap is just another binding in the same union. The authority to impose or remove a cap is *placement*, exactly as for grants:

- **instance-wide** — a seeded or operator `PlatformRoleBinding`, `Scope: "*"`, e.g. `{Subject: system:authenticated, Deny: create on */token}`;
- **operator per-org** — an operator `PlatformRoleBinding`, `Scope: Organization/acme` (the org cannot remove it — it is a Platform binding, operator-authored);
- **org self-cap** — an org-parented `RoleBinding`, `Scope: Organization/acme`, `{Subject: system:authenticated, Deny: delete on Environment}` (org-admin-authored).

All are read live per request, so tightening a cap takes effect immediately with nothing else to rewrite.

**Scope-faithfulness nuance.** An operator per-org cap uses `system:authenticated` in a `PlatformRoleBinding`, which is *not* org-clamped, so it reaches every authenticated actor on the org's resources — including a team-less identity. An org self-cap is an org-parented binding, so its `system:authenticated` auto-clamps to the org's members (§1); a team-less identity an operator empowered directly sits outside the org's self-governance domain and remains cappable only by an operator per-org cap. This is the same team-tie boundary already documented in §1, applied to caps.

`org-admin` is a literal platform-shipped `PlatformRole`, scoped to one org (§6.7's scope-only rule). Its exact verb list is deployment-dependent — the same operator-authored `PlatformRole` mechanism that lets an operator define `resource-owner` (§6.2) also decides how much of an org's own bookkeeping its admins can see, and this is expected to differ by how a given instance is run, not something the architecture fixes:

- **Multi-tenant SaaS default:** `{ Allow: * on every main resource }` — full ordinary CRUD within the org, same as today, but no subresources. In particular, org-admins cannot mutate platform bookkeeping through `/status` or `/finalizers`, or mint credentials through `/token`. This mirrors `resource-owner`'s own secure default: omission, rather than a Deny enumeration, means a newly registered subresource is not silently granted.
- **Self-hosted, single-team default:** two statements allowing `*` on every main resource and `*` on every registered subresource — unrestricted. If the same people run the platform and use it, walling bookkeeping off from org-admins protects them from a mistake nobody there is actually shielded from anyway.

Both are the identical mechanism, chosen once at deployment time by whoever authors the Role, not two different architectures — and the same lever widens `resource-owner` if a deployment wants owners themselves to touch their own resources' bookkeeping. An operator-authored cap `Deny` stays available as a hard backstop regardless of what any org-authored `Role` or §6.5 override later grants: a SaaS operator can place a `Deny` on selected subresource tuples in an instance-wide or per-org cap — a `PlatformRoleBinding` no org can remove — so the restriction holds even if some org tries to route around its own `org-admin` definition.

Its own definition, like any Role, is edit-gated by the write-time grant gate (below) — but because it's a platform-shipped `PlatformRole` rather than an org-authored `Role` (placement, §3), only an operator may edit its statement list; an org-admin holding it cannot widen it for themselves. Each org's bootstrap binding for it can be provisioned at either placement level, and that choice is itself a deployment knob: a root-parented `PlatformRoleBinding` keeps the org's admin roster operator-managed; an org-parented `RoleBinding` lets the org's admins self-service additional admins (the grant gate passes — they hold what they're handing out). No extra mechanism — org-creation bootstrap (§10) just picks per deployment posture. `org-admin` is a reserved Role name, recognized specially by the platform the same way `rise.dev/owner` is a reserved label key — every organization is provisioned with exactly one `org-admin`-Role binding, `Scope`-targeted to that org, at org-creation time (§10); an org-admin's authority to author its own caps and §6.7's structural constraint both key off that reserved name directly, not off any separate "is this an admin" flag.

**Role authority is placement, in three tiers.** Who may edit a Role — and against whose permissions the edit is checked — is a consequence of where the object sits in the tree (§3), not a bespoke ownership rule. A **seeded** Role is root-parented *and* evaluator-guaranteed: baked in at platform bootstrap, no write path can ever modify it, not even an operator (today only `PlatformRole/system-admin`, §1 — nothing above an operator is left to recover a self-inflicted lockout). A **platform** Role (`PlatformRole`, root-parented — e.g. `resource-owner`, `org-admin`) is editable by whoever holds `update` at root: operators only, by ordinary default-deny, and recoverable by another operator if botched. An **org** Role (`Role`, parented under one Organization) is editable by whoever holds `update` at that position — org-admins by default, delegable further like any other grant, and referenceable only from that org's own `RoleBinding`s (§4's reference-direction rule). This also answers a question the write-time subset check alone can't: a Role referenced by bindings across multiple orgs, whose editors hold different effective permissions in each, would have no single well-defined "whose permissions apply" answer when its body is edited — placement fixes it, structurally. Editing an org `Role` is checked against the editor's own permissions in its one parent org (the general grant gate below); editing a `PlatformRole` requires holding `update` on kind `PlatformRole`, which by default only operators do, and the editor is still, as for any subject, held to holding what they hand out.

**The write-time grant gate.** Every write that introduces a grant to any subject — authoring or editing a RoleBinding (its `Subject`, `Scope`, `LabelSelector`, or `roleRef`), editing an existing Role's own statement list, a gated label write (§6.6), or adding a subject to a group (Team membership, or a `User`'s org membership via a Team tie — §1, §4), whether at resource creation or on an existing resource — is valid only if the newly-implied grant is `⊆` the writer's own current effective permissions at the moment of the write. That single check also enforces every applicable cap for free: because a cap `Deny` applies to the writer too, their own effective permissions are *already* capped, so "`⊆` my own effective permissions" cannot hand out what a cap has already removed from me. (An operator, whose Denies are ignored, can author a grant a cap would nullify; but the cap re-applies to the *recipient* at read time via Deny-wins, so the cap still holds.) For a Role edit, "the writer's permissions" are unambiguous because authority is placement (above) — an org `Role`'s edits are checked against the editor's permissions in its one parent org; a `PlatformRole` can only be edited by an operator in the first place. Editing a RoleBinding's `Scope` to move it across an org boundary is checked against both orgs — equivalent to independently validating a delete at the old scope and a create at the new one. This is the single canonical statement of the rule; §4's binding examples, §6.6's label-write gate, and §7's token issuance all point back to it rather than restating it.

Creating or retargeting a `UserIdentity`, `ControllerTrustPolicy`, or `ServiceAccountTrustPolicy` is also an identity grant: it can let an external principal become the parent identity before RBAC evaluation. In addition to ordinary create/update permission on that child kind and the authentication-specific schema checks, the write-time gate treats the parent's full live effective grant as newly implied and requires it to be `⊆` the writer's own effective permissions. The external `issuer`/`subject` identity of a UserIdentity and the parent reference of every identity-policy resource are immutable; remapping is delete plus create and is checked as a fresh grant. This prevents a narrowly delegated identity-policy editor from attaching their own issuer subject to a more privileged User, ServiceAccount, or Controller. Tightening or deleting a mapping introduces no grant and needs only ordinary write authority.

This granter-subset check is performed once, at write time, and not re-evaluated afterward — if the writer's own permissions later shrink, grants they already made are unaffected. (Their permissions shrinking does not retroactively invalidate what they already gave away — only a live cap `Deny`, tightened later, can claw back what a grant provides, by applying to the recipient at read time.)

**The subset check is Deny-aware and scope-exact.** The `⊆` comparison is not an Allow-list containment. The newly-implied grant is compared against the §3 Allow-and-no-Deny evaluation of the writer's *full* effective policy — Allows net of Denies — computed pointwise per `(verb, kind, subresource?)`, over *exactly* the new grant's `Scope`∩`LabelSelector` domain, not "the writer holds this somewhere." A writer holding `{Allow: *; Deny: delete on Environment}` must not be able to hand out an unrestricted `{Allow: *}`: an Allow-only comparison would leak `delete on Environment` straight past their own Deny. And a verb held only at a narrow scope must not authorize a broader-scope grant — the domains must match, not merely intersect non-emptily. The `(Scope, LabelSelector)` domain comparison is *intensional* — evaluated over the selector *specifications*, never over the set of resources currently matching — so future resources are covered fail-closed. Same-key `LabelSelector` domains order by specificity: no-selector ⊒ `{key}` ⊒ `{key, value}` (an unrestricted or key-only selector's domain contains a value-restricted one's, not the reverse). Selectors on *different* keys are treated as possibly-intersecting and the check fails closed — §1's "different keys never collide" rule is a *replacement*-collision rule and must not be reused to prove domain-disjointness here. A union of value-restricted Allows never covers an unrestricted-selector domain.

**A concrete Role-write example.** Bob holds only `update` on kind `PlatformRole` (granted via an operator-authored `PlatformRoleBinding` — a narrow maintenance grant) and holds no other binding. Bob edits the `resource-owner` Role, appending `{Allow: create on */token}`. Because Role edits go through the same write-time grant gate as everything else, this write is checked against Bob's own effective permissions: Bob does not himself hold token creation on anything, so the newly-implied grant is not `⊆` his effective permissions, and the write is **rejected** — even though hundreds of subjects are bound to `resource-owner` via the platform-wide default (§6.2) and would otherwise all have been silently escalated by one edit.

**Restrictions are transparent, not opaque.** Caps are operator- or org-authored `PlatformRoleBinding`s/`RoleBinding`s — inspectable bindings, readable like any other resource. An org-admin sees *why* they are capped two ways: (a) the explain/simulator endpoint surfaces the applicable `Deny` bindings on any denial, so a denial is diagnosable rather than opaque; and (b) a read grant (`get`/`list`) on the `PlatformRoleBinding`s whose `Scope` covers their org lets them inspect the caps directly. A denial is therefore always diagnosable, never a bare rejection with no visible cause.

**Live, uncached enforcement.** Every grant and every cap `Deny` is resolved fresh on every request. Tightening a cap or narrowing a Role takes effect immediately for every subject currently relying on it, with nothing to rewrite — this is also what makes "revoke the role" exactly as effective as "revoke the token" (§7).

### 6. Ownership and attribution

There is no ownership primitive in this model. Nothing in §1–§5 knows what an "owner" is: the evaluator sees subjects, bindings, and labels — nothing more. "Ownership" exists only as the *effect* of one binding (§6.2) that happens to grant owner-like permissions to whoever a label names. Remove that binding and the concept vanishes from the platform without touching the engine; override it (§6.5) and ownership *means something else* in that org. The label is likewise just a convenient, inspectable targeting mechanism: `rise.dev/owner` carries no authorization semantics of its own — no label key does — a key becomes access-relevant exactly when, and only for as long as, some binding's `LabelSelector` references it (§6.6 step 2). What this section defines is therefore not an ownership *feature* but a shipped default *convention*: a reserved key, one seeded dynamic binding, and the write-gating that any access-driving label automatically inherits. (A dedicated single-subject `ownerRef` field — a true ownership primitive — was considered and rejected; see Alternatives considered.)

#### 6.1 — Attribution is one governed label

A single reserved key, `rise.dev/owner`, holds a bare name (`platform`, `niklas`) — never a `kind:name` string. Values are validated to contain no path or subject separators (`/`, `:`), so a value can never smuggle an alternate path or `kind:name` into §6.3's org-fixing resolution. The label stores minimal, display-friendly data; the binding that selects on it (§6.2) declares how to interpret that data. Nested resources without their own value inherit one through `effectiveLabels` — a computed field, always resolved live (never stored or cached, consistent with §5's live-evaluation philosophy — both the read-path display value and the authorization-path match in §4 are the same computation), resolved by walking the already-fetched ancestor chain leaf-to-root, **nearest value wins per key**:

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
roleRef:       { kind: PlatformRole, name: resource-owner }
Scope:         "*"
```

`resource-owner` is a literal platform-shipped `PlatformRole`, defined as:

```
resource-owner = { Allow: [get, list, update, delete] on * }
```

— deliberately excluding `create` and every subresource. Ownership alone never grants the ability to update `/status` or `/finalizers`, create a token for an owned ServiceAccount, or create new child resources; those require a separately-granted Role, same as for any non-owner subject. This is the multi-tenant SaaS default; like `org-admin` (§5), it's operator-authored `PlatformRole` data, and a self-hosted operator who wants owners to see their own resources' bookkeeping widens it the same way `org-admin` is widened — no separate mechanism, same lever. (An organization that wants ownership to imply more, within whatever the deployment's own default already allows, can grant it explicitly via its own override binding, §6.5 — subject to the same write-time grant gate as any other grant.)

Unlike the operator and org-admin tiers, the org-*user* tier ships no baseline `create`-granting Role: ordinary org-user access beyond ownership is deployment-configured via org-admin delegation or org-creation bootstrap (§10), and `resource-owner` intentionally omits `create` (above).

When the resolved subject happens to be the caller themselves, that is simply the self-ownership case falling out for free — no separate condition type is needed for it.

#### 6.3 — Resolving a dynamic subject's organization

A dynamic binding's `Subject` template has no concrete identity until it's evaluated against a specific resource. For an org-native kind (Team, ServiceAccount), the resolved subject's organization is taken to be **the matched resource's own organization** — consistent with those kinds' org-native identity (§1): a `rise.dev/owner: platform` label on a resource under `acme-corp` resolves to `acme-corp/team:platform`, never a `platform`-named team in some other org. The binding's own `Scope` therefore governs a different thing: which resources' evaluations consider the rule at all (its applicability domain — `"*"` for the platform default, one org for an override), not the resolved subject's organization, which is always derived per-resource. This is why §6.2's binding can validly carry both a `LabelSelector` and a `Scope: "*"` without contradicting §1's org-matching rule for static Team bindings — that rule constrains literal subjects; a template's resolved-subject org is constrained separately, per match, as stated here. On a root-scoped resource with no organization to resolve against, dynamic resolution of an org-native subject has no org to fix and therefore **fails closed** — it confers no ownership rather than defaulting to any org.

#### 6.4 — Individual ownership and organization-specific grouping need no new subject kind

Subject kind stays closed (User, Team, ServiceAccount, Controller — each carries real membership-resolution machinery, not worth making pluggable). Label *keys* are open — any organization can introduce one:

```
# individual ownership — same mechanism, a different kind and key
Subject:       user:${ref.name}
LabelSelector: { key: rise.dev/assignee }
roleRef:       { kind: PlatformRole, name: resource-owner }

# an org's own grouping concept — reuses Team, never registers a new kind
Subject:       team:${ref.name}
LabelSelector: { key: rise.dev/squad }
roleRef:       { kind: Role, name: project-editor }
```

A "squad" never exists as a subject kind — it is a Team, targeted via a label key the organization chose to call `rise.dev/squad`. This covers grouping concepts whose *membership* is ordinary Team membership; it does not provide a way to define a group with genuinely different membership semantics (externally-synced, rotation-based, non-exclusive overlapping groups, etc.) — that would require a real pluggable subject-kind registry, which is deliberately out of scope (Alternatives considered).

#### 6.5 — Organizations can override the default

The seeded ownership binding is ordinary `Scope: "*"` data. §1's wildcard-replace rule governs overrides the same way it governs any other wildcard — an org-specific binding for the same `(Subject, LabelSelector key)` pair replaces the platform default outright for that org:

```
Subject:       team:${ref.name}
LabelSelector: { key: rise.dev/owner }
roleRef:       { kind: Role, name: project-viewer } # read-only ownership; an org-parented RoleBinding
Scope:         Organization/acme-corp
```

The override write still passes the ordinary write-time grant gate (§5) — no override-specific mechanism.

This "override" works specifically because the default binding uses a wildcard `Scope`, and §1's replace-outright rule applies only to a wildcard-vs-specific collision — it is not a general "narrower always overrides broader" mechanism. Two non-wildcard bindings for the same subject at different tree depths union additively (§4) rather than replace; narrowing access below what a non-wildcard ancestor binding grants requires an explicit `Deny` statement in the narrower binding's Role (§4's worked trace), not simply placing a binding at a deeper scope.

#### 6.6 — Label writes that retarget access are gated by the write-time grant gate itself

There is no hardcoded list of protected fields. On any write — creation or update — that sets or changes `metadata.labels[K]`:

1. If the value for `K` is unchanged from the resource's current effective value, no gate — ordinary `update` permission suffices.
2. If no binding *anywhere applicable to this location in the tree* (by `Scope` and kind, regardless of whether it currently matches this resource's present labels) selects on `K` via its `LabelSelector`, no gate. This check is evaluated against binding *applicability*, not the resource's pre-write label state — a resource that has never carried key `K` before is still gated on its first write, since a binding selecting on `K` could apply to it the moment the value is set.
3. Otherwise, resolve effective permissions before and after the proposed value and diff them — where "the value" is the `effectiveLabels`-*resolved* ownership (§6.1), inherited ancestor values included and nearest-wins applied, not the resource's own stored label read in isolation. *Removing* an access-driving label is a "change" like any other and is gated: dropping a child's own `rise.dev/owner` makes it inherit an ancestor's owner via nearest-wins — an escalation if that ancestor names the writer's team, which a diff over the resource's own stored label would misread as `victim → absent` and wave through as de-escalation. The diff is simulated, computed atomically with the write so a concurrent binding change cannot open a window between simulation and commit. The newly-implied grant is computed over *all* subjects any selecting binding resolves to before and after — not only the writer's own access — and each such grant must be `⊆` the writer's own current effective permissions — §5's general write-time grant gate, applied here. Moreover, the before/after diff spans not only `r` but every resource that inherits `r`'s value for `K` through `effectiveLabels` (§6.1) — `r`'s `K`-inheriting subtree — since relabeling `r` can newly grant access over descendants that inherit the changed value.

A key becomes gated the moment some binding's `LabelSelector` references it, and stays ungated otherwise: protection is a consequence of binding existence, never a hardcoded field name.

*Implementation note:* the subtree diff (step 3) is a cold path, implementable via a recursive `parent_uid` query; its atomicity is covered by §5's existing write-consistency requirement for the grant gate, needing no §6.6-specific mechanism.

**A narrow, explicit exception applies at creation.** A subject holding `create` on a kind may, in that same creation request, set an owner-selecting label to name only *themselves*, or a Team they are currently a member of (itself an ordinary grant-gated fact — joining a team is its own gated write, not something a creator can manufacture on the fly to widen this exception) — without that specific write needing to independently pass the general subset check. This is not "handing out access you don't hold": there is no prior owner being displaced, only a first claim, and the claim is restricted to identities the creator can already act as. Naming a team the creator does *not* belong to is not covered by this exception and falls back to the general rule: checked `⊆` the creator's own effective permissions like any other grant, and rejected unless they hold some independent basis for it (e.g. being an org-admin).

"Creation" here means bringing a genuinely new, previously-nonexistent resource identity into being — never a write that targets an identity that already exists in the store, even one currently soft-deleted or otherwise inactive. Restoring a soft-deleted resource, or an upsert-style write that would create-or-update depending on whether the target already exists, is **not** creation for this exception's purposes and is unconditionally subject to the general rule instead: an implementer must resolve "does this identity already exist" before deciding whether the exception can apply, exactly because the exception's own safety rests on there being no prior owner to displace — which is only true for a genuinely new identity. The exception applies exactly once, under that definition — every later write to the same label, including the very next `update`, is unconditionally subject to the general rule above.

When the owner-selecting value would resolve through *multiple* applicable templated bindings on the key, the exception applies only if *every* subject it resolves to is one the creator may claim — themselves, or a team they currently belong to; if any resolved subject is one they cannot claim, the general subset rule governs the whole write. A `delete`+`create` sequence that reclaims a freed name is the sanctioned move primitive (§4), not an ownership-takeover flaw: reclaiming a name follows from holding `delete`+`create` on that scope — a permission-configuration decision — and defending against it would break "move."

The check is a genuine subset comparison, not merely "does this write avoid dropping access to zero." An editor with no independent claim to `resource-owner` could relabel `rise.dev/owner: platform → their-own-team` without ever dropping the resource's access to zero — they would simply redirect it to themselves. The subset check blocks this; a caller who currently holds the role being handed off (the resource's actual current owner, or an org-admin whose access is independent of any label, §6.7) passes trivially, so legitimate transfers are unaffected.

Referential-integrity validation (§6.7) runs only *after* this gate passes — a caller who would be denied by this check never learns whether the value they attempted resolves to a real Team/User, avoiding turning the validation step into an unauthenticated existence oracle.

#### 6.7 — Orphan prevention is separate from escalation prevention

*Escalation* — an unauthorized party redirecting access to themselves — is §6.6's job. *Orphaning* — a legitimate write accidentally locking everyone out, typically a typo — needs two different mechanisms:

- **Referential-integrity validation at write time.** A value written to a label some binding selects on must resolve to a real Team/User, checked synchronously, rejected with a fuzzy-match suggestion (`Team 'platfrom' does not exist. Did you mean 'platform'?`) rather than silently stored.
- **Admin access stays independently derived, enforced structurally.** The `org-admin` Role (§5) may only ever be granted via a `Scope`-targeted binding, never a `LabelSelector`-targeted one — a platform-level constraint on binding authorship, not merely a convention. This makes "no resource is reachable *only* via a dynamic ownership binding" a checkable rule rather than an assumption: since `org-admin` access can never be routed through a mutable label by construction, even a validly-transferred-but-wrong reassignment stays recoverable by an org-admin.

### 7. Token issuance for ServiceAccount/Controller identities

Authentication — proving which known Rise identity a credential represents, and whether that caller may assume another identity at all (issuer/JWKS/claims trust-policy match) — remains a distinct concern from authorization; trust-policy matching is never folded into the RBAC model. The boundary between them is nevertheless normative: the authorization engine accepts only an `AuthenticatedPrincipal` carrying an already-parsed canonical `SubjectId`, never a JWT, a raw `sub` claim, or a caller-supplied subject string.

**Rise's roles across the two planes.** Two distinct planes each use the word "authorization," and keeping them apart is load-bearing for the boundary above and for the deferred `/token`-on-`User` direction (§10):

| Plane | Role | Played by |
|---|---|---|
| User login (interactive) | OAuth Authorization Server | the upstream OAuth/OIDC IdP (Dex by default) |
| User login (interactive) | OAuth Relying Party | Rise |
| Token issuance | Security Token Service (RFC 8693) | Rise |
| Access control (RBAC) | Policy Decision Point + Enforcement Point | Rise |

Rise is the sole authority on the access-control plane — the Policy Decision Point this whole model describes — and its own token issuer (a Security Token Service, for workload exchange and sessions). But for *interactive* user login it is a relying party, not the authorization server; that role stays with the configured upstream IdP. "Authorization Server" (an OAuth login/consent role) and "authorization" (an access-control decision) are different concerns on different planes, and this model governs only the latter. This is why unifying the OAuth login flow onto `/token` (§10) means sharing the *issuance core*, not making Rise an authorization server: interactive login stays a relying-party flow, while `/token` serves only the non-interactive RFC 8693 exchange.

For readability, this section calls `(create, ServiceAccount|Controller,
token)` a **token-create permission**. That is prose shorthand, never a new
verb in the policy schema.

**Only known Rise principals reach authorization.** Every authentication adapter first validates the credential's signature, issuer, intended audience, expiry/not-before, and credential type, then maps it to an existing, active Rise identity. User SSO performs an exact lookup of a live `UserIdentity.spec.(issuer, subject)` pair and resolves that resource's User parent; matching email alone never maps or links an account. A syntactically plausible `sub` is lookup input, not identity proof. The only subject kinds that may be request principals are User, ServiceAccount, and Controller. Team, `org:<name>`, `system:authenticated`, and `system:operators` are derived membership targets only and are rejected as token principals; operator status is derived from a successfully authenticated User plus the live allowlist. After lookup, the adapter constructs `AuthenticatedPrincipal { subject: SubjectId, provenance, token_class, actor }`; only that typed value crosses into authorization. An unknown, deleted/disabled, malformed, or non-principal subject fails authentication even if the surrounding token is otherwise validly signed.

**External workload credentials stop at token exchange.** A ServiceAccount or Controller's external source-issuer credential is accepted only by `POST` to that target identity's `/token` subresource, never directly by the generic resource API. The exchange handler validates the external credential, finds candidate `ServiceAccountTrustPolicy`/`ControllerTrustPolicy` resources by normalized issuer, evaluates their audience and claim constraints, and requires the result to resolve unambiguously to one existing active source ServiceAccount or Controller before the `(create, target kind, token)` authorization check runs; zero or multiple matches fail authentication. Arbitrary external `sub` values never become authorization subjects. It then checks the target identity's trust policy and the known source principal's token-create permission, and issues a Rise-signed access token whose `sub` is the target's canonical `SubjectId`. The resource API accepts Rise-issued workload access tokens (and Rise-authenticated User sessions) only; for a workload token it revalidates Rise issuer, signature, Rise API audience, time bounds, access-token type/class, canonical `sub`, permitted principal kind, and live existence/active state before constructing the principal. Thus Controllers and ServiceAccounts must exchange their source credentials for a Rise-issued token before calling the resource API, creating one authentication choke point without letting external JWT vocabulary leak into policy evaluation.

Creating a token is *additionally* gated by `(create, ServiceAccount|Controller, token)` (§2), held on the specific identity being assumed — analogous to AWS STS `AssumeRole` requiring both a trust policy on the role and an identity-based `sts:AssumeRole` grant on the caller. `create`, rather than `get`, is intentional: issuance performs a non-idempotent security-sensitive operation and returns a new credential even though no Token resource row is persisted. This is deliberately a privilege-elevation-capable pattern: the resulting token resolves the **target** identity's own effective permissions, live, on every subsequent request — not the calling subject's. A caller who holds only `create` on a ServiceAccount's `token` subresource, and nothing else, can still mint it a token wielding that ServiceAccount's full, broader grant; the minting caller's own permissions are irrelevant to what the minted token can do once issued.

**Controller identities are root-scoped.** A Controller is org-agnostic (§1), so its identity resource sits at the tree root, not under any org. `create` on a Controller's `token` subresource therefore requires a `PlatformRoleBinding` — operator-only by placement (§4's containment rule) — and an org-admin's org-scoped grant provably cannot reach it. Without this, an org-admin whose default policy included that subresource could mint a token for an org-agnostic Controller, which then resolves that Controller's cross-org grant — a cross-tenant escalation. A ServiceAccount, org-native by construction, stays reachable by its own org's token-create grants.

**Chaining is bounded to one hop.** A token obtained via token exchange cannot itself be used as the calling identity for a further token-exchange request. Only a directly-authenticated caller — a User session, or a ServiceAccount/Controller presenting its own source-issuer credentials, never an already-minted Rise token — may mint a token for a target identity. This is enforced structurally, not by convention, and it turns on two *separate* claims. Attribution rides the standard RFC 8693 `act` (actor) claim: every Rise-minted token records the actor chain — who minted it — in `act`, purely for delegation-attribution and audit. The one-hop *gate*, by contrast, keys on a distinct, unconditional token-class marker (`token_class`) that the exchange endpoint stamps on *every* token it mints, independent of `act`'s presence or content: the endpoint rejects any presented credential bearing that class as a caller identity for minting, regardless of what token-create grants it would otherwise satisfy. The two are deliberately decoupled — were the gate keyed on the audit claim instead, a future change to *when* `act` is emitted (say, omitting it for some first-party mint) would silently reopen chaining; a mandatory class marker cannot be so weakened. A User session token and a directly-issued source JWT (signed by the identity's own configured trust-policy issuer, not by Rise) carry no `token_class`, so legitimate first-hop minting is unaffected. Neither claim influences authorization — every request still resolves the target identity's own live grants — but `act` is attached to audit logging on every request the token makes, so actions taken as the target identity stay attributable to whoever minted the session, and incident response for a compromised minting caller can enumerate exactly the sessions it opened. This bound would be defeated if a trust policy could be configured to accept Rise's own token issuer as a valid source-issuer — a caller could then present an already-minted token to the *authentication* layer (not the token-exchange endpoint) as if it were independent source-issuer credentials for a second identity, re-entering as a "directly-authenticated caller" a second time. Trust policies may therefore never name Rise's own issuer/audience as an accepted source — this closes the direct, degenerate case, enforced at trust-policy write time.

It does **not**, by itself, close the harder case where the round trip goes through a legitimately-trusted *external* system: minting a token with a non-Rise `requested_audience` (below), presenting it to that external system, and receiving back a genuinely externally-issued credential (no `act`/`token_class`, a different issuer entirely) — which can then be presented to authenticate as a second Rise identity whose trust policy legitimately trusts that same external issuer, an entirely ordinary configuration for workload-identity federation. Rise cannot generally distinguish such a credential from one issued through an unrelated path, since provenance isn't preserved across an external system it doesn't control. This is accepted as a structurally harder, unclosed risk rather than papered over: authentication and trust-policy configuration are explicitly a separate concern from this model (opening paragraph, this section), and a full closure would require either the external system preserving and exposing mint provenance (out of Rise's control) or forbidding federation to any audience whose issued credentials could plausibly be trusted back by another Rise identity (which would defeat the purpose of federation). Operators configuring trust policies for federated identities should treat this the same way they'd treat any cross-system credential-laundering risk in a multi-hop trust chain. Without the one-hop bound at all, a caller holding token-create on identity A, where A itself holds token-create on identity B, could traverse an arbitrarily long chain to reach whatever B (or C, or D...) is entitled to, with no single grant along the way reflecting the actual resulting reach. The one-hop bound trades away legitimate multi-level minting automation (an orchestrator minting tokens for workers that themselves mint tokens for sub-workers) for a locally-reasonable blast radius: whoever grants token-create on some identity X can evaluate the risk from X's own grants alone, without needing to trace X's own token-create grants transitively.

The token carries identity only, never baked-in permissions — every request re-resolves the **target identity's** live grants, any applicable cap included, exactly as for User sessions. Revoking or narrowing the target's own Role is therefore exactly as effective as revoking the token itself, before its TTL naturally expires. This does **not** extend to the token-create grant that authorized issuing the token in the first place: revoking a caller's token-create grant on identity A stops them from minting a *new* token for A, but has no effect on a token they already minted — it continues to resolve A's own live permissions for the rest of its TTL, same as any other token for A. Responding to a suspected compromise of the *minting caller* (rather than the target identity itself) means acting on the target's own grants, or waiting out the TTL — there is no separate token-revocation list. This is why TTLs are kept short and bounded by the platform maximum (below) rather than treated as a formality.

**Accepted risk — token-create re-delegation.** Holding token-create on identity `A` is equivalent to holding `A`'s full reach, so the write-time grant gate's subset check (§5) permits re-delegating token-create on A onward: token-create on A is trivially `⊆` token-create on A. The one-hop bound (above) limits token-*exchange* chaining — how many times a minted token can be re-presented — not the spread of the *grant* itself, which propagates like any other permission and is bounded only by org-level binding-authorship. This is documented rather than gated.

A token-exchange request may ask for **less** than the target identity's full effective grant — a narrower `requested_scope` (encoded in the token as a fixed cap, layered on top of the target's live-resolved grant rather than replacing it — the cap can only ever narrow further what the live resolution would otherwise allow, never substitute for it), and/or a different `requested_audience` (native RFC 8693 concepts: federating a token out to an external system such as AWS STS versus Rise's own API). A request may never ask for more than the target identity itself holds.

Max token TTL is a single platform-global configuration value — not per-org, not composed. A token-exchange request may ask for a shorter TTL; a request above the platform maximum is rejected (or clamped to it) at issuance. This is a fixed platform constant checked at the moment of issuance, not a store-resolved restriction — it can never be unset, so no misconfiguration can yield an unbounded-TTL token.

### 8. One canonical kind token — no plural forms

A kind has exactly one name: the `kind` itself (`Deployment`, `RuntimeClass`). Role statements (`kinds:`), `Scope` paths (§4), reference declarations (§9), and the resource API's URL grammar all use that same token — the URL grammar becomes `{group}/{version}/{Kind}/{ancestor}…/{name}` (a collection/`list` URL drops the trailing item name: `{group}/{version}/{Kind}[/{ancestor}…]`), and a subresource appends its canonical name to an item URL: `{group}/{version}/{Kind}/{ancestor}…/{name}/{subresource}`. `ResourceDefinition` no longer declares a plural at all. Kubernetes maintains a parallel plural vocabulary for REST-style collection URLs, at the cost of every RBAC rule (`resources: ["deployments"]`) naming things differently from every manifest (`kind: Deployment`), with a lookup command (`kubectl api-resources`) existing largely to map between the two. A naming scheme that needs a lookup table is a tax, and collection-URL aesthetics don't pay for it. This changes the shipped URL grammar, which is sanctioned: the surface carries no compatibility constraints (Context).

### 9. References to platform-provided resources

> **Deferred.** This section describes a designed but **deferred** capability — platform-provided *selectable* resources (e.g. `RuntimeClass`). It is not part of the initial model or its conformance suite, and §5–§7 do not depend on it (it excises cleanly). The `use` verb (§2) and the `references:` `ResourceDefinition` declaration are retained now as reserved vocabulary to avoid a later schema migration. Deferred, to be decided and implemented as a tracked follow-up: reference materialization at deployment creation, the per-org `use`-against-consuming-resource's-org check, and the default-label owner/admin-tier write gate — together with the deferred feasibility items (the `at:` reference-path grammar, and restricting declared references to root-scoped platform-provided referent kinds).

Some resources exist to be *referenced* rather than contained: a platform-level `RuntimeClass` (root-scoped, operator-managed) describes how project deployments are reconciled, and organizations select one rather than own one. Some classes are for every org; others are provisioned for one specific customer. The interesting permission is not CRUD on the class — that stays operator-only by ordinary default-deny — but who may *select* it.

**Reference declarations.** A `ResourceDefinition` may declare that a field (or label key) of its kind references another kind:

```
references:
  - at:        spec.runtimeClass     # a field path or a label key
    kind:      RuntimeClass
    verb:      use
```

Declared once at kind registration, as data — the same family as `ResourceDefinition`-declared subresources (§2), never per-field engine code. Any write that sets or changes a declared reference additionally requires the writer to hold `use` (§2) on the *referenced instance*, evaluated by the ordinary algorithm (§4). An unchanged value on a later write is not re-checked (same rule as §6.6 step 1), and the check runs before existence disclosure (same ordering as §6.6/§6.7): a writer without `use` cannot probe whether a class exists.

**Availability is instance-targeted bindings.** A root-scoped instance is a node in the tree, so §4's `Scope` targets it with nothing new:

```
# everyone may use the standard class
Subject: system:authenticated
Scope:   RuntimeClass/standard
roleRef: { kind: PlatformRole, name: rc-user }                   # PlatformRoleBindings (§3)

# gpu-b is provisioned for acme-corp only
Subject: org:acme-corp
Scope:   RuntimeClass/gpu-b
roleRef: { kind: PlatformRole, name: rc-user }
```

Here `PlatformRole/rc-user = { Allow: use on RuntimeClass }`.

Multiple orgs → one binding each: explicit and auditable. "Org A cannot select org B's class" is not a rule anyone writes — it is the *absence of a grant*: org A's subjects hold no `use` binding on `gpu-b`, default-deny (§4 step 3) rejects the write without confirming the class exists, and org A cannot self-serve the grant — a binding whose `Scope` reaches a root-scoped instance must be a `PlatformRoleBinding` (§4's containment rule; org-parented `RoleBinding`s cannot leave their org's subtree), only operators can create those, and the write-time grant gate's subset check independently blocks handing out `use` they don't hold.

**Per-org `use` is checked against the consuming resource's org.** For a platform resource provisioned to one org (e.g. `gpu-b` granted only to `org:acme-corp`), the `use` grant is evaluated against the *consuming* resource's organization, not solely the acting subject's group membership. A `use` grant addressed to `org:acme-corp` authorizes selection only from resources within `acme-corp`'s subtree: a `User` who is a live member of both `acme-corp` and `beta-corp` cannot select `acme-corp`'s private `gpu-b` while deploying into `beta-corp`, because the resource being written lives under `beta-corp` and no `use` binding grants `gpu-b` there. Checking only the subject's own membership would break §9's cross-org isolation invariant. A consuming resource with no organization (a root-scoped resource) lies within no per-org subtree, so a per-org `use` grant simply does not apply to it — fail closed, never falling back to the acting subject's membership; instance-wide grants (`system:authenticated`) are unaffected since they are not per-org.

**Defaults are product data, not permission data.** Nothing product-specific accretes onto the RBAC core resources — Roles, RoleBindings, and cap `Deny`s stay purely authorization data (§5). The global default is a label on the class itself — `runtimeclass.rise.dev/is-default: "true"`, operator-writable because the class is operator-owned (the same pattern as Kubernetes' `storageclass.kubernetes.io/is-default-class`). Org- and Project-level overrides are a label on the Organization or Project (`runtimeclass.rise.dev/default: gpu-b`), and the override cascade — Deployment-explicit → Project → Organization → global — is `effectiveLabels`' nearest-wins walk (§6.1), with no new inheritance machinery. The default label key is itself covered by a reference declaration, so an org-admin setting their org's default is `use`-checked like anyone else — an org cannot default itself onto a class it was never granted. Beyond the `use` check, *writing* a reserved default label is gated to the owner/admin tier of the resource it is set *on*: an org-level default (`runtimeclass.rise.dev/default` on an `Organization`) requires org-admin or owner of that org, and a project-level default requires the Project's own owner — not the bare `update` an ordinary project editor holds. Otherwise a project editor holding only `update` could steer co-tenants' workload placement by rewriting the inherited default.

**Materialization at deployment creation.** When a deployment is created, the effective class is resolved once and written onto the Deployment as its own concrete value; that materializing write is a reference write, `use`-checked against **the deployer** — the User or ServiceAccount driving the deployment. This is why `org:<name>` includes ServiceAccounts (§1): CI-driven deploys must pass exactly where a human's would. The reconciler then reads only the materialized field and never evaluates `use` at all — every `use` check in the system has a well-defined, present subject. (Precedent: Kubernetes' DefaultStorageClass admission stamps the default `storageClassName` onto a PVC at create time.)

This deliberately gives the reference *snapshot* semantics, not §6.1's live semantics: the never-store rule exists for access-driving labels, where staleness is a security bug, whereas here the recorded value is the *output* of a decision made at a specific moment by a specific subject, and reproducibility is the point. The org's default label remains live as an *input* to the next deployment. Revoking an org's `use` grant therefore stops the *next* deployment, never a running one — consistent with the write-time grant gate applying at write time everywhere else (§5), and the right availability call: a revoked class ages out at the org's next deploy or rollback (which creates a new deployment and re-resolves against current grants).

**Boundary.** Org-admins cannot sub-delegate or per-instance-restrict `use` of platform-provided resources inside their org — those grants are `PlatformRoleBinding`s, outside any org's authorship reach by placement (§3, §4). Their levers are the org default label and a kind-level org self-cap `Deny` (`Deny: use on RuntimeClass`); per-instance, org-side restriction would need resource admission policies, which are out of scope (§10).

### 10. Explicitly out of scope

- Org-registrable Controllers/ResourceDefinitions — falls out for free once registration is just another grant-gated verb, not designed now.
- Migrating today's typed-table-backed APIs (`Project`, `User`, `Team`, `ServiceAccount`, `Deployment`, …) onto this model — happens as a separate, already-planned migration onto the generic resource store. The target identity layout is fixed in §1: current `users`/`teams`/`team_members`/`service_accounts` rows and config-backed Controllers are transitional inputs, not a second permanent subject store. Migration creates the corresponding identity, `UserIdentity`, `TeamMembership`, and trust-policy resources; ServiceAccounts move from Project to Organization placement and cease masquerading as synthetic Users.
- Ingress-level authentication for a deployed application's own end users — a different problem domain entirely.
- How a brand-new organization's first `org-admin` binding is created (the org-creation bootstrap) — necessarily an operator action, the same way the very first Role/RoleBinding on the whole instance must be, but the org-creation workflow itself is not designed here.
- A pluggable subject-kind registry letting organizations define groups with custom membership semantics (§6.4) — organization-specific *naming* of a grouping concept is supported today by pairing an existing kind with an organization-chosen label key; genuinely custom membership resolution is not, and would need a larger extension to the closed subject-kind list.
- A first-class cross-org sharing primitive — a deliberate grant reaching subjects of another org. The recipient boundary (§1) bans cross-org sharing through org bindings by construction; operator-authored `PlatformRoleBinding`s are the only cross-org grant path today. A tenant-authorable sharing mechanism is deferred; nothing here forecloses it.
- Platform-provided *selectable* resources and reference materialization (§9) — designed but deferred. The `use` verb (§2) and the `references:` declaration ship as reserved vocabulary, but reference materialization at deployment creation, the per-org `use`-against-consuming-resource's-org check, the default-label owner/admin-tier write gate, and the remaining feasibility items (the `at:` reference-path grammar, restricting referents to root-scoped platform-provided kinds) are a tracked follow-up, not part of the initial model.
- Resource admission policies — org- or operator-authored rules constraining what may be written below a given scope (e.g. required labels, or per-instance restriction of which platform resources an org's own subjects may reference, §9). A future mechanism; nothing here forecloses it.
- Concrete streaming, connection, proxy, and virtual-object subresource
  contracts (`logs`, `proxy`, `scale`, and similar). This ADR fixes their
  authorization tuple and the shared registration seam, but their handler
  interfaces, transport semantics, and response types are drafted in
  [ADR-0002](../0002-generic-resource-subresource-execution-model/) (§3).
- Extending the `token` subresource to the `User` kind — user self-service
  personal tokens, operator-delegated minting on behalf of a user, and exposing
  non-interactive external-assertion→Rise-token exchange (RFC 8693) for users
  through it. The interactive browser login flow stays separate: Rise remains an
  OAuth relying party (the upstream OAuth IdP is the authorization server), and
  the token-issuance
  logic is shared as one issuance core (`rise-backend-auth`) that both the login
  callback and `/token` call, rather than routing interactive login through the
  `/token` endpoint. Deferred with two hazards to design first: because `User` is root-scoped,
  `(create, User, token)` is grantable only by a `PlatformRoleBinding` (§4
  containment) — operator-only, so an org-admin structurally cannot grant it,
  and delegated minting would otherwise hand out a target user's *cross-org*
  reach unless the minted token is clamped to the minter's own scope authority;
  and self-service minting needs a binding primitive for a grant on the `User`
  whose identity equals the subject, which the label-value `${ref.name}`
  templating (§6.4) does not express. This deferral does **not** affect SSO
  login, which is authentication — a User's own external credential mapped to a
  live `UserIdentity` (§7), gated by that trust mapping, never by an RBAC
  `(create, User, token)` grant. Any future unification onto `token` must keep
  that self-authentication leg trust-policy-gated, not RBAC-gated, to avoid a
  login bootstrap paradox.

## Consequences

**Positive.**

- One evaluator decides access for every subject kind — Users, Teams,
  ServiceAccounts, Controllers, and Operators run the same algorithm, replacing
  five disjoint authorization code paths.
- Operator access becomes inspectable and auditable as data: the seeded
  `system:operators` binding is a stored row the same explain/audit tooling
  can read, instead of an invisible bypass branch (§1).
- Who-can-do-what is runtime-configurable per deployment: Roles, RoleBindings,
  and caps are all rows (a cap is just a `Deny` binding), and platform-shipped
  defaults (`org-admin`, `resource-owner`) are operator-authored data, so a
  SaaS and a self-hosted instance get different postures from the same
  architecture (§5, §6.2).
- Restrictions and grants are one mechanism: a cap is an ordinary `Deny`
  binding, applied by the same Deny-wins union (§4) as every grant, so the
  write-time grant gate is a single `⊆`-own-permissions check — a cap the
  writer is subject to already sits inside their own effective permissions, so
  they cannot delegate past it (§5).
- Revocation is live: caps and memberships are re-resolved on every
  request, so tightening a cap or narrowing an identity's Role takes
  effect immediately — including for every outstanding token of that identity
  (§5, §7).
- An org-admin can see why they are capped: caps are inspectable bindings, and
  the explain endpoint surfaces the applicable `Deny` on any denial, so a
  denial is diagnosable rather than opaque (§5).
- Reference authorization — who may *select* a platform-provided resource
  like a `RuntimeClass` — reuses the same evaluator, bindings, default-deny,
  and existence-masking as everything else; making a class available to an
  org is one auditable binding (§9).
- Cross-tenant isolation is structural, not asserted: the recipient boundary
  (§1) intersects every org binding's grant with live membership in that
  binding's own org, so an org binding cannot reach a foreign or org-agnostic
  subject; only operator-authored `PlatformRoleBinding`s cross org lines.
- The operator tier cannot be locked out of an org: the operator guarantee is
  keyed on the requesting caller, so whenever a request's membership expansion
  includes `system:operators` the evaluator yields unconditional access to
  every main-resource and registered-subresource tuple, ignoring every `Deny` in the union — including one
  targeting the caller's own `user:` identity (§1).
- Collection (`list`) authorization is per-item, not scope-level: every item in
  a listing is independently evaluated, projected to the explicit base-field
  allowlist, and expanded to the full object when the caller also holds `get`;
  a caller with no `list` grant gets a masked-empty result rather than a 403 —
  so existence/owner visibility and full-object data visibility are grantable
  independently (§4).
- Max token TTL is a single platform-global configuration constant, checked at
  issuance — not per-org, not composed — so it can never be unset and no
  misconfiguration can yield an unbounded-TTL token (§7).

**Negative / accepted risks.**

- Wildcard replacement is outright, not merged: an org-specific binding
  silently discards everything the wildcard binding provided beyond what it
  restates (§1).
- Cap tightening has no dry-run/impact-preview — an operator or org-admin
  can strand subjects (including their own controllers) with no warning before
  committing the write (§5, and Alternatives considered).
- A cap can `Deny` specific `(verb, kind, subresource?)` tuples but cannot restrict an org to a
  *whitelist* of kinds: the kind space is open-ended (new kinds register at
  runtime), so "only these kinds, nothing else" has no faithful `Deny`
  encoding — the same open-kind problem §3 solves for grants. Verb caps (the
  real use case — "no token-create", "no `delete` in `prod`") are unaffected
  (§3).
- An org self-cap covers its own member population (its `system:authenticated`
  auto-clamps to org members), so it does not reach a team-less identity an
  operator empowered directly; capping such an identity requires an operator
  per-org cap (§5).
- There is no token-revocation list. Responding to a compromised *minting
  caller* means acting on the target identity's own grants or waiting out the
  TTL (§7).
- The federation round-trip laundering path — minting a token for an external
  audience and re-entering via an externally-issued credential a second
  identity's trust policy accepts — is left structurally open; only the
  direct case (trusting Rise's own issuer) is closed (§7). Because the second
  identity can hold a different, possibly greater grant in a different org,
  this is potentially a cross-org privilege *increase*, not merely same-tier
  lateral movement.
- token-create re-delegation is not gated: holding token-create on identity `A`
  is equivalent to holding `A`'s reach, and the write-time grant gate's subset
  check permits handing token-create on A onward. The one-hop bound limits
  token-exchange chaining, not the spread of the grant; it is bounded only by
  org-level binding-authorship and accepted as documented (§7).
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
  org's next deploy or rollback. Snapshot semantics are safe only where
  selection is not a security boundary (§9).
- Platform-provided selectable references (§9) are designed but deferred: the
  `use` verb and the `references:` declaration ship as reserved vocabulary,
  while materialization and its checks are a tracked follow-up (§9, §10).
- Org-admins cannot per-instance restrict or sub-delegate `use` of
  platform-provided resources inside their org; their levers are the org
  default label and a kind-level org self-cap `Deny`, until admission policies
  exist (§9, §10).
- The resource-API RBAC items in `ROADMAP.md` (and everything sequenced on
  them) are to be planned against this model.

## Alternatives considered

- **Pure-additive, union-only permission sets (Kubernetes Role/RoleBinding-style), no Deny.** Cannot express subtraction from a wildcard — "everything except `delete` on `Environment`" has no faithful positive encoding against an open-ended, runtime-extensible set of resource kinds. Rejected; §3's Deny-capable evaluator, combined via union-then-evaluate in §4 step 3, is adopted specifically to make this expressible — including letting a narrower binding's Role genuinely subtract from what a broader one grants (§4's worked trace), not merely add to it.
- **Folding ownership into wildcard statements covering both main resources and every subresource**, rather than a distinct owner Role. Would silently over-grant: an owner would automatically gain token creation and finalizer updates alongside ordinary access, defeating the deliberate subresource separation in §2. Rejected in favor of the named `resource-owner` Role with the explicit main-resource verb list pinned down in §6.2 (`get`/`list`/`update`/`delete` only).
- **A dedicated single-subject `ownerRef` field as a separate ownership mechanism alongside the Role/binding model**, inherited down the ancestor chain as a union. Two independent inheritance and authorization mechanisms doing near-identical jobs complicates reasoning about why a subject has access to something, and union-across-the-whole-ancestor-chain semantics would mean a descendant could never fully exclude an ancestor's owner. Rejected; §6 subsumes ownership into the same Role/RoleBinding/label primitives as everything else — and unlike `ownerRef`, an ordinary Deny-bearing binding at a narrower scope genuinely can exclude a broader ancestor's grant (§4), so the exclusion capability `ownerRef` lacked is available for every kind of grant, not just reintroduced for ownership specifically.
- **Labels driving RBAC directly, with no write-gate on the label itself.** Ordinary `update` access on a resource would let any editor silently redirect which subject holds a derived Role — an ungated escalation path. Rejected in favor of §6.6's binding-triggered write-time grant gate, which in turn is one instance of §5's general rule that *every* write changing effective access — including RoleBinding and Role edits, not only labels — passes through the same check.
- **A dedicated bespoke verb per protected field** (e.g. `setTeamLabel`), rather than a generic mechanism. Every newly-sensitive field would need a new verb and new engine code. Rejected in favor of §6.6, where protection is a consequence of a `LabelSelector` binding's existence, not a hardcoded field name — and generalized further in §5 so the same reasoning covers Role/RoleBinding writes, not only labels.
- **Gating label writes on "does not drop access to zero"** rather than the standard subset check. Defends availability only — it never checks *who* gains access, only that the total doesn't hit zero — so it would still permit an unauthorized party to redirect access to themselves. Rejected in favor of the genuine subset comparison in §6.6.
- **A separate four-layer ceiling stack for restrictions** — operator and org ceilings composed by pointwise intersection, with numeric `min()` for values like max token TTL, stored in dedicated `InstancePolicy`/`OrganizationPolicy` resources and `Organization.spec`. Superseded once max token TTL moved to a platform-global config constant (the numeric axis was the one thing ordinary bindings could not express): with that removed, restrictions collapse into placement-tiered `Deny` bindings, unifying grants and caps into one Deny-wins mechanism (§4, §5) and subsuming the `⊆ ceiling` write-time check into `⊆ granter's own permissions` (§5). Costs: a kind-*whitelist* cap is inexpressible against an open-ended kind space (§3), and an org self-cap covers its member population rather than a team-less, operator-granted identity (§5) — both accepted.
- **Exempting machine identities (Controller, ServiceAccount) from an org's own self-cap**, to avoid an org accidentally stranding its own controller. Rejected — an exemption would require every cap `Deny` to first determine whether the subject it catches is a machine or a human, adding a second evaluation path everywhere caps apply for a narrow footgun-avoidance benefit; caps apply uniformly instead (Deny-wins over the same union), with no asymmetry by subject kind, and the resulting footgun is accepted as-is (§1, §5).
- **A cap-tightening (or wildcard-replacement) dry-run/impact-preview warning.** Would require simulating the write's effect across every subject with a live binding under the tightened rule before committing it — expensive and stateful in a way the rest of the write path deliberately isn't, and it doesn't integrate cleanly into a generic REST write path. Deferred; the footgun is accepted, not solved, for now (§1, §5).
- **An open, pluggable subject-kind registry**, to let organizations define arbitrary group types (e.g. "squad") with their own membership resolution. Subject kind carries real infrastructure (membership resolution, org-native-vs-agnostic encoding, token-create semantics) not worth making pluggable. Rejected; §6.4 shows organization-specific *naming* of a grouping concept is expressible by pairing an existing kind (Team) with an organization-chosen label key — genuinely custom membership resolution is a separate, larger ask this does not address, and remains out of scope (§10).
- **Clamping a minted token's scope to the calling subject's own permissions.** Forecloses a legitimate privilege-elevation pattern — a low-privilege, long-lived caller minting a token for a higher-privilege, short-lived ServiceAccount, the same shape as AWS STS `AssumeRole`. Rejected; §7 gates *who may mint*, not what the minted token may then do.
- **A single global namespace for Role names, with no placement.** Any org editing any Role by name would make cross-org authority attribution ambiguous the moment a Role is bound in more than one org — against whose permissions is an edit checked? Rejected in favor of placement-derived authority (§3, §5): `PlatformRole` (root-parented, operator-editable, bindable by any org) vs. `Role` (org-parented, org-editable, bindable only from its own org) — editing a Role always has exactly one unambiguous parent org to check the editor's permissions against, fixed by its parent.
- **A variable parent kind for policy objects** ("parented at root *or* under an Organization"), instead of two kind pairs. The store's exact-parent model is load-bearing: ancestor kinds in URLs and `Scope` paths derive deterministically from the leaf kind's single parent chain, and union parents reintroduce path ambiguity in the general case. Rejected; two same-shaped kind pairs (§3), the same fork Kubernetes resolves with `ClusterRole`/`Role` — an enum entry is cheaper than an invariant.
- **A reserved "platform organization" holding platform-level Roles/bindings**, keeping one kind pair. The `Scope`-containment rule (§4) would immediately need an exception — platform bindings carry `Scope: "*"`, which no org subtree contains — and an org that isn't a tenant is a modeling smell, not a simplification. Rejected in favor of root placement (§3).
- **Bare-name Role references with org-local-then-platform fallback resolution**, instead of structured, kind-qualified references. An org creating a `Role` named `resource-owner` would shadow the platform Role and silently retarget any later binding written with the bare name — the same one-unambiguous-answer failure mode §1's wildcard rules exist to prevent. Rejected; `roleRef` always names both the target `kind` and `name` (§4).
- **Multiple stored subjects on one RoleBinding**, as Kubernetes supports. This saves duplicate binding objects, but subject identity participates in Rise's wildcard replacement, dynamic resolution, org-recipient validation, grant-gating, and audit explanations. Adding or removing one entry would partially mutate a binding, and an org-specific collision might replace a wildcard for only a subset of its subjects — complexity Kubernetes' additive-only model does not face. Rejected for the initial model in favor of one subject per binding and Team/`org:` groups for populations; a future `subjects:` input may be pure syntactic sugar expanded into independent bindings (§4).
- **Email address as `User.metadata.name`**, requiring `@` in the resource-name grammar. Email is mutable, awkwardly normalized, may repeat across issuers, and would make an authentication attribute the stable authorization key; admitting it also weakens a path grammar shared by every kind. Rejected for generated DNS-safe immutable User names plus dedicated `(issuer, subject)` UserIdentity resources; UI/CLI translates to presentation fields (§1).
- **Embedding SSO mappings, trust policies, or member arrays in the parent identity's `spec`.** This makes independently governed security edges share one revision and turns membership changes into whole-object rewrites; one generic TrustPolicy child kind also cannot have both Controller and ServiceAccount parents under the exact-parent model. Rejected for dedicated, fixed-parent UserIdentity, TeamMembership, ControllerTrustPolicy, and ServiceAccountTrustPolicy resources (§1).
- **A separate `authorization.rise.dev` API group for subject resources.** Adds qualification/versioning vocabulary while subjects and `roleRef` intentionally rely on one reserved built-in role/identity domain. Rejected; these built-ins use `rise.dev/v1alpha1`, and custom same-named kinds in other groups never participate in SubjectId resolution or built-in indexes (§1).
- **Unbounded token-create chaining** (letting a minted token itself be used to mint a further token). Lets a caller holding token-create on one identity reach arbitrarily far through a chain of that identity's own token-create grants, with no single grant reflecting the actual resulting reach. Rejected in favor of the one-hop bound in §7 — only directly-authenticated callers may mint.
- **Applying the general subset check to owner-label writes at creation with no exception.** Would mean a subject holding only `create` on a kind could never become the resulting resource's owner, since ownership (`resource-owner`) is strictly more than `create` alone implies — breaking the single most common operation the model exists to support. Rejected in favor of the narrow, membership-bounded creation-time exception in §6.6, which only ever lets a creator name themselves or a team they already belong to, never an arbitrary third party.
- **Permitting a ServiceAccount/Controller's trust policy to accept Rise's own token issuer as a valid source.** Would let a caller launder an already-minted token back into the authentication layer as if it were independent source-issuer credentials for a second identity, defeating the token-create one-hop bound by re-entering as a "directly-authenticated caller." Rejected; trust policies may never name Rise's own issuer/audience (§7).
- **A general `fields:` include/exclude axis on Role statements**, replacing named subresources (and potentially §6.6's label-write gate) with field-path matching on the ordinary `update` verb. Rejected on both counts. Folding in §6.6 doesn't work at all: its gate depends on whether *some other, unrelated binding* currently exists (a live property of the whole binding table, not data a Role statement can carry) and on whether a *value* changed, not merely which *path* was touched — properties no static field syntax can express, and the diff computation such a fold would require is vacuous everywhere except labels, since nothing else in this model resolves a Subject off a field value. Replacing `status`/`finalizers` subresources does not work either: it destroys `resource-owner`'s secure-by-default-via-omission property (§6.2) and introduces an ambiguous, security-critical path-containment language into the write-time grant gate's `⊆` check. Something as simple as `status.*` has two plausible readings (single-segment versus recursive wildcard), while `fields: ["metadata.*"]` could silently include `metadata.finalizers`. Named, registered subresources keep those boundaries structural and reuse the same `(verb, kind, subresource)` evaluator as non-field operations such as token creation.
- **A single fixed definition for `org-admin`/`resource-owner`, uniform across every deployment.** A multi-tenant SaaS operator and a self-hosted single-team operator want genuinely different answers to "should ordinary users/org-admins see platform bookkeeping (`status`/`finalizers`)" — the former wants it locked to `system:operators`/Controllers, the latter may reasonably not care, since the same people already have full infrastructure visibility. Rejected in favor of treating both Roles' verb lists as ordinary, operator-authored, deployment-time configuration (§5, §6.2) rather than an architectural constant — the identical mechanism already used for everything else Role-shaped, requiring no new primitive to support either deployment model or anything in between.
- **Nesting a ServiceAccount under a single owning Project**, as its tree position. Access reach is granted entirely through bindings (§4), independent of tree position, so a "home" Project does no real work — it only couples the SA's inherited attribution (§6.1) to whichever Project happened to parent it, and requires re-parenting (or duplicating) the SA to give it first-class standing against a second Project it's equally bound against. Rejected in favor of parenting ServiceAccount directly under its org, a sibling of Project (§1) — matching how Team is already positioned.
- **Operator status as a hardcoded bypass branch in the evaluator**, checked before Role/binding resolution rather than expressed as data. Makes operator access the one thing the model's own explain/audit tooling can't account for, and duplicates logic the ordinary evaluator already has (union bindings, evaluate Allow/Deny). Rejected in favor of `system:operators` (§1): a reserved subject, resolved via the same live config-allowlist check as today, granted access through one seeded, immutable binding — operators run the same algorithm as everyone else, differing only in that their own request ignores every `Deny` so no cap can reduce their access, and only for that one reserved subject.
- **Treating the seeded `system:operators` binding as immutable data only, with no evaluator-level guarantee behind it.** Immutability through the ordinary write path (§5) protects only against mutation via this model's own API — not a bad migration, a restore from an old backup, or direct database access losing the row entirely. That residual risk is unacceptable for the one subject with no recovery authority above it. Rejected in favor of a hardcoded, evaluator-guaranteed grant for `system:operators` specifically, mirrored as a healable data row for audit/tooling parity — matching how Kubernetes redundantly hardcodes `system:masters` alongside its ordinary, self-healing `cluster-admin` ClusterRoleBinding, rather than relying on either mechanism alone.
- **Making the `system:operators` binding fully virtual too, with no stored row at all** (matching how membership itself is virtual). Would remove operator access from the same explain/audit tooling that inspects everyone else's — exactly the gap `system:operators` was introduced to close by replacing a hardcoded bypass branch in the first place (above). Rejected; the binding stays data, mirrored and healable — only the evaluator's guarantee of its *effect* is hardcoded, not its existence as an inspectable object.
- **Making the seeded `system-admin` Role and its binding ordinary operator-editable `PlatformRole` data rather than immutable.** Would let an operator edit or delete their own bootstrap grant through the ordinary write path — trivially passing the subset check, since they hold everything — with no higher authority left to recover from it, unlike every other documented risk in this ADR. Rejected in favor of a third, **seeded** Role-ownership tier (§5) that no write path can modify, editable by no one.
- **Allowing a static Subject to pair with a value-less `LabelSelector`.** Would grant a fixed subject access to any resource carrying *any* value for that label key, regardless of what it actually says — access disconnected from the value the selector nominally matches on. Rejected; value-less selectors are reserved for dynamic (templated) subjects, where the matched value is actually used (§4).
- **Kubernetes-style plural resource names** (a `plural` declared per kind, used in collection URLs and grants). Creates a permanent dual vocabulary — rules and URLs naming `deployments` while every object says `kind: Deployment` — with a lookup step (`kubectl api-resources`) as the ongoing price of collection-URL aesthetics. Rejected; the `kind` token is the single canonical name everywhere (§8) and `ResourceDefinition` declares no plural.
- **A distinct separator between the kind and the path in `Scope`** (e.g. a `RuntimeClass:gpu-b`-style form), for visual clarity. Rejected to keep `Scope` byte-identical to the URL path form (§4, §8) — one grammar to learn, one parser to trust.
- **`get` as the reference gate** ("if you can read it, you can select it"), instead of a distinct `use` verb. Couples two independent decisions: a catalog may be browsable without being selectable (visible-but-gated offerings), and selectable without being readable (a class's internals — node selectors, cost plumbing — are not the selector's business). Rejected in favor of `use` (§2, §9), mirroring the Kubernetes `use` verb on PodSecurityPolicies.
- **An `allowedOrgs` list on the referenced resource's spec** as the availability mechanism. Moves an authorization decision out of the one system built to answer authorization questions, needs its own evaluation and audit path, and caps out at org granularity. Rejected; availability is ordinary instance-targeted `use` bindings (§9), which also express team- or ServiceAccount-narrow grants with no extra machinery.
- **Encoding product defaults (e.g. the default `RuntimeClass`) in the RBAC core resources.** Would accrete product-specific settings onto authorization data. Rejected; the core stays agnostic — defaults live on the product resources themselves as labels, and the override cascade is `effectiveLabels` (§6.1, §9).
- **Scope-level all-or-nothing `list` authorization** (or a 403 on inaccessible collections), instead of per-item filtering. Rejected in favor of per-item filtering with existence-masking and per-item `get` expansion (§4): all-or-nothing either over-discloses — returning full items to anyone who can list the scope — or leaks scope population, since a 403 confirms the collection is non-empty, and it cannot express "see names org-wide, data only for owned."
- **Live `use` re-evaluation at reconcile time**, instead of materializing the resolved class onto the Deployment at creation. Leaves the check with no well-defined subject (a reconciler acts for nobody in particular) and turns a grant revocation into retroactive breakage of running workloads. Rejected; the effective class is materialized at deployment creation and `use`-checked against the deployer (§9), matching Kubernetes' DefaultStorageClass admission behavior — revocation applies from the next deployment.
- **Server-auto-stamping the `rise.dev/owner` label at resource creation**, so a creator never has to write it. Would force the generic resource core to hardcode knowledge of the ownership label — the one thing §6 exists to keep out of the engine, where ownership is purely the emergent effect of a seeded binding over an ordinary label. Rejected in favor of the creation-time exception (§6.6): the creator writes the label, gated to claiming only themselves or a team they belong to, and the core stays agnostic.
- **Using the presence of the `act`/attribution claim as the one-hop gate**, rather than a separate token-class marker. Coupling the security gate to an audit claim means a later change to *when* `act` is emitted (e.g. omitting it for some first-party mint) would silently reopen chaining. Rejected in favor of an unconditional `token_class` marker stamped on every exchange-minted token, decoupled from `act` (§7).
- **Passing a validated JWT's raw `sub` string into authorization.** Signature validation proves who issued bytes, not that `sub` names an existing Rise principal or even a principal-capable subject kind; an external token could otherwise spell `system:operators`, a group, or a malformed lookalike and rely on downstream parsing. Rejected: authentication maps credentials to an existing active Rise identity, constructs a typed canonical `SubjectId`, and the resource API accepts external workload credentials only at token exchange (§1, §7).
- **Adding arbitrary JSON-path filters or index declarations to the generic resource API for identity lookup.** Makes storage projections part of the public resource abstraction before any general use case exists. Rejected for fixed partial expression indexes over the built-in identity kinds and narrow Postgres lookup adapters; ordinary resource writes maintain the indexes transactionally without changing `ResourceStore` or client APIs (Implementation structure).
- **Allowing org bindings to target arbitrary subjects (a cross-org grant).** Would let an org author a binding whose grant reaches a foreign org's subjects, or an org-agnostic Controller, with no membership relationship to the granting org. Rejected in favor of the recipient boundary's org-membership intersection (§1) — an org binding's grant reaches only live members of its own org — with deliberate cross-org sharing deferred to a future first-class primitive (§10).

## Implementation structure

*Where the code lives, not what the model is. This realizes the sections above; it is a design intent, not a normative rule.*

The evaluation logic is security-critical, and the value of a small, auditable core is highest exactly there. The carve-up's goal is that the decision logic — union, Deny-wins, the subset check, wildcard replacement, the label-write gate — can be read, fuzzed, and tested **without a database and without any Rise product concept**. What a `Deployment` is, what `rise.dev/` means, and how rows reach Postgres must never leak into it. One fact drives most of the structure: the RBAC objects — `Role`, `RoleBinding`, `PlatformRole`, `PlatformRoleBinding` — are all **resources** in the generic store (§3, §5), so reading a subject's bindings, cap `Deny`s included, is an ordinary `ResourceStore` read, not a bespoke authorization data path. Max token TTL is not among these reads: it is a platform-global config constant checked at token issuance (`rise-backend-auth`, §7), never a store-resolved fact.

Three tiers, separating security decisions, fact-retrieval, and product meaning:

- **Tier 0 — pure policy algebra** (new crate, e.g. `rise-authz-policy`; ~zero deps). The Allow/Deny evaluator over `(Verb, Kind, Option<Subresource>)`, the Deny-aware subset check and the intensional `(Scope, LabelSelector)` domain lattice, wildcard replacement with Deny-preservation, `${ref.name}` substitution — all pure functions over its **own** small `Verb`/`Kind`/`Subresource`/`Statement`/`Policy` types, no store, no I/O, no reserved-key constants. Carries the pure-logic acceptance scenarios as unit tests.
- **Tier 1 — the evaluation engine** (new crate, e.g. `rise-authz`). The §4 algorithm, membership expansion, `effectiveLabels` diffing (§6.6), the recipient-boundary intersection, and `list` filtering. Its request entry point accepts only a typed `AuthenticatedPrincipal`/`SubjectId`, never token claims or strings. It needs only two traits it evaluates against — `ResourceStore` (facts from the tree, cap `Deny` bindings included) and a small new `MembershipResolver` (which it defines) — plus `rise-authz-policy` and `rise-resource-api`'s envelope types. No JWT parsing, SQLX, Axum, or token signing; testable end-to-end against in-memory fake stores.
- **Tier 2 — Rise wiring** (`rise-deploy`). The authentication adapters that validate credentials and map them to existing active identities, the `MembershipResolver` implementation, the seed data (reserved label keys; the `system-admin`/`resource-owner`/`org-admin` contents and deployment variants; the seeded bindings), the operator allowlist source, the authz choke point (`src/server/resources/authz.rs`, replacing today's `require_operator`), the HTTP handlers, and the `list` metadata-vs-full projection and 403-vs-masked-empty mapping. The generic resource layer owns subresource routing and the shared `status`/`finalizers` mutation strategies; `rise-backend-auth` supplies the `token` create handler. Only that token handler accepts external workload JWTs; all other generic resource handlers receive only Rise-authenticated principals.

**The facts come from the store crate, not scattered in `rise-deploy`.** The tree and binding reads are the *existing* `ResourceStore` trait, grown with generic hierarchy/label operations implemented in `rise-resource-store`'s Postgres store: ancestor chain, the K-inheriting subtree (`WITH RECURSIVE` over `parent_uid`), `effectiveLabels` resolution, and list-by-kind-under-scope — product-agnostic operations over a labeled hierarchical store. This matches the repo's SQLX split (`rise-resource-store` owns resource-store SQLX; `rise_deploy::db` owns legacy typed-table SQLX). The authz engine's product-specific seam remains **`MembershipResolver`**: its target implementation reads `TeamMembership` resources and derives org membership from their Team parents; operator status remains config-sourced. During migration only, a compatibility implementation may read legacy `team_members`, but that table is not part of the target model.

**Postgres secondary indexes are storage projections, not API features.** The generic resource API and `ResourceStore` trait do not gain arbitrary JSON-path search. Instead, `rise-resource-store` migrations add partial expression indexes over the built-in `rise.dev` kinds in `resource_store.resources`, following the existing `ResourceDefinition` index precedent:

- a unique live `UserIdentity` index on normalized `(spec->>'issuer', spec->>'subject')`;
- an issuer index across live `ControllerTrustPolicy` and `ServiceAccountTrustPolicy` rows, used to narrow candidates before claim-pattern evaluation in Rust;
- a unique live membership-edge index on `(parent_uid, TeamMembership.spec.userRef.uid)`, preventing duplicate membership resources for the same User in one Team, plus a reverse index on `spec.userRef.uid`; Team-to-members lookup already uses the generic `(parent_uid, group, kind)` index.

Every predicate includes the `rise.dev` API group, exact built-in Kind, and `deletion_timestamp IS NULL`, so a custom kind with the same name in another group cannot collide. Because these are expression indexes on the canonical JSONB rows, ordinary generic create/update/delete transactions maintain them automatically — no trigger-maintained mirror table or application dual-write can drift. Schema validation guarantees the indexed fields and normalized issuer form before persistence; the unique index remains the concurrency authority.

Authentication and membership use narrow `IdentityLookup`/`MembershipLookup` Postgres adapters alongside `PgStore`, backed by those indexes and returning typed facts or resource UIDs. They are not methods on the generic `ResourceStore`, are not exposed as client-selectable filters, and do not introduce identity lookup/index semantics into `rise-resource-api` or the pure authorization crate beyond the already-shared `SubjectId`. This requires storage migrations and a small Postgres adapter, but no change to the resource envelope, URL shape, ResourceDefinition API, or authorization algebra. Generic indexed-field declarations for user-defined kinds are a separate future feature and are unnecessary for these fixed built-ins.

**Prerequisite refactor:** the `ResourceStore` trait currently lives in `rise-resource-store`, which carries `sqlx`. Move the trait (and its Row/Params model types), plus the canonical `SubjectId` and `Scope` parser/types shared by routing, persistence, and authz, down into the dep-light `rise-resource-api`, leaving only `PgStore` + `sqlx` in `rise-resource-store`, so the engine compiles against the trait without transitively pulling a database driver. Two further splits fall along existing seams: token issuance (§7) — the *authorization* half (the `(create, kind, token)` check and the one-hop `token_class` rule) is engine logic, the *authentication/issuance* half (credential validation and identity lookup, signing, `act`/`token_class` claims, the platform-global max-TTL check, trust-policy match) is `rise-backend-auth`; and `list` (§4) — per-item filtering is engine, the metadata-vs-full projection is a `rise-resource-api`/server serialization concern. `effectiveLabels` as a plain read is a store op; the §6.6 before/after *simulation with a hypothetical value* is engine logic over store-provided ancestor labels, keeping the store free of authorization semantics.

```
rise-authz-policy   (pure algebra; own Verb/Kind/Statement types; ~zero deps)
        ▲
rise-authz (engine) ──► rise-resource-api  (envelope types + the ResourceStore
   defines MembershipResolver              & MembershipResolver traits, no sqlx)
        ▲                        ▲
        │                        │ impl
rise-deploy ──► rise-resource-store (PgStore + identity/membership index adapters:
  impl MembershipResolver         ancestors, effectiveLabels, indexed built-in lookups;
  over TeamMembership + config;   the sqlx home)
  seed data; authz.rs choke point; HTTP; list projection; token wiring
```

The payoff: the pure algebra and engine are testable with fakes and no Postgres, so the acceptance suite partitions three ways — pure-logic → Tier 0 unit tests; tree/membership → Tier 1 with fake stores; wiring (masking, `list` projection, token endpoint) → server integration — and the most security-sensitive code has the fewest dependencies. Two structure choices are left revisitable: whether tiers 0 and 1 are one crate (modules `policy`/`engine`) or two — leaning **one with a hard internal boundary**, split when the pure tier earns it (as `rise-backend-docker` was extracted only once its seam matured, #377) — and whether tier 0 reuses `rise-resource-api`'s verb/kind types or defines its **own** (leaning own, for a standalone, portable policy library at the cost of a thin mapping layer). Leaving the `ResourceStore` trait in the sqlx-bearing crate and letting the engine take the transitive database dependency was considered and rejected — it bloats the security core's dependency graph and undercuts fake-testability.

## Appendix: acceptance scenarios (normative)

These scenarios pin the semantics an implementation must satisfy; several
encode findings from the adversarial-review rounds this design went through
(Context) — they are regression tests against reintroducing found-and-fixed
bugs, not illustrative examples. The conformance test suite is expected to
cover every scenario here **except those tagged `§9`** (the platform-resources
group and any `§9`-tagged entry), which are deferred with §9 (§10) and belong
to that follow-up's conformance suite, not the initial model's. Each entry is
tagged with the section whose rule it pins.

### Evaluation & subjects (§1, §4)

1. **Union of applicable bindings.** Given `acme-corp/team:platform` holds a scope binding at `Environment/acme-corp/env-prod` (`{Allow: * on Deployment}`), and `Deployment/acme-corp/env-prod/foo` carries `rise.dev/owner: platform`, when a team member requests `delete` on `foo`, then both bindings collect, their Roles union, and the request is allowed (§4 worked trace).
2. **Deny wins across bindings.** Given the same subject additionally holds an env-scoped binding whose Role is `{Deny: delete on Environment}`, when they request `delete` on the Environment itself, then it is denied despite the broader Allow (Deny-wins union; §3, §4).
3. **Deny is scope-blind.** Given a Deny-bearing binding at `Organization/acme-corp` and an Allow at a narrower scope beneath it, when the denied verb is requested below, then it is denied — there is no "more specific wins" between ordinary bindings (§3, §4; Decision primer).
4. **Default deny.** Given no binding applicable to subject `S` on resource `r`, when `S` requests any verb, then the combined policy is empty, no Allow can match, and the request is denied — no implicit grant (§4 step 3).
5. **Membership expansion is live.** Given a user whose only access to `r` flows through an `acme-corp/team:platform` binding, when they are removed from the team, then their very next request is denied — membership is resolved live, per request (§1).
6. **Operator lifecycle.** Given a canonical User name/UID added to the operator allowlist, then that User's next request expands to `system:operators` and draws on `system-admin`; removed, the next request falls back to the User's own grants. Changing the User's email has no effect. A request whose expansion includes `system:operators` has every `Deny` collected in step 1 ignored, so no cap can reduce operator access; every other subject, org-admins included, is fully subject to every cap (§1, §5).
7. **Literal and template never collide.** Given a static `Subject: acme-corp/team:platform` binding and a dynamic `Subject: team:${ref.name}` binding whose template resolves to that Team on some acme resource, then wildcard replacement compares authored Subject forms only — neither replaces the other (§1).
8. **Wildcard replacement is per-org.** Given the seeded `Scope: "*"` ownership binding and an org-scoped override binding for the same `(Subject-template, LabelSelector-key)` pair at `Organization/acme-corp`, when evaluating a resource in `acme-corp`, then the override replaces the seeded binding outright; every other org keeps the default (§1, §6.5).
9. **Value-narrowed replacement is per-resource.** Given a wildcard binding on `{key: rise.dev/owner}` and an override on `{key: rise.dev/owner, value: "legacy"}`, then only resources whose label equals `legacy` collect the override (and drop the wildcard); resources carrying any other value stay governed by the wildcard binding, undiminished (§1).
10. **Scope omission is org-sensitive.** Given an org-parented `RoleBinding` with omitted `Scope`, when written, then its stored Scope is its parent `Organization/<name>`; given a `PlatformRoleBinding` for static org-native subject `acme-corp/team:platform` with omitted `Scope`, then its stored Scope is `Organization/acme-corp`; given any other `PlatformRoleBinding` with omitted `Scope`, then its stored Scope is `"*"` (§1, §4).
10b. **Org-native Scope validation.** Given a static binding for an org-native subject (`acme-corp/team:platform`) supplying `Scope: Organization/other-org` or `Scope: "*"`, when written, then it is rejected — a supplied Scope must lie within the subject's own org (§1).
11. **Static subject, value-less selector.** Given a binding pairing a literal `Subject: acme-corp/team:platform` with `LabelSelector: {key: rise.dev/owner}` and no `value`, when written, then it is rejected at write time (§4).
11b. **Subject grammar and existence are fail-closed.** Given a binding whose literal Subject has an unknown kind, missing org for an org-native kind, extra separator, unrecognized `system:` name, or names a nonexistent Rise identity, when written, then it is rejected before persistence. Given a dynamic template outside the closed supported forms, it is rejected at authoring; given a supported template whose substituted value cannot parse as a canonical concrete `SubjectId`, it matches nobody at evaluation (§1, §4).
11c. **Scope grammar, hierarchy, and existence are shared.** Given a Scope with an unknown Kind, wrong number/order of ancestor components, empty/dot/extra component, query/fragment, non-canonical encoding, or nonexistent target node, when written, then it is rejected by the same canonical parser used by resource routing; a target created in the same atomic transaction is accepted (§4, §8).
11d. **One subject per stored binding.** Given a write containing `subjects:` or otherwise more than one subject, then it is rejected; callers use independent bindings or a group subject. A future convenience may expand a list into independent bindings before persistence, but the stored/evaluated model remains singular (§4).
11e. **Built-in subject-resource placement.** Given identity resources in `rise.dev/v1alpha1`, then User and Controller are root-scoped, Team and ServiceAccount are Organization children, and UserIdentity, TeamMembership, ControllerTrustPolicy, and ServiceAccountTrustPolicy are children only of their one declared identity parent; attempting another group or parent shape is rejected (§1).
11f. **User identity is not email.** Given two live UserIdentity writes with the same normalized `(issuer, subject)`, concurrent or sequential, then exactly one succeeds; differing issuers may carry the same external subject and differing users may carry the same profile email. Changing `primaryEmail` neither changes the User's generated DNS-safe name nor relinks authentication, and an `@`-containing metadata name is rejected by the unchanged generic name grammar (§1, §7).
11g. **Membership is a governed edge resource.** Given a TeamMembership referencing an unknown User UID, it is rejected; given a User referenced by live TeamMembership resources under Teams, deleting that User without deleting those memberships in the same authorized transaction is rejected. Team-to-user expansion and user-to-Team/org expansion resolve the same live edge set (§1, §4).

### Restrictions & the grant gate (§5)

12. **A cap `Deny` binding clips live.** Given an operator adds a `PlatformRoleBinding {Subject: system:authenticated, Scope: Organization/acme, Deny: delete on Deployment}`, when any acme subject next requests `delete` on a `Deployment`, then it is denied (Deny-wins over the same union, §4) with no existing binding rewritten; removing the cap binding re-allows the next request (§4, §5).
13. **An org self-cap applies to the org's own admins.** Given an org places a self-cap `RoleBinding {Subject: system:authenticated, Scope: Organization/acme, Deny: <verb>}`, when one of that org's own admins requests that verb, then it is denied — a cap `Deny` catches every subject in scope, with no subject-kind or seniority exemption; only a request whose expansion includes `system:operators` ignores it (§5, §1).
13b. **Delegation cannot exceed a cap.** Given an org-admin subject to a `{Deny: create on */token}` cap, when they try to grant `(create, kind, token)` to a team in their org, then it is rejected — the cap has removed token creation from the admin's *own* effective permissions, so the write-time grant gate (`⊆` own permissions) blocks it, needing no separate check (§5).
14. **Granter subset, positive.** Given an org-admin effectively holding verb-set `X`, when they bind a Role granting `X` to a team in their org, then the write is allowed (the write-time grant gate; §5).
15. **Granter subset, negative (Role edit).** Given Bob holds only `update` on kind `PlatformRole`, when he appends `{Allow: create on */token}` to `resource-owner`, then the write is rejected — the newly-implied grant is not `⊆` his own effective permissions, however many subjects are bound to the Role (the write-time grant gate; §5).
16. **Role authority is placement.** Given org B's admin editing a `Role` parented under org A, then the write is rejected — their grants don't cover org A's subtree (§3, §5). Given anyone — an operator via the ordinary write path included — editing the seeded `PlatformRole/system-admin` or its `system:operators` binding, then the write is rejected (seeded tier; §1, §5).
17. **Granter check is write-time only.** Given a granter's own permissions shrink after they authored a binding, then the grants they already made are unaffected — contrast scenario 12: only a live cap `Deny` claws back (§5).
18. **Cross-org Scope move.** Given a RoleBinding's `Scope` edited to move it from org A into org B, then the write is validated against both orgs — equivalent to a delete at the old scope plus a create at the new one (the write-time grant gate; §5).
18b. **Identity mapping cannot bypass the grant gate.** Given a writer with `create on UserIdentity` but whose effective permissions do not contain those of User `alice`, when they attach their own external `(issuer, subject)` beneath Alice, then the write is rejected; the same rule applies to adding a ServiceAccount/Controller trust policy. Given an authorized writer deletes or tightens an existing mapping, no newly-implied grant exists and ordinary write authority suffices (§1, §5, §7).

### Ownership & labels (§6)

19. **Nearest wins, no union.** Given `Project/secret-app` carries `rise.dev/owner: platform` and its child Environment carries `rise.dev/owner: devops`, then the child's `effectiveLabels` resolve to `devops` only, and `acme-corp/team:platform` holds no owner-derived access on the child absent another binding (§6.1).
20. **Escalation blocked, oracle avoided.** Given an editor holding `update` but no claim to `resource-owner`, when they relabel `rise.dev/owner` to their own team, then the write is rejected by the subset check — and rejected *before* referential-integrity validation runs, so they never learn whether the named team exists (§6.6, §6.7).
21. **Legitimate transfer.** Given the current owner — or an org-admin, whose access is label-independent — relabels `rise.dev/owner` to another team, then the write passes the subset check; a typo'd team name is rejected with a fuzzy-match suggestion (§6.6, §6.7).
22. **Creation exception is narrow.** Given a subject holding only `create`, when they set `rise.dev/owner` to a team they belong to within the creation request, then it is allowed; naming a team they don't belong to falls back to the general rule and is rejected. A restore or upsert targeting an existing (even soft-deleted) identity is not creation — the general rule governs (§6.6).
23. **Ungated keys stay ungated.** Given a label key no applicable binding's `LabelSelector` references, when it is written, then only ordinary `update` permission is required — no grant gate (§6.6 step 2).
24. **`org-admin` is never label-derived.** Given a binding granting the `org-admin` Role via a `LabelSelector`, when written, then it is rejected structurally — `org-admin` may only ever be granted via a `Scope`-targeted binding (§6.7).

### Token issuance (§7)

25. **Both gates required.** Given a caller whose credentials satisfy a target ServiceAccount's trust policy but who holds no token-create grant on it, then the mint is rejected; given the grant but no trust-policy match, authentication fails before the grant is ever consulted (§7).
26. **Elevation is intended; `act` is audit-only.** Given a caller holding *only* token-create on an SA, then the minted token wields the SA's full, broader live-resolved grant; it carries `act: <caller>` (attribution, attached to audit logging on every request the token makes) and a `token_class` marker, neither of which influences authorization (§7).
27. **One hop.** Given any exchange-minted token (carrying the `token_class` marker, independent of `act`'s presence or content), when presented as the calling identity for a further token exchange, then it is rejected — regardless of what token-create grants it would otherwise satisfy (§7).
28. **No self-issuer trust.** Given a trust policy naming Rise's own issuer/audience as an accepted source, when written, then it is rejected at trust-policy write time (§7).
29. **Revocation asymmetry.** Given outstanding tokens for SA `A`: narrowing `A`'s own Role narrows every outstanding token immediately (live resolution); revoking the caller's token-create grant on `A` stops new mints but leaves already-issued tokens valid to TTL (§7).
30. **TTL and scope bounds.** Given a token-exchange request asking for a TTL above the platform-global maximum, then it is rejected (or clamped to the maximum) at issuance — there is no per-org TTL and nothing to compose. A `requested_scope` may only narrow the target's live-resolved grant, never widen it (§7).
30b. **Raw token subjects never reach authz.** Given any credential whose `sub` is malformed, unknown, deleted/disabled, or names Team, `org:<name>`, `system:authenticated`, or `system:operators`, when presented, then authentication rejects it and never invokes the authorization engine — even if the token is otherwise validly signed (§1, §7).
30c. **External workload credentials are exchange-only.** Given a Controller or ServiceAccount source-issuer JWT that validly maps to an existing active Rise identity, when presented directly to the generic resource API, then it is rejected; when presented to token exchange with a matching trust policy and sufficient token-create, then Rise returns a Rise-signed token for the target canonical `SubjectId`, which the resource API may accept (§7).
30d. **External `sub` is lookup input, not authority.** Given an externally signed token whose `sub` text is `system:operators` or otherwise resembles a canonical Rise identity but has no configured mapping to an existing source User/ServiceAccount/Controller, then token exchange rejects it before authorization; spelling a privileged subject never manufactures that principal (§1, §7).
30e. **Authenticated principal is typed.** Given a resource request reaches the authorization engine, then its caller is already an `AuthenticatedPrincipal` containing a parsed `SubjectId`; no engine entry point accepts a JWT, claims map, or raw subject string (§1, §7, Implementation structure).
30f. **Workload trust matching is unambiguous.** Given a verified external workload JWT whose normalized issuer index yields zero matching live trust policies after audience/claim evaluation, authentication fails; given more than one policy resolves it to different source identities, authentication also fails rather than choosing one by query order (§1, §7).

### Subresources (§2, §7)

30g. **Main-resource grants do not imply subresources.** Given a Role allowing
`*` verbs on `Deployment` with omitted `subresources`, then main-resource CRUD
is allowed but `(update, Deployment, status)` and every other Deployment
subresource remain denied. Given a separate statement with
`subresources: "*"`, it covers every registered Deployment subresource but not
the main resource (§2, §3).
30h. **Status is one stored object with a separate write path.** Given a
Deployment whose stored status is `availableReplicas: 2`, when an authorized
main-endpoint apply includes `status.availableReplicas: 99`, then the apply
succeeds but preserves `2` and acquires no field ownership for status. Given
the caller instead holds `(update, Deployment, status)` and writes through
`/status`, then only status changes, every non-status field is preserved, and
`metadata.generation` does not increment (§2).
30i. **Status permissions do not combine across endpoints.** Given a caller
holding only `(update, Deployment, status)`, a main update is denied even if
its payload changes only status. Given a caller holding only main-resource
`update`, the same payload cannot change stored status (§2).
30j. **Token issuance is create on a subresource.** Given a caller satisfying
the trust policy and holding `(create, ServiceAccount, token)` on the target,
then `POST .../ServiceAccount/.../<name>/token` may return a newly minted
credential without storing a Token resource. `get` on that subresource grants
nothing, and `get` on the ServiceAccount does not authorize issuance (§2, §7).

### Kind naming, references, platform resources (§8, §9)

31. **One kind token.** Given the kind `Deployment`: `kinds:` in Role statements, `Scope` paths, and resource URLs all accept exactly that token; no plural form exists anywhere and `ResourceDefinition` declares none (§8).
32. **`get`/`use` independence.** Given a subject with `get` but not `use` on a `RuntimeClass`, then reading it succeeds but a write referencing it at `spec.runtimeClass` is rejected; given `use` but not `get`, selection by name succeeds while reads are denied (§2, §9).
33. **Cross-org isolation without disclosure.** Given `RuntimeClass/gpu-b` is granted only to `org:acme-corp`, when another org's subject writes `spec.runtimeClass: gpu-b`, then it is rejected without disclosing whether `gpu-b` exists (`use` check precedes existence disclosure); that org's admin also cannot author a binding at `RuntimeClass/gpu-b` — reaching a root-scoped instance requires a `PlatformRoleBinding` (operator-only by placement), and the write-time grant gate's subset check independently blocks it (§4, §9).
34. **Org default is use-checked.** Given an org-admin setting `runtimeclass.rise.dev/default` on their Organization: naming a class the org holds `use` on is allowed; naming an ungranted class is rejected — the default label key is itself a declared reference (§9).
35. **Materialization semantics.** Given an SA in `acme-corp` (covered by an `org:acme-corp` `use` binding) creating a deployment, then the effective class resolves via the nearest-wins cascade, is stamped onto the Deployment, and is `use`-checked against the SA — allowed. Given the grant is later revoked, the running deployment is unaffected; the org's next deploy or rollback fails the check (§9, §6.1).
36. **`system:authenticated` reach.** Given the binding `Subject: system:authenticated, Scope: RuntimeClass/standard`, then any authenticated subject, of any kind, in any org, may select `standard` (§9, §1).

### Policy-object placement (§3, §4)

37. **Scope containment.** Given an org-parented `RoleBinding` under `acme-corp` whose `Scope` names `Organization/other-org`, a root-scoped instance, or `"*"`, when written, then it is rejected — an org binding's `Scope` must lie within its parent org's subtree; a root-parented `PlatformRoleBinding` carries any of those Scopes freely (§4).
38. **Reference direction.** Given an org `RoleBinding` with `roleRef: { kind: PlatformRole, name: resource-owner }`, then it is valid (platform Roles bind org-locally); given a `PlatformRoleBinding` whose `roleRef.kind` is `Role`, when written, then it is rejected — org-authored policy never escapes its org through a platform-wide binding (§4).
39. **No shadowing.** Given `acme-corp` creates a `Role` named `resource-owner`, then every existing and future `roleRef: { kind: PlatformRole, name: resource-owner }` is unaffected — references are kind-qualified, never resolved by bare-name fallback (§4).

### Ownership is emergent, not primitive (§6)

40. **No binding, no ownership — two distinct cases.**
    - **(40a) No selecting binding, no gate.** Given a resource where *no* binding's `LabelSelector` references `rise.dev/owner` at that location in the tree, then the label confers no access whatsoever, §6.6 step 2's no-gate branch fires (no binding selects the key), and writing the label requires only ordinary `update` — labels carry no authorization semantics of their own; "ownership" is entirely the effect of a selecting binding (§6, §6.6).
    - **(40b) Selecting binding present but granting nothing.** Given a §6.5 override binding on `{key: rise.dev/owner}` whose Role grants nothing (so a selecting binding *is* present — the override by construction carries `LabelSelector: {key: rise.dev/owner}`), then §6.6 step 2 falls through to step 3 (a selecting binding is present), but step 3's before/after diff over that binding's grant is empty, so the write passes with no additional permission needed (§6, §6.6).

### Security-hardening scenarios (§1, §4, §5, §6, §7, §9)

41. **Operator cannot be locked out.** Given an ordinary binding (of either placement) whose `Subject` is `system:operators`, whether its Role is Allow or Deny, when written, then it is rejected — only the platform-seeded bootstrap binding may target it. And given a `{Deny: * on every main resource and subresource}` row that somehow reached the store for `system:operators`, when any operator makes a request, then their effective policy still allows every main-resource and registered-subresource tuple — the guarantee overrides §4 steps 1–3, so the Deny is ignored (§1, §2).
41b. **Operator guarantee fires on the caller, not the subject row.** Given a `{Deny: * on *}` RoleBinding whose `Subject` is an operator's personal `user:` identity, when that operator makes any request, then their effective access is unreduced — the operator guarantee fires on the caller and ignores every `Deny` in the union, whatever subject it targets (§1).
42. **Org binding cannot Deny/DoS a foreign or org-agnostic subject.** Given an org-parented binding under `acme-corp` whose `Subject` is a shared `Controller` (or `system:operators` or an unrecognized `system:*` name), when written, then it is rejected — a `Controller` is a member of no single org and so cannot be targeted by an org binding, and those `system:` names are reserved; an org cannot Deny or otherwise degrade a cross-org subject through its own bindings. (`system:authenticated` is exempt — it is targetable and auto-clamps to the org's own members, per scenario 49.) (§1).
43. **Membership add is grant-closure-gated.** Given a writer holding only `update on Team`, when they add themselves to a Team bound to a broad Role, then the write is rejected — the newly-implied grant is the full union of every binding targeting that Team, which is not `⊆` the writer's own effective permissions (§4, §5).
44. **User→org membership is live.** Given a `User` who is a member of `org:acme-corp` only by virtue of one Team owned by `acme-corp`, when they are removed from that last Team, then their very next request no longer draws on any `org:acme-corp` grant — org membership is resolved live from Team ties, never a stored roster (§1).
44b. **Leaving an org also leaves its self-cap boundary.** Given that User separately holds an operator-authored `PlatformRoleBinding` Allow on an acme resource and is covered by an acme org self-cap only through their last Team tie, when that tie is removed, then both the org grants and the org self-cap disappear; the platform Allow remains. The removal requires ordinary Team-update authority but is not grant-gated, because platform-granted access to a non-member is governed by operator caps, not org self-caps (§1, §4, §5).
45. **Deny-aware subset.** Given a writer whose full effective policy is `{Allow: *; Deny: delete on Environment}`, when they author a grant of unrestricted `{Allow: *}`, then it is rejected — the `⊆` check compares against Allows net of Denies, so the grant is not a subset (it would leak `delete on Environment`) (§5).
46. **Scope-exact subset.** Given a writer holding a verb only at a narrow `Scope`, when they author a grant of that verb at a broader `Scope`, then it is rejected — the subset check is computed over exactly the new grant's domain, and the writer does not hold the verb across all of it (§5).
46b. **Cross-key subset fails closed.** Given a writer holding `{Allow: * on *}` plus a `{key: tier, value: restricted}`-scoped `{Deny: delete on Deployment}`, when they author a grant of `{Allow: delete on Deployment}` narrowed by a *different* key `{key: owner}`, then it is rejected — selectors on different keys are treated as possibly-intersecting and the subset check fails closed; §1's cross-key non-collision is a *replacement* rule, not a domain-disjointness proof (§5).
47. **Deny survives replacement, across placement tiers.** Given a platform `PlatformRoleBinding` wildcard for subject `S` carrying a `Deny`, and an org-parented `RoleBinding` for the same `S` and `LabelSelector` key that restates only Allow content, when evaluating in that org, then the org binding's Allow replaces the wildcard's Allow (cross-placement replacement is exactly how an org overrides a platform default, §6.5) but the platform `Deny` is preserved — replacement may drop only Allow content, never subtract a `Deny`, so a platform restriction cannot be escaped by an org override (§1).
48. **Effective-label removal is gated.** Given a child resource carrying its own `rise.dev/owner` that shadows an ancestor value naming the writer's team, when the writer *removes* the child's label so it inherits the ancestor's owner, then the write is gated and rejected — the diff is over `effectiveLabels`-resolved ownership and treats removal as an access-driving change, not a de-escalation to absent (§6.6).
48b. **Owner-relabel spans the inheriting subtree.** Given the writer relabels `rise.dev/owner` on resource `r` such that the new value would grant the writer's own team ownership over `r`'s descendants that inherit `r`'s value through `effectiveLabels`, when written, then it is rejected — the §6.6 diff spans `r`'s `K`-inheriting subtree, not `r` alone, so the newly-granted access over descendants is caught (§6.6).
49. **Recipient boundary.** Given an org-parented binding under `acme-corp` whose `Subject` is `org:other-corp` or a `Team` owned by another org, when written, then it is rejected as provably foreign; given the same binding targeting `system:authenticated`, it is accepted but auto-clamps to `acme-corp`'s own authenticated members and cannot expose platform-wide (§1).
50. **Controller token creation is platform-only.** Given an org-admin holding `(create, Controller, token)` only through an org-scoped grant, when they attempt to mint a token for an org-agnostic Controller, then it is rejected — the Controller identity is root-scoped, so a grant reaching it requires a `PlatformRoleBinding` an org-admin cannot author, preventing a cross-tenant Controller token (§7).
51. **Multi-org class isolation.** Given a `User` who is a live member of both `acme-corp` and `beta-corp`, and `RuntimeClass/gpu-b` granted `use` only to `org:acme-corp`, when they deploy into `beta-corp` selecting `gpu-b`, then it is rejected — `use` is evaluated against the consuming resource's org (`beta-corp`), where no grant exists, not the subject's membership elsewhere (§9).
52. **Token gate is independent of `act`.** Given every exchange-minted token carries the `token_class` marker regardless of `act`'s presence or content, when such a token is presented as the calling identity for a further exchange, then it is rejected on the class marker alone — a change to when `act` is emitted cannot reopen chaining (§7).
53. **Default-label authority.** Given a project editor holding only bare `update`, when they set `runtimeclass.rise.dev/default` on the Project, then it is rejected — writing a reserved default label requires the resource's owner/admin tier; when the Project's owner sets it (and holds `use` on the named class), it is allowed (§9).
54. **Snapshot reference does not re-check.** Given a materialized reference, the resolved value is snapshotted at write time and a later `use` revocation does not disturb the running resource; revocation applies from the org's next deploy or rollback, which re-resolves against current grants (§9).

### List authorization (§4)

55. **Org-wide list projects each item by its own `get`.** Given `list on Project` granted to `system:authenticated` at `Organization/acme` (auto-clamped to acme members by §1's recipient boundary), when an acme member lists projects, then every acme project is returned. Items on which they lack `get` contain exactly the allowlisted `apiVersion`, `kind`, and `metadata` fields (including name and `effectiveLabels`); items on which they also hold `get` contain the full stored object, including arbitrary kind-specific top-level fields — data visibility and existence/owner visibility remain independently granted (§4).
56. **No list grant is masked-empty, not 403.** Given a caller with no applicable `list` grant on `beta-corp`'s projects, when they list that collection, then they receive a masked-empty result rather than a 403 — the collection's population is never confirmed, consistent with §6.6/§9 existence-masking (§4).

## References

- `ROADMAP.md`, Workstream 1 ("Multi-Tenancy & Generic Resource API") — owns
  live status for the resource-API RBAC items this model informs.
- [Generic Resource API](../../generic-resource-api/) — the shipped,
  operator-only surface this model will govern.
- `crates/rise-resource-api`, `crates/rise-resource-store` — the envelope types
  and the `ResourceStore` trait/impl the Implementation structure builds on.
