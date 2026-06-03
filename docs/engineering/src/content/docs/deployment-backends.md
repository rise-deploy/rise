---
title: "Deployment Backends"
description: "Overview of Rise's deployment backends and a feature-parity matrix across them."
---

Rise deploys container apps through a pluggable **deployment backend** (the
deployment controller). Two backends ship today:

- **[Kubernetes](/operator-docs/kubernetes/)** — the original, most widely used
  backend. Deploys apps as Deployments/Services/Ingresses on a cluster.
- **[Docker](/operator-docs/docker/)** — the single-host counterpart. Runs apps
  as plain Docker containers routed by Traefik. No cluster, no Helm.

Both are first-class. Pick Kubernetes for multi-node, autoscaling, production
clusters; pick Docker for a single host, edge boxes, demos, or local
development.

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

## Feature matrix

Legend: ✅ supported · ⚠️ partial / with caveats · ❌ not supported (see note).

| Feature | Public API surface | Kubernetes | Docker | Notes |
|---------|--------------------|:----------:|:------:|-------|
| HTTP ingress routing (`{project}.<domain>`) | implicit | ✅ | ✅ | Ingress vs. Traefik router. |
| Path-based routes | `[routes]` | ✅ | ✅ | Longest-prefix match on both. |
| Custom domains | project custom domain | ✅ | ✅ | Registration emits the route + auth wiring. |
| TLS termination | custom domain / ingress | ✅ | ✅ | cert-manager (K8s) vs. Traefik ACME / Let's Encrypt HTTP-01 (Docker). |
| Access classes (`None` / `Authenticated` / `Member`) | `access_requirement` | ✅ | ✅ | nginx `auth-url` (K8s) vs. Traefik forwardAuth (Docker). |
| `/.rise` auth endpoints on the app host | implicit | ✅ | ✅ | High-priority route to the backend on both. |
| Multi-container deployments | `[containers]` | ✅ | ✅ | Separate Deployments (K8s) vs. one container per spec (Docker). |
| Cross-container service discovery | `RISE_CONTAINER_HOST__*` | ✅ | ✅ | Service DNS (K8s) vs. container-name DNS on `rise_default` (Docker). |
| Auto-injected env vars (`RISE_APP_URL`, `RISE_CONTAINER`, `PORT`, …) | implicit | ✅ | ✅ | Same variable contract on both. |
| CPU / memory limits | `cpu`, `memory` | ✅ | ✅ | Pod resources (K8s) vs. `nano_cpus`/`memory` (Docker). |
| HTTP health checks | `health_check` | ✅ | ✅ | Readiness/liveness probes (K8s) vs. controller HTTP probe (Docker). |
| Deployment observability (Pods tab) | `controller_metadata.pod_status` | ✅ | ✅ | Same `pod_status` JSON shape rendered by the frontend. |
| Rollback | `rise deploy rollback` | ✅ | ✅ | Re-resolves the prior deployment's image on both. |
| Private image pull | registry config | ✅ | ✅ | imagePullSecret (K8s) vs. host-daemon `docker login` (Docker). |
| Workload identity tokens | token-exchange API | ✅ | ✅ | Backend-agnostic (issued by the control plane). |
| Replicas > 1 (horizontal scale) | `replicas` | ✅ | ❌ | **Docker limitation:** one container per spec today; `replicas>1` runs a single container (warned). |
| Zero-downtime active switch | implicit (blue/green) | ✅ | ⚠️ | Service selector flip (K8s) is atomic; Docker recreates the container, so there is a brief routing gap. |
| Per-group network isolation | implicit | ✅ | ❌ | **Docker limitation:** NetworkPolicy (K8s) has no single-host equivalent; all app containers share `rise_default`. |

Keep this table in sync with the code. When you add or change a backend feature,
update the relevant row (and add a row for a brand-new feature) in the same
change.
