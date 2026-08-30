---
title: "Deployment Backends"
description: "Overview of Rise's deployment backends and a feature-parity matrix across them."
---

Rise deploys container apps through a pluggable **deployment backend** (the
deployment controller). Three backends ship today:

- **[Kubernetes](/operator-docs/kubernetes/)** — the original, most widely used
  backend. Deploys apps as Deployments/Services/Ingresses on a cluster.
- **[Docker](/operator-docs/docker/)** — the single-host counterpart. Runs apps
  as plain Docker containers routed by Traefik. No cluster, no Helm.
- **[Amazon ECS](/operator-docs/ecs/)** — the managed-AWS counterpart. Runs apps
  as Fargate tasks routed by Traefik's ECS provider. No cluster to operate, no
  host to own. Designed in
  [ADR-0005](/operator-docs/adr/0005-ecs-deployment-backend/).

All three are first-class. Pick Kubernetes for multi-node, autoscaling,
production clusters; Docker for a single host, edge boxes, demos, or local
development; ECS when you are on AWS and chose it specifically to avoid running
a cluster.

The ECS backend is **newer than the other two** and its first release
deliberately omits several features rather than half-implementing them. Each
omission fails closed with an actionable error at deploy time — see the `❌`
rows below — so nothing silently half-works.

## Feature parity is a goal, not an accident

Rise aims for **semantic feature parity and correctness across all deployment
backends**. Any capability that is configurable through a public Rise API
surface — `rise.toml`, project/deployment settings, environment variables, the
HTTP API — should behave the **same way** on every backend, and should be
**supported by every backend where it is technically possible**.

The qualifier matters: some features depend on capabilities a single-host Docker
daemon simply does not have (horizontal scale-out across nodes, per-workload
network policies). Those gaps are legitimate and are recorded in the matrix
below with a note. A gap that is *merely unimplemented* — not a fundamental
limitation — is a parity **bug** to be tracked and closed, not an accepted
difference.

When a feature lands on one backend, the other backend's support is **never
assumed to follow implicitly**. The parity question must be raised explicitly
during planning and review (see the contributor guideline in `CLAUDE.md`), and
this matrix kept up to date as the source of truth for what each backend
supports.

## Where the shared code lives

Two of the three backends front workloads with **Traefik** and configure it
identically — label the workload, let the provider discover it, read
`serverStatus` back for readiness. That machinery is one implementation in
`rise-backend-traefik`, shared by Docker and ECS, rather than two copies that
drift into a routing bug nobody notices until traffic stops.

It is deliberately **not** in `rise-backend-core`. Core is the contract seam
*every* backend shares — models, the `DeploymentBackend` and `DeploymentStore`
traits, URL building, the state machine, and the runtime-agnostic reconcile
helpers. Traefik is a routing choice two backends happen to make; Kubernetes
routes with nginx ingress annotations and depends on none of it. Putting Traefik
in core would have meant every backend depending on one backend-group's proxy.

```
rise-backend-core        ← the contract seam (all backends)
   └── rise-backend-traefik   ← routing (Traefik-fronted backends only)
          ├── rise-backend-docker
          └── rise-backend-ecs
```

A future backend that fronts workloads differently — an ALB-native ECS mode, say
— depends on core and skips the Traefik crate entirely.

## Feature matrix

Legend: ✅ supported · ⚠️ partial / with caveats · ❌ not supported (see note).

| Feature | Public API surface | Kubernetes | Docker | ECS | Notes |
|---------|--------------------|:----------:|:------:|:---:|-------|
| HTTP ingress routing (`{project}.<domain>`) | implicit | ✅ | ✅ | ✅ | Ingress vs. Traefik router (Docker provider) vs. Traefik router (ECS provider). |
| Path-based routes | `[routes]` | ✅ | ✅ | ✅ | Longest-prefix match on all three; ECS reuses the same label renderer as Docker. |
| Custom domains | project custom domain | ✅ | ✅ | ✅ | Registration emits the route + auth wiring. |
| TLS termination | custom domain / ingress | ✅ | ✅ | ⚠️ | cert-manager (K8s) vs. Traefik ACME / Let's Encrypt HTTP-01 (Docker). On ECS, Traefik terminates TLS the same way, but its ACME store is a file: run a single Traefik task, or put an ACM-terminated load balancer in front and set `ingress_schema: https`. [`modules/rise-ecs`](/operator-docs/ecs/terraform/) provisions either shape and enforces the single-replica constraint. |
| Access classes (`None` / `Authenticated` / `Member`) | `access_requirement` | ✅ | ✅ | ✅ | nginx `auth-url` (K8s) vs. Traefik forwardAuth (Docker and ECS). On ECS `auth_backend_url` must be reachable **from inside the cluster** (a Cloud Map name or internal load balancer); the backend refuses to start if a class requires auth and the URL is empty. |
| Per-route access requirement | `[routes].access` | ✅ | ✅ | ✅ | A route can loosen (`public`) or tighten (`member`) the project's requirement. Enforced by proxy-native routing — one Ingress per requirement group sharing the host (K8s) vs. per-router forwardAuth (Docker) — plus a server-stamped `&access=<req>` the shared `ingress_auth` handler enforces. Only the auth gate varies per route; `ingress_class` and `custom_annotations` stay per-project (a per-host limitation). ECS enforces it the same way as Docker — the same shared label renderer and the same server-stamped `&access=<req>`. |
| `/.rise` auth endpoints on the app host | implicit | ✅ | ✅ | ✅ | High-priority route to the backend on all three. |
| Multi-container deployments | `[containers]` | ✅ | ✅ | ❌ | **ECS: not yet.** Cross-container discovery needs Cloud Map registration, which is unimplemented — `RISE_CONTAINER_HOST__*` would be absent and the containers could not reach each other. A deployment with >1 container is **failed on its first reconcile** with that reason rather than deployed half-working — the message is the deployment's failure reason, not a log line. |
| Cross-container service discovery | `RISE_CONTAINER_HOST__*` | ✅ | ✅ | ❌ | **ECS: not yet** — see the row above; Cloud Map service registration is the intended mechanism (ADR-0005 D10, verified by spike). |
| Auto-injected env vars (`RISE_APP_URL`, `RISE_CONTAINER`, `PORT`, …) | implicit | ✅ | ✅ | ✅ | Same variable contract on all three (one shared implementation in `rise-backend-core`). |
| CPU / memory limits | `cpu`, `memory` | ✅ | ✅ | ⚠️ | Pod resources (K8s) vs. `nano_cpus`/`memory` (Docker) vs. Fargate task size (ECS). **Fargate accepts only a fixed table of CPU/memory pairs**, so a request is rounded **up** to the smallest valid combination — never down. This is billing-visible: Rise's defaults (`500m`/`256Mi`) resolve to 0.5 vCPU / 1 GB, because 512 CPU units require ≥1024 MiB. The resolved size is logged. A request beyond the largest Fargate size fails the deployment with that reason on its first reconcile. |
| HTTP health checks | `health_check` | ✅ | ⚠️ | ⚠️ | Readiness/liveness probes (K8s) vs. Traefik per-server health check (Docker). The Docker backend offloads checking to Traefik, honoring `path`, `period_seconds` (→ Traefik `interval`) and `timeout_seconds` (→ Traefik `timeout`); readiness is read back from Traefik's `serverStatus` and is **authoritative with no fallback**, so a `health_check` on Docker **requires a reachable `traefik_api_url`** (without one a health-checked deployment never becomes Healthy). The remaining knobs — `initial_delay_seconds`, `failure_threshold` and the separate `liveness_enabled`/`readiness_enabled` toggles — are K8s-only. On ECS, readiness is read from Traefik's `serverStatus` exactly as on Docker (authoritative, no fallback), so a `health_check` **requires a reachable `traefik_api_url`**; the same K8s-only knobs are unavailable. |
| Deployment observability (Timeline tab) | `GET .../deployments/{id}/events` | ⚠️ | ⚠️ | ⚠️ | One append-only event log per deployment (ADR-0006), read by both the Timeline tab and the log console's rail. All three backends record **deployment-level status transitions**, written by the control plane, so that much is identical. **No backend emits replica-level events yet** — container start, exit and restart are unrecorded on all three, so a rollout's per-replica detail is currently invisible everywhere. Equal, and equally incomplete: a gap to close, not a backend difference. |
| Rollback | `rise deploy rollback` | ✅ | ✅ | ✅ | Re-resolves the prior deployment's image on all three. |
| Private image pull | registry config | ✅ | ✅ | ⚠️ | imagePullSecret (K8s) vs. host-daemon `docker login` (Docker) vs. **the ECS task execution role** — no pull secret is minted or stored at all, which is why ECR is the recommended registry there. ECS re-authenticates at every task start, so it cannot use the short-lived scoped tokens `gitlab`/`jfrog` issue (refused at startup) and needs `repository_credentials_secret_arn` for a static-credential registry. ECR must be in the cluster's own account: Rise writes no ECR repository policy. See [Container registry](/operator-docs/ecs/#container-registry). |
| Workload identity tokens | token-exchange API + `[identity].audiences` | ✅ | ✅ | ❌ | Both backends deliver the bootstrap credential + per-audience token files to `/var/run/secrets/rise/identity/` and refresh the tokens before they expire. K8s mounts a per-deployment Secret as a volume and a leader-elected controller loop resyncs a project when one of its deployments is due (~2/3 of `identity_token_ttl_seconds` after each mint) so the sync webhook re-mints the token before it expires — Metacontroller does not resync a steady project on its own; Docker writes the same files via the Docker archive API (`PUT /containers/{id}/archive`) right after create and re-mints on its own reconcile loop, recovering the credential from the running container across recreates. The token-exchange endpoint is backend-agnostic. **ECS: not yet.** There is no archive-API analogue for writing files into a running Fargate task; a sidecar writing to a shared task volume is the intended mechanism (ADR-0005 D8). A deployment declaring `[identity].audiences` is **failed on its first reconcile** with that reason rather than started without its token files. |
| Secret env-var isolation | `rise env set --secret` | ✅ | ⚠️ | ✅ | K8s stores secret env in a per-project Secret; Docker flattens them into plain container env (visible to `docker inspect`). **ECS writes them to SSM Parameter Store as `SecureString`s** and references them by ARN, so `DescribeTaskDefinition` reveals a parameter name and nothing else. Parameters are per-deployment, so a rollback resolves the values that deployment shipped with. |
| Replicas > 1 (horizontal scale) | `replicas` | ✅ | ✅ | ✅ | The request is bounded by `deployment_constraints.max_replicas` (Docker default 10, `RISE_MAX_REPLICAS`) on both backends, and additionally hard-capped at 50 by the Docker controller. Docker runs N containers per spec behind ONE Traefik service (round-robin LB) and ONE shared, replica-free network alias (Docker DNS round-robins). Recreates roll one replica at a time — a running, drifted replica is replaced only while every other replica is healthy, so capacity never drops by more than one. K8s uses a Deployment's `replicas`. On ECS this is the service's `desiredCount` — ECS owns the replicas, so scaling needs no new task-definition revision. Bounded by `deployment_constraints.max_replicas`, and additionally capped at 100 by the controller as a backstop against exhausting the account's Fargate vCPU quota. |
| Zero-downtime active switch | implicit (blue/green) | ✅ | ✅ | ✅ | K8s does an atomic blue/green Service selector flip. Docker's cutover overlaps old and new containers on one Traefik service and drains the old via Traefik's per-server health check — no recreate gap, but a **rolling overlap rather than an atomic switch** (a documented, intentional backend difference per the parity policy). ECS overlaps old and new **services** on one Traefik service and drains via Traefik's health check — a rolling overlap like Docker's, not K8s's atomic flip. Within a deployment, drift is applied by `UpdateService`: ECS performs the rolling replacement itself, so there is no remove-then-create gap (which would hurt far more here, where a task starts in tens of seconds). One asymmetry against Docker, from the provider rather than the backend: Traefik reads ECS by polling and Docker by daemon event, so a retiring ECS task can stay in the routing table for up to `refreshSeconds` after it stops — a bounded window with no Docker equivalent (ADR-0005 D12). |
| Reclaiming workloads after a controller-class change | `Organization.deploymentControllerClass` | ❌ | ❌ | ✅ | Moving an organization to another class leaves the old controller's workloads running: it stops reconciling the project, and the new controller only ever sees its own class, so nothing collects them. Until they go, an unconstrained proxy serves the project from two independent deployments at once. ECS retires them — its sweep is cluster-wide and keyed on its own class tag, so it can reach a project it no longer owns. Docker says so explicitly in the code ("must be cleaned up manually") and Kubernetes has no equivalent pass at all. **A parity gap, not a limitation** — surfaced per the parity policy, not yet tracked as work. |
| Runtime logs (`rise deployment logs`, Logs tab) | `deployment_logs` | ✅ | ✅ | ✅ | Kubernetes reads the Pod log API, Docker reads the daemon, and ECS reads the controller's configured CloudWatch group with `FilterLogEvents` and `StartLiveTail`. The ECS stream prefix is `{resource_prefix}/{project_uuid}/{deployment_uuid}/`, so project renames do not break retained-log lookup and all replicas and containers are merged for one deployment. Loki remains available as a backend-agnostic persistent store. |
| Per-container log filter (`--container`, `?container=`, container chips) | `deployment_logs` | ⚠️ | ⚠️ | ✅ | Every line carries the container that produced it, and the filter narrows to one or more of the deployment's containers (`app` for a single-container deployment). ECS resolves both from the awslogs stream name (`{stream_prefix}/{container}/{task-id}`) and pushes a one-container filter down as the stream prefix; Loki resolves both from the `labels.container` stream label — a shipper that doesn't emit it serves unattributed lines and matches nothing when filtered. **Kubernetes and Docker read one workload at a time:** each container of a deployment runs as its own Pod/container, and the live readers stream a single one, so an unfiltered read shows one container rather than all of them merged. The filter picks which — it is how a multi-container deployment's other containers are reached at all. Merging several live streams into one paginated feed is unimplemented on both: a parity gap against the historical backends, not a fundamental limitation. |
| Per-group network isolation | implicit | ✅ | ❌ | ⚠️ | **Docker limitation:** NetworkPolicy (K8s) has no single-host equivalent. On ECS, per-service security groups *could* express this but are not yet wired per group — a tracked gap, not a fundamental limitation. |

Keep this table in sync with the code. When you add or change a backend feature,
update the relevant row (and add a row for a brand-new feature) in the same
change.
