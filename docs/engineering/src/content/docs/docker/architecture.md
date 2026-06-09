---
title: "Architecture"
---

## Overview / architecture

The standalone stack brings up everything Rise needs to deploy container apps
through the `docker` deployment controller (`deployment_controller.type: docker`):

| Service | Container | Role |
|---------|-----------|------|
| `rise` | `rise-backend` | Control plane + Docker reconciler. Mounts the host Docker socket to create/route app containers. |
| `postgres` | `rise-postgres` | State store (migrations auto-run on startup). |
| `dex` | `rise-dex` | OIDC provider (demo IdP). |
| `registry` | `rise-registry` | OCI registry for app images. |
| `traefik` | `rise-traefik` | Reverse proxy; routes `{project}.<domain>` to app containers. |

The reconciler creates one container per running deployment and stamps two
families of labels on each (see
`src/server/deployment/controller/docker/labels.rs`):

- **Bookkeeping labels** (`rise.dev/managed-by=rise`, `…/project`,
  `…/deployment-id`, `…/route-hash`, etc.) so the reconciler can find its
  containers, detect drift, and garbage-collect orphans.
- **Traefik labels** (`traefik.enable`, the per-router ``Host(`…`)`` rule,
  entrypoint, service port, optional TLS certresolver, optional forwardAuth
  middleware) on routable containers.

All containers — Rise's own services and the app containers the reconciler
creates — join the `rise_default` Docker network. Traefik is pinned to that same
network (`--providers.docker.network=rise_default`, mirrored by
`deployment_controller.traefik_network`) so it can reach app containers at their
address on that network. Traefik discovers each app container through the Docker
provider and the per-router `traefik.docker.network` label (the app containers
carry no extra network alias of their own). The Compose project name is fixed to
`rise` so the network is always named `rise_default` regardless of the launch
directory.

## Cutover & health checks

When a new deployment becomes the active one for its group, traffic moves from
the old containers to the new via a **health-driven rolling overlap**: the new
and old containers join **one group-scoped Traefik load-balancer service**
immediately. When the project sets a `health_check`, the reconciler emits Traefik
health-check labels (`traefik.http.services.<svc>.loadbalancer.healthcheck.*`,
e.g. `path`, `interval`, `timeout`) so Traefik routes only to servers that pass
the check. The new deployment is **retired-old-gated** on Traefik's per-server
`serverStatus`: the reconciler reads it via the **internal Traefik API** at
`deployment_controller.traefik_api_url` and only drops the old deployment once
the new servers are actually `UP` in Traefik's rotation. When `traefik_api_url`
is unset or unreachable it **falls back to Rise's own mirror probe**. Old and
new overlap during a deploy — there is no single atomic cutover; this is a
rolling update, like a Kubernetes rolling update (vs. an atomic blue/green
switch).

> **No Traefik credential needed.** The Traefik API is enabled internally
> (`--api.insecure=true`) and reached only over the `rise_default` network
> (`http://rise-traefik:8080`); it is **never published to the host**. If you do
> put the API behind basic-auth, embed the credentials in `traefik_api_url`'s
> userinfo (`http://user:pass@host:8080`).

> **Probing is opt-in.** With **no** `health_check` set, a deployment is
> considered ready **as soon as its container is running**. Setting a
> `health_check` switches readiness to a **2xx–3xx** check, enforced by
> **Traefik's** per-server health check (mirrored by **Rise's own probe** when no
> Traefik API is reachable).
>
> The zero-gap rollover guarantee only holds **when a `health_check` is set.**
> Without one Traefik has no per-server check to drain against, so a new server
> joins the load balancer the moment it is *running* — Traefik may route to it
> before the app inside has finished starting (the same exposure a Kubernetes pod
> with no readiness probe has). Set a `health_check` for apps that need traffic
> withheld until they are ready.

`traefik_api_url` defaults differ per environment: the standalone stack defaults
to the in-network `http://rise-traefik:8080` (the standalone Traefik enables its
API internally without publishing `:8080`), while the host-run dev backend
(`mise br docker`) overrides it to `http://localhost:8090` (the dev Traefik
publishes its dashboard there).
