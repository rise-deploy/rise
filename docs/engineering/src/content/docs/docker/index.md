---
title: "Docker deployment backend"
---

The Docker deployment backend runs applications as plain Docker containers on a
single host and routes traffic to them with [Traefik](https://traefik.io/)
(Docker provider). It is the lightweight counterpart to the
[Kubernetes backend](/operator-docs/kubernetes/): no cluster, no Helm — just a
Docker daemon and a Compose stack.

A complete, runnable reference stack ships in the repository as
[`docker-compose.standalone.yaml`](https://github.com/NiklasRosenstein/rise/blob/main/docker-compose.standalone.yaml)
with a local/dev overlay
[`docker-compose.standalone.local.yaml`](https://github.com/NiklasRosenstein/rise/blob/main/docker-compose.standalone.local.yaml).
This guide explains how the stack is wired and how to run it locally or in
production.

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
`deployment_controller.traefik_network`) so it can reach app containers by their
network alias. The Compose project name is fixed to `rise` so the network is
always named `rise_default` regardless of the launch directory.

## Two run modes

The same base file serves both production and local/dev; the difference is
whether you layer the overlay, which config the backend mounts, and which
Traefik entrypoints exist.

| | Production (base file) | Local / dev (base + overlay) |
|---|---|---|
| Traefik entrypoints | `web` (:80) + `websecure` (:443), 80→443 redirect | `web` (:80) only |
| TLS | Let's Encrypt via the `le` ACME resolver (HTTP-01) | none (plain HTTP) |
| Domain | `${RISE_DOMAIN}` | `rise.localhost` |
| Backend config mounted | `config/compose-docker.production.yaml` | `config/compose-docker.local.yaml` |
| Public registry / Dex routers | exposed via Traefik | dropped (`labels: !reset []`) |

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

> The demo secrets, passwords and the bundled Dex IdP are fine for local use but
> **insecure for anything real** — see [Production deployment](#production-deployment).

## Production deployment

The base file alone is production-*shaped* (TLS on by default) but not
production-*ready*. Treat it like a Helm `values.yaml`: a sound base you harden
before exposing it.

### 1. DNS

Point these records at the host running the stack:

| Host | Purpose |
|------|---------|
| `rise.${RISE_DOMAIN}` | Control plane (UI + API) |
| `*.${RISE_DOMAIN}` | App ingress (`{project}.${RISE_DOMAIN}`) |
| `registry.${RISE_DOMAIN}` | Container registry |
| `dex.${RISE_DOMAIN}` | Bundled Dex IdP (only if you keep it) |

App hosts must be subdomains of the control-plane host: the post-login same-host
redirect is validated against `server.public_url`
(`validate_redirect_url` in `src/server/auth/handlers.rs`).

### 2. TLS / Let's Encrypt

Bring the stack up with the domain and ACME contact set:

```bash
export RISE_DOMAIN=rise.example.com ACME_EMAIL=ops@example.com
docker compose -f docker-compose.standalone.yaml up -d
```

Traefik requests certificates via the HTTP-01 challenge on the `web` entrypoint
(`certificatesresolvers.le`); all plain HTTP is redirected to HTTPS.

While testing, point Traefik at the **Let's Encrypt staging** directory to avoid
hitting production rate limits, then drop it for real certificates:

```bash
export ACME_CA_SERVER=https://acme-staging-v02.api.letsencrypt.org/directory
```

### 3. Environment variables (production)

| Variable | Default | Notes |
|----------|---------|-------|
| `RISE_DOMAIN` | — (required) | Base domain; drives all host templates. |
| `ACME_EMAIL` | — | Contact for Let's Encrypt. |
| `ACME_CA_SERVER` | LE production | Set to LE staging while testing. |
| `REGISTRY_BASIC_AUTH` | empty | htpasswd users for the public registry (see [Container registry](#container-registry-pushpull-close-the-loop)). |
| `DEX_ISSUER` | `https://dex.${RISE_DOMAIN}` | OIDC issuer (see [Authentication / Dex](#authentication--dex)). |
| `RISE_IMAGE_TAG` | `0.22.0` | **Manual** image pin — bump on upgrade. There is no automatic "latest released" resolution. |
| `RISE_IMAGE_REPOSITORY` | `ghcr.io/rise-deploy/rise` | Override for a fork/mirror. |
| `POSTGRES_PASSWORD` | `rise123` | Change it. |

### 4. What to change before exposing it

The shipped `config/compose-docker.production.yaml` and `dev/dex/config.yaml`
contain **well-known placeholder secrets that provide no security**. Before any
real use:

- **Regenerate every secret.** Replace `server.jwt_signing_secret`,
  `encryption.key` (each `openssl rand -base64 32`), `auth.client_secret`, and
  `POSTGRES_PASSWORD`. The `auth.client_secret` must stay in sync with the Dex
  client secret in `dev/dex/config.yaml`.
- **Use a real IdP.** The bundled Dex is a demo provider with static passwords —
  replace it (see below).
- **Lock down exposed host ports.** Postgres (`5432`) and the registry (`5000`)
  are published to the host for convenience; bind them to localhost or drop the
  mappings (the registry is reachable through Traefik).
- **Persist & back up volumes.** `postgres_data`, `registry_data`,
  `traefik_acme` are local volumes — use durable storage and a backup strategy.
- **Set resource limits** per service to fit your host (none are set in the
  reference file).

## Access classes & ingress authentication

Ingress authentication is driven by each project's **access class**
(`deployment_controller.access_classes`), specifically its `access_requirement`:

- **`None`** (e.g. the `public` class) — open, no auth. App routers carry only
  the host rule.
- **`Authenticated` / `Member`** (e.g. the `private` class) — the reconciler
  stamps a Traefik **forwardAuth** middleware on the app's router.

For a non-`None` class, the reconciler emits (see `render_traefik_labels` in
`labels.rs`):

```
traefik.http.middlewares.<router>-auth.forwardauth.address
    = http://rise:3000/api/v1/auth/ingress?project=<project>&signin_redirect=1
traefik.http.middlewares.<router>-auth.forwardauth.authResponseHeaders
    = X-Auth-Request-Email,X-Auth-Request-User
traefik.http.routers.<router>.middlewares = <router>-auth@docker
```

Traefik issues a subrequest to `/api/v1/auth/ingress` (the
`deployment_controller.auth_backend_url`, internally `http://rise:3000`) before
proxying each request:

- A valid Rise session cookie → `200` plus `X-Auth-Request-Email` /
  `X-Auth-Request-User`, which Traefik copies onto the forwarded request.
- No / invalid cookie → because the middleware sets `signin_redirect=1`, the
  handler returns a **`302`** to the **same-host** `/.rise/auth/signin` page
  (`build_signin_redirect_url` in `auth/handlers.rs`). This mirrors the
  Kubernetes nginx `auth-signin` flow.

The redirect target host is reconstructed from the `X-Forwarded-Proto` /
`X-Forwarded-Host` / `X-Forwarded-Uri` headers, so the user is sent to the login
page on **the app's own host** and returned to where they started after signing
in. The configured `auth_signin_url` is only a degraded fallback used when
`X-Forwarded-Host` is absent.

### Fail-closed

The backend **refuses to start** if any access class with a non-`None`
requirement is configured while `deployment_controller.auth_backend_url` is
empty (`build_app_state` in `src/server/state.rs`). Without the backend URL the
forwardAuth middleware cannot be wired, and those projects would otherwise be
served publicly with no authentication. Either set `auth_backend_url` (e.g.
`http://rise:3000`) or change the requirement to `None`.

### `/.rise` routing

The session cookie that forwardAuth reads must be set on the **app's host**, not
the control plane. To serve the login / ingress-auth endpoints on every app host,
the base Compose file adds a high-priority Traefik router on the `rise-backend`
container:

```
traefik.http.routers.rise-dotrise.rule     = PathPrefix(`/.rise`)
traefik.http.routers.rise-dotrise.priority = 1000
```

`priority=1000` exceeds the app routers' default (rule-length) priority, so a
request for `/.rise/...` on an app host is served by the Rise backend rather than
the app. This mirrors the Kubernetes `/.rise` Ingress path. In the local overlay
the same router is re-stamped as `rise-dotrise-web` on the plain `web`
entrypoint (since `websecure`/`le` do not exist there).

## Authentication / Dex

The backend uses a **single OIDC issuer** for both server-side token exchange and
browser logins. The issuer URL must resolve **identically** from inside the Rise
backend container and from the user's browser (OIDC "split-horizon").

### Local / dev: `rise-dex`

Local and dev stacks use the issuer `http://rise-dex:5556/dex`, which is what the
bundled `dev/dex/config.yaml` actually serves. `rise-dex` resolves:

- inside the network — via the container name plus an explicit `rise-dex`
  network alias on the `rise_default` network; and
- on the host/browser — via the `rise-dex → 127.0.0.1` `/etc/hosts` entry added
  by `mise run setup`, plus the published `:5556` host port.

Using one hostname that resolves the same everywhere avoids issuer-mismatch
failures where the backend and the browser would otherwise disagree on the
issuer URL.

### Production caveat (important)

Dex validates that the issuer it *serves* equals its configured `issuer`. The
shipped `dev/dex/config.yaml` advertises `http://rise-dex:5556/dex`. If a
production operator points `DEX_ISSUER` at `https://dex.${RISE_DOMAIN}` (the
default in the base Compose file), they **must also** change Dex's served
`issuer` in `dev/dex/config.yaml` to match — otherwise Dex rejects the
discovery/token requests.

The bundled Dex is a **demo IdP** (in-memory storage, static passwords). For
production, prefer an external IdP: set `auth.issuer`, `auth.client_id` and
`auth.client_secret` to the external provider's values and drop the `dex`
service (and its public router) from the stack.

## Container registry (push/pull close-the-loop)

The registry is the part most likely to trip operators up, because the push path
(a developer's machine) and the pull path (the Rise host's Docker daemon) can use
**different URLs for the same registry**.

The `oci-client-auth` registry provider **mints no credentials**
(`src/server/registry/providers/docker.rs` returns empty user/pass for both push
and pull). It assumes the relevant Docker client has already done `docker login`.
The config carries two URLs:

| Config key | Used by | Reference value |
|------------|---------|-----------------|
| `client_registry_url` | `rise deploy` on a developer host (**push**) | `registry.${RISE_DOMAIN}` |
| `registry_url` | the Rise host's Docker daemon (**pull**) | `rise-registry:5000` |

Both URLs point at the **same registry content** — only the network path differs.

### 1. Expose the registry (operator, once)

The base Compose file exposes the registry via Traefik at
`registry.${RISE_DOMAIN}` with TLS (`le`) and a basicauth middleware. Generate an
htpasswd entry and pass it as `REGISTRY_BASIC_AUTH` (escape `$` as `$$` in a
`.env` file):

```bash
htpasswd -nbB ci 's3cret'
# ci:$2y$05$....   →  REGISTRY_BASIC_AUTH='ci:$2y$05$....'  (or $$ in .env)
```

An internet-exposed registry **must** have auth — hence the basicauth
middleware. The internal pull path can stay unauthenticated within the trusted
host network.

### 2. Push (developer host, possibly remote)

Log in with the basicauth credentials, then deploy. The CLI uses the stored
`docker login` — `oci-client-auth` supplies no creds of its own.

```bash
docker login registry.${RISE_DOMAIN}      # basicauth user/pass
rise deploy --project myapp --image ...    # builds + pushes to client_registry_url
```

The push targets `client_registry_url` (`registry.${RISE_DOMAIN}`).

### 3. Pull (Rise host's Docker daemon)

The Rise server's Docker daemon pulls images using `registry_url`:

- **Internal path (default).** When pulling via `rise-registry:5000` on the same
  host/network, **no auth is needed** — it never crosses the authenticated
  Traefik edge.
- **Public path.** If the daemon must pull via the public
  `registry.${RISE_DOMAIN}` (e.g. a remote/multi-host setup), the **daemon**
  itself needs credentials too: run `docker login registry.${RISE_DOMAIN}` on the
  Rise host, or add a matching entry to the daemon's `~/.docker/config.json`.

Because both URLs reference the same registry, an image pushed to
`registry.${RISE_DOMAIN}` is the exact image pulled via `rise-registry:5000`.

## Troubleshooting

**Traefik 404s every route / "client version 1.24 is too old".** Traefik must be
new enough to negotiate the host Docker daemon's API version. The reference stack
pins `traefik:v3.7.1`, which negotiates the API directly over the raw socket
(no socket-proxy needed) against Docker 29.x (API 1.54). Older v3.x ship a Docker
client pinned to API 1.24, which the daemon rejects, after which Traefik 404s
every route. Confirm the provider connected:

```bash
docker logs rise-traefik 2>&1 | grep "Provider connection established with docker"
```

**Private app redirect loop.** If an unauthenticated request to a private app
loops instead of landing on a login page, the session cookie is being set on the
wrong host. Verify the `/.rise` router is in place (so the signin page is served
on the app host) and that the `302` `Location` points at
`{app-host}/.rise/auth/signin` — not the control plane. Also confirm
`cookie_secure` matches the scheme (`false` for HTTP local, `true` for HTTPS).

**Backend refuses to start ("…access class(es) … require authentication … but
`auth_backend_url` is empty").** This is the fail-closed guard. Set
`deployment_controller.auth_backend_url` (e.g. `http://rise:3000`) or set the
offending access classes to `access_requirement: None`.

**OIDC issuer mismatch / Dex rejects login in production.** `DEX_ISSUER` and the
issuer Dex actually serves (`dev/dex/config.yaml`) must be identical. See the
[production caveat](#production-caveat-important).

**App `404` right after a deploy reports Healthy.** Traefik observes new
containers asynchronously via the Docker provider, so routing lags the API's
"Healthy" mark by a few seconds. A `404` immediately after Healthy usually just
means the route is not registered yet — retry briefly.

**App containers left running after `docker compose down`.** App containers are
created by the Rise reconciler, not by Compose, so `compose down` leaves them.
Remove them by their bookkeeping label:

```bash
docker rm -f $(docker ps -aq --filter "label=rise.dev/managed-by=rise")
```
