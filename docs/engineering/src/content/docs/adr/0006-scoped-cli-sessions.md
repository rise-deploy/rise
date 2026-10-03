---
title: "ADR-0006: Scoped CLI Sessions"
---

## Status

**Proposed**. Date: 2026-10-03.

Not implemented. The agent-facing half that works with today's CLI has
shipped: the Rise App Builder skill tells coding agents to log in with
`rise login --device` and hand the verification URL to the user. Everything
below, including the request flags the skill will gain, is design.

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
change them.

## Decision

### 1. One ceiling grammar: sessions carry `authorization_details`

A CLI session token may carry ADR-0001 §7's `authorization_details` claim,
with the same parser (`AuthorizationCap::from_details`) and the same semantics:

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
`rise.dev/rbac` entries. The same token then means the same thing on today's
typed handlers and on the generic resource API once Project and Environment
migrate (`ROADMAP.md` §4). The migration changes where a decision is evaluated,
not what a token permits.

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
An organization-level target is deferred (§10).

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
name under ADR-0003) as the names those tables migrate to. They are registered
in the built-in kind registry ahead of their storage migration so the shared
Scope and ResourceKind parsers accept them. Registration fixes their identity
for policy data. The resource API serves no routes for them until each one
migrates. `logs`, `value` and `registry-credentials` join `status`,
`finalizers` and `token` in the subresource registry, as ADR-0001 §2 and
ADR-0002 anticipate.

Presets name common bundles. The CLI and the approval page offer presets first
and individual permissions second:

| Preset | Permissions | Allowed on |
|---|---|---|
| `read` | `view` | project, environment |
| `deploy` | `view`, `logs`, `deploy` | project, environment |
| `develop` | `view`, `logs`, `deploy`, `env-vars` | project, environment |
| `admin` | all eight | project |

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

**ADR-0001 §7 is amended:** a `rise.dev/rbac` entry may carry an optional
`labelSelector`, with ADR-0001 §4's grammar restricted to the static form (a
`value` is required):

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
  both before and after. Moving an env var from `staging` to `production` needs
  both environments, as ADR-0001 §6.6 requires of any label write that retargets
  access.

An environment grant compiles to three entries:

1. Scope `rise.dev/Project/<org>/<p>`, permission `get` on `Project` only, so
   the CLI can resolve the project by name.
2. Scope `rise.dev/Environment/<org>/<p>/<e>`, permission `get` on
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
- **Stopping a deployment group** (`POST …/deployments/stop`) is covered only
  when every deployment it would stop is bound to a covered environment.

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
today. With `--scope`, the CLI sends `access` on
`POST /auth/authorize {flow: "code"}`. The server validates it syntactically,
keeps it with the transient PKCE state, compiles it at code exchange (the user
is authenticated then), and mints it into the session. The user typed the
flags, so no extra consent page is shown. A target the user can't access fails
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
- flags targets that don't exist or that the approver can't access, and blocks
  approval while any remain;
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
   `AuthorizationCap`. The typed adapter builds the target's `ResourceTree`,
   with effective labels including the synthesized `rise.dev/environment`, and
   calls the same `ceiling_for` the engine uses.

Rules:

- **The ceiling applies to admins.** The admin bypass skips step 1, never step
  2. A scoped session of an admin user is as narrow as anyone's.
- **Collections are filtered, not refused.** `GET /projects`, deployment lists
  and env var lists return only items the ceiling covers, without a 403. This
  is the same masking ADR-0001 §4 applies to `list`.
- **Every route is classified.** Each typed route is either
  *resource-targeted* (it goes through the choke point) or *ambient*. Ambient
  routes are reachable by any session: `GET /users/me`, platform capabilities,
  extension types, quickstart templates, access classes, the device endpoints.
  A test walks the router and fails on any unclassified route. A new endpoint
  is denied to scoped sessions until someone classifies it.
- Install-wide writes (project create, teams, `/encrypt`) are classified as
  needing an unrestricted session.

The resource API needs no new enforcement: it already intersects every
decision with the principal's cap. The work there is building the
`AuthenticatedPrincipal` for a session from the session's claim rather than
`Unrestricted`.

### 8. CLI surface

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

### 9. Agent guidance

Once §6–§8 ship, the Rise App Builder skill's login invariant gains a request
step:

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
That's why the scoped login replaces the stored session (§8). The guarantee is
strongest where the agent runs in its own sandbox and only ever holds the
session it requested.

### 10. Explicitly out of scope

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

- One ceiling model covers identity tokens, CLI sessions and, after migration,
  every typed resource. The typed adapter is a stand-in for engine evaluation.
  The migration deletes it without changing any token's meaning.
- Every typed handler moves onto the choke point, a sweeping but mechanical
  change. The route-classification test is what keeps it complete.
- Four kinds are registered before their storage exists, and three subresource
  names before their routes do. That fixes their policy identity early,
  including in RoleBindings someone could write against them now.
- `rise.dev/environment` becomes a platform-governed label, the first one set
  from a typed column. The resource-API migration of Deployment and env vars
  must preserve it.
- ADR-0001 §7 gains `labelSelector` on ceiling entries. The engine's
  `ceiling_for` and `statements_covering` must apply it. In the grant gate's
  domain comparison, a selector entry never covers a whole domain, just as a
  partial Scope doesn't today.
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
