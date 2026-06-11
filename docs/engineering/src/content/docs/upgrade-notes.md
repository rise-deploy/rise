---
title: "Upgrade Notes & Breaking Changes"
---

This page is the canonical, per-release reference for changes that affect
operators running a Rise installation: breaking changes, required actions, and
new or changed configuration. Read the section for the version you are upgrading
**to** before upgrading.

:::note
This page is fed from the [**Rise Rollout Tracker**](https://github.com/orgs/rise-deploy/projects/1)
GitHub Project. Any tracked item whose `Operator impact` is not `None` must have a
matching entry here for its `Target release` before it is marked `Done`. The
project's "Operator impact" view is the worklist; this page is what operators read.
:::

## Impact legend

| Badge | Meaning |
|---|---|
| **Breaking** | Requires action or behavior changes incompatibly; upgrading without reading may break installs. |
| **Action required** | Upgrade succeeds, but an operator action is needed to get correct/expected behavior. |
| **Config change** | New or changed configuration is available; defaults preserve existing behavior. |

---

## Unreleased

_Changes merged to `develop` but not yet in a tagged release, plus in-flight PRs
proposed for the next train. Moved into a version section at tag time._

In-flight PRs with operator impact (not yet merged):

- **Behavior change — workload identity on the Docker backend** ([#378](https://github.com/rise-deploy/rise/issues/378)).
  The Docker controller now delivers the same workload-identity material as
  Kubernetes — the bootstrap credential and one token file per `[identity].audiences`
  entry — to `/var/run/secrets/rise/identity/` inside each app container (via the
  Docker archive API), and refreshes the token files before they expire. No new
  configuration; this closes a parity gap, so a Docker app that sets
  `[identity].audiences` now receives its tokens instead of nothing. Identity
  files are delivered when a container is created, and the controller also
  self-heals already-running containers that lack them on the next reconcile, so
  apps running before the upgrade pick up their identity material without a
  redeploy (mirroring the Kubernetes controller re-establishing it on each sync).
- **Config change — auth token exchange (phase 1)** ([#367](https://github.com/rise-deploy/rise/pull/367)).
  Adds the RFC 8693 exchange endpoint and a Rise `Access` token kind. Purely
  additive; existing token flows are unchanged, legacy in-handler verification
  remains the fallback. See
  [`ROADMAP.md`](https://github.com/rise-deploy/rise/blob/develop/ROADMAP.md)
  § "Workstream 2 — Authentication & Token Exchange".
- **Config change — Docker deployment backend** ([#358](https://github.com/rise-deploy/rise/pull/358)).
  Selectable via `deployment_controller.type = "docker"`. Single-host; Kubernetes
  remains the default, so existing installs are unaffected unless they opt in.
  A new deployment rolls over via Traefik health checks, with old and new
  overlapping in one load-balanced service (a rolling update, vs. Kubernetes'
  atomic blue/green). Probing is **opt-in**: no `health_check` means
  ready-when-running; a set `health_check` is a **2xx–3xx** check.
  Operator-relevant settings on the Docker controller (env-driven; the shipped
  standalone compose sets working defaults):
  - `traefik_api_url` (default in-network `http://rise-traefik:8080`) — the rolling
    gate reads Traefik's `serverStatus`, the **authoritative** readiness signal for
    health-checked containers (no fallback). The standalone Traefik enables its API
    internally (`--api.insecure=true`, port **not** published). If you run your own
    Traefik and any project uses a `health_check`, you **must** expose its API to
    the backend over the internal network (optionally with basic-auth embedded in
    the URL); without it a health-checked deployment never becomes Healthy. It may
    be left unset only when no project uses health checks.
  - Replicas: the Docker config raises `deployment_constraints.max_replicas` to 10
    (`RISE_MAX_REPLICAS`); the controller additionally hard-caps at 50.
  The deployment-backend feature matrix, the Docker operator pages, and the
  cutover/health-check docs ship with this PR.
- **Action required — reserved `RISE_` env-var prefix** ([#355](https://github.com/rise-deploy/rise/pull/355)).
  User-supplied environment variable keys beginning with `RISE_` are rejected at
  the API and at deploy time (project env vars and per-container
  `[containers.X.env]`). If any of your users' apps set `RISE_*` keys, rename them
  before upgrading.

---

## 0.23.0

First release of the generic resource substrate (compatibility phase). None of
these change behavior for existing installs by default; the items below are the
configuration knobs they introduce.

- **Config change — Operator role (`auth.operator_users`).** The generic resource
  API (`/api/v1/resources`) is gated to a new, separately configured Operator
  role. `auth.admin_users` do **not** receive Operator access. No action needed
  unless you want operators to manage generic resources. See
  [`ROADMAP.md`](https://github.com/rise-deploy/rise/blob/develop/ROADMAP.md).
- **Config change — default Organization / Kubernetes `controller_class_name`.**
  Backend startup bootstraps a single default Organization and backfills existing
  users, teams, and projects to it under an advisory lock. Existing installs
  resolve to the **same** namespace names as before (`rise-` prefix → `rise-myapp`).
  The Kubernetes controller's `controller_class_name` defaults to a stable value
  for existing installs if unset.

### Watch for later (not yet released)

These are tracked as finalization gates and will land in a **future** release —
listed here so operators can anticipate them:

- **Breaking (future) — multi-tenancy phase 2.** Tightening
  `organization_resource_uid` to `NOT NULL` after backfill, and migrating typed
  tables onto the generic resource model. Tracked in
  [#372](https://github.com/rise-deploy/rise/issues/372).
- **Breaking (future, behind operator toggle) — removal of the legacy auth path.**
  Auth token-exchange phase 3 removes the legacy in-handler verification path,
  gated behind an operator toggle for a transparent fallback window. Tracked in
  [#374](https://github.com/rise-deploy/rise/issues/374).
