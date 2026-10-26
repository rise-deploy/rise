---
title: "ADR-0006: Scoped CLI Sessions"
---

## Status

**Implemented** for the typed API (§1–§7, §9–§10). Date: 2026-10-03.

§8 happens kind by kind as the typed tables migrate. The pure model (permissions,
presets, compilation, ceiling evaluation) lives in
`rise-backend-auth::session_scope`; enforcement is the route guard in
`src/server/auth/session_scope.rs`. The e2e scenario `scoped-device-login` covers
the device flow end to end.

Scope: enforcement on the **typed API** only. The generic resource API, the
`rise-authz` engine and the kind registry are left unchanged. Scoped sessions
are refused there until Project, Environment and Deployment become generic
kinds (§8).

## Context

A coding agent working on a user's app needs a Rise CLI session. The device
flow (`rise login --device`) already fits: the agent starts it, the user
approves on Rise's own `/device` page from a fresh browser session, and the CLI
receives a session for that user. That session can do everything the user can
do: every project they own or share through a team, every environment
including production, and for admins every project on the install. An agent
asked to "deploy my branch to staging" ends up holding the user's full
authority for 24 hours.

Rise already has the mechanism to narrow a credential. ADR-0001 §7 defines a
signed Allow ceiling, the RFC 9396 `authorization_details` claim with
`type: rise.dev/rbac` entries. On every decision, a token's effective policy is
`live RBAC ∩ union(entries)`. The resource API enforces it today for
ServiceAccount and Controller identity tokens. Two gaps keep it from covering
this case:

1. **User sessions don't carry it.** `RiseClaims` has no
   `authorization_details`, and neither login flow has a way to request one.
2. **The surface an agent uses isn't on the resource API.** Projects,
   environments, deployments, env vars, domains, extensions and service accounts
   are typed tables behind typed handlers (`ROADMAP.md` §4). Those handlers
   authorize with ownership checks (`check_read_permission`,
   `check_write_permission`, `user_can_access`) plus an admin bypass. The only
   environment-level restriction is the typed service account's
   `allowed_environment_ids`, enforced in `create_deployment`.

This ADR designs per-session permissions at project and environment
granularity, how a login requests them, and how the user who approves can
change them. Projects, environments and deployments have no generic resource
kinds yet, so the design is enforced on the typed API now. It is shaped to
carry over to the generic resource API unchanged once those kinds exist (§8).

## Decision

### 1. One ceiling grammar: sessions carry `authorization_details`

A CLI session token may carry an `authorization_details` claim in ADR-0001
§7's wire format (`type: rise.dev/rbac` entries, each with one Scope and its
permission statements), with the same semantics:

- **Omitted** means the user's full live access. This is what every session
  minted today has, and it stays the default.
- **Present** is a non-empty list of well-formed `rise.dev/rbac` entries. An
  empty or malformed value is an invalid credential, never full access.
- It is a **ceiling, never a grant**: a session holds an operation only if the
  user's live access allows it *and* an entry covers it.

The claim is signed into the session and can't be changed after minting.
Narrowing or widening means a new login. Web UI sessions (`rise_client:
browser`) are never scoped: they approve device logins, and approval needs the
user's own authority.

There is deliberately **no second scope vocabulary** (no `project:read`-style
OAuth scope strings). The user-facing permission names in §3 compile to
`rise.dev/rbac` entries, the format the resource API's engine already
evaluates. Today only the typed API reads them (§7). After the migration the
engine reads the same claim, so a session means the same thing before and
after (§8).

### 2. Targets: a project or one of its environments

A grant names one target:

| Target | CLI form | Covers |
|---|---|---|
| Project | `my-app` | The Project, everything below it, and every environment |
| Environment | `my-app/staging` | That Environment and the project resources bound to it (§4) |

On the wire a target is an ADR-0001 Scope path. The organization segment is the
project's organization, resolved by the server at approval time (§6), never typed
by the user: `my-app` becomes `rise.dev/Project/<org>/my-app`, and
`my-app/staging` becomes `rise.dev/Environment/<org>/my-app/staging`.

Targets are names, as Scope paths are. Renaming the project or environment
leaves the session covering nothing there, which fails closed. Recreating a
deleted name lets the session reach the new resource only as far as the user's
live access does.

Install-wide operations have no target and are outside any scoped session:
creating projects, managing teams, and the admin surface. A scoped session
can't do them; the user does them, or the agent asks for a full-access login.
An organization-level target is deferred (§11).

### 3. Permissions and presets

A grant pairs a target with a set of named permissions. Each name expands to
fixed `(verb, ResourceKind, subresource?)` statements:

| Permission | Allows | Statements | Project | Environment |
|---|---|---|:-:|:-:|
| `view` | See the project, its environments, deployments (incl. containers and events), domains, extensions, service accounts, app users, env var names and plain values | `get`, `list` on every kind below | ✅ | ✅ |
| `logs` | Read deployment logs | `(get, Deployment, logs)` | ✅ | ✅ |
| `deploy` | Create deployments, push their images, stop and roll back deployments | `(create, Deployment)`, `(update, Deployment)`, `(update, Deployment, status)`, `(create, Deployment, registry-credentials)` | ✅ | ✅ |
| `env-vars` | Set and delete env vars | `create`, `update`, `delete` on `EnvironmentVariable` | ✅ | ✅ environment-specific only |
| `secrets` | Read retrievable secret values | `(get, EnvironmentVariable, value)` | ✅ | ✅ environment-specific only |
| `configure` | Manage environments, custom domains, extensions, app users | `create`, `update`, `delete` on `Environment`, `CustomDomain`, `Extension`, `AppUser` | ✅ | ❌ |
| `service-accounts` | Manage CI service accounts and mint their tokens | `create`, `update`, `delete` on `ServiceAccount`; `(create, ServiceAccount, token)` | ✅ | ❌ |
| `project-admin` | Rename, re-own, change access class, delete the project | `update`, `delete` on `Project` | ✅ | ❌ |

Kinds are `rise.dev/`-qualified. `Project`, `Environment`, `Deployment` and
`ServiceAccount` are the names `ROADMAP.md` §4 already uses. This ADR fixes
`EnvironmentVariable`, `CustomDomain`, `AppUser` and `Extension` (the family
name under ADR-0003) as the names those tables migrate to. The same goes for
the subresources `logs`, `value` and `registry-credentials`. None of them is
added to the resource API's kind or subresource registry. The typed check
keeps them in its own closed list (§7). Each name is registered in the generic
registry when its table migrates, under the name fixed here (§8).

Presets name common bundles. The CLI and the approval page offer presets first
and individual permissions second:

| Preset | Permissions | Allowed on |
|---|---|---|
| `read` | `view` | project, environment |
| `deploy` | `view`, `logs`, `deploy` | project, environment |
| `develop` | `view`, `logs`, `deploy`, `env-vars` | project, environment |
| `admin` | all eight | project |

Presets are fixed in code for now. The token always carries the expanded
statements, never a preset name. So changing a preset's definition changes
what future logins request, never what an existing session may do.

`deploy` reaches secrets in practice. A deployment can run code that prints its
own environment, including protected values that the API never returns. So
`secrets` is a meaningful boundary only for sessions without `deploy` on the
same target, and the approval page says so when both are requested.

### 4. Environment coverage: a `labelSelector` on ceiling entries

ADR-0001 places Deployment and the env-var kinds under Project, not under
Environment. A Scope at `rise.dev/Environment/<org>/my-app/staging` therefore
covers the Environment resource itself but none of the deployments into it.
The typed service account's `allowed_environment_ids` is a one-off fix for
exactly this gap.

A session's `rise.dev/rbac` entry may therefore carry an optional
`labelSelector`, with ADR-0001 §4's grammar restricted to the static form (a
`value` is required). Today only the typed check evaluates it. ADR-0001 §7
adopts it when the engine starts reading session ceilings (§8):

```json
{
  "type": "rise.dev/rbac",
  "scope": "rise.dev/Project/default/my-app",
  "labelSelector": { "key": "rise.dev/environment", "value": "staging" },
  "permissions": [
    { "verbs": ["get", "list", "create", "update"], "kinds": ["rise.dev/Deployment"] }
  ]
}
```

The entry covers a resource iff its Scope covers the resource **and** the
resource's `effectiveLabels[key] == value`. A selector can only narrow an
entry, so the ceiling is still a ceiling. Without a selector, an entry behaves
exactly as before.

`rise.dev/environment` becomes a **governed label**. It is set by the platform,
never by a writer:

- An environment-bound resource carries it with its environment's name: a
  Deployment created with an environment, or an environment-specific env var.
  Today the typed adapter synthesizes it from `environment_id`. After
  migration it is set from the resource's environment reference.
- Every other resource has none. A deployment created without an environment,
  or a project-wide env var, is never covered by an environment grant. This
  matches today's service-account rule that an environment-restricted caller
  must name an environment.
- It is reserved on every other kind and on direct label writes, so it can't
  be inherited from a Project or set to retarget a resource.
- A write that changes which environment a resource is bound to must be covered
  both before and after, as ADR-0001 §6.6 requires of any label write that
  retargets access. On the typed API, moving an env var between environments
  needs a project-wide grant, which covers both.

An environment grant compiles to three entries:

1. Scope `rise.dev/Project/<org>/<p>`, permissions `get` and `list` on
   `Project` only, so the CLI can resolve the project by name and find it in
   the project list.
2. Scope `rise.dev/Environment/<org>/<p>/<e>`, permissions `get` and `list` on
   `Environment`.
3. Scope `rise.dev/Project/<org>/<p>` with the `rise.dev/environment = <e>`
   selector, carrying the granted permissions' statements on the
   environment-bound kinds.

### 5. Escalation closure: what an environment grant can never reach

An environment grant must not be convertible into authority over another
environment. Each ❌ in §3 closes such a path:

- **Environment `update`/`delete`** is excluded. An environment's deployment
  group and production flag decide where its deployments land. Remapping
  `staging` onto the production group would turn a staging grant into a
  production one.
- **Service accounts** are excluded. A service account with a trust policy for
  an issuer the session controls (a CI repository) mints tokens bounded only by
  the service account's own access.
- **Project-wide env vars** are excluded. They apply to every environment, so
  writing one changes production.
- **Domains, extensions, app users and the project itself** are project-wide by
  construction.
- **Stopping a deployment group** (`POST …/deployments/stop`) and listing
  deployment groups need a project-wide grant: a group may span environments.

The general rule for new permissions: an operation may join an environment
grant only if its effects stay inside that environment.

### 6. Requesting and granting access

**The requester proposes; the approving user decides.** A request is a draft for
the approval page, not a bound on the result. The user may narrow it, widen
it, or grant full access. The approver holds the authority being delegated, and
anything they grant is still capped by their live access.

Requests and approvals use a user-facing shape, not raw
`authorization_details`. The server compiles it once a user is authenticated,
which avoids resolving project names before anyone signs in:

```json
{
  "access": {
    "kind": "restricted",
    "grants": [
      { "project": "my-app", "permissions": ["view", "logs"] },
      { "project": "my-app", "environment": "staging", "preset": "develop" }
    ]
  }
}
```

The other form is `{ "kind": "full" }`. A grant gives `preset`, `permissions`,
or both (union). Repeated grants on one target union.

**Regular login (`rise login`, browser/PKCE).** No `--scope`: full access, as
today. With `--scope`, the CLI sends `access` with the code exchange
(`POST /auth/code/exchange`). The authorize step keeps no server-side state, and
the code and verifier already authorize a full session, so a request that only
narrows needs no earlier binding. The server checks its shape before spending
the code, compiles it once the user is resolved, and mints it into the session.
The user typed the flags, so no extra consent page is shown. A target the user can't access fails
the login with a clear error, rather than minting a session that covers
nothing.

**Device login (`rise login --device`).** The CLI sends `access` on
`POST /auth/authorize {flow: "device"}`. Without `--scope`, the request is
`{kind: "full"}`. At start the server only checks the syntax: the endpoint is
unauthenticated and must not reveal whether a project exists. It stores the
request on the `device_authorizations` row. `GET /auth/device` returns it to the
approval page, which:

- shows the request as editable rows (target, preset, permission checkboxes),
  pre-filled from the request, with "add project/environment" and a **Full
  access** option;
- marks a full-access request prominently, with a warning that the CLI can do
  everything the user can, admin rights included;
- blocks approval while a row lacks a project or a permission. The server
  refuses an approval naming a project or environment that doesn't exist or
  that the approver can't access, and the page shows its message;
- shows the §3 note on `deploy` and `secrets` when both appear.

`POST /auth/device/approve` requires `access`. There is no default on approval:
the page sends exactly what it displayed, so "the field was missing" can never
mean full access. The server compiles it against the approver's own access and
stores the resulting `authorization_details` with the approval. It mints them
into the session on redemption. Every device-flow guarantee holds unchanged:
fresh browser session, single-use hashed code, re-resolution at redemption.

**The CLI shows what was granted.** After login, the CLI decodes the session's
claim and prints the granted access, because the approver may have changed the
request:

```console
✓ Login successful!
  Access: restricted
    my-app            view, logs
    my-app/staging    develop (view, logs, deploy, env-vars)
```

`GET /users/me` returns the same summary, so `rise` can show it without
re-logging in.

### 7. Enforcement on the typed API

Every typed handler authorizes through one choke point. The choke point takes
an operation (`verb`, ResourceKind, subresource) and a typed target (project,
optional environment):

1. **Live access**, unchanged: today's ownership/membership check and the
   admin bypass.
2. **Session ceiling**: the operation must be covered by the session's
   `authorization_details`. The typed check evaluates the ceiling itself. It
   needs no resource store or registry. It knows the closed set of typed kinds
   and subresources from §3. It knows the two Scope shapes in §2. It reads the
   target's `rise.dev/environment` label from `environment_id`. It applies the
   same coverage rule as ADR-0001 §7 plus the §4 selector. A claim naming any
   other kind, Scope shape or selector key is an invalid credential.

Rules:

- **The ceiling applies to admins.** The admin bypass skips step 1, never step
  2. A scoped session of an admin user is as narrow as anyone's.
- **Collections are filtered.** `GET /projects` returns only covered projects.
  Environment and deployment lists in a project return only covered items, as
  ADR-0001 §4 masks `list`. A project where the session holds nothing of that
  kind answers 403 instead of an empty list, which tells an agent its login is
  the problem. The deployment filter runs on the requested page, so a page can
  come back short.
- **Every route is classified, and unknown means refused.** A guard behind the
  auth middleware classifies each typed route by method and matched path. It
  is *ambient* (`GET /users/me`, platform capabilities, extension types,
  quickstart templates, access classes) or *resource-targeted*. The guard
  decides a resource-targeted route from its path, its `?environment=` query,
  or, for a deployment, that deployment's environment. A *handler* route needs
  the request body or a list to filter, so the handler consults the session's
  scope itself. The guard turns a successful answer from a handler that never
  did into a 500. Any other route, a new one included, needs an unrestricted
  session until someone classifies it.
- Install-wide operations (project create, teams, `/encrypt`, user lookup,
  device approval, the deprecated project-less deployment status route) need
  an unrestricted session.

**Scoped sessions are refused on the generic resource API.** Today the
resource API builds every session's principal as unrestricted. Rather than
teach it to read session ceilings before its engine knows these kinds, the
session authentication adapter rejects any session carrying
`authorization_details` on `/api/v1/resources` with a 403 that names the
reason. That's a single layer on the resource routes, ahead of the resource
API's own handling, which stays unchanged.
It fails closed: a scoped session can never reach the resource API with full
access. The agent workflows this ADR targets (projects, environments,
deployments, env vars) don't use that API.

**No CLI session opens a deployed app.** The ingress check behind private and
members-only apps takes an app-scoped ingress token, or the web UI's own
session for an app served under Rise's host, and nothing else. A CLI session,
scoped or not, sent as the `rise_jwt` cookie is treated as no session, so a
scoped login can't reach apps outside its ceiling through the app's cookie.

### 8. Path to the generic resource API

When a typed table migrates (`ROADMAP.md` §4), its scoping moves with it:

1. Register the kind, and its subresources from §3, under the names fixed
   there.
2. Amend ADR-0001 §7 with the §4 `labelSelector` on ceiling entries. Teach the
   engine's `ceiling_for` and `statements_covering` to apply it. In the grant
   gate's domain comparison, a selector entry never covers a whole domain, just
   as a partial Scope doesn't today. Set `rise.dev/environment` as a governed
   label from the resource's environment reference. This happens once, with
   the first environment-bound kind (Deployment or env vars).
3. Build session principals from the session's claim
   (`AuthorizationCap::from_details`) instead of as unrestricted. Drop the §7 refusal once every kind a session can
   name has migrated, or narrow it to the kinds still typed.
4. Delete the migrated kind's arm of the typed check.

Every step keeps the shared session-ceiling cases passing on both evaluators.
Step 2 removes `engine` from the environment grants' `refusedBy` there, and a
kind placed anywhere but directly under its Project changes the engine runner's
resource tree to match.

No session changes meaning along the way: the claim's format, its Scopes and
its statements are already the engine's. Presets may then become data: the
CLI and approval page could offer operator-defined `PlatformRole`s (say
`agent-deploy`) next to the built-in presets. The token still carries expanded
statements, so editing such a role never widens a session already issued.

### 9. CLI surface

```bash
rise login --device --scope my-app=read --scope my-app/staging=develop
rise login --scope my-app=deploy,env-vars          # browser login, scoped
rise login --device --full-access                  # explicit; same as no --scope
```

`--scope <target>=<preset|permission>[,…]` may repeat. `--full-access`
conflicts with `--scope` and exists so that a request for everything is never
implicit in an agent's command line.

When a new login would replace a stored session with broader access, the CLI
warns: "This replaces your current full-access login for <url> on this
machine." Credentials stay one per backend URL, shared by every profile for
that URL. A scoped login replacing a full one is usually what the user wants
when an agent shares their machine.

### 10. Agent guidance

Once §6, §7 and §9 ship, the Rise App Builder skill's login invariant gains a
request step:

- Read the target from the task: the project from `rise.toml`, the environment
  from what the user asked for.
- Request the narrowest preset that does the job: `read` to inspect, `deploy`
  to ship a build, `develop` when env vars change, project `admin` only when
  asked to manage the project itself.
- Pass `--full-access` only when the task needs an install-wide operation, such
  as creating a project, and say why when handing over the link.
- After login, read the printed access. If the user narrowed it and a command
  later fails with 403, ask before requesting more; don't re-request on a loop.

**Scoping protects only what the agent can't read.** An agent with shell access
on the user's machine can read any credential file on that machine. A narrow
session for the agent means little if the user's full session sits beside it.
That's why the scoped login replaces the stored session (§9). The guarantee is
strongest where the agent runs in its own sandbox and only ever holds the
session it requested.

### 11. Explicitly out of scope

- **Organization- or install-level targets** (creating projects, managing
  teams). Deferred until projects are resource-API Organization children, where
  a `rise.dev/Organization/<org>` Scope needs nothing new.
- **Choosing the session lifetime** on the approval page. Scoped sessions keep
  the standard session TTL. A shorter, approver-chosen TTL is the obvious next
  step, and it composes with this design.
- **Revoking a single session.** Sessions remain stateless JWTs. Deactivating
  the approving `UserIdentity` ends every session it minted (ADR-0001 §7),
  which is today's revocation story for every session.
- **Converging typed service accounts onto ceilings.** `scopes` +
  `allowed_environment_ids` express a subset of §3–§4. Re-expressing them as
  `authorization_details` belongs to the ServiceAccount migration
  (`ROADMAP.md` §2, §4), not here.

## Consequences

- One ceiling format covers identity tokens, CLI sessions and, after
  migration, every typed resource. The typed check is a stand-in for engine
  evaluation. The migration deletes it without changing any token's meaning.
- The ceiling is evaluated twice for a while: by the typed check for typed
  kinds and by the engine for generic ones. Both must apply the same coverage
  rule. Shared cases (claim, target, operation → verdict) in
  `crates/rise-backend-auth/testdata/session-ceilings.json` run against both
  and keep them in step; the engine side takes them over at migration.
- Scoped sessions can't use the generic resource API until §8 lands. Anything
  an agent needs from that API in the meantime needs a full-access login.
- Every typed handler moves onto the choke point, a sweeping but mechanical
  change. The route-classification test is what keeps it complete.
- The names in §3 are fixed now but registered only at migration. A migration
  that wants a different name must also translate outstanding session claims.
  Sessions are short-lived, so in practice it waits one session TTL.
- `rise.dev/environment` becomes a platform-governed label, the first one set
  from a typed column. The resource-API migration of Deployment and env vars
  must preserve it.
- Sessions get larger by the size of their ceiling. A typical agent grant
  compiles to three or four entries.
- Device approval gains a decision the user must actually read. The page has to
  keep the default path (approve the request as shown) short, and make full
  access stand out.

## Alternatives considered

**OAuth-style scope strings** (`deploy:my-app:staging`). They are familiar from
other platforms, but they are a second permission language. It would need its
own parser and its own mapping onto RBAC, and its own migration once the typed
tables move. `authorization_details` already exists, is enforced, and is what
RFC 9396 introduced to replace stringly-typed scopes.

**Making Deployment a child of Environment.** It would make environment Scopes
cover deployments with no selector. But deployments without an environment
(preview groups) would need a parent, and ADR-0001's parent model is exact and
immutable, so moving a deployment between environments would become
delete-and-recreate. It would also reopen `ROADMAP.md` §4's placement for every
environment-bound kind. A governed label keeps the tree and adds one narrowing
rule.

**The request binds; the approver can only narrow.** This is safer if the
approver is careless, but it blocks a legitimate case: the agent asks for
`read`, and the user knows it needs to deploy. The approver already holds the
authority, and an unnoticed widening needs the approver to make the change
themselves.

**Personal access tokens** (long-lived, created in the UI, pasted into the
agent). They are simpler to build, but they put a bearer secret in the agent's
transcript. They also outlive the task and need their own storage and
revocation. The device flow keeps the user in the approval loop and the
credential short-lived.

**A separate credential slot per profile for the agent.** That would leave the
user's full session usable next to the agent's narrow one. On a shared machine
the agent can read both files, so the narrow one protects nothing. In a sandbox
there is only one anyway.

## References

- [ADR-0001](./0001-unified-permission-model.md) §2 (subresources), §4
  (Scopes, label selectors), §6.6 (label writes that retarget access), §7
  (authorization details).
- [RFC 9396](https://www.rfc-editor.org/rfc/rfc9396) — OAuth 2.0 Rich
  Authorization Requests.
- [RFC 8628](https://www.rfc-editor.org/rfc/rfc8628) — OAuth 2.0 Device
  Authorization Grant.
- `src/server/auth/device.rs`: device login as it exists today.
