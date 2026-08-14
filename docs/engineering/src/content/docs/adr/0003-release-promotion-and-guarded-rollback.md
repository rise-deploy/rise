---
title: "ADR-0003: Release Promotion and Guarded Rollback"
---

## Status

**Proposed.** Date: 2026-08-14.

Scope: promoting a deployment between environments (e.g. staging →
production) and automatically recovering from a deployment that goes
unhealthy after a successful rollout. Phases 0–1 are the proposed decision.
Phase 2 (automatic Rollback Guard) is a draft direction pending the open
questions in §5. Phase 3 (`PromotionPolicy` as a resource) is a tracked
dependency, not started. Same-environment `rise deploy`/`rise deploy --from`
behavior is unchanged; backend parity applies throughout, with one documented
exception (§5).

## Context

Environments, per-environment config/domains, and cross-environment redeploy
(`rise deploy --from <id> -E <env>`) already exist — most of what
"promotion" needs is already there, just without policy, safety checks, or a
purpose-built interface. Nothing today reacts automatically to health.

Constraints this design routes around, found while grounding it in the
current code:

- `--from` never validates that the source deployment actually came from a
  declared source environment.
- Supersession on `Healthy` is unfenced (last-Healthy-wins, no ordering by
  creation time) — automation would hit this race far more than manual use
  does.
- No timeout reacts to sustained `Unhealthy` today; `first_healthy_at` can't
  express "continuously healthy" (it survives flaps by design); and `Failed`
  deployments have no infrastructure on either backend, so a naive
  timeout-straight-to-`Failed` manufactures an outage before any replacement
  exists.
- `rolled_back_from_deployment_id` isn't audit metadata — it drives image-tag
  resolution and `can_create_from` eligibility. A new provenance field can't
  simply replace it.
- `--use-source-env-vars` copies Extension-provenance secrets verbatim
  (stale-credential risk on rollback) and already 500s today on projects with
  active extensions.
- `deployment_constraints` on `Environment` is already admin-gated — the
  precedent this design reuses for promotion policy.
- The generic resource API is operator-only until `ROADMAP.md` §1's RBAC work
  lands — not yet a fit for a project-owned policy object.

This converged through several rounds of adversarial review against the
code; drafts that ignored the above (trigger-then-teardown, reusing
`rolled_back_from_deployment_id`, a togglable barrier flag) were rejected —
see Alternatives.

## Decision

### 1. One primitive, three callers

The operation is: create a deployment in environment `E` sourced from
deployment `D`. Manual promotion, the Rollback Guard, and a future
`PromotionPolicy` are different callers supplying `D`/`E`, not three separate
mechanisms.

### 2. Prerequisite hardening (Phase 0)

- Fence supersession: only supersede deployments older than the one that just
  went `Healthy`.
- Filter `Extension`/`System`-provenance vars out of the deployment-to-deployment
  env var copy (also fixes the existing 500).
- Print source deployment ID/digest/env-var diff on every `--from` deploy,
  not only `rise promote`.

### 3. Manual promotion (Phase 1)

`rise promote <project> <deployment-id> [--target-environment production]` —
the source deployment ID is required and never auto-resolved, so soak time is
a deliberate human choice, not an inferred "current Healthy deployment."

Given `D`, promotion: validates `D`'s environment against any configured
promotion-source policy; digest-pins the image; resolves
replicas/cpu/memory (per container) fresh against the *target* environment's
constraints rather than inheriting the source's; recomputes env vars against
the target environment by default; records `promoted_from_deployment_id` /
`promoted_from_environment_id` / `created_via` as provenance kept separate
from `rolled_back_from_deployment_id` (unchanged lineage role); prints a diff
and confirms (`--yes` for CI).

Promotion-source policy lives on `Environment`, admin-gated like
`deployment_constraints` — not `rise.toml`. When set, it also blocks direct
`rise deploy -E <env>` unless the caller is admin. `require_approval`/
`approvers` are out of scope — no approval state/API/UI exists; that's a
separate future workstream. This is opt-in and operator-impacting (changes
the documented CI/CD pattern) and needs an Upgrade Notes entry.

No new `promotion` CLI noun/resource yet — there's nothing durable to
represent without an approval workflow.

### 4. Migration compatibility barriers

Not a boolean — a flag needs active resetting after each migration or it
wrongly blocks every later rollback, the same footgun as a forgotten CLI
flag. Instead: `Deployment.migration_version: i64`, declared explicitly
(bumped in `rise.toml` by whoever authors a migration) and monotonically
non-decreasing per project; a project-scoped, append-only `migration_barriers`
ledger (`{version, reason, recorded_at}`, entries only ever added);
compatibility between versions `A` and `B` (`B ≥ A`) is blocked iff a ledger
entry falls in `(A, B]`, checked across the full chain-flattened lineage.
Crossing automatically (§5) is a hard block; crossing manually (`--from`)
warns and requires `--force`.

### 5. Guarded automatic rollback (Phase 2 — draft)

Scoped to guard-enabled environments only (a new `unhealthy_since` timestamp,
not `first_healthy_at`) — never changes global `Failed` semantics elsewhere.

Trigger fires *before* teardown: at the unhealthy-timeout, create the
last-known-good replacement while the sick deployment keeps running, and let
the existing Healthy-triggers-supersede mechanism retire it once the
replacement is Healthy — reusing zero-downtime supersession rather than
building new teardown logic. If the replacement also fails, stop (one hop,
no chaining) and alert.

Concurrency needs a partial unique index (one candidate per source
deployment) plus status-guarded compare-and-swap updates, not a Pending/
Deploying existence check (rollback rows start in `Pushed`). Env vars freeze
the target's own `User`/`Toml`/`Cli` values and recompute `System`/
`Extension` fresh. Migration barriers block unconditionally. `created_via`
and decline-to-act are both surfaced, not just successful actions. Docker's
health signal is Traefik-dependent — document as a parity limitation in
`deployment-backends.md`, don't silently degrade.

**Open before this leaves draft:** the trigger's exact atomicity guarantee;
the precise "last-known-good" selection rule; whether admins bypass
promotion-source gating; what a blocked CI deploy returns.

### 6. Storage: typed now, resource store later

Promotion policy, the barrier ledger, and guard config are typed
columns/tables, admin-gated like `deployment_constraints` — not a
generic-resource-store kind yet, since that API stays operator-only until
`ROADMAP.md` §1 ships project-scoped RBAC. Once it does, `PromotionPolicy` is
the natural first feature built natively on the resource store. Tracked here
as a dependency; no work starts before then.

## Rollout

Status legend matches `ROADMAP.md`: `[x]` shipped · `[~]` in progress ·
`[ ]` planned.

**Phase 0**
- [ ] Fence supersession to only supersede older deployments.
- [ ] Filter Extension/System vars out of the env var copy path.
- [ ] Print source ID/digest/env-var diff on every `--from` deploy.

**Phase 1**
- [ ] `migration_version` + append-only `migration_barriers` ledger.
- [ ] `rise promote`: source-environment check, digest-pinning,
  target-relative resource/env resolution, diff/confirm.
- [ ] `promoted_from_deployment_id` / `promoted_from_environment_id` /
  `created_via`, independent of `rolled_back_from_deployment_id`.
- [ ] Admin-gated `promotion_source_environment_id` on `Environment`,
  enforced in `create_deployment`.
- [ ] `ci-cd.md` update + Upgrade Notes entry.

**Phase 2** (blocked on §5's open questions)
- [ ] `unhealthy_since` primitive, guard-scoped.
- [ ] Event-driven trigger with verified concurrency (partial unique index +
  CAS).
- [ ] Fire-before-teardown sequencing.
- [ ] Env var freeze/recompute split.
- [ ] Observability incl. decline-to-act.
- [ ] `deployment-backends.md` Docker parity note.

**Phase 3** (blocked on `ROADMAP.md` §1)
- [ ] Not started.

## Consequences

**Positive.** Promotion reuses the deploy pipeline's existing correctness
work instead of duplicating it; one primitive serves manual and later
automated callers. The barrier ledger is exact and maintenance-free in the
common case. The Rollback Guard reuses zero-downtime supersession instead of
new teardown machinery. The Phase 0 fixes are worth shipping regardless of
the rest.

**Negative.** Promotion-source gating changes a documented, currently
unrestricted deploy path and needs real operator migration. The barrier
ledger still relies on migration authors remembering to record a genuinely
breaking change — a process gap, not a solved problem. The new `Deployment`
columns should be timed against `ROADMAP.md` §4's typed-object migration so
they aren't done twice.

**Risk.** The Rollback Guard is the highest-risk piece — health-reactive
automation that gets sequencing or concurrency wrong turns a partial outage
into a full one, which is why it stays in draft. Gating direct
`rise deploy -E production` needs real operator communication, not just a
docs update landing with the code.

## Alternatives considered

- Auto-resolving the source deployment as "current Healthy in the source
  environment" — rejected: can't distinguish a deployment that's soaked for
  weeks from one that just finished.
- A boolean `migration_barrier` flag (CLI or `rise.toml`) — rejected: needs
  active resetting after each migration, the same forgotten-flag failure
  mode relocated.
- Git-diff-based automatic barrier detection — rejected/deferred: needs new
  `git_commit_sha` capture and is fragile across build tools; an explicit
  version + ledger is simpler.
- Reusing `rolled_back_from_deployment_id` for promotion provenance —
  rejected: breaks image-tag resolution and the one-hop cap.
- Triggering the guard on `Unhealthy → Failed` with teardown first —
  rejected: `Failed` deployments have no infrastructure on either backend, so
  this manufactures the outage the feature exists to prevent.
- Building `PromotionPolicy` on the generic resource store now — rejected for
  now: operator-only access until `ROADMAP.md` §1 ships; would need a
  bespoke authz shim likely to diverge from that design.
- A `rise promotion create/list/approve` resource family now — rejected:
  nothing durable to represent without an approval workflow.

## References

- [ADR-0001: Unified Permission Model](./0001-unified-permission-model.md) —
  precedent for admin-gated policy fields, reused for promotion-source policy.
- `ROADMAP.md` §1 (Generic resource and authorization foundation) — dependency
  for §6/Phase 3.
- `ROADMAP.md` §4 (Typed-object migration) — sequencing note for Phase 1's
  new `Deployment` columns.
- `docs/user/src/content/docs/user-guide/{deployments,environments,
  environment-variables,ci-cd}.md` — current documented behavior this design
  extends or changes.
- `docs/engineering/src/content/docs/deployment-backends.md` — feature-matrix
  update required alongside Phase 2.
