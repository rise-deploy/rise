---
title: "Quick start"
---

## Quick start (local HTTP overlay)

This is the fastest way to try the backend and is exactly what the e2e test
(`scripts/ci/e2e-docker.sh`) runs. No `RISE_DOMAIN` / `ACME_EMAIL` needed.

```bash
docker compose -f docker-compose.standalone.yaml \
               -f docker-compose.standalone.local.yaml up -d
```

This serves the control plane on `http://rise.localhost:3000` and apps on
`http://{project}.rise.localhost` over plain HTTP. The `.localhost` suffix
resolves to `127.0.0.1` in most browsers, so no `/etc/hosts` edits are needed
for app hosts.

> **Why `.localhost` here, but `.local` for Kubernetes?** The Docker backend
> runs everything on a single host, so `*.localhost` — which RFC 6761 reserves
> to `127.0.0.1` — is ideal: it wildcards to loopback automatically, with no
> per-app `/etc/hosts` entries. The Kubernetes/minikube setup instead uses
> `rise.local`, deliberately *because* it does **not** auto-resolve: it is
> overridden (via a pod `host_aliases` entry and `/etc/hosts`) to the
> cluster/host IP so pods and your browser can reach a **non-loopback** address.
> Using `.localhost` there would wrongly resolve to each pod's own loopback and
> to `127.0.0.1` in the browser. The two conventions are intentional, not a
> drift — keep `.localhost` for Docker and `.local` for Kubernetes.

> The demo secrets, passwords and the bundled Dex IdP are fine for local use but
> **insecure for anything real** — see [Production deployment](/operator-docs/docker/production/).

### Local development (run Rise on the host, no image)

For the inner-loop while hacking on the backend you don't need to build the Rise
image at all. `mise br docker` brings up only the support services from the dev
`docker-compose.yml` (Postgres, Dex, registry, Traefik — **not** a Rise
container) and runs Rise on the host via `cargo run --features cli,backend --
backend server`, reusing the env-driven `config/docker.yaml` (run_mode `docker`)
with host-facing overrides (`DATABASE_URL`/`DEX_ISSUER`/`RISE_REGISTRY_URL`
pointed at `localhost` / the `/etc/hosts` aliases). It is the Docker backend of
the unified `mise br [k8s|docker]` task (`k8s`, the default, runs the host backend
against the Kubernetes dev config). Migrations auto-run on startup, so no
`db:migrate` step is needed.

> For the broader getting-started / two-backend onboarding (Kubernetes and
> Docker side by side), see the [Local Development](/operator-docs/development/)
> guide. This section covers the Docker-specific wiring only.

**Prerequisites (one-time):** run `mise setup hosts` once — it adds
`rise-dex → 127.0.0.1` to `/etc/hosts`, which host-Rise needs to reach the OIDC
issuer `http://rise-dex:5556/dex`. That is the **only** setup required: the
registry uses `localhost:5000` (which Docker treats as insecure by default, so
no `daemon.json` change), and app / control-plane hosts use `*.rise.localhost`
(loopback per RFC 6761, resolved automatically by browsers). You do **not** need
the full `mise setup` (that provisions minikube/Kubernetes, which the Docker
backend doesn't use).

The twist is **container→host reachability**: Traefik's forwardAuth and the app
containers run in containers but must reach Rise on the host. `mise br docker`
solves this with Docker's magic host gateway, which is **portable across Docker
Desktop (macOS/Windows) and Linux** — no per-platform overrides:

- **Traefik → backend (forwardAuth):** the task sets
  `RISE_AUTH_BACKEND_URL=http://host.docker.internal:3000`. Docker Desktop
  resolves `host.docker.internal` to the host automatically; on Linux the dev
  `docker-compose.yml` Traefik service carries
  `extra_hosts: ["host.docker.internal:host-gateway"]` so it resolves there too
  (a no-op on Docker Desktop).
- **App → backend:** the task sets `RISE_APP_BACKEND_IP=host-gateway` (with
  `RISE_APP_BACKEND_HOST_ALIAS=rise.localhost`), so Rise's
  `app_backend_host_aliases` machinery injects `rise.localhost:host-gateway`
  into every managed app container's `extra_hosts`. Docker replaces the special
  `host-gateway` value with the host gateway per container on **both** Docker
  Desktop and Linux, so apps reach host-Rise (validate the `rise_jwt` cookie,
  OIDC discovery) without any DNS lookup.

The new `deployment_controller.app_backend_ip` setting (env
`RISE_APP_BACKEND_IP`) is the explicit override that supplies `host-gateway`
verbatim, bypassing the DNS resolution used for a containerized backend. It is
**local-dev only** — leave it unset in production.

- **`/.rise/*` → backend (login on app hosts):** for a **private** app the
  forwardAuth handler 302-redirects unauthenticated requests to
  `{app-host}/.rise/auth/signin`, which must be served by the **backend** (to set
  the session cookie on the app's host). The standalone compose does this with a
  high-priority `PathPrefix(`/.rise`)` router as a **label on the `rise-backend`
  container** — but the dev stack has no backend container (Rise runs on the
  host), so a Docker-provider label can't target it. Instead the dev Traefik runs
  a **file provider** (`dev/traefik/dynamic/rise-dotrise.yml`, mounted at
  `/etc/traefik/dynamic`) defining a `priority=1000` `/.rise` router whose service
  URL is `http://host.docker.internal:3000`. Without it, `/.rise/auth/signin` on
  an app host falls through to the app router and is (wrongly) served by the app.
