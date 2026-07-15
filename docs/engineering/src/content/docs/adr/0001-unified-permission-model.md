---
title: "ADR-0001: Unified Permission Model"
---

## Status

**Proposed** (under review). Date: 2026-07-10.

scope: the generic resource API (`/api/v1/resources/...`) and
ServiceAccount/Controller token issuance (the `token` subresource). It does
not change how `rise project create`, `rise deployment create`, or other
typed-table-backed CLI commands work; those converge onto this model
automatically once their tables migrate onto the generic resource store, which
is separate, already-planned work (`ROADMAP.md` §4, Typed-object migration).

## Context

Rise today has several disjoint authorization mechanisms, each with its own
code path. The generic resource API is operator-only: access is gated on
membership in the `auth.operator_users` config allowlist, with no finer
granularity. The typed APIs (projects, groups, deployments, …) each carry their
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
org (a compliance-restricted customer). Within each organization where they are
an admin, an org admin has every authority not removed by that platform ceiling;
the same User may independently be an admin in several organizations. Org admins
must be able to delegate access further, but never beyond their own boundaries. And
it should be *one* runtime-configurable mechanism covering Users, Groups,
ServiceAccounts, Controllers, and Operators alike — not five code paths that
happen to agree.

The design below was converged through multiple independent adversarial-review
rounds; its wording is deliberate, particularly around the security-sensitive
edges (label-write gating, token minting, operator bootstrap).

## Decision

Every actor in Rise — a person, a group, a CI service account, a controller
process, or a platform operator — is a **subject**. Every subject's access to
every resource is decided the same way, by the same evaluator, regardless of
what kind of subject it is.

Resources live in a tree — an Organization contains Projects, and a Project
contains Environments, Deployments, and other children (e.g. Environment
`env-prod` under Project `app` in the org `acme-corp`).
Access is granted by binding a **Role** (a named bundle of permissions — "can
update Deployments, can read Environments," built from `verbs` like
`get`/`update`/`delete`) to a subject, placed at some point in that tree. A
binding's grant applies to everything at or below where it's placed, and —
optionally — can be narrowed further to only the resources carrying a specific
label.

A subject's effective access on a resource is the combination of everything
its applicable bindings grant, **minus** every applicable Deny retained for
that caller's tier. Among retained statements, Denial always wins over
allowance — there is no "more specific wins" precedence between ordinary
bindings. Platform Denies apply to everyone except operators; org Denies apply
to ordinary org members and workloads but are ignored by current admins of
that org (§5). This is what lets the platform cap an org's admins while also
letting those admins impose a lower ceiling on their ordinary population.
After tier filtering, a restriction is just one more `Deny` folded into the
same combination, never a separate capping step on top of it.

A **restriction** ("cap") is itself just a `Deny` binding, folded into the
same Deny-wins combination as every other binding while retaining its binding
placement as provenance. An operator may place a platform Deny instance-wide
or scope it to one org; an org admin may place an org Deny to narrow ordinary
subjects in that org without narrowing current admins. There is one further
rule, applied only
at the moment of a write: whoever authors a grant — binding a Role to a
subject, or editing a Role's own definition — must already hold everything they
are handing out. The check compares net before/after EffectivePolicy, so it
also catches authority exposed by removing a Deny or changing admin status;
you cannot hand out authority outside your own current effective boundary.

Every request starts from current authorization facts in the database — no
policy snapshot is baked into a token, and any memoization is confined to that
one immutable request snapshot (§5), so
tightening a cap or narrowing a Role takes effect immediately for everyone relying on it. This
is what makes revoking a Role exactly as effective as revoking a token before
it expires — but only for the identity the token belongs to: narrowing what a
ServiceAccount itself can do immediately narrows every outstanding token for
it. Revoking the separate grant that let someone *mint* that token in the
first place does not reach back and affect a token already issued (§7).

Ownership works through this same mechanism, not a separate one. A resource
can carry a label — `rise.dev/owner: group:platform` or
`rise.dev/owner: user:u-01jz…` — naming the group or person it
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

**Org-agnostic — User, Controller.** A single identity can legitimately hold different roles in multiple different organizations: a person is a member of two customers' orgs with different access in each; a single Controller process reconciles resources across many orgs. Nothing about the subject's identifier ties it to one organization. A binding for one of these kinds has a normalized `scope` (§4) — either one specific org, or a wildcard (`"*"`) meaning "the default for every org this identity touches, unless a more specific binding exists for that org."

**Org-native — Group, ServiceAccount.** These exist within exactly one organization by construction: a Group has one owning org, and a ServiceAccount is created and lives directly under an org (`serviceaccount:acme-corp/ci-bot`) — a sibling of Project in the resource tree, not nested under any one Project. This is deliberate: a ServiceAccount's reach comes entirely from what it's bound to via ordinary RoleBindings (§4), which can span any number of Projects/Environments in its org; tying its identity to a single "home" Project would suggest a relationship that has no bearing on what it can actually do, and would couple its `effectiveLabels`-inherited attribution (§6.1) to whichever Project happened to parent it. The org is baked into the identifier itself. A **static** binding (a literal, fixed subject — §4) for these kinds either omits an explicit `scope` (normalized to the subject's own Organization scope) or, if one is supplied, it must lie within the subject's own org. In particular, explicit `scope: "*"` is invalid for a static org-native subject: a wildcard in the bound Role's `kinds` still ranges only within the binding's scope and does not make an org-scoped binding reach root-scoped kinds. A **dynamic** binding (a subject template — §4, §6.3) has no concrete subject to infer an org from until it is evaluated against a specific resource; §6.3 states how its resolved subject's org is determined in that case.

**Canonical subject identifiers.** Authorization never operates on an unparsed string. One shared `SubjectId` type accepts exactly these concrete forms:

```
user:<name>
controller:<name>
group:<org>/<name>
serviceaccount:<org>/<name>
org:<name>
system:authenticated
system:operators
```

`<org>` and `<name>` use the generic resource API's canonical resource-name grammar. Empty components, extra `/` or `:`, dot segments, query/fragment syntax, and non-canonical encodings are rejected; there is no permissive fallback interpretation. A literal User, Controller, Group, ServiceAccount, or `org:<name>` in a binding must resolve at write time to an existing Rise resource (including resources created in the same atomic transaction). The two `system:` forms are virtual and follow the additional authoring restrictions below. Only exact dynamic forms declared by the closed template grammar may contain a marker: kind-fixed templates such as `group:${ref.name}` and `user:${ref.name}`, plus the typed `${ref.subject}` template defined in §6.1. Arbitrary interpolation syntax is rejected when the binding is written. Dynamic substitution produces a concrete canonical `SubjectId` and is parsed again at evaluation time, failing closed if invalid. For an org-native template the matched resource supplies `<org>` as defined in §6.3, so `group:${ref.name}` on an acme resource resolves to `group:acme-corp/<value>` rather than to an incomplete literal.

**Subject records and their relationships are built-in resources.** All persisted identity, membership, and authentication-policy objects use the existing built-in `rise.dev/v1alpha1` API group; there is no separate `authorization.rise.dev` group. Placement is fixed by kind:

| Kind | Parent | Purpose |
|---|---|---|
| `User` | root | Stable human identity and non-authoritative profile fields |
| `UserIdentity` | `User` | One external SSO `(issuer, subject)` mapping |
| `Controller` | root | Stable org-agnostic workload identity |
| `ControllerTrustPolicy` | `Controller` | One accepted external issuer/audience/claims policy |
| `Group` | `Organization` | Org-native group identity |
| `GroupMembership` | `Group` | One typed reference to a member `User` |
| `ServiceAccount` | `Organization` | Stable org-native workload identity |
| `ServiceAccountTrustPolicy` | `ServiceAccount` | One accepted external issuer/audience/claims policy |

The separate trust-policy kinds are intentional: a generic kind has exactly one declared parent, so one `TrustPolicy` kind cannot be parented under both Controller and ServiceAccount without violating the store's exact-parent invariant. Likewise, Group membership is one child resource per edge rather than an array in `Group.spec`: this avoids whole-object lost updates, gives each membership its own grant-gated lifecycle/audit identity, and supports lookups in both directions. `GroupMembership.spec.userRef` is an immutable, kind-qualified reference to an existing User UID; changing the member is a delete plus create, so adding the replacement User passes the ordinary membership grant gate. Membership itself is boolean for authorization — any descriptive membership role is ordinary metadata and never a second permission system. Deleting a User is blocked while memberships reference it (or an authorized cleanup deletes those membership resources in the same transaction), so no dangling membership can authenticate or expand access.

**User names are stable generated identifiers, not email addresses.** A User gets an immutable, collision-resistant DNS-safe resource name such as `u-<lowercase-ulid>` (with the store's ordinary uniqueness constraint as the final authority). The generic resource-name grammar remains unchanged and does not admit `@`; relaxing it for one identity kind would either weaken every resource path or require kind-specific metadata-name parsing in the core. `User.spec` may carry presentation fields such as `displayName` and `primaryEmail`, but email is mutable, case-sensitive in troublesome ways, and non-unique across issuers, so it is never a subject key and Rise never auto-links accounts by email. `UserIdentity.spec` carries the authoritative external `issuer` and `subject`; that pair is globally unique among live UserIdentity resources. Both kinds also carry a platform-managed `spec.active` boolean, defaulting to `true`. An inactive User cannot log in and every already-issued token for that User fails authentication. An inactive UserIdentity blocks login through that exact upstream identity and is excluded from operator-selector matching, but does not invalidate sessions established through the User's other identities. These fields are governed resource data, but shipped policy does not let Users edit their own identity records. UI/CLI surfaces resolve and display email/name while bindings, references, URLs, and audit records use the canonical User name.

Literal subjects and `roleRef`s are deliberately **name-bound**: deleting and recreating an identity or Role under the same canonical name makes existing policy refer to the replacement, matching Kubernetes RBAC's name-based references. Credentials add a separate immutable binding. Every persisted User, ServiceAccount, and Controller has a store-assigned UID, and every Rise-issued token carries both the canonical `sub` and that UID in `rise_uid`. Authentication requires both values to identify the same live, enabled resource. Recreating `serviceaccount:acme/ci` may intentionally reactivate name-bound RoleBindings for the replacement, but every token issued for the old UID fails immediately. Dynamic label subjects follow the same name-bound policy semantics; membership references remain UID-bound as specified above.

**Deferred constrained product operation for Project ServiceAccounts.** Generic `create`/`delete` on `rise.dev/ServiceAccount` remains ordinary RBAC authority and is shipped only to org admins and operators; it may intentionally exercise the name-bound behavior above and is not implicitly transformed into a second grant-gate rule. A future user-facing Rise operation will preserve today's "create a ServiceAccount for this Project" flow as a trusted, fixed-shape compound operation authorized against that Project. In one transaction it allocates a fresh collision-resistant canonical ServiceAccount name that the product flow never reuses, stores any friendly requested name as presentation data, creates the Organization-owned ServiceAccount, and creates only the platform-defined Project-scoped bindings and trust-policy data for that flow. The product-created identity does not receive an ownership label that would incidentally expose generic deletion; its lifecycle remains behind the constrained operation. The caller supplies no arbitrary Role or RoleBinding policy and need not hold generic permission to create those resources; instead, the compound operation applies §5's effective-delta subset check to the resulting ServiceAccount policy and rejects any bundle outside the caller's current capped EffectivePolicy on that Project. Its paired deletion operation disables or removes the identity and cleans up only the flow-owned authorization data; the retired canonical name remains unavailable to the product flow. These are constraints on the future design; its route, authorization tuple, execution shape, implementation, and conformance case are deferred to ADR-0002/ROADMAP and do not block this ADR's initial conformance suite.

Trust-policy resources contain public matching configuration (issuer, audience, required claim constraints), never private signing keys or bearer credentials. UserIdentity and workload trust-policy writes are ordinary governed resource writes, with their schemas and the authentication-specific validation in §7 applied before persistence. `org:<name>`, `system:authenticated`, and `system:operators` remain virtual predicates and have no corresponding identity row; an Operator remains an active User with at least one live, active UserIdentity selected by restart-loaded configuration.

**Operator** is a platform-wide root identity selected by configuration. Each process loads an immutable set of `(issuer, subject)` pairs from `operatorIdentities` at startup; it does not resolve those pairs to a frozen UID set. After a credential resolves to User UID `U`, `U` is an operator iff `U` is active and any live, active `UserIdentity` child of `U` has an exact pair in that configured set. The configured identity is therefore a lookup selector for the User, not a requirement that the current login used that same identity: logging in through any active secondary UserIdentity already attached to `U` yields the same User UID and operator status.

Interactive login provides the bootstrap path. After validating an enabled upstream IdP credential, Rise looks up its exact `(issuer, subject)`, including inactive live rows. An inactive mapping, or an active mapping whose parent User is inactive, fails authentication and is never treated as unknown for JIT provisioning. If no live mapping exists, one authentication-plane transaction creates a generated User and that UserIdentity; a unique-index conflict caused by concurrent first logins retries by loading the winning mapping. This includes a pair whose previous mapping was deleted: deletion is unlinking, not durable deactivation, and a later valid login provisions a fresh User and UserIdentity with a fresh UID. Old tokens therefore never revive. Durable disablement uses `active: false`; operators should normally deactivate Users or identities rather than delete them.

The new User has no org authority by default, but if the new identity pair is in `operatorIdentities`, the same login immediately qualifies the User as an operator. Consequently deleting a configured mapping revokes the old User's operator expansion only until the next valid login recreates the pair; durable operator removal requires deactivating that live UserIdentity or removing the configured selector and restarting/draining all instances. This fixed JIT operation is the configuration-rooted exception to ordinary grant-gated identity linking. It never attaches an unknown identity to an existing User: a genuine secondary identity must be linked through an explicit governed flow, and Rise never infers account equivalence from email or profile fields. Once attached, every active identity authenticates as the same active User UID.

Plain OAuth does not define end-user identity claims, so an OAuth-only upstream needs a provider adapter that supplies the same stable issuer/subject identity contract. Email is never an operator selector. The configured selector set is not hot-reloaded: changing it takes effect only after restart and revocation is complete only after every API instance using the old configuration has been drained. UserIdentity rows remain live authorization facts, however; adding, activating, deactivating, or deleting a configured identity under a User changes that User's operator status on the next request in every running process, subject to the JIT recreation rule above.

What changes is how operator status is *expressed*: rather than a hardcoded bypass branch in the evaluator, operator status is membership in one reserved subject, `system:operators` (a `system:`-prefixed name is reserved for platform-recognized pseudo-subjects, never an ordinary User/Group/ServiceAccount/Controller row). The platform seeds exactly one binding for it:

```
subject: system:operators
scope:   "*"
roleRef: { kind: PlatformRole, name: system-admin }      # a PlatformRoleBinding (§3)
```

where `PlatformRole/system-admin` allows every verb on every main resource and
every registered subresource (§2).

An operator's request runs through the *same* evaluation algorithm as anyone else's (§4 steps 1–3) — no separate code path. One thing is special-cased for any request whose membership expansion (§4 step 1) includes `system:operators` — i.e. any request by a current operator: the Deny-wins union of steps 1–3 is overridden, so no `Deny` collected in step 1 can reduce an operator's effective access. This is load-bearing because a cap is itself a `Deny` binding (§5): an operator caps every other subject, including org-admins, but an operator's *own* request ignores every `Deny` — otherwise an operator could accidentally lock themselves, and everyone else, out by placing an instance-wide cap that only they can author, with no one above an operator able to fix it. The granter-subset half of the write-time grant gate (§5) needs no special-casing at all: since `system:operators` always holds every main-resource and subresource permission, any grant an operator hands out trivially satisfies `⊆` their own effective permissions.

**`system:` names are reserved, and `system:operators` is never a binding target.** The `system:` prefix is reserved, enforced at both subject creation and binding-`subject` authoring: an unrecognized `system:`-prefixed subject (anything but the platform-recognized `system:operators` and `system:authenticated`) is rejected wherever it appears. Among the recognized names, `system:operators` may *never* be named as the `subject` of an ordinary binding — only the platform-seeded bootstrap binding above targets it; an ordinary `RoleBinding` or `PlatformRoleBinding` whose `subject` is `system:operators` is rejected at write time. Otherwise an org could author a `{Deny: * on *}` binding catching operators and lock them out of an org with no in-model recovery — the exact state §1 exists to prevent. (`system:authenticated` and `org:<name>` remain ordinary group predicates a binding may target — §4's list authorization and general org-wide grants use them, independent of §9 — bounded by the recipient boundary below.)

**Membership never replaces a caller's own identity.** When User `user:u-01jz…` makes a request, that remains the caller's subject regardless of which UserIdentity authenticated it. What differs is **membership expansion** (§4 step 1): evaluation considers not only bindings targeting the User directly, but also bindings targeting a current Group and, when any live, active UserIdentity child matches restart-loaded `operatorIdentities`, `system:operators`. Group and operator membership are live instances of the same expansion rule. Since the only binding targeting `system:operators` grants `system-admin`, the User's combined policy includes every main-resource and registered-subresource permission, unioned with whatever they separately hold. Deactivating or removing the matching UserIdentity revokes that expansion on the next request; changing profile email or logging in through another already-linked identity has no effect. A configuration change additionally requires restart and draining old processes. The default ownership binding (§6.2) similarly resolves either the User directly or one of their Groups; Group-targeted ownership reaches each member through this same expansion.

**Group subjects.** Two reserved group forms exist beyond persisted Group resources: `system:authenticated` — every authenticated subject, of any kind — and `org:<name>` — every subject belonging to that organization: its org-native subjects (Groups, ServiceAccounts) and its User members alike. A User belongs to `org:<name>` iff they are a current member of at least one Group owned by that org **or** are directly targeted by a qualifying org-admin RoleBinding (§5). The latter is the bootstrap edge that lets the first administrator govern a newly created organization without inventing a magic Group; it is policy data, not a second general membership mechanism. Removing a User's last Group tie and last direct admin binding removes that membership live. An ordinary org `RoleBinding`, including one directly naming `user:`, is intersected with this boundary, so a group-less, non-admin User receives no org-authored grant. To grant ordinary org-private access, first place the User in a Group. An operator may deliberately reach a group-less User through a scoped `PlatformRoleBinding`, outside org governance.

SSO/directory synchronization creates and deletes ordinary `GroupMembership` resources. A directory-wide Group is the normal way to make every synchronized User an org member. The synchronizer authenticates with a Rise-issued principal and its membership writes pass the same grant gate as manual writes; mapping an upstream directory Group to an admin-bearing Rise Group therefore requires admin-level delegation authority. ServiceAccount inclusion in `org:<name>` remains deliberate: org-wide grants must reach CI identities too.

The absolute `org:<name>` form is useful to an operator binding an org population to a root-scoped resource (§9). Inside an org-parented binding the name is redundant — `system:authenticated` already clamps to that binding's org — but the absolute form retains one meaning everywhere and is not overloaded with relative names such as `org:admins`.

**Org bindings target only their own org.** An org-parented `RoleBinding`'s grant to any subject is intersected with live membership in that binding's *own* org — a subject receives the grant only while it is a current member of that org. An org-native subject (`Group`/`ServiceAccount`) whose baked-in org differs from the binding's org is provably foreign and contributes no grant; a `User` subject receives the grant only while a live member of the binding's org through a Group tie or the direct admin bootstrap edge above (so a user in `acme` and `beta` gets an `acme`-scoped binding's grant in `acme` alone, and loses it on leaving `acme`); `system:authenticated` inside an org binding auto-clamps to that org's authenticated members and cannot expose platform-wide. A `Controller` — org-agnostic, a member of no single org — likewise receives no org-binding grant. These semantically inert bindings remain valid, safe policy data and are surfaced by policy auditing rather than rejected synchronously. `PlatformRoleBinding`s are root-placed and normalize to `subjectMembership: Any`, allowing deliberate cross-org or platform-wide operator grants. A PlatformRoleBinding may opt back into contextual containment with `ResourceOrganization` (§4); the seeded ownership binding does so, while RuntimeClass availability and explicit non-member grants use `Any` as appropriate. This bans cross-org sharing through org bindings by construction and prevents contextual ownership from outliving org membership; a first-class tenant-authorable cross-org sharing primitive is deferred (§10).

**The binding is data; the operator predicate is derived — deliberately.** `system:operators`'s grant (the binding above) is a stored row, same table as every other binding. Membership is never stored as a separate RBAC record: evaluation intersects restart-loaded `operatorIdentities` configuration with the active User's live, active UserIdentity children. This is forced by the bootstrap problem the Operator concept exists to solve: if the initial relationship required an already-authorized RBAC write, nothing could create the first operator. The configured selector set is the one root of trust originating outside the system Rise governs; UserIdentity rows remain ordinary inspectable resources plus the narrow JIT bootstrap path above.

The binding has no equivalent forcing problem — it's never granted by anyone at runtime, only seeded once at bootstrap — so it can safely be data, with one refinement. Being immutable through the ordinary write path (§5's **seeded** Role-ownership tier: no write path can ever modify it, not even an operator) only protects against mutation through this model's own API — it says nothing about a bad migration, a restore from an old backup, or direct database access losing the row entirely, outside any write path this model governs. That residual risk is unacceptable for the one subject with no recovery authority above it. Operator status is a property of the requesting caller, not of any one subject row. Whenever a request's live membership expansion (§4 step 1) includes `system:operators`, the evaluator yields the complete main-resource and registered-subresource policy for that request unconditionally — it ignores every `Deny` collected in step 1 from *any* subject, including a `Deny` targeting the caller's own `user:` identity or any cap binding. No binding can reduce an operator's effective access. The write-time rejection of bindings that target `system:operators` (above) remains as defence-in-depth but is not load-bearing on its own: configured selectors can change across restarts and their matching UserIdentity rows can change live after a binding is written, so the guarantee must hold at evaluation regardless of what `Deny` rows exist. This guarantee is hardcoded in the evaluator — not something solely read from, and therefore losable with, a table row. The row is still materialized alongside that guarantee, purely so the same explain/audit tooling that inspects everyone else's access can inspect this one too without a special case; if it's ever found missing or altered outside the write path, that's healed by re-materializing it, not a live authorization dependency.

This mirrors how Kubernetes actually handles `system:masters`: a hardcoded superuser check in the authorizer grants it full access with no ClusterRole or ClusterRoleBinding required at all, *and*, redundantly, an ordinary `cluster-admin` ClusterRoleBinding also binds the same group to the same power as a stored object — kept self-healing (missing permissions/subjects on default, `kubernetes.io/bootstrapping=rbac-defaults`-labeled objects are restored automatically) rather than merely immutable. Every other `system:`-prefixed built-in role (`system:node`, `system:kube-scheduler`, etc.) gets only the self-healing-data half, no hardcoded bypass, because losing one of those is recoverable by whoever holds `system:masters` — the same distinction already drawn above between `system-admin` (nothing above it, needs the hardcoded guarantee) and `org-admin` (recoverable by an operator, doesn't). Kubernetes' authorization decisions are live on every request in both cases, same as this model's throughout (§5); what's actually startup-scoped there is narrower — only the drift-repair of default objects' stored contents, not authorization itself.

**Wildcard resolution.** When two bindings target the same `(subject, labelSelector-key-if-any)` pair — one with `scope: "*"` and one with a more specific `scope` — the more specific one **replaces the wildcard outright, for that scope** — it does not merge with it. "Same subject" for this comparison means the same literal subject, or the same subject *template* text; a dynamic binding on `labelSelector: {key: rise.dev/owner}` never collides with one on `labelSelector: {key: rise.dev/squad}`, even if both use the identical template `${ref.subject}` — they are different rules. This comparison is always performed on the binding's authored `subject` field exactly as written — literal `SubjectId` against literal `SubjectId`, or raw template string against raw template string — never on a resolved value: a literal binding (`subject: group:acme-corp/platform`) and a dynamic one (`subject: group:${ref.name}`) never collide with each other, even where the template resolves to that same concrete Group, so a platform-wide dynamic default is never silently discarded just because one particular resource's resolved subject happens to match some unrelated static binding. Where a `labelSelector`'s optional `value` also differs between two otherwise-colliding bindings, replacement is evaluated per-resource, at the same point §4 step 1 collects applicable bindings, not as a blanket scope-wide swap — a `value`-narrowed selector only matches (and so only competes with and replaces a broader same-key selector for) resources whose label actually equals that value; resources carrying any other value never collect the narrowed binding in step 1, so the broader selector continues to govern them, undiminished. This replacement rule applies to any subject (not only Controller) whenever a wildcard `scope` is in play, including the dynamic ownership bindings in §6 — and crucially it applies **across placement tiers**: an org-parented `RoleBinding` may replace a root-parented `PlatformRoleBinding`, which is exactly what lets an org override the platform-seeded ownership default (§6.5), whose default *is* a `PlatformRoleBinding`. What replacement may never do is *subtract a `Deny`*: it preserves every `Deny` statement the superseded binding carried and may drop only the wildcard binding's *Allow* content. That single invariant — not a blanket placement prohibition — is what stops an org from escaping an operator's platform restriction: a restriction expressed as a `Deny` survives replacement regardless of who authored the superseding binding, while an all-`Allow` default (like `resource-owner`, §6.2) remains freely overridable. It exists to keep "what does this rule resolve to, in this org" a single, unambiguous answer instead of an additive combination of whatever bindings happen to apply — the one place bindings do not simply combine (§4 covers the ordinary, additive case).

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
therefore `(verb, ResourceKind, subresource?)`: status update is
`(update, rise.dev/Deployment, status)`, while delegated ServiceAccount token
issuance is `(create, rise.dev/ServiceAccount, token)`. The main resource has no subresource value.
Permissions never flow implicitly between the two: `update on rise.dev/Deployment`
does not authorize `(update, rise.dev/Deployment, status)`, and a status grant does not
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
preserve them and only `(update, ResourceKind, finalizers)` may change them.

This ADR standardizes that shared authorization and handler seam, plus
the concrete `status`, `finalizers`, and `token` strategies. Streaming,
connection, proxy, and virtual-projection contracts needed by possible future
`logs`, `proxy`, or `scale` subresources are explored in
[ADR-0002](./0002-generic-resource-subresource-execution-model.md). Adding one
later does not change the RBAC algebra: it registers a handler and is authorized
by the same `(verb, ResourceKind, subresource)` tuple.

### 3. Roles and the Allow/Deny evaluator

A **Role** is a named, reusable **policy**: an order-irrelevant list of statements,

```
{ effect: Allow | Deny, kinds: ["rise.dev/Deployment"] | ["rise.dev/*"] | "*", verbs: ["update", "delete"] | "*", subresources?: ["status"] | "*" }
```

A `ResourceKind` is the canonical, version-independent `<api-group>/<Kind>`
pair: `rise.dev/Deployment` or `widgets.example.com/Widget`. An exact value
matches one kind; `rise.dev/*` matches every Kind in that API group; bare `"*"`
matches every group and Kind. Versions are absent because all served versions
of one `(group, Kind)` share authorization. Unqualified Kind strings are
rejected, closing the ambiguity allowed by storage, where different API groups
may legitimately use the same Kind name.

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

A subject's access on `(verb, ResourceKind, subresource?)` under a given policy is permitted iff at least one `Allow` statement matches and no applicable `Deny` statement matches — Deny wins after §5's placement-tier filtering. Pure-additive permission sets cannot express subtraction from an open-ended wildcard; a Deny expresses it directly:

```
Allow: * on *
Deny:  delete on rise.dev/Environment
```

Roles and RoleBindings are data (rows), not compiled match arms — operators and org admins configure who can do what at runtime, no redeploy required. Restrictions (§5) are `Deny`-bearing bindings of this same policy shape, not a separate construct: platform placement caps admins and ordinary subjects, while org placement caps only ordinary org subjects. A Role's own statement list is, like any other resource, ordinarily writable by whoever holds `update` on its kind — but because editing a widely-bound Role changes what every subject bound to it can do, that write is gated the same way a RoleBinding write is (§5).

**Two kind pairs, one per placement level.** The resource store's parent model is exact — a kind declares one parent, not a choice of parents — so policy objects come as two same-shaped pairs, the same fork Kubernetes resolves with `ClusterRole`/`Role`: **`Role` and `RoleBinding`** are parented under an `Organization` (org-level policy, authored by whoever holds `create`/`update` there — org-admins by default, further delegable like anything else), while **`PlatformRole` and `PlatformRoleBinding`** are parented at the root (platform-level policy — operator-authored, not by a bespoke rule but because only `system:operators` holds `create` at root under ordinary default-deny). Where this document says "binding" or "Role" without qualification, the statement applies to both pairs alike.

### 4. RoleBindings — targeting a subject to a slice of the resource tree

A **RoleBinding** attaches a Role to a subject, at a `Scope`, optionally narrowed by a `labelSelector`. Serialized fields are lower camel case:

```
subject:        <literal SubjectId, e.g. group:acme-corp/platform> | <subject template, e.g. group:${ref.name} or ${ref.subject}>
subjectMembership:  Any | ResourceOrganization # always present after normalization
scope:          <path, e.g. rise.dev/Environment/acme-corp/app/env-prod> | "*" # always present after normalization
labelSelector?: { key: <label key>, value?: <fixed value> }      # optional narrowing filter
roleRef:        { kind: PlatformRole | Role, name: <name> }
```

`subject` is deliberately singular in the initial model. Giving the same Role to unrelated subjects uses one binding per subject; giving it to a population uses a Group, `org:<name>`, or `system:authenticated`. A future `subjects:` convenience may normalize each entry into an independent virtual binding without changing policy semantics, but storing several subjects in one normative binding is rejected for now. In this model subject identity participates in wildcard replacement, dynamic-template resolution, recipient-boundary validation, grant-gating, and audit explanation; keeping it singular avoids partial replacement or partial mutation semantics that Kubernetes' additive-only bindings do not have.

`subjectMembership` is a closed enum whose serialized values are PascalCase. It is accepted only on `PlatformRoleBinding`; org `RoleBinding`s reject the field because their parent-org recipient boundary is already structural. Omitted platform input normalizes to the persisted value `Any`; explicit `null` and unknown values are invalid. `Any` adds no membership constraint, including for deliberate scoped grants to non-members.

`ResourceOrganization` adds a constraint only when the target resource is org-contained **and** the concrete subject being tested is not inherently org-scoped. A User then needs a current Group tie or direct qualifying admin affiliation to that target org. A Controller has no such membership and therefore does not match; controller grants that merely need geographic narrowing use an explicit `scope` with `Any`. For `${ref.subject}`, the check occurs after resolving the concrete User or Group. For `system:authenticated`, it applies to the actual requesting principal, so a demo-org read binding can choose `Any` to include every authenticated principal or `ResourceOrganization` to include only that org's members. Group, ServiceAccount, and `org:<name>` subjects already carry their organization, so the additional constraint is a no-op for them; it is likewise a no-op on root-scoped targets. These no-op combinations are valid policy data. The normalized value is retained in binding provenance and participates in matching, EffectivePolicy, wildcard replacement simulation, the grant gate, request snapshots, and explain output.

The `scope` field is always present after write-time normalization and establishes the binding's applicability domain. If omitted on an org-parented `RoleBinding`, it defaults to that binding's parent `rise.dev/Organization/<name>`; if omitted on a root-parented `PlatformRoleBinding` with a static Group or ServiceAccount subject, it defaults to that subject's `rise.dev/Organization/<name>`; in every other `PlatformRoleBinding` case it defaults to `"*"`. `labelSelector`, when present, narrows the grant to matching resources inside the Scope. A Role statement's `kinds: "*"` means every qualified ResourceKind inside that domain; only `scope: "*"` reaches both org-contained and root-scoped resources.

A Scope path is a resource URL with only the version segment removed: the target's API group and Kind first, then ancestor names root-first, then its own name. `rise.dev/Environment/acme-corp/app/env-prod` identifies Environment `env-prod` below Project `app`; `rise.dev/Organization/acme-corp` identifies the org; and `rise.dev/RuntimeClass/standard` identifies a root-scoped instance. Thus `/rise.dev/v1alpha1/Project/acme/app` normalizes to `rise.dev/Project/acme/app`. The leaf's qualified ResourceKind deterministically selects its `ResourceDefinition`; ancestor groups and Kinds are derived from that definition's parent chain.

`Scope` is likewise a shared parsed type, never an opaque string. At write time the parser accepts exactly `"*"` or `<api-group>/<Kind>/<names...>`; rejects unknown or unqualified ResourceKinds, empty/extra components, dot segments, query/fragment syntax, embedded separators, and non-canonical encodings; resolves exact `(group, Kind)` through the registry; and verifies following names against the parent chain. The target must exist or be created in the same atomic transaction. The normalized Scope is persisted and compared everywhere.

Two write-time validation rules tie a binding's placement (§3) to its content. **Containment:** an org-parented `RoleBinding`'s `scope` must lie within its own parent org's subtree; a root-parented `PlatformRoleBinding`'s `scope` is unrestricted — `"*"`, any org path, or a root-scoped instance such as `rise.dev/RuntimeClass/gpu-b` (§9). This makes "org admins cannot author platform-wide or cross-org grants" structural, not asserted. **Reference direction:** the structured `roleRef` names its target with separate `kind` and `name` fields — `{ kind: PlatformRole, name: resource-owner }` or `{ kind: Role, name: deploy-viewer }`. An org `RoleBinding` may reference its own org's `Role`s or any `PlatformRole` (how platform-shipped Roles are bound org-locally without duplication); a `PlatformRoleBinding` may reference only `PlatformRole`s — org-authored policy can never escape its org through a platform-wide binding. References are never resolved by bare-name fallback: an org creating a `Role` named `resource-owner` shadows nothing, because every existing `{ kind: PlatformRole, name: resource-owner }` reference keeps meaning exactly that.

**Static** targeting — a fixed subject:

```
subject: group:acme-corp/platform
scope:   rise.dev/Environment/acme-corp/app/env-prod
roleRef: { kind: Role, name: deployment-editor }
```

```
subject:       group:acme-corp/platform
labelSelector: { key: rise.dev/group, value: "platform" }
roleRef:       { kind: Role, name: project-editor }
```

(Role names other than `resource-owner`, §6.2, are illustrative throughout this document — `project-editor`, `deployment-editor`, etc. are examples of Roles an operator or org would define, not literal platform-shipped defaults.)

A `labelSelector` carrying a `value` pairs naturally with a static Subject — an equality filter on an already-fixed grant, no extraction needed. One without a `value` pairs with a dynamic Subject — an existence match whose matched value feeds `${ref.name}` or `${ref.subject}` (below). A static Subject combined with a value-less `labelSelector` is **rejected at write time**: it would grant a fixed subject access to any resource carrying *any* value for that key, regardless of what it says, which is never the intent of a literal, non-templated binding. A dynamic Subject may also use a value-carrying selector: the fixed value is still resolved per matched resource, which matters for org-relative Group references whose canonical organization comes from that resource.

**Dynamic** targeting — the subject is resolved from the matched label's own value at evaluation time. A kind-fixed template interpolates a bare resource name:

```
subject:       group:${ref.name}
labelSelector: { key: rise.dev/squad }
roleRef:       { kind: Role, name: project-editor }
```

The typed form instead requires the label value to carry its User-or-Group kind:

```
subject:       ${ref.subject}
labelSelector: { key: rise.dev/owner }
roleRef:       { kind: PlatformRole, name: resource-owner }
```

Evaluating a dynamic binding first resolves the `labelSelector` against the resource's `effectiveLabels`. `${ref.name}` substitutes the raw value into its binding-declared kind and then parses the concrete subject. `${ref.subject}` parses the value through §6.1's closed `SubjectRef` grammar and canonicalizes it against the matched resource's organization. Both paths finish in the same concrete `SubjectId` parser used for static bindings and fail closed if resolution is invalid or nonexistent. §6.3 covers org-relative resolution.

**Evaluation algorithm**, for subject `S` requesting `(verb, ResourceKind, subresource?)` on resource `r`:

1. Expand `S`'s current Groups and virtual memberships, and determine whether `S` is an org admin in `r`'s org from exact `org-admin` RoleBindings (§5).
2. Collect every binding targeting `S`, an expanded Group/virtual subject, or a template resolving to any of those subjects against `r`, whose Scope, optional `labelSelector`, and normalized `subjectMembership` match. Retain each contribution's binding UID and platform/org placement tier.
3. Apply wildcard replacement (§1): suppress superseded wildcard Allow content but preserve all Deny contributions and their provenance.
4. Filter Denies by tier (§5): operators ignore all; an org admin ignores org-binding Denies in that org; every other principal retains both tiers.
5. Union the surviving statements and permit the qualified tuple iff an Allow matches and no retained Deny matches. Intersect that result with the token authorization-detail ceiling (§7).

A **worked trace**: `group:acme-corp/platform` requests `delete` on `rise.dev/Deployment/acme-corp/app/foo`, which carries `rise.dev/owner: group:platform`.

- Collection finds two bindings: (a) a scope binding at `rise.dev/Project/acme-corp/app` granting `{Allow: * on rise.dev/Deployment}`; (b) the seeded ownership binding, resolving to `group:acme-corp/platform`. No wildcard collision or retained Deny applies.
- The union allows `delete` on `rise.dev/Deployment`, and the caller's token ceiling does not narrow it.
- **Result: allowed.**

Now suppose an operator has separately authored a `PlatformRoleBinding` at
`rise.dev/Project/acme-corp/app` with Role
`{Deny: delete on rise.dev/Environment}`. That statement is unioned into the same
combined policy for Environment resources under that scope. For the Environment
itself, Deny wins and deletion is blocked despite the broader Allow. Narrow
placement alone subtracts nothing; the applicable platform Role must carry the
matching Deny (§5).

**Collection (`list`) authorization and read granularity.** `get` and `list` are two independently evaluated read granularities. A collection request is rooted at the requested scope node and a Kind; its result contains exactly the items of that Kind under the scope that the caller holds `list` on, each independently evaluated through the full §4 algorithm (per-item `effectiveLabels`, wildcard-replacement, Deny-wins union with any applicable cap). It is filtered per-item, never scope-level all-or-nothing. For each included item, the response projector constructs a fresh object from an explicit base-field allowlist: `apiVersion`, `kind`, and `metadata` (name, labels, `effectiveLabels`, timestamps). It must not implement metadata-only output by deleting known fields such as `spec` and `status`, because generic resources may carry arbitrary other top-level fields. If the caller also holds `get` on that individual item, the projector returns the full stored object, including any kind-specific top-level fields; otherwise it returns only the three allowlisted base fields. This permits the common list-and-inspect path to avoid follow-up `get` round trips without letting `list` alone disclose resource data. Items the caller cannot `list` are omitted and their existence is masked: a caller with no applicable `list` grant receives a masked-empty result, not a 403 that would confirm the scope is populated (consistent with §6.6/§9 existence-masking). Note the corollary of returning `effectiveLabels`: because §6.1 resolves *every* label key nearest-wins down the tree, a `list` grant exposes an ancestor's inherited label values (not just `rise.dev/owner`) on the listed children — org-wide by construction, and same-org only since inheritance never crosses the org boundary. An org that puts sensitive metadata in an ancestor label should not grant broad `list` beneath it.

This separates existence/owner visibility from data visibility, each grantable independently per RoleBinding: e.g. `list on Project` granted to `system:authenticated` at `rise.dev/Organization/acme` (auto-clamped to acme members by §1's recipient boundary) lets every acme member see all acme project names and owner labels — resolving name-conflict friction — while `get`/`update`/`delete` stay narrow to owned projects via the ownership binding. Cross-org isolation holds: with no `list` binding on another org's collection, that org's resources are masked entirely. Name-uniqueness is enforced per-parent (an org's Project names are unique within that org, not globally), so a create-conflict can only reveal a sibling's existence within a scope the creator can already `create` in — an intra-scope existence hint, never a cross-org leak (per-parent uniqueness, and no non-operator creates at the root where globally-named kinds live). `list` and `create` are independent verbs, so this hint does not depend on the creator holding `list`.

**Adding a subject to a Group is grant-gated.** Group membership drives direct grants, org membership, and potentially org-admin classification. Adding User `M` to Group `G` therefore computes `M`'s complete effective before/after delta (§5), including transitive `org:<name>` grants and any org Denies that cease to apply if `G` is admin-bound. Otherwise bare Group-update authority would permit self-promotion.

Removing a subject from a Group is not grant-gated. It removes that Group's
grants. If this was the User's last Group tie **and** they have no direct
qualifying org-admin binding, it also removes every org-parented grant and
org-tier Deny because the User is no longer governed as a member of that org.
A separate operator-authored Allow may still reach the non-member, but every
platform Deny remains applicable to the resource independently of membership.
Escaping an org ceiling while retaining a platform grant is therefore an
explicit operator-governance case, not authority the departed org may continue
to control.

**Parents are immutable.** A resource cannot be re-parented through this API: a "move" is a `delete` at the old location plus a `create` at the new one, each independently gated (§5). There is thus no parent-change write for the grant gate to police — the model relies on the store's exact, immutable-parent property.

### 5. Deny tiers, org-admin assignment, and the write-time grant gate

Restrictions apply through the same evaluator, but a Deny's **binding placement**
determines its tier. The referenced Role kind does not: a Deny reached through a
root-parented `PlatformRoleBinding` is platform policy, while a Deny reached
through an org-parented `RoleBinding` is that organization's policy.

The evaluator retains this provenance until Deny filtering:

- a request whose live expansion includes `system:operators` ignores every
  Deny;
- an org admin in the target resource's org ignores Denies contributed by that
  org's `RoleBinding`s, but remains subject to every applicable platform Deny;
- every other principal is subject to both platform and applicable org Denies.

This is the tier contract: an organization may impose a lower ceiling on its
ordinary Users and workloads without limiting its own administrators; the
platform may limit those administrators; operators remain the recovery tier.
It is still one algorithm — filter Deny contributions by caller tier and
binding provenance, then run the ordinary Allow-and-no-Deny evaluator (§3).

**Org-admin status comes from RoleBindings, not a magic Group name.**
`PlatformRole/org-admin` ships with the global baseline:

```
{ effect: Allow, kinds: "*", verbs: "*" }
{ effect: Allow, kinds: "*", verbs: "*", subresources: "*" }
```

The shipped default is Allow-only, but `PlatformRole/org-admin` uses the
ordinary Role schema rather than a special validator. An operator may edit it
to change the global admin baseline for every organization. A Deny added to it
and delivered through a qualifying org `RoleBinding` is org-tier and therefore
ignored by the current admin it establishes; it does not form a ceiling.
Per-org or instance-wide admin ceilings remain operator-authored Denies
delivered through `PlatformRoleBinding`s.

A User is an org admin in Organization `O` iff their live
membership-expanded subjects match an org-parented RoleBinding that:

- is parented under `O`;
- has exact normalized `scope: rise.dev/Organization/O`;
- has no `labelSelector`; and
- has `roleRef: { kind: PlatformRole, name: org-admin }`.

This structural predicate is computed before Deny filtering and is not inferred
from the current contents of the Role. The binding may target the User directly
or any ordinary Group. It is therefore possible to assign one person, a
manually managed Group, or an SSO-synchronized directory Group without a
reserved `org-admins` Group or a second membership system. Listing an org's
admins means listing these exact bindings and expanding their current Groups.

Organization creation is one atomic operator transaction: create the
Organization and one exact, scope-only RoleBinding from
`PlatformRole/org-admin` to an operator-selected existing User. Failure of any
part rolls the whole transaction back. Further admins are added by creating
another qualifying RoleBinding or adding a User to a Group already targeted by
one. Removing the last qualifying relationship revokes admin status live;
operator recovery can always create a replacement binding.

**Role authority follows placement.** The evaluator-guaranteed
`PlatformRole/system-admin` and its `system:operators` binding are seeded,
immutable, and healed if missing. Other PlatformRoles — including
`resource-owner` and `org-admin` — are root resources editable by principals
holding the corresponding root permission, operators by default. Org Roles are
parented under one Organization and editable there. Every edit that changes
effective authority still passes the grant gate below.

**The grant gate compares effective before/after deltas.** For every
authorization-changing write, compute pointwise over the affected domain:

```text
newly_implied =
    EffectivePolicy(recipient, after)
  − EffectivePolicy(recipient, before)

require newly_implied ⊆ EffectivePolicy(writer, before)
```

`EffectivePolicy` includes membership expansion, org-admin classification,
wildcard replacement, binding-tier Deny filtering, Deny-wins evaluation, and
the principal's token authorization-detail ceiling (§7). It is the net set of
allowed `(verb, ResourceKind, subresource?)` tuples over the exact
`Scope ∩ labelSelector` domain, not the raw Allow body of a Role. This is why
an admin covered by a platform Deny can add another admin covered by the same
Deny: denied tuples appear in neither effective policy and are never delegated.
Conversely, promotion to admin may remove org-tier Denies, so that newly exposed
authority is part of the delta and blocks self-promotion by a narrower writer.

The gate covers creating, deleting, or editing Roles and bindings; changing
`subject`, `subjectMembership`, `scope`, `labelSelector`, or `roleRef`; adding
GroupMembership; access-driving label changes (§6.6); and creating or
retargeting identity mappings. Deleting a Deny is a grant because it can enlarge
the effective Allow set. Group removal follows §4's explicit governance-boundary
rule: losing Group ties removes Group-derived grants and, when no direct admin
bootstrap edge remains, the last tie ends the org's authority over that
non-member; any surviving operator-authored access remains governed by platform
Denies.

For a Role edit, the before/after calculation spans every live binding of that
Role. For a binding move it spans old and new domains. Creating or retargeting a
`UserIdentity`, activating a User or UserIdentity, or creating/retargeting a
`ControllerTrustPolicy` or `ServiceAccountTrustPolicy` treats the parent
identity's complete effective policy as newly reachable; external identity
fields and parent references are immutable, so remapping is delete plus create.
Tightening or deleting an identity mapping, including setting `active: false`,
introduces no authority and needs only ordinary write access. Deletion is
nevertheless not durable disablement for a JIT-managed UserIdentity because a
later valid upstream login may provision the pair again (§1, §7).
The §1/§7 JIT login transaction is narrower: an unknown validated external
identity may create only a new generated User and its first UserIdentity, never
attach itself to an existing User. That empty User has no grants unless the
pair is explicitly trusted by restart-loaded `operatorIdentities`, which is the
external bootstrap authority. Linking any secondary identity to an existing
User remains subject to the general grant gate above.

**The check and mutation are one serializable operation.** Every writer that
changes authorization facts — Roles, bindings, GroupMemberships,
identity/trust mappings, or access-driving labels — uses the same Postgres
transaction path at `SERIALIZABLE` isolation with bounded retry. It re-reads
the writer, recipient, relevant memberships, policies, and before/after
resource state inside the transaction. Predicate reads participate, so a
concurrent revocation, cap change, insertion, or membership write either
precedes the check or forces a retry. Ordinary `READ COMMITTED`
check-then-write is forbidden; uniqueness constraints remain the final
authority for duplicate mappings and membership edges.

**The subset comparison is Deny-aware and scope-exact.** It is computed
intensionally over policy domains, never merely over resources that exist now.
A narrow Scope cannot justify a broader grant. Same-key label domains order as
no-selector ⊒ `{key}` ⊒ `{key,value}`; selectors on different keys are
treated as possibly intersecting and fail closed. A union of value-restricted
Allows never covers an unrestricted-selector domain.

The gate is write-time only. If the writer later loses authority, existing
grants remain until their own binding/membership changes or a live Deny removes
their effect. Narrowing the target identity's Role still affects every
outstanding token for that identity (§7).

**Live enforcement permits request-local memoization, not stale policy.** Each
request may build one immutable `AuthorizationSnapshot` containing the typed
principal and UID, token ceiling, expanded Groups, org-admin status, applicable
bindings/Roles, and their provenance. Primary checks, secondary references,
list projection, grant simulation, and explain output may memoize decisions
against that snapshot. No snapshot is reused across requests in the initial
implementation: every request obtains current database facts.

A future cross-request cache requires a transactionally incremented global
`authorization_epoch` on every authorization-changing write and keys entries
by at least principal UID, token-cap hash, epoch, and relevant resource
identity. A global epoch is deliberately preferred first because Role edits and
ancestor-label changes can affect whole subtrees; fine-grained invalidation is
unsafe until proven.

**Restrictions remain transparent.** Explain output retains binding UID and
placement tier for every contribution, including Denies ignored because the
caller is an operator or org admin. Where granted read access, org admins may
also inspect platform bindings covering their org.

### 6. Ownership and attribution

There is no ownership primitive in this model. Nothing in §1–§5 knows what an "owner" is: the evaluator sees subjects, bindings, and labels — nothing more. "Ownership" exists only as the *effect* of one binding (§6.2) that happens to grant owner-like permissions to whoever a label names. Remove that binding and the concept vanishes from the platform without touching the engine; override it (§6.5) and ownership *means something else* in that org. The label is likewise just a convenient, inspectable targeting mechanism: `rise.dev/owner` carries no authorization semantics of its own — no label key does — a key becomes access-relevant exactly when, and only for as long as, some binding's `labelSelector` references it (§6.6 step 2). What this section defines is therefore not an ownership *feature* but a shipped default *convention*: a reserved key, one seeded dynamic binding, and the write-gating that any access-driving label automatically inherits. (A dedicated single-subject `ownerRef` field — a true ownership primitive — was considered and rejected; see Alternatives considered.)

#### 6.1 — Attribution is one governed label

A single reserved key, `rise.dev/owner`, holds a typed `SubjectRef` in one of two forms:

```
user:<name>   # absolute root User, e.g. user:u-01jz…
group:<name>  # Group relative to the matched resource's Organization
```

This is deliberately narrower than `SubjectId`: ServiceAccounts, Controllers, virtual subjects, full cross-org Group identifiers, aliases such as `user:me`, and malformed/unknown kinds are rejected. The relative Group form canonicalizes to `group:<org>/<name>` only during binding evaluation; it fails closed on a root resource with no Organization. UI aliases may be resolved before submission, but persisted values always contain the stable User name or Group name above. `SubjectRef` is generic dynamic-binding input rather than hardcoded owner behavior: any value-less `labelSelector` paired with `subject: ${ref.subject}` receives the same parsing and resolution.

Nested resources without their own value inherit one through `effectiveLabels` — a computed field, always resolved live (never stored or cached, consistent with §5's live-evaluation philosophy — both the read-path display value and the authorization-path match in §4 are the same computation), resolved by walking the already-fetched ancestor chain leaf-to-root, **nearest value wins per key**:

```
Project "secret-app"      rise.dev/owner: group:platform
  └─ Environment "prod"   rise.dev/owner: group:devops # more specific, set later

effectiveLabels for "prod":  { "rise.dev/owner": "group:devops" }
```

A more specific descendant's label shadows its ancestor's; it does not additionally union with it. Restoring broader access on a shadowed resource is always possible — bind another Role at the broader scope — it is simply not automatic. `effectiveLabels` is the one ancestor-inheritance mechanism in the system; ownership reuses it rather than maintaining a parallel one.

#### 6.2 — The default ownership rule

One platform-seeded dynamic binding replaces any implicit "you can act on what you own" logic:

```
subject:       ${ref.subject}
subjectMembership: ResourceOrganization
labelSelector: { key: rise.dev/owner }
roleRef:       { kind: PlatformRole, name: resource-owner }
scope:         "*"
```

`resource-owner` is a literal platform-shipped `PlatformRole`, defined as:

```
resource-owner = { Allow: [get, list, update, delete] on * }
```

— deliberately excluding `create` and every subresource. Ownership alone never grants the ability to update `/status` or `/finalizers`, create a token for an owned ServiceAccount, or create new child resources; those require a separately-granted Role. Both `resource-owner` and `org-admin` are ordinary operator-editable PlatformRoles with shipped defaults; changing `org-admin` changes the global admin baseline, while scoped platform Denies provide per-org ceilings (§5).

Unlike the operator and org-admin tiers, the org-*user* tier ships no baseline `create`-granting Role: ordinary org-user access beyond ownership is configured through org-admin delegation, and `resource-owner` intentionally omits `create`.

When the resolved subject is the caller, self-ownership falls out without a separate condition type, but `subjectMembership: ResourceOrganization` keeps that access live only while the User remains affiliated with the resource's org. An explicit static PlatformRoleBinding with `subjectMembership: Any` is the deliberate operator escape hatch for non-member access.

#### 6.3 — Resolving a dynamic subject's organization

A dynamic binding's `subject` template has no concrete identity until it is evaluated against a specific resource. For `group:${ref.name}`, and for a `group:<name>` value consumed by `${ref.subject}`, the resolved Group's organization is **the matched resource's own organization**. Thus `rise.dev/owner: group:platform` on an acme resource resolves to `group:acme-corp/platform`, never a `platform`-named Group elsewhere. `user:<name>` remains root-absolute.

The binding's own `scope` governs which resources consider the rule, not the resolved Group's organization, which is derived per resource. `subjectMembership: ResourceOrganization` then constrains a resolved User against that same org. This is why §6.2's binding can validly carry a `labelSelector` and `scope: "*"` without becoming a cross-org User grant. On a root-scoped resource, an org-relative Group reference still has no organization and fails closed; the membership constraint itself is simply a no-op because there is no resource Organization, and never defaults to an arbitrary org.

#### 6.4 — Individual ownership and organization-specific grouping need no new subject kind

Subject kind stays closed (User, Group, ServiceAccount, Controller — each carries real membership-resolution machinery, not worth making pluggable). Label *keys* are open — any organization can introduce one:

```
# individual ownership — same mechanism, a different kind and key
subject:       user:${ref.name}
labelSelector: { key: rise.dev/assignee }
roleRef:       { kind: PlatformRole, name: resource-owner }

# an org's own grouping concept — reuses Group, never registers a new kind
subject:       group:${ref.name}
labelSelector: { key: rise.dev/squad }
roleRef:       { kind: Role, name: project-editor }
```

A "squad" never exists as a subject kind — it is a Group, targeted via a label key the organization chose to call `rise.dev/squad`. This covers grouping concepts whose *membership* is ordinary Group membership; it does not provide a way to define a group with genuinely different membership semantics (externally-synced, rotation-based, non-exclusive overlapping groups, etc.) — that would require a real pluggable subject-kind registry, which is deliberately out of scope (Alternatives considered).

#### 6.5 — Organizations can override the default

The seeded ownership binding is ordinary `scope: "*"` data. §1's wildcard-replace rule governs overrides the same way it governs any other wildcard — an org-specific binding for the same `(subject, labelSelector key)` pair replaces the platform default outright for that org:

```
subject:       ${ref.subject}
labelSelector: { key: rise.dev/owner }
roleRef:       { kind: Role, name: project-viewer } # read-only ownership; an org-parented RoleBinding
scope:         rise.dev/Organization/acme-corp
```

The override write still passes the ordinary write-time grant gate (§5) — no override-specific mechanism.

This override works because the default uses wildcard `scope: "*"`; two non-wildcard bindings union rather than replace. An org may narrow ordinary subjects below an existing non-wildcard Allow with an explicit org-tier Deny. Current org admins ignore that Deny, while a platform-tier Deny still limits them (§5).

#### 6.6 — Label writes that retarget access are gated by the write-time grant gate itself

There is no hardcoded list of protected fields. On any write — creation or update — that sets or changes `metadata.labels[K]`:

1. If the value for `K` is unchanged from the resource's current effective value, no gate — ordinary `update` permission suffices.
2. If no binding *anywhere applicable to this location in the tree* (by Scope and ResourceKind, regardless of whether it currently matches this resource's present labels) selects on `K` via its `labelSelector`, no gate. This check is evaluated against binding applicability, not the resource's pre-write label state.
3. Otherwise, resolve effective permissions before and after the proposed value and diff them — where "the value" is the `effectiveLabels`-*resolved* ownership (§6.1), inherited ancestor values included and nearest-wins applied, not the resource's own stored label read in isolation. *Removing* an access-driving label is a "change" like any other and is gated: dropping a child's own `rise.dev/owner` makes it inherit an ancestor's owner via nearest-wins — an escalation if that ancestor names the writer's group, which a diff over the resource's own stored label would misread as `victim → absent` and wave through as de-escalation. The diff is simulated, computed atomically with the write so a concurrent binding change cannot open a window between simulation and commit. The newly-implied grant is computed over *all* subjects any selecting binding resolves to before and after — not only the writer's own access — and each such grant must be `⊆` the writer's own current effective permissions — §5's general write-time grant gate, applied here. Moreover, the before/after diff spans not only `r` but every resource that inherits `r`'s value for `K` through `effectiveLabels` (§6.1) — `r`'s `K`-inheriting subtree — since relabeling `r` can newly grant access over descendants that inherit the changed value.

A key becomes gated the moment some binding's `labelSelector` references it, and stays ungated otherwise: protection is a consequence of binding existence, never a hardcoded field name.

*Implementation note:* the subtree diff (step 3) is a cold path, implementable via a recursive `parent_uid` query; its atomicity is covered by §5's existing write-consistency requirement for the grant gate, needing no §6.6-specific mechanism.

**A narrow, explicit exception applies at creation.** A subject holding `create` on a kind may, in that same creation request, set an access-driving label only when every selecting binding resolves its value to the creator's own canonical User subject or to a Group they currently belong to (itself an ordinary grant-gated fact — joining a Group is its own gated write, not something a creator can manufacture on the fly to widen this exception). For the seeded owner binding, this means `rise.dev/owner: user:<caller's-stable-name>` or `rise.dev/owner: group:<one-of-the-caller's-groups>`. No immediate RoleBinding is created: the already-seeded dynamic binding applies to the newly persisted label in the same transaction and makes the owner grant effective immediately. This is not displacement because the resource has no prior owner. A different User or a Group the creator does not belong to falls back to the general subset rule. If recipient constraints make the proposed policy a no-op, its effective grant delta is empty and the write is safe to persist; if it grants authority, the creator must independently hold the complete authority being granted.

"Creation" here means bringing a genuinely new, previously-nonexistent resource identity into being — never a write that targets an identity that already exists in the store, even one currently soft-deleted or otherwise inactive. Restoring a soft-deleted resource, or an upsert-style write that would create-or-update depending on whether the target already exists, is **not** creation for this exception's purposes and is unconditionally subject to the general rule instead: an implementer must resolve "does this identity already exist" before deciding whether the exception can apply, exactly because the exception's own safety rests on there being no prior owner to displace — which is only true for a genuinely new identity. The exception applies exactly once, under that definition — every later write to the same label, including the very next `update`, is unconditionally subject to the general rule above.

When the value resolves through *multiple* applicable templated bindings on the key, the exception applies only if *every concrete SubjectId* is the creator or one of their Groups; if any resolved subject is not claimable, the general subset rule governs the whole write. A `delete`+`create` sequence that reclaims a freed resource name is the sanctioned move primitive (§4), not an ownership-takeover flaw: reclaiming a name follows from holding `delete`+`create` on that scope — a permission-configuration decision — and defending against it would break "move."

The check is a genuine subset comparison, not merely "does this write avoid dropping access to zero." An editor with no independent claim to `resource-owner` could relabel `rise.dev/owner: group:platform → group:their-own-group` without ever dropping the resource's access to zero — they would simply redirect it to themselves. The subset check blocks this; a caller who currently holds the role being handed off (the resource's actual current owner, or an org-admin whose access is independent of any label, §6.7) passes trivially, so legitimate transfers are unaffected.

Referential-integrity validation (§6.7) runs only *after* this gate passes — a caller who would be denied by this check never learns whether the value they attempted resolves to a real Group/User, avoiding turning the validation step into an unauthenticated existence oracle.

#### 6.7 — Orphan prevention is separate from escalation prevention

*Escalation* — an unauthorized party redirecting access to themselves — is §6.6's job. *Orphaning* — a legitimate write accidentally locking everyone out, typically a typo — needs two different mechanisms:

- **Referential integrity at write time.** A value written to a label some binding selects on must parse through that binding's declared template and resolve to every required concrete subject. For `${ref.subject}`, that means an existing User or same-org Group. Invalid kind prefixes and nonexistent subjects are rejected synchronously, with an authorized fuzzy-match suggestion where appropriate. Current membership is deliberately not a validity condition: a foreign or group-less User may be recorded as an owner while `ResourceOrganization` makes the binding grant nothing, and a later membership change may make it effective only through the ordinary serializable grant gate.
- **Admin access stays independently derived, enforced structurally.** `PlatformRole/org-admin` may only be referenced by an exact org-root, scope-only RoleBinding, never a `labelSelector` binding (§5). Admin status therefore cannot depend on an access-driving label, and an operator can recover an orphaned resource without any magic Group name.

Semantically inert policy is an auditing concern, not a write-time validity error. A future Role/policy auditing workflow should flag ownership labels that currently grant nobody, selectors that match no resources, recipient-boundary or `subjectMembership` combinations that are no-ops, stale references, and Allows shadowed by replacement or Deny. Explain output must show why such data contributes no effective tuple. The synchronous mutation path retains only the checks needed for safe interpretation and non-escalation: closed-schema parsing, referential integrity, structural scope/placement constraints, and the effective-delta grant gate.

### 7. Rise-issued identities and token issuance

Authentication proves which known Rise identity a credential represents;
authorization decides what that identity may do. The engine accepts only:

```text
AuthenticatedPrincipal {
  subject: SubjectId,
  subject_uid: ResourceUid,
  provenance: AuthenticationProvenance,
  actor: optional ActorChain,
  authorization_cap: AuthorizationCap
}
```

Every adapter validates signature, issuer, audience, time bounds, and credential
type, then resolves `(sub, rise_uid)` to the same live, active User,
ServiceAccount, or Controller. Group and virtual subjects can never be token
principals. Operator status is derived only after User authentication. For a
Rise-issued principal, unknown, deleted, inactive, malformed, or UID-mismatched
identities fail before authz; §1's JIT rule separately governs a validated
upstream interactive login whose mapping is absent.

**There are three issuance flows.**

1. **Interactive User login.** Rise acts as an OIDC relying party, validates the
   upstream identity, and resolves its exact
   `UserIdentity.spec.(issuer, subject)`, including inactive live mappings. An
   inactive identity or inactive parent User fails without JIT. If no live
   mapping exists, the fixed JIT transaction from §1 creates a new generated
   User plus that first identity and converges concurrent attempts through the
   unique mapping constraint, even if a prior mapping for the pair was deleted.
   It never links to an existing User implicitly. Rise then issues a session for
   the parent User UID and derives operator membership from any of that User's
   live, active identities, not only the credential used for this login. Provider adapters may ingest
   verified profile email, but email never replaces `(issuer, subject)` as the
   authoritative key or links accounts.
2. **Workload token exchange.** An external workload JWT is accepted only at
   the intended ServiceAccount or Controller's `/token` subresource. The URL
   supplies the target; Rise checks only trust-policy children of that exact
   target and issues a token for exactly that target. This is authentication as
   the workload's configured Rise identity, so it has no RBAC token-create
   check and never performs a global source-identity search.
3. **Delegated token issuance.** An already Rise-authenticated User,
   ServiceAccount, or Controller may create another target's `/token` only
   when its current EffectivePolicy contains
   `(create, target ResourceKind, token)` on that exact target. No external
   assertion or target trust-policy check participates.

The two `/token` modes are disjoint: a request supplies either an external
subject assertion for workload token exchange or a Rise bearer for delegated
issuance, never both. A trust policy may not name Rise's issuer as an external
source. External workload assertions are rejected by every ordinary endpoint.

Workload-exchange failures **after entering a registered ServiceAccount or
Controller `/token` route** are deliberately indistinguishable. A nonexistent,
soft-deleted, or disabled target; a UID-addressed route that resolves to the
wrong kind; zero or multiple matching trust policies; and invalid
issuer/audience/signature/claims all return the same coarse authentication
failure and never reveal whether the named identity or policy exists. A kind
that does not register `token` — for example Deployment — has no such route and
returns the ordinary route-not-found response before authentication. Target
resolution is part of authentication for a valid workload-exchange route, even
though ordinary delegated requests resolve their parent after authentication.

`create`, rather than `get`, is intentional for delegated issuance: it is a
non-idempotent operation returning a new credential without persisting a Token
row. A Controller is root-scoped, so its token-create grant must reach that root
resource through a PlatformRoleBinding. A ServiceAccount is org-native and may
be reached by an org RoleBinding.

**Delegation may chain only across explicit grants.** A delegated token
exercises the target's live EffectivePolicy, not the caller's, so token-create
is intentionally elevation-capable. The target may mint again only if it
itself holds token-create on the next target. Every delegated token records a
bounded nested `act` chain for audit; actor data never grants access, and an
issuance exceeding the platform chain-length limit is rejected.

**Structured authorization details form a signed Allow ceiling.** A narrowed
Rise token uses the RFC 9396 `authorization_details` claim. Each entry has
exactly one qualified Scope and one or more permission statements:

```json
{
  "authorization_details": [
    {
      "type": "rise.dev/rbac",
      "scope": "rise.dev/Project/acme/app",
      "permissions": [
        {
          "verbs": ["get", "list"],
          "kinds": ["rise.dev/Deployment"]
        }
      ]
    },
    {
      "type": "rise.dev/rbac",
      "scope": "rise.dev/Project/acme/catalog",
      "permissions": [
        {
          "verbs": ["get"],
          "kinds": ["rise.dev/Deployment"],
          "subresources": ["status"]
        }
      ]
    }
  ]
}
```

Entries union with each other; statements within one entry union over that
entry's singular Scope. The resulting union is an Allow ceiling, never a
grant:

```text
token EffectivePolicy = live RBAC ∩ union(rise.dev/rbac details)
```

Permission statements reuse Role grammar exactly. Omitted `subresources`
means the main resource only; a non-empty list names only those subresources;
`"*"` means every registered subresource but not the main resource. An empty
`scope`, `verbs`, `kinds`, or `subresources` list is invalid rather than
a no-op or wildcard. Qualified ResourceKind and Scope parsers are shared with
policy data. Duplicate entries are allowed because distinct Scopes may carry
different permissions; malformed entries and unknown authorization-detail
types are rejected for a Rise API token rather than ignored.

Omitted `authorization_details` on an internally issued token means the full
live target policy. A present but invalid/empty detail set never falls back to
full access. The parsed union travels as `AuthorizationCap` on
`AuthenticatedPrincipal` and participates in every primary and secondary
decision, list projection, reference check, label/grant gate, and explain
result. `aud` remains the separate standard claim controlling where the
credential is accepted.

Both workload exchange and delegated issuance may accept the same
`authorization_details` request structure. It can only narrow the issued
target token. For delegated issuance, the caller's current capped
EffectivePolicy must authorize token creation, but the child does not silently
inherit the caller's cap: token-create is the explicit delegation boundary and
the requested child details constrain the target.

Tokens carry identity and a ceiling, never a snapshot of grants. Every request
re-resolves the target identity, Groups, Roles, bindings, and Denies. Narrowing
or deleting the target affects outstanding tokens immediately; recreating the
same name under a new UID does not revive them. Revoking a caller's token-create
grant stops new issuance but does not revoke already-issued target tokens.
Tokens remain short-lived under one platform-global maximum TTL.

### 8. One canonical qualified ResourceKind — no plural forms

A resource kind has one qualified identity: `<api-group>/<Kind>`, for example `rise.dev/Deployment`. Role statements and authorization details use that exact `ResourceKind`; Scope paths start with it; reference declarations store the same `(group, Kind)` pair. HTTP retains the served version between group and Kind: `{group}/{version}/{Kind}/{ancestor…}/{name}`. `ResourceDefinition` no longer declares a plural. This preserves one Kind vocabulary while preventing two API groups' same-named Kinds from colliding in authorization. Version conversion cannot change authority because all served versions normalize to the same ResourceKind.

### 9. References to platform-provided resources

> **Deferred.** This section describes a designed but **deferred** capability — platform-provided *selectable* resources (e.g. `RuntimeClass`). It is not part of the initial model or its conformance suite, and §5–§7 do not depend on it (it excises cleanly). The `use` verb (§2) and the `references:` `ResourceDefinition` declaration are retained now as reserved vocabulary to avoid a later schema migration. Deferred, to be decided and implemented as a tracked follow-up: reference materialization at deployment creation, the per-org `use`-against-consuming-resource's-org check, and the default-label owner/admin-tier write gate — together with the deferred feasibility items (the `at:` reference-path grammar, and restricting declared references to root-scoped platform-provided referent kinds).

Some resources exist to be *referenced* rather than contained: a platform-level `RuntimeClass` (root-scoped, operator-managed) describes how project deployments are reconciled, and organizations select one rather than own one. Some classes are for every org; others are provisioned for one specific customer. The interesting permission is not CRUD on the class — that stays operator-only by ordinary default-deny — but who may *select* it.

**Reference declarations.** A `ResourceDefinition` may declare that a field (or label key) of its kind references another kind:

```
references:
  - at:        spec.runtimeClass     # a field path or a label key
    kind:      rise.dev/RuntimeClass
    verb:      use
```

Declared once at kind registration, as data — the same family as `ResourceDefinition`-declared subresources (§2), never per-field engine code. Any write that sets or changes a declared reference additionally requires the writer to hold `use` (§2) on the *referenced instance*, evaluated by the ordinary algorithm (§4). An unchanged value on a later write is not re-checked (same rule as §6.6 step 1), and the check runs before existence disclosure (same ordering as §6.6/§6.7): a writer without `use` cannot probe whether a class exists.

**Availability is instance-targeted bindings.** A root-scoped instance is a node in the tree, so §4's `Scope` targets it with nothing new:

```
# everyone may use the standard class
subject: system:authenticated
scope:   rise.dev/RuntimeClass/standard
roleRef: { kind: PlatformRole, name: rc-user }                   # PlatformRoleBindings (§3)

# gpu-b is provisioned for acme-corp only
subject: org:acme-corp
scope:   rise.dev/RuntimeClass/gpu-b
roleRef: { kind: PlatformRole, name: rc-user }
```

Here `PlatformRole/rc-user = { Allow: use on rise.dev/RuntimeClass }`.

Multiple orgs → one binding each: explicit and auditable. "Org A cannot select org B's class" is not a rule anyone writes — it is the *absence of a grant*: org A's subjects hold no `use` binding on `gpu-b`, default-deny (§4 step 3) rejects the write without confirming the class exists, and org A cannot self-serve the grant — a binding whose `Scope` reaches a root-scoped instance must be a `PlatformRoleBinding` (§4's containment rule; org-parented `RoleBinding`s cannot leave their org's subtree), only operators can create those, and the write-time grant gate's subset check independently blocks handing out `use` they don't hold.

**Per-org `use` is checked against the consuming resource's org.** For a platform resource provisioned to one org (e.g. `gpu-b` granted only to `org:acme-corp`), the `use` grant is evaluated against the *consuming* resource's organization, not solely the acting subject's group membership. A `use` grant addressed to `org:acme-corp` authorizes selection only from resources within `acme-corp`'s subtree: a `User` who is a live member of both `acme-corp` and `beta-corp` cannot select `acme-corp`'s private `gpu-b` while deploying into `beta-corp`, because the resource being written lives under `beta-corp` and no `use` binding grants `gpu-b` there. Checking only the subject's own membership would break §9's cross-org isolation invariant. A consuming resource with no organization (a root-scoped resource) lies within no per-org subtree, so a per-org `use` grant simply does not apply to it — fail closed, never falling back to the acting subject's membership; instance-wide grants (`system:authenticated`) are unaffected since they are not per-org.

**Defaults are product data, not permission data.** Nothing product-specific accretes onto the RBAC core resources — Roles, RoleBindings, and cap `Deny`s stay purely authorization data (§5). The global default is a label on the class itself — `runtimeclass.rise.dev/is-default: "true"`, operator-writable because the class is operator-owned (the same pattern as Kubernetes' `storageclass.kubernetes.io/is-default-class`). Org- and Project-level overrides are a label on the Organization or Project (`runtimeclass.rise.dev/default: gpu-b`), and the override cascade — Deployment-explicit → Project → Organization → global — is `effectiveLabels`' nearest-wins walk (§6.1), with no new inheritance machinery. The default label key is itself covered by a reference declaration, so an org-admin setting their org's default is `use`-checked like anyone else — an org cannot default itself onto a class it was never granted. Beyond the `use` check, *writing* a reserved default label is gated to the owner/admin tier of the resource it is set *on*: an org-level default (`runtimeclass.rise.dev/default` on an `Organization`) requires org-admin or owner of that org, and a project-level default requires the Project's own owner — not the bare `update` an ordinary project editor holds. Otherwise a project editor holding only `update` could steer co-tenants' workload placement by rewriting the inherited default.

**Materialization at deployment creation.** When a deployment is created, the effective class is resolved once and written onto the Deployment as its own concrete value; that materializing write is a reference write, `use`-checked against **the deployer** — the User or ServiceAccount driving the deployment. This is why `org:<name>` includes ServiceAccounts (§1): CI-driven deploys must pass exactly where a human's would. The reconciler then reads only the materialized field and never evaluates `use` at all — every `use` check in the system has a well-defined, present subject. (Precedent: Kubernetes' DefaultStorageClass admission stamps the default `storageClassName` onto a PVC at create time.)

This deliberately gives the reference *snapshot* semantics, not §6.1's live semantics: the never-store rule exists for access-driving labels, where staleness is a security bug, whereas here the recorded value is the *output* of a decision made at a specific moment by a specific subject, and reproducibility is the point. The org's default label remains live as an *input* to the next deployment. Revoking an org's `use` grant therefore stops the *next* deployment, never a running one — consistent with the write-time grant gate applying at write time everywhere else (§5), and the right availability call: a revoked class ages out at the org's next deploy or rollback (which creates a new deployment and re-resolves against current grants).

**Boundary.** Org-admins cannot sub-delegate or per-instance-restrict `use` of
platform-provided resources inside their org — those grants and ceilings are
`PlatformRoleBinding`s, outside org authorship by placement (§3, §4). Their
local lever is the org default label; finer org-side restriction would need
resource admission policy, which is out of scope (§10).

### 10. Explicitly out of scope

- Org-registrable Controllers/ResourceDefinitions — falls out for free once registration is just another grant-gated verb, not designed now.
- Migrating today's typed-table-backed APIs (`Project`, `User`, existing `Team`, `ServiceAccount`, `Deployment`, …) onto this model — happens separately. Existing Teams become `Group` resources and `team_members` become `GroupMembership`; ServiceAccounts move from Project to Organization placement and cease masquerading as synthetic Users.
- Ingress-level authentication for a deployed application's own end users — a different problem domain entirely.
- A pluggable subject-kind registry letting organizations define groups with custom membership semantics (§6.4) — organization-specific *naming* of a grouping concept is supported today by pairing an existing kind with an organization-chosen label key; genuinely custom membership resolution is not, and would need a larger extension to the closed subject-kind list.
- A first-class cross-org sharing primitive — a deliberate grant reaching subjects of another org. The recipient boundary (§1) bans cross-org sharing through org bindings by construction; operator-authored `PlatformRoleBinding`s are the only cross-org grant path today. A tenant-authorable sharing mechanism is deferred; nothing here forecloses it.
- Platform-provided *selectable* resources and reference materialization (§9) — designed but deferred. The `use` verb (§2) and the `references:` declaration ship as reserved vocabulary, but reference materialization at deployment creation, the per-org `use`-against-consuming-resource's-org check, the default-label owner/admin-tier write gate, and the remaining feasibility items (the `at:` reference-path grammar, restricting referents to root-scoped platform-provided kinds) are a tracked follow-up, not part of the initial model.
- Resource admission policies — org- or operator-authored rules constraining what may be written below a given scope (e.g. required labels, or per-instance restriction of which platform resources an org's own subjects may reference, §9). A future mechanism; nothing here forecloses it.
- Concrete streaming, connection, proxy, and virtual-object subresource
  contracts (`logs`, `proxy`, `scale`, and similar). This ADR fixes their
  authorization tuple and the shared registration seam, but their handler
  interfaces, transport semantics, and response types are drafted in
  [ADR-0002](./0002-generic-resource-subresource-execution-model.md) (§3).
- Extending the `token` subresource to the `User` kind — user self-service
  personal tokens, operator-delegated minting on behalf of a user, and exposing
  non-interactive external-assertion→Rise-token exchange (RFC 8693) for users
  through it. The interactive browser login flow stays separate: Rise remains an
  OIDC relying party (the upstream IdP is the authorization server), and
  the token-issuance
  logic is shared as one issuance core (`rise-backend-auth`) that both the login
  callback and `/token` call, rather than routing interactive login through the
  `/token` endpoint. Deferred with two hazards to design first: because `User` is root-scoped,
  `(create, rise.dev/User, token)` is grantable only by a `PlatformRoleBinding` (§4
  containment) — operator-only, so an org-admin structurally cannot grant it,
  and delegated minting would otherwise hand out a target user's *cross-org*
  reach unless the minted token is clamped to the minter's own scope authority;
  and self-service minting still needs a defined provisioning convention for a
  typed self-reference on each root User plus a narrowly-scoped dynamic token
  binding. `${ref.subject}` can express that relationship, but no such label or
  binding is shipped here, and a User cannot bootstrap a protected label on
  their already-existing root identity themselves. This deferral does **not** affect SSO
  login, which is authentication — a User's own external credential mapped to a
  live `UserIdentity` (§7), gated by that trust mapping, never by an RBAC
  `(create, rise.dev/User, token)` grant. Workload token exchange similarly
  authenticates as its configured target without RBAC; delegated issuance is
  the distinct token-create-gated mode (§7). Any future
  unification onto `token` must keep that self-authentication leg
  trust-policy-gated, not RBAC-gated, to avoid a login bootstrap paradox.

## Consequences

**Positive.**

- One evaluator decides access for every subject kind — Users, Groups,
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
- Restrictions and grants use one statement algebra while retaining binding
  provenance: platform Denies limit org admins, org Denies limit ordinary org
  subjects, and the grant gate compares net EffectivePolicy deltas (§5).
- Revocation is live across requests: Denies and memberships are re-resolved on every
  request, so tightening a cap or narrowing an identity's Role takes
  effect immediately — including for every outstanding token of that identity
  — while one request may safely memoize its AuthorizationSnapshot (§5, §7).
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
- Cap tightening has no dry-run/impact-preview — an operator can strand
  subjects with no warning before committing the write (§5).
- A cap can `Deny` specific `(verb, ResourceKind, subresource?)` tuples but cannot restrict an org to a
  *whitelist* of kinds: the kind space is open-ended (new kinds register at
  runtime), so "only these kinds, nothing else" has no faithful `Deny`
  encoding — the same open-kind problem §3 solves for grants. Verb caps (the
  real use case — "no token-create", "no `delete` in `prod`") are unaffected
  (§3).
- There is no token-revocation list. Responding to a compromised *minting
  caller* means acting on the target identity's own grants or waiting out the
  TTL (§7).
- Token-create can deliberately form delegation chains. Each edge is an
  explicit live RBAC grant and the actor chain is audited, but reviewers must
  reason about the transitive reach of identities allowed to mint (§7).
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
  platform-provided resources inside their org; their local lever is the org
  default label until admission policies exist (§9, §10).
- The resource-API RBAC items in `ROADMAP.md` (and everything sequenced on
  them) are to be planned against this model.

## Alternatives considered

- **Pure-additive, union-only permission sets, no Deny.** Cannot express subtraction from a wildcard over an open-ended ResourceKind space. Rejected for tiered platform/org Denies (§3–§5).
- **Folding ownership into wildcard statements covering both main resources and every subresource**, rather than a distinct owner Role. Would silently over-grant: an owner would automatically gain token creation and finalizer updates alongside ordinary access, defeating the deliberate subresource separation in §2. Rejected in favor of the named `resource-owner` Role with the explicit main-resource verb list pinned down in §6.2 (`get`/`list`/`update`/`delete` only).
- **A dedicated single-subject `ownerRef` field as a separate ownership mechanism alongside the Role/binding model**, inherited down the parent chain as a union. Two independent inheritance and authorization mechanisms complicate explanations, and a descendant could never fully exclude an ancestor's owner. Rejected; §6 expresses ownership with Role/RoleBinding/label primitives, while an operator-authored platform Deny can still enforce a narrower hard restriction (§4, §5).
- **Labels driving RBAC directly, with no write-gate on the label itself.** Ordinary `update` access on a resource would let any editor silently redirect which subject holds a derived Role — an ungated escalation path. Rejected in favor of §6.6's binding-triggered write-time grant gate, which in turn is one instance of §5's general rule that *every* write changing effective access — including RoleBinding and Role edits, not only labels — passes through the same check.
- **A dedicated bespoke verb per protected field** (e.g. `setGroupLabel`), rather than a generic mechanism. Every newly-sensitive field would need a new verb and new engine code. Rejected in favor of §6.6, where protection follows from a `labelSelector` binding's existence.
- **Gating label writes on "does not drop access to zero"** rather than the standard subset check. Defends availability only — it never checks *who* gains access, only that the total doesn't hit zero — so it would still permit an unauthorized party to redirect access to themselves. Rejected in favor of the genuine subset comparison in §6.6.
- **Applying org-authored Denies to org admins.** Rejected because an org admin
  must retain the global admin baseline up to platform policy. Org Denies limit
  ordinary members and workloads; binding placement lets current admins ignore
  them without weakening platform Denies (§5).
- **A cap-tightening (or wildcard-replacement) dry-run/impact-preview warning.** Would require simulating the write's effect across every subject with a live binding under the tightened rule before committing it — expensive and stateful in a way the rest of the write path deliberately isn't, and it doesn't integrate cleanly into a generic REST write path. Deferred; the footgun is accepted, not solved, for now (§1, §5).
- **An open, pluggable subject-kind registry**, to let organizations define arbitrary group types (e.g. "squad") with their own membership resolution. Subject kind carries real infrastructure (membership resolution, org-native-vs-agnostic encoding, token-create semantics) not worth making pluggable. Rejected; §6.4 shows organization-specific *naming* of a grouping concept is expressible by pairing an existing kind (Group) with an organization-chosen label key — genuinely custom membership resolution is a separate, larger ask this does not address, and remains out of scope (§10).
- **Clamping a minted token's scope to the calling subject's own permissions.** Forecloses a legitimate privilege-elevation pattern — a low-privilege, long-lived caller minting a token for a higher-privilege, short-lived ServiceAccount, the same shape as AWS STS `AssumeRole`. Rejected; §7 gates *who may mint*, not what the minted token may then do.
- **A single global namespace for Role names, with no placement.** Any org editing any Role by name would make cross-org authority attribution ambiguous the moment a Role is bound in more than one org — against whose permissions is an edit checked? Rejected in favor of placement-derived authority (§3, §5): `PlatformRole` (root-parented, operator-editable, bindable by any org) vs. `Role` (org-parented, org-editable, bindable only from its own org) — editing a Role always has exactly one unambiguous parent org to check the editor's permissions against, fixed by its parent.
- **A variable parent kind for policy objects** ("parented at root *or* under an Organization"), instead of two kind pairs. The store's exact-parent model is load-bearing: ancestor kinds in URLs and `Scope` paths derive deterministically from the leaf kind's single parent chain, and union parents reintroduce path ambiguity in the general case. Rejected; two same-shaped kind pairs (§3), the same fork Kubernetes resolves with `ClusterRole`/`Role` — an enum entry is cheaper than an invariant.
- **A reserved "platform organization" holding platform-level Roles/bindings**, keeping one kind pair. The `Scope`-containment rule (§4) would immediately need an exception — platform bindings carry `scope: "*"`, which no org subtree contains — and an org that isn't a tenant is a modeling smell, not a simplification. Rejected in favor of root placement (§3).
- **Bare-name Role references with org-local-then-platform fallback resolution**, instead of structured, kind-qualified references. An org creating a `Role` named `resource-owner` would shadow the platform Role and silently retarget any later binding written with the bare name — the same one-unambiguous-answer failure mode §1's wildcard rules exist to prevent. Rejected; `roleRef` always names both the target `kind` and `name` (§4).
- **Multiple stored subjects on one RoleBinding**, as Kubernetes supports. This saves duplicate binding objects, but subject identity participates in Rise's wildcard replacement, dynamic resolution, org-recipient validation, grant-gating, and audit explanations. Adding or removing one entry would partially mutate a binding, and an org-specific collision might replace a wildcard for only a subset of its subjects — complexity Kubernetes' additive-only model does not face. Rejected for the initial model in favor of one subject per binding and Group/`org:` groups for populations; a future `subjects:` input may be pure syntactic sugar expanded into independent bindings (§4).
- **Email address as `User.metadata.name`**, requiring `@` in the resource-name grammar. Email is mutable, awkwardly normalized, may repeat across issuers, and would make an authentication attribute the stable authorization key; admitting it also weakens a path grammar shared by every kind. Rejected for generated DNS-safe immutable User names plus dedicated `(issuer, subject)` UserIdentity resources; UI/CLI translates to presentation fields (§1).
- **Embedding SSO mappings, trust policies, or member arrays in the parent identity's `spec`.** This makes independently governed security edges share one revision and turns membership changes into whole-object rewrites; one generic TrustPolicy child kind also cannot have both Controller and ServiceAccount parents under the exact-parent model. Rejected for dedicated, fixed-parent UserIdentity, GroupMembership, ControllerTrustPolicy, and ServiceAccountTrustPolicy resources (§1).
- **A separate `authorization.rise.dev` API group for subject resources.** Adds qualification/versioning vocabulary while subjects and `roleRef` intentionally rely on one reserved built-in role/identity domain. Rejected; these built-ins use `rise.dev/v1alpha1`, and custom same-named kinds in other groups never participate in SubjectId resolution or built-in indexes (§1).
- **A special one-hop token rule.** Rejected because each delegated mint already
  has an explicit `(create, target ResourceKind, token)` authorization edge. It would
  make a valid Rise-issued workload identity behave differently from a User and
  prevent intentional automation chains. Bounded `act` chains provide audit;
  RBAC remains the authority (§7).
- **Applying the general subset check to owner-label writes at creation with no exception.** Would mean a subject holding only `create` on a kind could never become the resulting resource's owner, since ownership (`resource-owner`) is strictly more than `create` alone implies — breaking the single most common operation the model exists to support. Rejected in favor of the narrow, membership-bounded creation-time exception in §6.6, which only ever lets a creator name themselves or a group they already belong to, never an arbitrary third party.
- **Permitting a workload trust policy to accept Rise's own issuer as an
  external source.** Rejected because Rise-issued callers use delegated mode;
  accepting the same credential through workload exchange creates two competing
  authorization paths for one request (§7).
- **A general `fields:` include/exclude axis on Role statements**, replacing named subresources (and potentially §6.6's label-write gate) with field-path matching on the ordinary `update` verb. Rejected on both counts. Folding in §6.6 doesn't work at all: its gate depends on whether *some other, unrelated binding* currently exists (a live property of the whole binding table, not data a Role statement can carry) and on whether a *value* changed, not merely which *path* was touched — properties no static field syntax can express, and the diff computation such a fold would require is vacuous everywhere except labels, since nothing else in this model resolves a Subject off a field value. Replacing `status`/`finalizers` subresources does not work either: it destroys `resource-owner`'s secure-by-default-via-omission property (§6.2) and introduces an ambiguous, security-critical path-containment language into the write-time grant gate's `⊆` check. Something as simple as `status.*` has two plausible readings (single-segment versus recursive wildcard), while `fields: ["metadata.*"]` could silently include `metadata.finalizers`. Named, registered subresources keep those boundaries structural and reuse the same `(verb, ResourceKind, subresource)` evaluator as non-field operations such as token creation.
- **Making `org-admin` immutable.** Rejected: the operator may change the global
  admin baseline by editing `PlatformRole/org-admin`; per-org variation belongs
  in scoped platform Denies (§5).
- **Nesting a ServiceAccount under a single owning Project**, as its tree position. Access reach is granted entirely through bindings (§4), independent of tree position, so a "home" Project does no real work — it only couples the SA's inherited attribution (§6.1) to whichever Project happened to parent it, and requires re-parenting (or duplicating) the SA to give it first-class standing against a second Project it's equally bound against. Rejected in favor of parenting ServiceAccount directly under its org, a sibling of Project (§1) — matching how Group is already positioned.
- **Operator status as a hardcoded bypass branch in the evaluator**, checked before Role/binding resolution rather than expressed as data. Makes operator access the one thing the model's own explain/audit tooling can't account for, and duplicates logic the ordinary evaluator already has (union bindings, evaluate Allow/Deny). Rejected in favor of `system:operators` (§1): a reserved subject derived by matching restart-loaded configured identity selectors against an active User's live, active UserIdentity children, then granted access through one seeded, immutable binding. Operators run the same algorithm as everyone else, differing only in that their own request ignores every `Deny` so no cap can reduce their access.
- **Treating the seeded `system:operators` binding as immutable data only, with no evaluator-level guarantee behind it.** Immutability through the ordinary write path (§5) protects only against mutation via this model's own API — not a bad migration, a restore from an old backup, or direct database access losing the row entirely. That residual risk is unacceptable for the one subject with no recovery authority above it. Rejected in favor of a hardcoded, evaluator-guaranteed grant for `system:operators` specifically, mirrored as a healable data row for audit/tooling parity — matching how Kubernetes redundantly hardcodes `system:masters` alongside its ordinary, self-healing `cluster-admin` ClusterRoleBinding, rather than relying on either mechanism alone.
- **Making the `system:operators` binding fully virtual too, with no stored row at all** (matching how membership itself is virtual). Would remove operator access from the same explain/audit tooling that inspects everyone else's — exactly the gap `system:operators` was introduced to close by replacing a hardcoded bypass branch in the first place (above). Rejected; the binding stays data, mirrored and healable — only the evaluator's guarantee of its *effect* is hardcoded, not its existence as an inspectable object.
- **Making the seeded `system-admin` Role and its binding ordinary operator-editable `PlatformRole` data rather than immutable.** Would let an operator edit or delete their own bootstrap grant through the ordinary write path — trivially passing the subset check, since they hold everything — with no higher authority left to recover from it, unlike every other documented risk in this ADR. Rejected in favor of a third, **seeded** Role-ownership tier (§5) that no write path can modify, editable by no one.
- **Allowing a static Subject to pair with a value-less `labelSelector`.** Would grant a fixed subject access to any resource carrying *any* value for that label key, regardless of what it actually says — access disconnected from the value the selector nominally matches on. Rejected; value-less selectors are reserved for dynamic (templated) subjects, where the matched value is actually used (§4).
- **Kubernetes-style plural resource names.** Rejected in favor of one
  group-qualified ResourceKind, such as `rise.dev/Deployment`, across policy,
  Scope, discovery, and versioned URLs (§8).
- **An unqualified or separately-delimited Scope Kind.** Rejected for
  `<api-group>/<Kind>/<names...>`, the version-independent normalization of the
  resource URL (§4, §8).
- **`get` as the reference gate** ("if you can read it, you can select it"), instead of a distinct `use` verb. Couples two independent decisions: a catalog may be browsable without being selectable (visible-but-gated offerings), and selectable without being readable (a class's internals — node selectors, cost plumbing — are not the selector's business). Rejected in favor of `use` (§2, §9), mirroring the Kubernetes `use` verb on PodSecurityPolicies.
- **An `allowedOrgs` list on the referenced resource's spec** as the availability mechanism. Moves an authorization decision out of the one system built to answer authorization questions, needs its own evaluation and audit path, and caps out at org granularity. Rejected; availability is ordinary instance-targeted `use` bindings (§9), which also express group- or ServiceAccount-narrow grants with no extra machinery.
- **Encoding product defaults (e.g. the default `RuntimeClass`) in the RBAC core resources.** Would accrete product-specific settings onto authorization data. Rejected; the core stays agnostic — defaults live on the product resources themselves as labels, and the override cascade is `effectiveLabels` (§6.1, §9).
- **Scope-level all-or-nothing `list` authorization** (or a 403 on inaccessible collections), instead of per-item filtering. Rejected in favor of per-item filtering with existence-masking and per-item `get` expansion (§4): all-or-nothing either over-discloses — returning full items to anyone who can list the scope — or leaks scope population, since a 403 confirms the collection is non-empty, and it cannot express "see names org-wide, data only for owned."
- **Live `use` re-evaluation at reconcile time**, instead of materializing the resolved class onto the Deployment at creation. Leaves the check with no well-defined subject (a reconciler acts for nobody in particular) and turns a grant revocation into retroactive breakage of running workloads. Rejected; the effective class is materialized at deployment creation and `use`-checked against the deployer (§9), matching Kubernetes' DefaultStorageClass admission behavior — revocation applies from the next deployment.
- **Server-auto-stamping the `rise.dev/owner` label at resource creation**, so a creator never has to write it. Would force the generic resource core to hardcode knowledge of the ownership label — the one thing §6 exists to keep out of the engine, where ownership is purely the emergent effect of a seeded binding over an ordinary label. Rejected in favor of the creation-time exception (§6.6): the creator writes the label, gated to claiming only themselves or a group they belong to, and the core stays agnostic.
- **Passing a validated JWT's raw `sub` string into authorization.** Signature validation proves who issued bytes, not that `sub` names an existing Rise principal or even a principal-capable subject kind; an external token could otherwise spell `system:operators`, a group, or a malformed lookalike and rely on downstream parsing. Rejected: authentication maps credentials to an existing active Rise identity, constructs a typed canonical `SubjectId`, and the resource API accepts external workload credentials only at token exchange (§1, §7).
- **Adding arbitrary JSON-path filters or index declarations to the generic resource API for identity lookup.** Makes storage projections part of the public resource abstraction before any general use case exists. Rejected for fixed partial expression indexes over the built-in identity kinds and narrow Postgres lookup adapters; ordinary resource writes maintain the indexes transactionally without changing `ResourceStore` or client APIs (Implementation structure).
- **Allowing org bindings to target arbitrary subjects (a cross-org grant).** Would let an org author a binding whose grant reaches a foreign org's subjects, or an org-agnostic Controller, with no membership relationship to the granting org. Rejected in favor of the recipient boundary's org-membership intersection (§1) — an org binding's grant reaches only live members of its own org — with deliberate cross-org sharing deferred to a future first-class primitive (§10).

## Implementation structure

*Where the code lives, not what the model is. This realizes the sections above; it is a design intent, not a normative rule.*

The evaluation logic is security-critical, and the value of a small, auditable core is highest exactly there. The carve-up's goal is that the decision logic — union, Deny-wins, the subset check, wildcard replacement, the label-write gate — can be read, fuzzed, and tested **without a database and without any Rise product concept**. What a `Deployment` is, what `rise.dev/` means, and how rows reach Postgres must never leak into it. One fact drives most of the structure: the RBAC objects — `Role`, `RoleBinding`, `PlatformRole`, `PlatformRoleBinding` — are all **resources** in the generic store (§3, §5), so reading a subject's bindings, cap `Deny`s included, is an ordinary `ResourceStore` read, not a bespoke authorization data path. Max token TTL is not among these reads: it is a platform-global config constant checked at token issuance (`rise-backend-auth`, §7), never a store-resolved fact.

Three tiers, separating security decisions, fact-retrieval, and product meaning:

- **Tier 0 — pure policy algebra** (new crate, e.g. `rise-authz-policy`; ~zero deps). The Allow/Deny evaluator over `(Verb, ResourceKind, Option<Subresource>)`, the Deny-aware subset check, Scope/selector lattice, wildcard replacement, and subject substitution — pure functions over small canonical types.
- **Tier 1 — the evaluation engine** (new crate, e.g. `rise-authz`). The §4 algorithm, Group expansion, structural org-admin detection, Deny-tier filtering, request-local `AuthorizationSnapshot`, effective-label diffing, recipient boundary, and list filtering. Its entry point accepts only a typed `AuthenticatedPrincipal`; no JWT claims or raw strings cross this boundary.
- **Tier 2 — Rise wiring** (`rise-deploy`). Authentication adapters, JIT User/UserIdentity provisioning, Group membership and configured-operator-identity resolution, seed data (`system-admin`, editable `resource-owner`/`org-admin`, and bootstrap bindings), the centralized authz choke point, handlers, list projection, and token wiring. Only `/token` accepts external workload assertions.

**The facts come from the store crate, not scattered in `rise-deploy`.** The tree and binding reads are the *existing* `ResourceStore` trait, grown with generic hierarchy/label operations implemented in `rise-resource-store`'s Postgres store: ancestor chain, the K-inheriting subtree (`WITH RECURSIVE` over `parent_uid`), `effectiveLabels` resolution, and list-by-kind-under-scope — product-agnostic operations over a labeled hierarchical store. This matches the repo's SQLX split (`rise-resource-store` owns resource-store SQLX; `rise_deploy::db` owns legacy typed-table SQLX). The authz engine's product-specific seam remains **`MembershipResolver`**: its target implementation reads `GroupMembership` resources, derives ordinary org membership from their Group parents, and tests the active User's live, active UserIdentity children against the process's restart-loaded operator selector set; the engine adds §5's direct qualifying admin binding as the sole bootstrap org-affiliation edge from bindings it already loaded. During migration only, a compatibility implementation may read legacy `team_members`, but that table is not part of the target model.

**Postgres secondary indexes are storage projections, not API features.** The generic resource API and `ResourceStore` trait do not gain arbitrary JSON-path search. Instead, `rise-resource-store` migrations add partial expression indexes over the built-in `rise.dev` kinds in `resource_store.resources`, following the existing `ResourceDefinition` index precedent:

- a unique live `UserIdentity` index on normalized `(spec->>'issuer', spec->>'subject')`, intentionally including inactive rows so deactivation cannot be bypassed by inserting or JIT-provisioning a duplicate active mapping;
- a parent-and-issuer index over live `ControllerTrustPolicy` and `ServiceAccountTrustPolicy` rows, used to narrow policies beneath the explicitly targeted identity before claim-pattern evaluation in Rust;
- a unique live membership-edge index on `(parent_uid, GroupMembership.spec.userRef.uid)`, preventing duplicate membership resources for the same User in one Group, plus a reverse index on `spec.userRef.uid`; Group-to-members lookup already uses the generic `(parent_uid, group, kind)` index.

Every predicate includes the `rise.dev` API group, exact built-in Kind, and `deletion_timestamp IS NULL`, so a custom kind with the same name in another group cannot collide. Because these are expression indexes on the canonical JSONB rows, ordinary generic create/update/delete transactions maintain them automatically — no trigger-maintained mirror table or application dual-write can drift. Schema validation guarantees the indexed fields and normalized issuer form before persistence; the unique index remains the concurrency authority.

Authentication and membership use narrow `IdentityLookup`/`MembershipLookup` Postgres adapters alongside `PgStore`, backed by those indexes and returning typed facts or resource UIDs. They are not methods on the generic `ResourceStore`, are not exposed as client-selectable filters, and do not introduce identity lookup/index semantics into `rise-resource-api` or the pure authorization crate beyond the already-shared `SubjectId`. This requires storage migrations and a small Postgres adapter, but no change to the resource envelope, URL shape, ResourceDefinition API, or authorization algebra. Generic indexed-field declarations for user-defined kinds are a separate future feature and are unnecessary for these fixed built-ins.

**Prerequisite refactor:** move the `ResourceStore` contract and canonical `SubjectId`, `SubjectRef`, `ResourceKind`, and `Scope` types into dep-light `rise-resource-api`, leaving `PgStore` + SQLX in `rise-resource-store`. Delegated `(create, ResourceKind, token)` and parsed authorization caps are engine concerns; workload trust validation, UID lookup, signing, `act`, and TTL live in `rise-backend-auth`. Per-item list filtering is engine work; projection is an API/server concern.

```
rise-authz-policy   (pure algebra; own Verb/ResourceKind/Statement types; ~zero deps)
        ▲
rise-authz (engine) ──► rise-resource-api  (envelope types + ResourceStore trait,
   defines MembershipResolver              canonical identity types, no sqlx)
        ▲                        ▲
        │                        │ impl
rise-deploy ──► rise-resource-store (PgStore + identity/membership index adapters:
  impl MembershipResolver         ancestors, effectiveLabels, indexed built-in lookups;
  over GroupMembership, UserIdentity + config; the sqlx home)
  seed data; authz.rs choke point; HTTP; list projection; token wiring
```

The payoff: the pure algebra and engine are testable with fakes and no Postgres, so the acceptance suite partitions three ways — pure-logic → Tier 0 unit tests; tree/membership → Tier 1 with fake stores; wiring (masking, `list` projection, token endpoint) → server integration — and the most security-sensitive code has the fewest dependencies. Two structure choices are left revisitable: whether tiers 0 and 1 are one crate (modules `policy`/`engine`) or two — leaning **one with a hard internal boundary**, split when the pure tier earns it (as `rise-backend-docker` was extracted only once its seam matured, #377) — and whether tier 0 reuses `rise-resource-api`'s verb/kind types or defines its **own** (leaning own, for a standalone, portable policy library at the cost of a thin mapping layer). Leaving the `ResourceStore` trait in the sqlx-bearing crate and letting the engine take the transitive database dependency was considered and rejected — it bloats the security core's dependency graph and undercuts fake-testability.

## Appendix: acceptance scenarios (normative)

The initial conformance suite covers every scenario except those explicitly
tagged `§9 deferred` or `product-operation deferred`.

### Resource and subject identity (§1, §3, §4, §8)

1. **ResourceKind is group-qualified.** Given two registered Kinds
   `alpha.example/Widget` and `beta.example/Widget`, a Role allowing only
   `alpha.example/Widget` never authorizes the beta Kind. Unqualified
   `Widget` is rejected in Roles, authorization details, and Scope parsing.
2. **Version is not authority.** Served versions
   `rise.dev/v1alpha1/Deployment` and `rise.dev/v1/Deployment` normalize to
   the same `rise.dev/Deployment` ResourceKind and policy.
3. **Scope is qualified and canonical.** `rise.dev/Project/acme/app` parses;
   missing/unknown group or Kind, wrong ancestor count/order, dot/empty/extra
   components, query/fragment, and non-canonical encodings fail. A target in
   the same atomic transaction is accepted.
4. **Scope omission is org-sensitive.** An org RoleBinding defaults to its
   parent `rise.dev/Organization/<name>`; a PlatformRoleBinding for static
   `group:acme/platform` or `serviceaccount:acme/ci` defaults to acme; every
   other PlatformRoleBinding defaults to `"*"`. Explicit wildcard Scope for a
   static org-native subject is rejected.
5. **Subject grammar is fail-closed.** `group:acme/platform` and
   `serviceaccount:acme/ci` parse; the old `acme/group:platform` form,
   missing org/name, extra separators, unknown kinds, unrecognized `system:`
   forms, and nonexistent literals are rejected. A `${ref.subject}` label value
   accepts only absolute `user:<name>` or org-relative `group:<name>` and
   canonicalizes the latter against the matched resource's org; other kinds,
   aliases, full cross-org Group paths, and root-relative Groups fail closed.
6. **One lowercase subject field.** Serialized bindings accept `subject`,
   `subjectMembership`, `scope`, `labelSelector`, and `roleRef`; capitalized or
   plural `subjects` forms are rejected by the closed schema. Platform bindings
   persist the PascalCase enum `Any` or `ResourceOrganization`; omission
   normalizes to `Any`, while explicit `null` and other values fail closed. Org
   RoleBindings reject the field as structurally redundant.
7. **Name-bound policy, UID-bound credentials.** A privileged generic
   recreation of `serviceaccount:acme/ci` intentionally reactivates its
   name-bound policy, but a token containing the old UID fails authentication.
   The constrained Project product flow never reuses that retired canonical
   name.
8. **Built-in placement.** User and Controller are root resources; Group and
   ServiceAccount are Organization children; UserIdentity, GroupMembership,
   and workload trust policies have only their declared fixed parent.
9. **User identity is not email.** Duplicate live `(issuer, subject)`
   UserIdentity mappings fail; profile email may repeat or change without
   changing User name, UID, bindings, or login mapping. An inactive exact
   mapping is found and denied, never treated as unknown for JIT.
10. **Operator identities select Users, not login methods.** The selector set is
    loaded at startup. A first validated login for an unknown pair atomically
    creates a generated User and UserIdentity, including on a fresh store, and
    concurrent attempts converge on the unique pair. A configured pair makes
    that User an operator immediately; any already-linked secondary identity
    authenticates as the same operator User, while an unknown identity creates
    a distinct User and is never email-linked. Setting the User inactive blocks
    every login and existing User token; setting one UserIdentity inactive
    blocks that login and its operator-selector contribution without disabling
    other identities. Deleting a configured mapping revokes the old User's
    operator expansion but a later valid login provisions the pair with a fresh
    User UID; durable removal uses `active: false` or removes the selector and
    restarts/drains every old API instance. No old token revives.

### Evaluation, Deny tiers, and membership (§1, §4, §5)

11. **Union and default deny.** Applicable Allows union; absent Allow denies.
    An applicable retained Deny wins over every Allow.
12. **Platform Deny reaches admins.** An acme org admin allowed all main and
    subresources is denied a tuple removed by a PlatformRoleBinding Deny scoped
    to `rise.dev/Organization/acme`.
13. **Org Deny exempts only that org's admins.** An acme RoleBinding Deny blocks
    ordinary acme Users and ServiceAccounts but not a current acme admin, and it
    has no effect in beta. An equivalent beta RoleBinding Deny blocks the same
    User in beta when they are not a beta admin and has no effect in acme.
14. **Operator ignores all Denies.** A caller whose authenticated User UID is
    in `system:operators` retains every tuple despite platform, org, Group, or
    direct-User Denies.
15. **Deny provenance survives replacement.** Wildcard replacement may discard
    superseded Allow content but retains each Deny and its platform/org binding
    tier before §5 filtering.
16. **Membership expansion is live.** Removing a User from a Group removes its
    Group-derived access on the next request.
17. **Ordinary org membership requires a Group tie.** A group-less User receives
    no grant from an ordinary org RoleBinding naming that User or
    `system:authenticated`. Adding any governed GroupMembership activates the
    boundary. A direct qualifying org-admin binding is the sole bootstrap
    exception. A PlatformRoleBinding with
    `subjectMembership: ResourceOrganization` obeys the same live resource-org
    clamp, while `Any` may deliberately reach a non-member. Omitted platform
    input normalizes to `Any`. `ResourceOrganization` is a no-op for an
    inherently org-scoped subject or a root target; for `system:authenticated`
    on an org resource it tests the actual caller, and a Controller does not
    match.
18. **Absolute org subject remains useful.** A PlatformRoleBinding targeting
    `org:acme` may grant acme subjects use of a root resource. Inside an acme
    RoleBinding, `system:authenticated` clamps to the same org population.
19. **Recipient boundary makes foreign subjects inert.** An org RoleBinding
    targeting a foreign Group/ServiceAccount or org-agnostic Controller is
    accepted but contributes no grant and is reported by policy auditing.
20. **Wildcard collision uses authored forms.** Literal and templated subjects
    never collide; selector keys differ independently; a value-specific
    replacement affects only resources matching that value.

### Org-admin assignment (§5)

21. **Atomic first admin.** Organization creation and its exact scope-only
    `PlatformRole/org-admin` RoleBinding to an existing User commit together
    or not at all. The direct binding itself establishes that first User's org
    membership; no pre-existing Group in the new org is required.
22. **No magic Group.** A direct User binding and a binding to any ordinary
    Group both establish admin status; the Group's name has no special meaning.
23. **Multi-org admin.** One User may match qualifying bindings in acme and beta,
    ignore each org's own Denies only there, and respect each org's distinct
    platform ceiling.
24. **Structural predicate.** A label-selected, descendant-scoped, foreign-org,
    or differently referenced binding never establishes org-admin status.
25. **Global baseline versus per-org ceiling.** Editing
    `PlatformRole/org-admin` changes every org's baseline; a scoped platform
    Deny changes only its matching org. The Role accepts the ordinary statement
    schema, but a Deny in it arrives through the qualifying org RoleBinding and
    is ignored by the admin; it never substitutes for a platform ceiling.
26. **Promotion includes removed org Denies.** Adding a User to an admin-bound
    Group or creating an admin RoleBinding computes the net post-promotion
    EffectivePolicy. A writer lacking any newly exposed tuple is rejected.
27. **Admin removal is live.** Removing the last matching direct/Group
    relationship revokes admin Allows and makes org Denies apply on the next
    request. If the User also has no Group tie, it ends their org membership
    entirely; an operator can recover by creating a new binding.
28. **SSO sync is governed.** A sync principal may add ordinary directory Group
    membership only within its delegated delta. Adding membership to an
    admin-bound Group requires admin-equivalent authority.

### Grant gate and consistency (§5, §6)

29. **Net delta respects platform Deny.** A capped admin may add another admin
    covered by the same platform Deny; the new admin remains denied that tuple.
30. **Role edit spans bindings.** Widening a Role computes effective deltas for
    every bound recipient and domain; an unbound Role body creates no authority
    until binding.
31. **Deleting Deny is a grant.** Removing or narrowing a Deny passes the same
    effective-delta subset check as adding an Allow.
32. **Scope and selector containment are exact.** Narrow authority cannot
    justify a broader Scope; same-key selectors order by specificity and
    different keys fail closed.
33. **Serializable with revocation.** Concurrent grant and writer revocation
    cannot both commit against stale assumptions; one retries and re-evaluates.
34. **Identity mapping is a grant.** Adding/retargeting UserIdentity or workload
    trust policy requires the parent's effective authority; tightening/deleting
    it requires ordinary write authority.
35. **Leaving org ends org governance.** Removing the final Group tie when no
    direct qualifying admin binding remains drops org grants and org Denies.
    Contextual platform Allows constrained by `ResourceOrganization`, including
    direct User ownership, also stop matching. Any surviving unconstrained
    platform Allow remains limited by platform Denies, not by the departed org.
36. **Request snapshot is local.** Repeated checks in one request may reuse one
    AuthorizationSnapshot; the next request re-reads changed memberships,
    bindings, Denies, and identity state. No cross-request cache is accepted
    without the transactional authorization epoch.
37. **List-only projection is allowlisted.** An item for which the caller holds
    `list` but not `get` contains only `apiVersion`, `kind`, and the documented
    `metadata` fields. Arbitrary top-level fields are absent even when they are
    neither named `spec` nor `status`.
38. **Per-item get expands lists.** A listed item for which the caller also
    holds `get` is returned as the full stored object. Items lacking `list` are
    omitted, and no applicable list grant yields a masked-empty collection.

### Ownership and labels (§6)

39. **Nearest label wins.** A child owner label shadows its ancestor; removal
    re-exposes the inherited value.
40. **Relabel delta is subtree-wide.** Adding, changing, or removing an
    access-driving label computes effective before/after authority across every
    descendant inheriting that value.
41. **Unauthorized redirect fails before lookup.** An editor lacking the
    resulting grant cannot relabel ownership to themselves and receives no
    existence oracle for the attempted Group/User.
42. **Creation exception is narrow, immediate, and org-clamped.** A genuine new
    resource may set an access-driving `${ref.subject}` value only to
    `user:<caller's-stable-name>` or a same-org `group:<name>` the caller belongs
    to. The pre-existing dynamic binding grants ownership in the same
    transaction without creating a RoleBinding. Another User or unrelated Group
    uses the general gate; a foreign/group-less existing User may be stored when
    the membership clamp makes the effective delta empty, receives no grant,
    and is reported by policy auditing. A later membership write that would
    activate that ownership passes the ordinary effective-delta grant gate.
    Removing the owner's final affiliation removes the grant on the next
    request. Restore/upsert also uses the general gate.
43. **Admin recovery is label-independent.** Org-admin status derives only from
    exact scope-only RoleBindings and survives every ownership-label change.

### Tokens and authorization details (§7)

44. **Workload token exchange is target-bound.** An external JWT presented to
    `serviceaccount:acme/ci` considers only that target's live trust policies
    and, on one match, issues a token for exactly its name and UID without an
    RBAC token-create check.
45. **Workload failures are masked only on valid token routes.** On a registered
    ServiceAccount/Controller `/token` route, missing/deleted/disabled targets,
    a UID resolving to the wrong kind, invalid assertions, and zero/multiple
    policy matches return the same coarse authentication error. `/token` on a
    kind that does not register it, such as Deployment, returns route-not-found
    before authentication.
46. **Delegated issuance uses RBAC only.** A Rise principal may mint an exact
    target only with current capped `(create, target ResourceKind, token)`;
    target trust policies do not participate.
47. **Modes cannot mix.** A request containing both external assertion and Rise
    caller credential fails; Rise's issuer is invalid in workload trust policy.
48. **Delegation chains explicitly.** A minted identity may mint the next only
    through its own live token-create grant; bounded nested `act` records all
    delegators and never grants authority.
49. **Main-resource detail omits subresources.** A detail allowing
    get/list on `rise.dev/Deployment` with omitted `subresources` permits the
    main resource only and denies status/finalizers.
50. **Subresource detail is separate.** A statement with
    `subresources:["status"]` permits only status; `"*"` covers registered
    subresources but not the main resource.
51. **Entries union by singular Scope.** Two `rise.dev/rbac` entries may give
    different permissions at different qualified Scopes; no Cartesian product
    is inferred.
52. **Malformed details fail closed.** Empty axes, unqualified kinds/Scopes,
    unknown types, malformed entries, and a present empty detail set fail token
    validation and never fall back to full policy.
53. **Cap applies everywhere.** Live RBAC outside the detail union is denied for
    main/subresource checks, references, list items/projection, grant writes,
    and explain simulation.
54. **Revocation asymmetry and TTL.** Target Role narrowing or deletion affects
    outstanding tokens immediately; revoking the caller's token-create stops
    new issuance only; every token respects the platform max TTL.

### Subresources (§2, §7)

55. **Main and status writes are separated.** Main update/apply preserves status
    and acquires no status field ownership; status update preserves every other
    field and does not increment generation.
56. **Finalizers are separated.** Main writes preserve finalizers; only
    `(update, ResourceKind, finalizers)` may change them.
57. **Token is create-only.** Delegated `POST .../token` requires create on the
    target subresource; get on the parent or token subresource grants nothing.

### Deferred platform references (§9 deferred)

58. **Use is independent of get.** Reading and selecting a root platform
    resource remain separately grantable.
59. **Consuming-org isolation.** A multi-org User may select an org-private
    RuntimeClass only while writing a consuming resource in that org.
60. **Materialization is a snapshot.** Revoking use affects the next deployment,
    not an already materialized running resource.

### Constrained product operations (§1, §5; product-operation deferred)

61. **Project ServiceAccount creation is confined (deferred).** An ordinary Project user
    cannot generically create or delete ServiceAccounts. The product operation
    atomically allocates a never-reused canonical name and creates only its
    fixed Project-scoped policy/trust bundle. The resulting effective policy
    must be a subset of the caller's current capped Project policy even though
    the caller need not hold generic Role/RoleBinding creation permission;
    arbitrary policy input and a previously retired canonical name are
    rejected. Its paired deletion cleans up only flow-owned authorization data
    and retires the identity.
## References

- `ROADMAP.md` §§1–4 — owns live delivery status for the unified RBAC,
  authentication, subresource, and typed-object migration work this model
  informs.
- [Generic Resource API](../generic-resource-api.md) — the shipped,
  operator-only surface this model will govern.
- `crates/rise-resource-api`, `crates/rise-resource-store` — the envelope types
  and the `ResourceStore` trait/impl the Implementation structure builds on.
