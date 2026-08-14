---
title: "ADR-0003: Release Promotion and Guarded Rollback"
---

## Status

**Proposed.** Date: 2026-08-14.

Scope: promoting a deployment from one environment to another (e.g. staging
→ production) and automatically recovering from a deployment that turns
unhealthy after a successful rollout. Phases 0–1 below (prerequisite fixes,
manual promotion, migration compatibility barriers) are the proposed decision.
Phase 2 (the automatic Rollback Guard) is a **draft direction** within this
same document — the primitive it builds on is decided, but its concurrency
mechanism and a few sequencing details need implementation-time verification
before it is ready to build; see "Open questions" under §5. Phase 3
(promotion policy as a first-class resource) is explicitly not started and is
recorded here only as a tracked dependency on `ROADMAP.md` §1.

This does not change how `rise deploy` (without `--from`) or `rise deploy
--from` work for same-environment redeploys and rollbacks; those keep their
current behavior. It also does not change deployment backend parity
obligations: every capability below applies to both the Kubernetes and Docker
backends, with one documented exception (§5, Docker health-signal fidelity).

## Context

Rise already models environments as a first-class, per-project concept
(`environments` table, one project → many environments, at most one flagged
`is_production`) with their own resource constraints, env var scoping, and
domains. Redeploying a prior build is already possible across environments:
`rise deploy --from <deployment_id> -E <environment>` inherits the source
deployment's image/spec unless overridden, and can target a different
environment than the source. In other words, most of the mechanics a
"promotion" feature needs already exist as an incidental side effect of the
rollback path — nothing today gives it policy, safety checks, or a purpose-fit
interface, and nothing reacts automatically to a deployment's health.

Investigating what a "promote to production" and "automatically roll back an
unhealthy deployment" feature would need surfaced several problems in the
current code that this design has to route around, not just build on top of:

- **No cross-environment source validation.** `create_deployment`'s `--from`
  path resolves the requested target environment independently of the source
  deployment; nothing checks that the source actually came from a declared
  "this is the promotion source" environment.
- **Deployment supersession is unfenced.** When a deployment reaches
  `Healthy`, the webhook (`src/server/deployment/webhook.rs`) supersedes every
  other active deployment in its group unconditionally — last-Healthy-wins,
  with no ordering check against creation time. A feature that creates more
  deployments automatically exercises this race far more than manual use does.
- **No health-driven state transition exists yet.** The `DeploymentStatus`
  state machine (`crates/rise-backend-core/src/state_machine.rs`) allows
  `Unhealthy → Failed`, but no code implements a timeout that performs it, on
  either backend. `Failed` deployments have no infrastructure (`webhook.rs`'s
  `statuses_without_infra` and the Docker reconciler's desired-state skip both
  treat `Failed` as torn down) — so naively wiring a timeout straight to
  `Failed` converts a partially-serving deployment into a full outage before
  any replacement is ready.
- **`first_healthy_at` cannot express "continuously healthy."** It is set once
  via `COALESCE` and deliberately preserved across `Unhealthy` flaps (with a
  test asserting exactly that), so it cannot answer "has this been healthy for
  N minutes without interruption."
- **`rolled_back_from_deployment_id` is not audit metadata — it drives image
  resolution.** `get_deployment_image_tag`
  (`src/server/deployment/utils.rs`) and `can_create_from`
  (`crates/rise-backend-core/src/state_machine.rs`) both key off it. A new
  feature that wants its own provenance field cannot simply stop setting this
  one; doing so produces an unpullable image tag for build-from-source
  deployments.
- **`--use-source-env-vars` copies Extension-provenance secrets verbatim,**
  including credentials that may have rotated since the source deployment ran
  — and, independent of this design, it already causes a live bug: on any
  project with an active extension, the raw copy collides with the extension
  hook's fresh insert and returns a 500.
- **Environment settings are already admin-gated** — `deployment_constraints`
  on `Environment` require project-admin authority
  (`src/server/environments/handlers.rs`), which is the precedent this design
  reuses for promotion-policy fields, rather than inventing a new
  authorization tier.
- **The generic resource API is real but not yet a fit for this feature.** It
  is fully shipped for storage and validation (built-in and external
  `ResourceDefinition` kinds), but access is operator-only
  (`require_operator`) until the org-scoped RBAC work in `ROADMAP.md` §1
  lands. A user-facing, project/team-owned policy object built on it today
  would either need operator involvement to configure or a hand-rolled authz
  shim in front of the generic store — ahead of, and likely diverging from,
  that work.

This design was converged through several rounds of adversarial review against
the actual code, not just the model above. Multiple earlier drafts of this
ADR were rejected outright on exactly these grounds — seeing "auto-rollback
timeout" reach `Failed` before a replacement exists, or "reuse
`rolled_back_from_deployment_id`" break image-tag resolution, is what forced
the redesigns recorded under "Alternatives considered" below.

## Decision

### 1. One primitive, three callers

The underlying operation is: **create a deployment in environment `E`,
sourced from deployment `D`.** This already exists (`--from`). Promotion, the
automatic Rollback Guard (§5), and any future policy-driven auto-promotion
(§6) are not three different mechanisms — they are three different *callers*
of the same operation, differing only in who or what supplies `D` and `E`: a
human for manual promotion, a health-driven controller for the Rollback
Guard, and eventually a declarative policy evaluator. Manual promotion is not
superseded by future automation; it remains the always-available override
once a policy exists to supply `D` automatically.

### 2. Prerequisite hardening (Phase 0)

Ship independently of the rest, because automation increases how often the
existing gaps are hit:

- **Fence supersession.** `handle_deployment_became_healthy` must only
  supersede deployments *older* (by creation order) than the one that just
  became healthy, not every active deployment unconditionally. Without this,
  an automatic rollback racing a concurrent human fix can supersede the fix.
- **Fix the env var copy.** Filter `Extension`/`System`-provenance rows out of
  the deployment-to-deployment env var copy path used by `--use-source-env-vars`.
  `System` and `Extension` vars are already recomputed fresh elsewhere in that
  same code path (PORT upsert, extension `before_deployment` hooks), so this
  is a filter, not new plumbing — and it incidentally fixes the existing
  extension/`--use-source-env-vars` 500.
- **Make `--from` transparent.** Print the source deployment ID, image
  digest, and an env-var diff on any `--from` deploy, not only on `rise
  promote`. Otherwise `promote` becomes the only verb that *feels* safe and
  `--from` becomes the de facto unsafe escape hatch by omission — recreating,
  through the CLI's own asymmetry, the dual mental model this design is meant
  to avoid.

### 3. Manual promotion (Phase 1)

**Explicit source deployment, not auto-resolved.** `rise promote <project>
<deployment-id> [--target-environment production]` takes the source
deployment ID as a required argument. There is deliberately no "promote
whatever is currently Healthy in staging" default: that auto-resolution
cannot distinguish a deployment that has soaked for weeks from one that
finished deploying ten seconds ago. Requiring the ID makes the act of
choosing it the soak-time judgment call, for the human path. `--target-environment`
follows the project's existing environment-resolution conventions (a single
non-source environment may default; two or more require the flag).

Given the source deployment ID, promotion:

- **Validates the source environment.** If the target environment has a
  configured promotion-source policy (below), the given deployment's
  `environment_id` must match it; otherwise the request is rejected. This
  closes the gap noted in Context — nothing today checks that a `--from`
  source actually came from the environment a caller claims.
- **Digest-pins the image** at promote time via the existing OCI client,
  rather than only inheriting the source's `image`/`image_digest` fields
  as-is — the promoted deployment must resolve to a concrete pullable image
  regardless of what happens to the source deployment's row afterward.
- **Resolves resources fresh against the *target* environment**, not the
  source: replicas/cpu/memory (and, for multi-container deployments, the
  per-container resource JSON) are computed against the target environment's
  own min/max constraints and clamped into range, rather than blindly
  inherited from the source and left to 400 if staging and production have
  different constraints. Plain same-environment `--from` rollback is
  unchanged — it keeps inheriting from the source, as today.
- **Recomputes env vars against the target environment's current
  configuration** by default (this *is* the point of promoting — running the
  build in production's config, not staging's), with an explicit override for
  the rare case where exact parity with the source is wanted.
- **Records provenance separately from image lineage.** New
  `promoted_from_deployment_id` (and `promoted_from_environment_id`, for
  cheap querying without walking a chain) plus a `created_via` enum value
  (`Manual`, `Promotion`, `AutoRollback`) are added *alongside*
  `rolled_back_from_deployment_id`, which keeps its existing job unchanged.
  This also gives a future policy evaluator (§6) exactly what it needs to
  answer "what's live in production and what did it come from" without
  redesigning storage later.
- **Prints the full diff and confirms.** Source deployment ID, image digest,
  and env-var diff are shown before proceeding; an interactive TTY confirms,
  `--yes` skips confirmation for CI.

**Promotion-source policy is a server-side, admin-gated `Environment`
setting**, not a `rise.toml` field: `rise environment update production
--promotion-source staging`, gated the same way `deployment_constraints`
already is — ordinary project members cannot edit it, closing the "anyone can
just delete the policy" hole a client-editable file would have. When set, it
also governs *direct* deploys: `rise deploy -E production` (bypassing
`promote` entirely) is rejected with a pointer to `rise promote`, unless the
caller is an admin (consistent with the existing admin-bypass convention on
typed APIs).

**Explicitly deferred, not designed here:** `require_approval`/`approvers`.
An approval gate needs pending-deployment state, an approver identity model,
and API/UI surface that do not exist anywhere in the codebase today — it is
its own workstream, not a flag on this one. Phase 1 ships source-environment
gating only.

**Explicitly operator-impacting.** Enabling promotion-source gating on an
environment changes the documented CI/CD pattern
(`docs/user/src/content/docs/user-guide/ci-cd.md`) where a production service
account deploys a fresh image directly. This must ship opt-in per environment,
with `ci-cd.md` updated to document the gated pattern and an entry added to
the operator Upgrade Notes page, per this repo's rollout-tracking
conventions.

**No new CLI noun family yet.** `rise promote` is not sugar for a `rise
promotion create` command backed by a durable `Promotion` resource — there is
no approval workflow yet to make such a resource meaningful, and inventing
one now would only have to be reconciled with whatever shape the approval
workstream needs later. `rise promote` is a real command in its own right; if
and when approval ships, a durable resource can be introduced deliberately at
that point.

### 4. Migration compatibility barriers

Neither of the two obvious approaches survived review: a CLI flag nobody
remembers to pass on automated deploys, and a `rise.toml` boolean that must be
manually reset after every migration or it wrongly blocks every later
rollback. Both are toggles requiring active, ongoing maintenance. The
replacement has no toggle at all:

- **`Deployment.migration_version: i64`** — the schema/migration version a
  deployment's code was built against, monotonically non-decreasing per
  project. Declared explicitly (`[migrations] version = N` in `rise.toml`,
  bumped by whoever authors the migration, in the same commit) rather than
  inferred from a git diff — every migration tool already numbers its
  migrations sequentially, so this surfaces a number that already exists
  rather than adding new git-plumbing dependencies.
- **An append-only, project-scoped `migration_barriers` ledger**: rows of
  `{version, reason, recorded_at}`. Entries are only ever added — a breaking
  migration's author adds one entry in the same commit; nothing is ever
  toggled back off, because there is nothing to unset. The next migration
  that isn't breaking simply doesn't add a new entry.
- **The compatibility check is a range query, not a flag read.** Moving a
  deployment at `migration_version = A` to run against a database at version
  `B` (`B ≥ A`, since versions only increase) is blocked iff any ledger entry
  falls in `(A, B]`. The default — no entries in range — is "compatible," so
  the common case needs no action from anyone. This check runs across the
  *full* chain-flattened lineage a `--from`/promotion/rollback walks, not
  only the immediate predecessor.
- Crossing a barrier automatically (Rollback Guard, §5) is a hard block.
  Crossing one manually (`--from`) prints a loud warning and requires
  `--force`.

### 5. Guarded automatic rollback (Phase 2 — draft direction)

The core decision is settled; the concurrency mechanism and a few sequencing
details need to be nailed down against the actual webhook/reconciler code at
implementation time (see "Open questions" below) before this phase is ready
to build.

- **The trigger is scoped to guard-enabled environments only**, and never
  redefines what `Failed` means platform-wide. Rise currently documents that
  it never auto-fails a long-`Unhealthy` deployment; some operators rely on
  that. A new `unhealthy_since` timestamp (not `first_healthy_at`, which is
  preserved across flaps by design) tracks continuous unhealthiness, and the
  timeout it drives only fires where an operator has explicitly opted an
  environment into the guard.
- **Fire before teardown, not after.** The single most important correction
  from review: triggering at `Unhealthy → Failed` is too late, because
  `Failed` deployments already have no infrastructure — the sequence would be
  degraded service → timeout → *zero* service → cold redeploy, manufacturing
  the outage the feature exists to prevent. Instead, the guard creates the
  last-known-good replacement deployment **at** the unhealthy-timeout, while
  the sick deployment is left running exactly as it is. The **existing**
  Healthy-triggers-supersede mechanism then retires the sick deployment once
  the replacement reaches Healthy — no new teardown logic, reusing the
  zero-downtime path that already exists for ordinary redeploys. If the
  replacement also fails to reach Healthy, the guard stops — no further
  automatic chaining beyond this one hop — and raises a human-visible alert;
  it does not force-kill anything further.
- **Concurrency needs a real mechanism, not a status-existence check.**
  Rollback-style deployment rows are inserted with status `Pushed`, not
  `Pending`/`Deploying` — a naive "does a Pending/Deploying auto-rollback
  already exist" idempotency check misses rows the guard itself just
  created and double-fires. The mechanism must be a partial unique index (one
  auto-rollback candidate per source deployment) plus status-guarded
  compare-and-swap updates (`UPDATE ... WHERE status = 'Unhealthy'`) rather
  than an unconditional `UPDATE ... SET status = ...`, so a second concurrent
  trigger loses at the database rather than racing in application code.
- **Env vars: freeze user input, recompute credentials.** The replacement
  deployment freezes the last-known-good deployment's own recorded
  `User`/`Toml`/`Cli`-provenance vars (so behavior matches what was actually
  proven to work) but recomputes `System`/`Extension`-provenance vars fresh,
  so a rollback cannot resurrect a rotated credential. Which vars were frozen
  vs. recomputed is always printed/logged.
- **Migration barriers (§4) gate this path unconditionally** — a barrier
  found in range blocks the automatic rollback outright; there is no
  `--force` equivalent for an unattended action.
- **Observability covers inaction, not just action.** `created_via` and a
  human-readable reason are shown in `rise d ls`/`rise d s` and the web UI for
  every automated deployment. Cases where the guard *declines* to act
  (barrier hit, no eligible predecessor, one-hop cap already spent) are
  logged too — a guard that only reports when it did something is worse than
  one that never existed, because it advertises safety it isn't providing in
  exactly the cases where it matters most.
- **Backend parity.** The guard consumes each backend's own readiness signal.
  Kubernetes' is native (pod readiness, already used by the existing
  Healthy/Unhealthy webhook logic). Docker's is Traefik-dependent with no
  fallback — the guard's fidelity differs, and can be entirely unavailable,
  on a Docker deployment without `traefik_api_url` configured. This must be
  documented as a fundamental backend limitation in the deployment-backends.md
  feature matrix in the same change that ships it, per this repo's parity
  rule — it is not something to silently leave weaker on Docker.

**Open questions before this phase moves out of draft:**

- Exact atomicity of the trigger: does the health-transition handling code
  path (K8s webhook request handling; Docker reconciler's leader-elected
  loop) actually provide enough to make the partial-unique-index approach
  sufficient, or does it need an explicit transaction/lock in addition?
- Precise selection rule for "last-known-good": most recent deployment in the
  group/environment that reached `Healthy` and has not itself been marked
  `Failed`, is the working definition — needs to be pinned down as precisely
  as `can_create_from` today, and tested.
- Whether admins bypass promotion-source gating on `rise deploy` the same way
  they bypass other typed-API checks, or whether this specific gate should be
  stricter — this is a real product decision, not an implementation detail.
- What a blocked direct `rise deploy -E production` actually returns to a CI
  job (error message content, exit code) so a gated pipeline fails
  legibly instead of hanging or looking like a transient error.

### 6. Storage sequencing: typed fields now, generic resource store later

Promotion-source policy, the migration barrier ledger, and the Rollback
Guard's configuration are implemented as ordinary typed Postgres columns/small
tables now, admin-gated the same way `deployment_constraints` already is —
**not** as a new generic-resource-store kind yet. The generic resource API's
access model is operator-only until the "centralized authorization choke
point replacing `require_operator`" work in `ROADMAP.md` §1 ("Unified
identity and RBAC") lands; building a project/team-owned policy object on it
today would mean either wrongly requiring operator access to configure a
per-project promotion policy, or hand-rolling a bespoke authorization shim in
front of the generic store ahead of, and likely diverging from, that
already-planned design.

Once that work lands, **a first-class `PromotionPolicy` resource — encoding
promotion criteria and driving automatic promotion, per the primitive in §1 —
is the natural first product feature to launch natively on the generic
resource store**, rather than a bespoke table migrated onto it later the way
`Environment`/`Deployment` are being migrated per `ROADMAP.md` §4. This is
recorded here as an explicit, tracked dependency; no implementation work on
it begins before the prerequisite RBAC work ships.

## Rollout

Status legend matches `ROADMAP.md`: `[x]` shipped · `[~]` in progress ·
`[ ]` planned.

**Phase 0 — prerequisite hardening**
- [ ] Fence deployment supersession to only supersede older deployments.
- [ ] Filter `Extension`/`System`-provenance vars out of the deployment env
  var copy path (also fixes the existing `--use-source-env-vars` +
  active-extension 500).
- [ ] Print source deployment ID/digest/env-var diff on every `--from` deploy.

**Phase 1 — manual promotion + migration barriers**
- [ ] `migration_version` (rise.toml-declared) and the append-only
  `migration_barriers` ledger.
- [ ] `rise promote <project> <deployment-id>`: source-environment
  validation, digest-pinning, target-environment-relative resource
  resolution, target-environment env var recompute, confirmation/diff output.
- [ ] `promoted_from_deployment_id`/`promoted_from_environment_id`/`created_via`
  fields, kept independent of `rolled_back_from_deployment_id`.
- [ ] Admin-gated `promotion_source_environment_id` on `Environment`, enforced
  inside `create_deployment` for every deploy targeting a gated environment.
- [ ] `ci-cd.md` update documenting the gated-production pattern; Upgrade
  Notes entry (operator-impacting).

**Phase 2 — guarded automatic rollback** (blocked on resolving the open
questions in §5; not yet ready to implement)
- [ ] `unhealthy_since` primitive, scoped to guard-enabled environments.
- [ ] Event-driven trigger with verified concurrency guarantees (partial
  unique index + status-guarded CAS).
- [ ] Fire-before-teardown sequencing, reusing existing supersede-on-healthy.
- [ ] Env var freeze/recompute split for the guard's replacement deployment.
- [ ] Observability: `created_via`, reason, and decline-to-act logging.
- [ ] `deployment-backends.md` parity note for Docker's Traefik-dependent
  health signal.

**Phase 3 — `PromotionPolicy` as a generic resource** (not started; blocked on
`ROADMAP.md` §1's centralized authorization work landing)
- [ ] No work begins before the prerequisite lands.

## Consequences

**Positive.** Promotion reuses the deploy pipeline's existing correctness
work (backend parity, env var resolution, resource validation) instead of
duplicating it; the same primitive serves manual and (later) automated
callers without a rewrite. The migration-barrier ledger is exact and
maintenance-free in the common case. The Rollback Guard, once out of draft,
reuses zero-downtime supersession that already exists rather than building
new teardown/traffic-shift machinery. Fixing supersession fencing and the env
var copy bug benefits the platform independent of whether the rest of this
ADR ships.

**Negative.** Promotion-source gating is a real behavior change to a
documented, currently-unrestricted deploy path, and requires operator/user
migration (docs, Upgrade Notes) wherever a project opts in. The migration
barrier ledger asks migration authors to do one extra thing (bump a version,
occasionally add a ledger entry) that nothing enforces they remember for a
*genuinely* breaking change with no detectable signal — this is a
process/discipline gap, not a solved problem, same as the equivalent gap in
every migration-compatibility scheme that isn't fully automated. `created_via`
and `promoted_from_*` are new columns on an already-migrating model
(`Deployment` is in `ROADMAP.md` §4's typed-object migration path) — Phase
1's schema additions should be reviewed against that migration's timing so
they aren't done twice.

**Risk.** The Rollback Guard (§5) is the highest-risk component: an
automation that creates deployments in reaction to health signals is
exactly the kind of thing that turns a partial outage into a full one, or
loops, if the concurrency and sequencing details are wrong — which is why it
stays in draft with explicit open questions rather than shipping alongside
Phase 1. Gating direct `rise deploy -E production` changes a documented,
currently-load-bearing CI/CD pattern; rolling it out requires real
communication to operators, not just a docs update landing alongside the
code.

## Alternatives considered

- **Auto-resolve "current Healthy deployment in the source environment" as
  the promotion source**, instead of requiring an explicit deployment ID.
  Rejected: nothing distinguishes a deployment that has soaked for weeks from
  one that finished ten seconds ago; this is the exact footgun the explicit-ID
  interface exists to remove.
- **A boolean `migration_barrier` flag**, set via CLI flag or `rise.toml`.
  Rejected on both fronts: a CLI flag nobody remembers to pass on automated
  deploys, and a `rise.toml` toggle that must be actively unset after each
  migration or it wrongly blocks every subsequent rollback — the same
  "someone must remember" failure mode, just relocated to a different
  artifact. The version-plus-ledger design needs no unsetting step.
- **Git-diff-based automatic barrier detection** (compare a configured
  migrations path between two deployments' commits). Rejected/deferred:
  requires new `git_commit_sha` capture (not present on `Deployment` today),
  is fragile across build tools and migration frameworks, and is strictly
  more complex than an explicitly declared, monotonic version number that
  migration tooling already produces.
- **Reusing `rolled_back_from_deployment_id` for promotion audit metadata**,
  treating it as symmetric with a new `promoted_from_deployment_id`. Rejected
  after direct code verification: the field drives image-tag resolution and
  `can_create_from` eligibility; omitting it on promotion-created deployments
  produces an unpullable image tag, and reusing it conflates image lineage
  with feature provenance, breaking the Rollback Guard's one-hop cap in the
  process.
- **Triggering the Rollback Guard on `Unhealthy → Failed`, tearing down
  first.** Rejected after direct code verification that `Failed` deployments
  have no infrastructure on either backend — this sequencing manufactures a
  full outage out of a partial one, the opposite of the feature's purpose.
- **Building `PromotionPolicy` on the generic resource store immediately.**
  Rejected for now: the generic API is operator-only until `ROADMAP.md` §1's
  RBAC work lands; building a project-owned policy object on it today means
  either the wrong access model or a bespoke authorization shim likely to
  diverge from that already-planned design. Recorded as an explicit future
  step instead (§6).
- **A `rise promotion create/list/approve` resource family, with `promote` as
  sugar**, mirroring `rise deployment`/`rise environment`. Rejected for Phase
  1: without an approval workflow, there is no durable object for such a
  resource to represent; introducing one now would likely be redesigned once
  the approval workstream is scoped. `rise promote` ships as a real command,
  not a sugar alias for a resource that doesn't exist yet.

## References

- [ADR-0001: Unified Permission Model](./0001-unified-permission-model.md) —
  precedent for admin-gated policy fields on typed resources
  (`deployment_constraints`), reused here for promotion-source policy.
- `ROADMAP.md` §1 (Generic resource and authorization foundation) — the
  centralized-authorization-choke-point item this ADR's §6/Phase 3 is blocked
  on.
- `ROADMAP.md` §4 (Typed-object migration) — sequencing note for the new
  `Deployment` columns in Phase 1.
- `docs/user/src/content/docs/user-guide/deployments.md`,
  `environments.md`, `environment-variables.md`, `ci-cd.md` — current
  documented behavior this design extends or changes.
- `docs/engineering/src/content/docs/deployment-backends.md` — feature-matrix
  update required alongside Phase 2.
