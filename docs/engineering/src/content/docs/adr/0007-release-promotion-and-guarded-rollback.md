---
title: "ADR-0007: Release Promotion and Guarded Rollback"
---

## Status

**Implemented (Phases 0–2).** Proposed 2026-08-14; implemented 2026-10-25.

Phase 3 (`PromotionPolicy` as a generic resource) is a tracked dependency and
has not started (§6). Scope: promoting a deployment between environments
(e.g. staging → production) and automatically recovering from a deployment
that goes unhealthy after a successful rollout. Same-environment
`rise deploy` and `rise deploy --from` keep their behavior apart from the
Phase 0 fixes. Backend parity applies throughout; the one documented
limitation is the Rollback Guard's health signal (§5).

The one primitive is `server::deployment::promotion`. The guard is
`server::deployment::rollback_guard`. The schema is in migrations
`20261025000002_add_release_promotion` and `20261025000003_add_rollback_guard`.

## Context

Environments, per-environment config and domains, and cross-environment
redeploys (`rise deploy --from <id> -E <env>`) already existed. Most of what
"promotion" needs was there, but without policy, safety checks or a
purpose-built interface. Nothing reacted automatically to health.

Constraints this design routes around, found while grounding it in the code:

- `--from` never validated that the source deployment came from a declared
  source environment.
- Supersession on `Healthy` was unfenced (last-Healthy-wins, no ordering by
  creation time), in three places: the supersede query, the straggler cleanup,
  and the Docker/ECS Traefik cutover, where an older, slow rollout could take
  routing from a newer active deployment. Automation would hit this race far
  more often than manual use does.
- No timeout reacted to sustained `Unhealthy`. `first_healthy_at` can't express
  "continuously healthy" (it survives flaps by design). `Failed` deployments
  have no infrastructure on any of the three backends (Kubernetes, Docker,
  ECS), so a naive timeout straight to `Failed` manufactures an outage before
  any replacement exists.
- `rolled_back_from_deployment_id` is not audit metadata. It always points at
  the *image origin* (chain-flattened) and drives image-tag resolution on every
  backend, URL building, `can_create_from` eligibility, Git-provenance
  inheritance and the creation event. A new provenance field can't replace it.
- `--use-source-env-vars` copied Extension- and System-provenance values
  verbatim (a stale-credential risk on rollback), and it returned a 500 on
  projects with active extensions because of a unique-key collision.
- `deployment_constraints` on `Environment` is admin-gated. That is the
  precedent this design reuses for promotion policy.
- A multi-container `--from` run inside a checkout sent containers built from
  the local `rise.toml` without images. The server then persisted push tags
  for the new deployment that were never pushed.

This converged through several rounds of adversarial review against the code.
Drafts that ignored the above (trigger-then-teardown, reusing
`rolled_back_from_deployment_id`, a togglable barrier flag) were rejected; see
Alternatives.

## Decision

### 1. One primitive, three callers

The operation is: create a deployment in environment `E` from deployment `D`.
Manual redeploys, rollbacks and promotions, the Rollback Guard, and a future
`PromotionPolicy` are different callers supplying `D` and `E`, not separate
mechanisms. `promotion::create_from_source` writes the deployment row, its
complete env-var materialization and its creation event in **one
transaction**. A `Pushed` row never exists without its variables, and a failed
extension hook rolls back instead of leaving a `Failed` row behind.

There is no new creation endpoint. Promotion stays
`POST /api/v1/deployments` with `from_deployment` and an `environment` that
differs from the source's. One read endpoint,
`GET /projects/{p}/deployments/{id}/plan`, previews what create would do: the
gate decision, the migration check, the pinned image and resource adjustments.
The CLI and the UI print it next to the existing env-var diff.

A cross-environment create-from **is** a promotion, whichever caller makes it
(`rise promote`, `rise deploy --from X -E other`, or the UI).

### 2. Prerequisite hardening (Phase 0)

- **Fence supersession by creation order** (`(created_at, id)`), on all three
  backends:
  - A deployment that turns Healthy supersedes only *older* ones.
  - If a newer deployment already serves the group, the late one is retired
    (`Superseded`, with a `superseded_by` event) instead of taking over.
  - The straggler cleanup only retires older deployments.
  - The Docker/ECS cutover never marks an older deployment active.
- **Filter env vars on the snapshot copy.** `--use-source-env-vars` copies
  everything except `system` and `extension` values, which are recomputed. This
  also fixes the 500.
- **Preview on every `--from`.** Print the source ID, the image/digest and the
  env-var diff on every `--from` deploy, not only `rise promote`.
- **Multi-container `--from`.** The CLI no longer sends containers or routes
  with `--from`. The server rejects containers without an image on a
  create-from.

### 3. Manual promotion (Phase 1)

`rise promote <deployment-id> [-p project] [-E target]`. The source deployment
ID is required and never auto-resolved, so soak time is a deliberate human
choice rather than an inferred "current Healthy deployment". The target
defaults to the project's production environment.

Given `D`, promotion:

- **Applies the promotion-source gate** (below).
- **Digest-pins the image** (every container image, for multi-container).
- **Sizes resources against the target.** Explicit values in the request (CLI
  flags or the target's `[environments.<target>.deploy]`) are validated as
  usual. Inherited values are *clamped* into the target environment's effective
  range, and each change is reported as an adjustment. For multi-container,
  cpu and memory are clamped per container, and total replicas out of range
  fail with 400.
- **Recomputes env vars against the target.** Later layers win:
  1. the target's Global and `env:<target>` vars;
  2. the source's `toml`/`cli` vars, minus env-scoped toml vars of other
     environments. This needs the new `toml:<env>` source value;
  3. extension vars, computed fresh;
  4. request overrides;
  5. `PORT` and the `RISE_*` system vars.

  `use_source_env_vars` is rejected across environments with 400.
- **Records provenance** in `created_via` (`deploy` / `redeploy` / `promote` /
  `rollback_guard`), `promoted_from_deployment_id` (the *immediate* source)
  and `promoted_from_environment_id`. These are kept separate from
  `rolled_back_from_deployment_id`, which keeps its image-origin role.
- **Prints the plan and diff, then confirms.** `--yes` skips the prompt. Run
  non-interactively without `--yes`, it refuses.

**Promotion-source policy** is `Environment.promotion_source_environment_id`,
admin-set like `deployment_constraints`, not part of `rise.toml`. The FK keeps
it within the project, and cycles are refused. An environment with source `S`
admits a deployment iff:

- it is created from a deployment whose environment is `S`, or the target
  itself (redeploying or rolling back something already admitted is fine); or
- the caller is an **unscoped human admin**.

Fresh builds, `--image` and promotions from any other environment are denied:

```json
HTTP 403
{"error": "Environment 'production' only accepts deployments promoted from 'staging'. Promote a staging deployment: rise promote <deployment-id> -E production",
 "error_code": "promotion_source_required",
 "details": {"environment": "production", "promotion_source_environment": "staging"}}
```

The admin bypass never applies to service accounts, `--scope` sessions or the
guard. It is recorded on the creation event (`promotion_gate_bypassed`) and
returned as a warning. The gate covers the whole environment, preview groups
included. An environment that is another one's promotion source can't be
deleted (409) until the dependent's policy is cleared.

`require_approval`/`approvers` are out of scope. No approval state, API or UI
exists; that is a separate future workstream. There is no new `promotion` CLI
noun or resource either, since there is nothing durable to represent without an
approval workflow.

### 4. Migration compatibility barriers

A boolean flag would need active resetting after each migration, or it would
wrongly block every later rollback. Barriers use a version and a ledger
instead:

- **`Deployment.migration_version`** (nullable `BIGINT`). Declared in
  `rise.toml` as `[project].migration_version` and bumped by whoever authors a
  migration. A fresh build takes it from the request. Every create-from
  inherits the source's value, because it is a property of the image; that is
  the chain-flattened lineage.
- **A project-scoped, append-only `migration_barriers` ledger**
  (`{version, reason, recorded_by, recorded_at}`). A trigger refuses `UPDATE`.
  Recorded with `rise project migration-barrier add`.
- **The environment's high-water mark `B`** is the maximum `migration_version`
  over deployments in the target environment that reached `Deploying` (so they
  may have run their migrations).
- **A candidate at version `A`** is:
  - fine when `A ≥ B` or `B` is unknown;
  - `unknown` when `A` is NULL (a warning on manual creates);
  - otherwise blocked iff a ledger entry `v` satisfies `A < v ≤ B`.

The check applies to every create, fresh builds included, since a lower
version is a backward move. Crossing a barrier manually returns 409
`migration_barrier` with
`details {candidate_version, environment_version, barriers[]}`. It can be
overridden with `force_migration_barrier` (CLI `--force`, UI "Deploy anyway"),
and the override is recorded as `migration_barrier_forced`. The guard never
crosses a barrier.

### 5. Rollback Guard (Phase 2)

An opt-in, per-environment controller (`rollback_guard_enabled`,
`rollback_guard_unhealthy_timeout_seconds`, 60–86400, default 300, admin-set).
It runs as a leader-elected loop (`rise-rollback-guard`) behind the operator
kill switch `rollback_guard.enabled`. It never changes `Failed` semantics
anywhere.

**Signal.** `unhealthy_since` is set when a deployment enters `Unhealthy`,
kept while it stays there and cleared on recovery. Unlike `first_healthy_at`,
it measures *continuous* unhealthiness. All three backends write health through
the same store calls, so the timestamp has parity by construction.

**Subject.** The guard watches the active deployment `S` of a guard-enabled
environment's **primary** group (preview groups are not guarded) that has been
`Unhealthy` for at least the timeout.

**Last-known-good (LKG).** `L` is the most recent deployment in the same
project, environment and group that is older than `S` by `(created_at, id)`
and has served (`first_healthy_at` set). The guard acts only if all of these
hold:

- `L` is `Superseded`, meaning it was retired by a newer deployment rather than
  stopped, expired or failed;
- `can_create_from(L)` holds;
- `L`'s migration version crosses no barrier and is known whenever the project
  has barriers.

**Fire before teardown.** The guard creates the replacement with the
create-from primitive while `S` keeps running. The fenced supersession retires
`S` once the replacement is Healthy, so no new teardown logic is needed.

- `created_via = 'rollback_guard'`, `rollback_guard_for_deployment_id = S`.
- `created_by_id = L.created_by_id`, because there is no system user; the
  `created_via` value and the event attributes are the authoritative
  attribution.
- Env vars freeze `L`'s snapshot except `system` and `extension`. Extension,
  `PORT` and `RISE_*` values are recomputed.
- Sizing is `L`'s, clamped into the current constraints. The image is `L`'s,
  without re-pinning.

**Trigger atomicity.** There is at most **one** replacement per unhealthy
deployment, ever, enforced by a partial unique index on
`rollback_guard_for_deployment_id`.

- The replacement row, its variables and its creation event commit in one
  transaction.
- That transaction first takes `SELECT … FOR UPDATE` on `S` and re-checks the
  subject predicate: still `Unhealthy`, still active, past the timeout, and no
  candidate yet.
- Health writers lock the same row, so either `S`'s recovery commits first and
  nothing is created, or the candidate commits first.
- A duplicate insert from a stale leader fails on the index and counts as
  "already triggered".
- The guard never tears anything down.

**Abort on recovery.** If `S` recovers (or a human replaces it) before the
candidate is Healthy, the guard cancels the candidate.

**One hop, no chaining.** The guard never acts on a deployment it created
itself (`chained`). If a candidate fails, the unique index prevents a second
one: the guard records a `failed` event (severity error) on `S` and `S` keeps
running, still Unhealthy.

**Declining is surfaced.** Every decision is a deduplicated `rollback_guard`
event on `S` with `outcome` `triggered` / `declined` / `aborted` / `failed`.
The decline reasons are:

- `chained`
- `platform_wide_unhealthy`
- `in_flight_limit`
- `newer_deployment_in_progress` (a human fix is underway)
- `no_last_known_good`
- `last_known_good_not_superseded`
- `last_known_good_unavailable`
- `migration_barrier`
- `migration_version_unknown`

**Fleet circuit breaker.** On Docker and ECS, health with a `health_check` path
is Traefik's `serverStatus`, so a Traefik API outage makes every health-checked
deployment Unhealthy at once. The guard declines the whole tick
(`platform_wide_unhealthy`) when both of these hold among active deployments:

- at least `fleet_unhealthy_min_count` (3) are Unhealthy;
- the Unhealthy share is at least `fleet_unhealthy_ratio` (0.5).

`max_in_flight` (3) caps how many candidates can be open at once.

**Parity.** The decision logic is shared, and the health signal is not:

- **Kubernetes:** pod readiness plus irrecoverable pod errors. It can flap
  briefly during a pod-template roll.
- **Docker and ECS:** Traefik `serverStatus`, with the outage caveat above. ECS
  holds Healthy while an in-place roll is still served by the outgoing
  revision.
- **Without a `health_check`:** all three only see process liveness, so the
  guard can't react to an app that runs but serves errors.

The matrix in `deployment-backends.md` marks this ⚠️ on Docker and ECS.

### 6. Storage: typed now, resource store later (Phase 3)

Promotion policy, the barrier ledger and the guard config are typed columns and
tables.

The generic resource API is no longer operator-only: the ADR-0001 engine,
RoleBindings and the choke point are live. The remaining blocker is that
`Project` and `Environment` are not resource kinds yet (`ROADMAP.md` §4,
"Migrate `Project` … and `Environment` … as built-ins"). ADR-0006 also refuses
scoped sessions on the resource API. Until that migration lands, a
project-owned `PromotionPolicy` has no parent to live under and no
project-membership grants to authorize it.

Once it lands, `PromotionPolicy` is the natural first feature built natively on
the resource store, and the typed columns above migrate with their tables. No
work starts before then.

### Deviations from the proposal

- **CLI syntax.** It is `rise promote <deployment-id> [-p project]`, not
  `rise promote <project> <deployment-id>`. Every Rise command takes the
  project from `-p/--project` or `rise.toml`.
- **Who records barriers.** Barrier recording is project-level (owners and
  members), not admin-gated like constraints. The ledger is append-only and
  can only *restrict* rollbacks, so it escalates nothing. Requiring an admin
  would push the process gap ("remember to record it") onto someone who
  doesn't author migrations.
- **Cross-environment `--from` is a promotion.** It is not a separate,
  unchecked path. Scripts that used `--use-source-env-vars` across
  environments now get 400.
- **Traefik dependence covers ECS too.** The health-signal limitation applies
  to Docker *and* ECS, which use the same authoritative `serverStatus`.
- **The Phase 3 blocker** is `ROADMAP.md` §4's Project/Environment migration,
  not §1's RBAC, which has shipped.
- **Scoped promotions need read access to the source.** A `--scope` session
  promoting across environments must also be allowed to read deployments in
  the source environment; otherwise it could copy that environment's
  `rise.toml`/CLI secrets into one it can deploy to.
- **No high-water mark, nothing to cross.** When the target environment has
  never started a deployment with a declared `migration_version`, the barrier
  check passes even for an undeclared candidate.
- **Abort on recovery also covers replacement.** An open guard replacement is
  cancelled when the unhealthy deployment recovers *or* is stopped,
  superseded or otherwise leaves the active set.
- **A failed extension hook creates nothing.** Creating from a source runs
  extension hooks before its transaction, so a failing hook returns an error
  instead of leaving a `Failed` row behind.

## Rollout

Status legend matches `ROADMAP.md`: `[x]` shipped · `[~]` in progress ·
`[ ]` planned.

**Phase 0**
- [x] Fence supersession by creation order (supersede, straggler cleanup,
  Docker/ECS cutover).
- [x] Filter Extension/System vars out of the snapshot env-var copy.
- [x] Print the plan and env-var diff on every `--from` deploy.
- [x] Multi-container `--from` no longer references unpushed images.

**Phase 1**
- [x] `migration_version` + append-only `migration_barriers` ledger.
- [x] `rise promote`: source-environment gate, digest-pinning,
  target-relative resource/env resolution, plan/diff/confirm.
- [x] `promoted_from_deployment_id` / `promoted_from_environment_id` /
  `created_via`, independent of `rolled_back_from_deployment_id`.
- [x] Admin-set `promotion_source_environment_id` on `Environment`, enforced
  in `create_deployment`.
- [x] `ci-cd.md` update + Upgrade Notes entry.

**Phase 2**
- [x] `unhealthy_since` primitive.
- [x] Leader-elected trigger with verified concurrency (partial unique index +
  row lock).
- [x] Fire-before-teardown sequencing; abort on recovery.
- [x] Env var freeze/recompute split.
- [x] Observability incl. decline-to-act.
- [x] `deployment-backends.md` parity notes (Docker and ECS).

**Phase 3** (blocked on `ROADMAP.md` §4's Project/Environment migration)
- [ ] Not started.

## Consequences

**Positive.**
- Promotion reuses the deploy pipeline's existing correctness work instead of
  duplicating it.
- One transactional primitive serves redeploy, rollback, promotion and the
  guard.
- The barrier ledger is exact and needs no maintenance in the common case.
- The Rollback Guard reuses zero-downtime supersession instead of new teardown
  machinery.
- The Phase 0 fixes pay off on their own.

**Negative.**
- Promotion-source gating changes a documented, previously unrestricted deploy
  path. Operators must move CI to `rise promote` before enabling it.
- The barrier ledger still relies on migration authors remembering to record a
  genuinely breaking change. That is a process gap, not a solved problem.
- The new `Deployment` columns must be carried by `ROADMAP.md` §4's typed-object
  migration.
- `toml` vars written before the upgrade can't be told apart by environment.

**Risk.**
- The Rollback Guard is health-reactive automation, and it is only as good as
  the health signal. It is opt-in per environment, behind an operator kill
  switch, one hop, never tears anything down, and declines platform-wide
  outages.
- Gating direct `rise deploy -E production` needs real operator communication,
  not just a docs update landing with the code.

## Alternatives considered

- **Auto-resolving the source deployment** as "current Healthy in the source
  environment". Rejected: it can't distinguish a deployment that has soaked for
  weeks from one that just finished.
- **A boolean `migration_barrier` flag** (CLI or `rise.toml`). Rejected: it
  needs active resetting after each migration, the same forgotten-flag failure
  mode relocated.
- **Git-diff-based automatic barrier detection.** Rejected/deferred: it is
  fragile across build tools. An explicit version and a ledger are simpler.
- **Reusing `rolled_back_from_deployment_id` for promotion provenance.**
  Rejected: it would break image-tag resolution and the one-hop cap.
- **Triggering the guard on `Unhealthy → Failed` with teardown first.**
  Rejected: `Failed` deployments have no infrastructure on any backend, so this
  manufactures the outage the feature exists to prevent.
- **A new promotion endpoint.** Rejected: a cross-environment create-from
  already is one, and a second path would drift from it.
- **Building `PromotionPolicy` on the generic resource store now.** Rejected
  for now: `Project` and `Environment` aren't kinds yet, so it would need a
  bespoke parent and authz shim likely to diverge from ROADMAP §4.
- **A `rise promotion create/list/approve` resource family now.** Rejected:
  there is nothing durable to represent without an approval workflow.

## References

- [ADR-0001: Unified Permission Model](./0001-unified-permission-model.md):
  precedent for admin-gated policy fields.
- [ADR-0006: Scoped CLI Sessions](./0006-scoped-cli-sessions.md): scoped
  sessions never get the admin bypass of the promotion gate.
- `ROADMAP.md` §4 (Typed-object migration): the dependency for §6/Phase 3,
  and it must carry the new `Deployment`/`Environment` columns.
- User docs: `user-guide/deployments.md` (promotion, barriers, Rollback Guard),
  `environments.md`, `environment-variables.md` and `ci-cd.md`.
- `deployment-backends.md`: matrix rows for promotion and the Rollback Guard.
