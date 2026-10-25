---
title: "ADR-0007: Release Promotion and Guarded Rollback"
---

## Status

**Implemented (Phases 0–2).** Proposed 2026-08-14; implemented 2026-10-25.
Phase 3 (`PromotionPolicy` as a generic resource) has not started (§6).

Scope: promoting a deployment between environments (e.g. staging → production)
and automatically recovering from a deployment that goes unhealthy after a
successful rollout. Same-environment `rise deploy` and `rise deploy --from`
keep their behavior apart from the Phase 0 fixes. Backend parity applies
throughout; the one documented limitation is the Rollback Guard's health
signal (§5).

Code: `server::deployment::promotion` (the primitive),
`server::deployment::rollback_guard`; migrations
`20261025000002_add_release_promotion` and `20261025000003_add_rollback_guard`.

## Context

Environments, per-environment config and cross-environment redeploys
(`rise deploy --from <id> -E <env>`) already existed, without policy, safety
checks or a purpose-built interface. Nothing reacted to health. Grounding the
design in the code surfaced these constraints:

- `--from` never checked that the source came from a declared source
  environment.
- Supersession on `Healthy` was unfenced (last-Healthy-wins) in the supersede
  query, the straggler cleanup and the Docker/ECS Traefik cutover: an older,
  slow rollout could take routing from a newer one. Automation would hit this
  race far more often than humans.
- Nothing reacted to sustained `Unhealthy`. `first_healthy_at` survives flaps
  by design, so it can't express "continuously healthy". `Failed` deployments
  have no infrastructure on any backend, so a timeout straight to `Failed`
  manufactures an outage before a replacement exists.
- `rolled_back_from_deployment_id` is not audit metadata: it points at the
  chain-flattened *image origin* and drives image-tag resolution, URL
  building, `can_create_from`, Git-provenance inheritance and the creation
  event. Provenance needs its own fields.
- `--use-source-env-vars` copied Extension and System values verbatim (stale
  credentials on rollback), and returned a 500 on projects with active
  extensions (unique-key collision).
- A multi-container `--from` inside a checkout sent containers from the local
  `rise.toml` without images; the server persisted push tags never pushed.
- Admin-gated `deployment_constraints` on `Environment` is the precedent for
  promotion policy.

## Decision

### 1. One primitive, many callers

The operation is: create a deployment in environment `E` from deployment `D`.
Redeploy, rollback, promotion, the Rollback Guard and a future
`PromotionPolicy` only differ in who supplies `D` and `E`.
`promotion::create_from_source` writes the row, its complete env-var
materialization and its creation event in **one transaction**, so a `Pushed`
row never exists without its variables. Extension hooks run before the
transaction, so a failing hook creates nothing instead of a `Failed` row.

There is no new creation endpoint: promotion is `POST /api/v1/deployments`
with `from_deployment` and a different `environment`. Any cross-environment
create-from **is** a promotion (`rise promote`, `rise deploy --from X -E
other`, the UI), never a separate unchecked path. A read endpoint,
`GET /projects/{p}/deployments/{id}/plan`, previews the gate decision,
migration check, pinned image and resource adjustments; the CLI and UI show it
next to the env-var diff.

### 2. Prerequisite hardening (Phase 0)

- **Fence supersession by `(created_at, id)`** on all backends: a deployment
  turning Healthy supersedes only older ones; if a newer one already serves
  the group, the late one is retired (`Superseded`, `superseded_by` event); the
  straggler cleanup retires only older ones; the Docker/ECS cutover never
  activates an older one.
- **Filter the snapshot copy.** `--use-source-env-vars` copies everything
  except `system` and `extension` values, which are recomputed (fixing the 500).
- **Preview every `--from`**: source ID, image/digest and env-var diff.
- **Multi-container `--from`**: the CLI sends no containers or routes; the
  server rejects image-less containers on a create-from.

### 3. Manual promotion (Phase 1)

`rise promote <deployment-id> [-p project] [-E target]` (the project comes
from `-p`/`rise.toml` like every other command). The source ID is required and
never auto-resolved, so soak time is a deliberate choice. The target defaults
to the production environment. Promotion:

- **applies the promotion-source gate** (below);
- **digest-pins** every container image;
- **sizes against the target**: explicit values (flags or
  `[environments.<target>.deploy]`) are validated as usual; inherited values
  are *clamped* into the target's range and reported as adjustments
  (per container for cpu/memory; out-of-range total replicas fail with 400);
- **recomputes env vars against the target**, later layers winning: the
  target's Global and `env:<target>` vars; the source's `toml`/`cli` vars minus
  other environments' env-scoped toml vars (the new `toml:<env>` source);
  fresh extension vars; request overrides; `PORT` and `RISE_*`.
  `use_source_env_vars` across environments is rejected with 400;
- **records provenance**: `created_via` (`deploy` / `redeploy` / `promote` /
  `rollback_guard`), `promoted_from_deployment_id` (the *immediate* source)
  and `promoted_from_environment_id`, separate from
  `rolled_back_from_deployment_id`;
- **prints plan and diff, then confirms** (`--yes` skips; non-interactive
  without `--yes` refuses).

A `--scope` session promoting across environments must also be allowed to
read deployments in the source environment; otherwise it could copy that
environment's `rise.toml`/CLI secrets into one it can deploy to.

**Promotion-source policy** is `Environment.promotion_source_environment_id`,
admin-set like `deployment_constraints`, not in `rise.toml`. The FK keeps it
within the project; cycles are refused. An environment with source `S` admits
a deployment iff it is created from a deployment in `S` or in the target
itself (redeploys/rollbacks of something already admitted), or the caller is
an **unscoped human admin**. Fresh builds, `--image` and promotions from
elsewhere are denied:

```json
HTTP 403
{"error": "Environment 'production' only accepts deployments promoted from 'staging'. Promote a staging deployment: rise promote <deployment-id> -E production",
 "error_code": "promotion_source_required",
 "details": {"environment": "production", "promotion_source_environment": "staging"}}
```

The admin bypass never applies to service accounts, scoped sessions or the
guard; it is recorded on the creation event (`promotion_gate_bypassed`) and
returned as a warning. The gate covers the whole environment, preview groups
included. An environment that is another's promotion source can't be deleted
(409) until that policy is cleared.

Approvals (`require_approval`/`approvers`) are out of scope, and so is a
`promotion` CLI noun or resource: without an approval workflow there is
nothing durable to represent.

### 4. Migration compatibility barriers

A boolean "breaking migration" flag would need resetting after each migration
or block every later rollback. Barriers use a version and a ledger:

- **`Deployment.migration_version`** (nullable `BIGINT`), declared as
  `[project].migration_version` in `rise.toml` and bumped by whoever authors a
  migration. Every create-from inherits the source's value: it is a property
  of the image.
- **An append-only, project-scoped `migration_barriers` ledger**
  (`version, reason, recorded_by, recorded_at`; a trigger refuses `UPDATE`),
  recorded with `rise project migration-barrier add`. Owners and members
  record barriers, not only admins: a barrier only *restricts*, so it
  escalates nothing, and gating it on admins would push "remember to record
  it" onto someone who doesn't author migrations.
- **The environment's high-water mark `B`** is the highest `migration_version`
  among its deployments that reached `Deploying` (so may have migrated).
- **A candidate at `A`** passes when `A ≥ B` or `B` is unknown (nothing
  declared has started there, even for an undeclared candidate); is `unknown`
  when `A` is NULL (a warning on manual creates); and is blocked iff a ledger
  entry `v` has `A < v ≤ B`.

The check applies to every create, fresh builds included. Crossing a barrier
returns 409 `migration_barrier` with
`details {candidate_version, environment_version, barriers[]}`; it can be
overridden with `force_migration_barrier` (CLI `--force`, UI "Deploy anyway"),
recorded as `migration_barrier_forced`. The guard never crosses a barrier.

### 5. Rollback Guard (Phase 2)

Opt-in per environment (`rollback_guard_enabled`,
`rollback_guard_unhealthy_timeout_seconds` 60–86400, default 300, admin-set);
a leader-elected loop (`rise-rollback-guard`) behind the operator kill switch
`rollback_guard.enabled`. It never changes `Failed` semantics.

**Signal.** `unhealthy_since` is set on entering `Unhealthy`, kept while there
and cleared on recovery: *continuous* unhealthiness. All backends write health
through the same store calls, so it has parity by construction.

**Subject.** The active deployment `S` of a guard-enabled environment's
**primary** group (preview groups are not guarded), `Unhealthy` for at least
the timeout.

**Last-known-good.** `L` is the most recent deployment in the same project,
environment and group, older than `S` by `(created_at, id)`, that has served
(`first_healthy_at` set). The guard acts only if `L` is `Superseded` (replaced,
not stopped, expired or failed), `can_create_from(L)` holds, and `L` crosses no
barrier (and has a known version whenever the project has barriers).

**Fire before teardown.** The replacement is created with the primitive while
`S` keeps running; fenced supersession retires `S` once the replacement is
Healthy, so no teardown logic is needed. It records `created_via =
'rollback_guard'` and `rollback_guard_for_deployment_id = S`, and takes
`created_by_id = L.created_by_id` (there is no system user; `created_via` and
the event are the authoritative attribution). Env vars freeze `L`'s snapshot
except `system`/`extension`, which are recomputed with `PORT`/`RISE_*`. Sizing
is `L`'s clamped into current constraints; the image is `L`'s, not re-pinned.

**Trigger atomicity.** At most **one** replacement per unhealthy deployment,
ever (partial unique index on `rollback_guard_for_deployment_id`). The
transaction takes `SELECT … FOR UPDATE` on `S` and re-checks the subject
predicate (still Unhealthy, active, past the timeout, no candidate). Health
writers lock the same row, so either the recovery or the candidate commits
first. A stale leader's duplicate insert fails on the index and counts as
already triggered.

**Abort on recovery.** If `S` recovers, or is stopped, superseded or otherwise
leaves the active set, before the candidate is Healthy, the candidate is
cancelled.

**One hop.** The guard never acts on its own replacements (`chained`). If a
candidate fails, the index prevents a second: a `failed` event (severity
error) is recorded on `S`, which keeps running.

**Declining is surfaced.** Every decision is a deduplicated `rollback_guard`
event on `S` with `outcome` `triggered` / `declined` / `aborted` / `failed`.
Decline reasons: `chained`, `platform_wide_unhealthy`, `in_flight_limit`,
`newer_deployment_in_progress` (a human fix is underway),
`no_last_known_good`, `last_known_good_not_superseded`,
`last_known_good_unavailable`, `migration_barrier`,
`migration_version_unknown`.

**Fleet circuit breaker.** On Docker and ECS, health with a `health_check` is
Traefik's `serverStatus`, so a Traefik API outage makes every health-checked
deployment Unhealthy at once. The guard skips the tick
(`platform_wide_unhealthy`) when at least `fleet_unhealthy_min_count` (3)
active deployments are Unhealthy *and* they are at least
`fleet_unhealthy_ratio` (0.5) of them. `max_in_flight` (3) caps open
candidates.

**Parity.** The decision logic is shared; the health signal is not.
Kubernetes uses pod readiness plus irrecoverable pod errors (can flap briefly
during a roll). Docker and ECS use Traefik `serverStatus` with the outage
caveat; ECS holds Healthy while an in-place roll is served by the outgoing
revision. Without a `health_check` every backend sees only process liveness,
so an app that runs but serves errors goes unnoticed. The matrix in
`deployment-backends.md` marks Docker and ECS ⚠️.

### 6. Storage: typed now, resource store later (Phase 3)

Promotion policy, the barrier ledger and the guard config are typed columns
and tables. The ADR-0001 engine is live, but `Project` and `Environment` are
not resource kinds yet (`ROADMAP.md` §4), and ADR-0006 refuses scoped sessions
on the resource API, so a project-owned `PromotionPolicy` has no parent and no
membership grants. Once §4 lands, `PromotionPolicy` is the natural first
feature built natively on the resource store, and these columns migrate with
their tables. No work starts before then.

## Rollout

- [x] **Phase 0**: supersession fencing (supersede, straggler cleanup,
  Docker/ECS cutover); snapshot filtering; plan/diff on every `--from`;
  multi-container `--from` fix.
- [x] **Phase 1**: `migration_version` + barrier ledger; `rise promote` with
  gate, digest-pinning, target-relative sizing/env, plan/diff/confirm;
  provenance columns; admin-set promotion source enforced in
  `create_deployment`; `ci-cd.md` and Upgrade Notes.
- [x] **Phase 2**: `unhealthy_since`; leader-elected trigger (unique index +
  row lock); fire-before-teardown and abort; env freeze/recompute split;
  decline observability; backend parity notes.
- [ ] **Phase 3**: blocked on `ROADMAP.md` §4.

## Consequences

**Positive.** Promotion reuses the deploy pipeline's correctness work; one
transactional primitive serves redeploy, rollback, promotion and the guard.
The barrier ledger is exact and maintenance-free in the common case. The guard
reuses zero-downtime supersession instead of new teardown machinery. The Phase
0 fixes pay off on their own.

**Negative.** Promotion-source gating restricts a documented, previously
unrestricted deploy path: operators must move CI to `rise promote` first, and
that needs real communication, not just docs. Barriers still rely on migration
authors recording breaking changes, a process gap rather than a solved
problem. The new `Deployment` columns must be carried by `ROADMAP.md` §4.
`toml` vars written before the upgrade can't be told apart by environment.

**Risk.** The guard is only as good as the health signal. It is opt-in, behind
a kill switch, one hop, never tears anything down, and declines platform-wide
outages.

## Alternatives considered

- **Auto-resolving the source** as "current Healthy in the source
  environment": can't tell weeks of soak from a just-finished rollout.
- **A boolean `migration_barrier` flag** (CLI or `rise.toml`): the
  forgotten-flag failure mode, relocated.
- **Git-diff-based barrier detection**: fragile across build tools; deferred.
- **Reusing `rolled_back_from_deployment_id` for provenance**: breaks
  image-tag resolution and the one-hop cap.
- **Guard on `Unhealthy → Failed` with teardown first**: `Failed` has no
  infrastructure, so this manufactures the outage it should prevent.
- **A new promotion endpoint**: a cross-environment create-from already is
  one; a second path would drift.
- **`PromotionPolicy` on the resource store now**: needs a bespoke parent and
  authz shim likely to diverge from ROADMAP §4.
- **A `rise promotion create/list/approve` family now**: nothing durable to
  represent without approvals.

## References

- [ADR-0001](./0001-unified-permission-model.md): precedent for admin-gated
  policy fields. [ADR-0006](./0006-scoped-cli-sessions.md): scoped sessions
  never get the gate's admin bypass.
- `ROADMAP.md` §4: the Phase 3 dependency; must carry the new columns.
- User docs: `deployments.md`, `environments.md`, `environment-variables.md`,
  `ci-cd.md`. Operator docs: `deployment-backends.md`.
