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
whether you layer the overlay, which `RISE_*` env vars are set, and which
Traefik entrypoints exist.

| | Production (base file) | Local / dev (base + overlay) |
|---|---|---|
| Traefik entrypoints | `web` (:80) + `websecure` (:443), 80→443 redirect | `web` (:80) only |
| TLS | Let's Encrypt via the `le` ACME resolver (HTTP-01) | none (plain HTTP) |
| Domain | `${RISE_DOMAIN}` | `rise.localhost` |
| Backend config | shipped `config/docker.yaml` (run_mode `docker`, baked into the image at `/etc/rise/docker.yaml`); nothing is mounted | same shipped file — its built-in defaults are already local |
| Config differences | prod `RISE_*` env on the `rise` service flip it to https / `le` / the real domain | overlay resets those env vars to the local HTTP defaults |
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

App hosts in the standard layout are **subdomains of the ingress domain**
(`{project}.${RISE_DOMAIN}`), which makes them *siblings* of the control-plane
host `rise.${RISE_DOMAIN}` rather than subdomains of it. The post-login deep-link
`redirect` is validated by `validate_redirect_url` (in
`src/server/auth/handlers.rs`) against both `server.public_url`'s host **and** the
configured ingress domain (derived from `production_ingress_url_template`), so an
app host like `secret.${RISE_DOMAIN}` is accepted and the user returns to the
exact page they started on with no extra setup.

Apps on an unrelated or **custom** domain also work, but the domain must be
**registered as a project custom domain** — that registration is what makes the
Docker controller emit the Traefik `Host(...)` rule (with forwardAuth) that
routes the domain to the app and serves `/.rise/auth/*` from the backend
(`reconciler.rs`). Login then succeeds: the session cookie is set on the custom
host via the one-time-token `/.rise/auth/complete` flow, independent of
`validate_redirect_url`. One caveat — the original deep-link is still validated
against `public_url`, so on a custom domain the user returns to the app **root**
(`/`) rather than the specific path requested. An **unregistered** host isn't
routed at all (Traefik returns 404); it doesn't reach login.

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
| `RISE_JWT_SIGNING_SECRET` | insecure repo placeholder | **Set it.** Overrides the demo secret baked into `config/docker.yaml`. `openssl rand -base64 32`. |
| `RISE_ENCRYPTION_KEY` | insecure repo placeholder | **Set it.** Overrides the demo AES key. `openssl rand -base64 32`. |
| `OIDC_CLIENT_SECRET` | `rise-backend-secret` | **Set it.** Keep in sync with the Dex client secret in `dev/dex/config.yaml`. |
| `ADMIN_EMAIL` | `admin@example.com` | Initial admin user. |
| `RISE_PLATFORM_ACCESS_POLICY` | `allow_all` | Who may use the CLI/API/dashboard. `allow_all` = any authenticated user; `restrictive` = allowlist only (see [Platform access](#platform-access)). |
| `RISE_PLATFORM_ALLOWED_EMAIL` | empty | A single email granted platform access when the policy is `restrictive`. For several users, mount a config override. |

The `rise` service also sets these `RISE_*` overrides on `config/docker.yaml`
to flip it from its local defaults to production (https / `le` resolver / the
real domain): `PUBLIC_URL`, `RISE_INGRESS_DOMAIN`, `RISE_INGRESS_SCHEME`,
`RISE_CERTRESOLVER`, `RISE_TRAEFIK_ENTRYPOINT`, `RISE_CLIENT_REGISTRY_URL`,
`RISE_COOKIE_SECURE`. You normally don't touch these directly.

### 4. What to change before exposing it

The shipped `config/docker.yaml` and `dev/dex/config.yaml` contain **well-known
placeholder secrets that provide no security** (the `RISE_*_SECRET` / `*_KEY`
env vars default to those placeholders when unset). Before any real use:

- **Regenerate every secret.** Set `RISE_JWT_SIGNING_SECRET`,
  `RISE_ENCRYPTION_KEY` (each `openssl rand -base64 32`), `OIDC_CLIENT_SECRET`,
  and `POSTGRES_PASSWORD`. The `OIDC_CLIENT_SECRET` must stay in sync with the
  Dex client secret in `dev/dex/config.yaml`. These override the demo defaults
  baked into `config/docker.yaml`.
- **Use a real IdP.** The bundled Dex is a demo provider with static passwords
  (`admin@example.com` / `password`) that grant a Rise admin session. The base
  Compose file therefore does **not** publish Dex to the internet — it is only
  reachable internally by the backend. Replace it with a proper IdP (see below).
  Do **not** layer `docker-compose.standalone.demo-idp.yaml` (which exposes Dex
  publicly at `dex.${RISE_DOMAIN}`) outside a throwaway demo.
- **Lock down exposed host ports.** Postgres (`5432`) and the registry (`5000`)
  are published to the host for convenience; bind them to localhost or drop the
  mappings (the registry is reachable through Traefik).
- **Persist & back up volumes.** `postgres_data`, `registry_data`,
  `traefik_acme` are local volumes — use durable storage and a backup strategy.
- **Set resource limits** per service to fit your host (none are set in the
  reference file).
- **Decide platform access.** By default any authenticated user may use the
  platform; lock it down if that is not what you want (see [Platform access](#platform-access)).

### Platform access

"Platform access" gates who may use the **control plane** — the `rise` CLI, the
API, and the dashboard — as distinct from merely logging in to a
forwardAuth-protected app. It is controlled by `auth.platform_access` in
`config/docker.yaml`:

- **`allow_all`** (the shipped default): any user who authenticates via the IdP
  may use the platform. Suitable when the IdP already restricts who can sign in.
- **`restrictive`**: only users on the allowlist may use the platform. Everyone
  else authenticates fine but receives a 403 ("configured for application access
  only") from the platform-access middleware.

`admin_users` (`ADMIN_EMAIL`) and `operator_users` always bypass this check.

To run a restricted stack via env, set `RISE_PLATFORM_ACCESS_POLICY=restrictive`
and grant one user with `RISE_PLATFORM_ALLOWED_EMAIL=you@example.com`. To grant
several users (or IdP groups via `allowed_idp_groups`), mount a config override
that replaces the `auth.platform_access` block instead of relying on the single
env var.

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

### App → backend resolution (local dev)

Apps that validate the `rise_jwt` cookie or perform OIDC discovery against the
public issuer/control-plane host (e.g. `rise.localhost`) must be able to reach
the Rise backend at that host. In a **local** stack the public host resolves to
the app container's *own* loopback, not the backend — so those calls would fail.

To fix this locally the Docker controller stamps `HostConfig.extra_hosts` on
every managed app container, mapping the configured alias host to the backend's
IP on the shared `rise_default` network. The backend IP is resolved at reconcile
startup from the `auth_backend_url` host (e.g. `rise`) via Docker DNS. The local
overlay enables it by setting `RISE_APP_BACKEND_HOST_ALIAS=rise.localhost`, which
populates `deployment_controller.app_backend_host_aliases`.

**Production needs nothing here** — and the alias list is empty by default.
Public DNS resolves `rise.${RISE_DOMAIN}` to Traefik, which terminates TLS and
forwards to the backend; injecting an `extra_hosts` override in production would
wrongly bypass Traefik and break TLS. (The captured backend IP is fixed at
container-create time; if the backend restarts with a new IP, existing app
containers keep the old entry until recreated — acceptable for local dev.)

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

The bundled Dex is a **demo IdP** (in-memory storage, static passwords —
`admin@example.com` / `password`). For production, prefer an external IdP: set
`auth.issuer`, `auth.client_id` and `auth.client_secret` to the external
provider's values and drop the `dex` service from the stack.

The base Compose file does **not** publish Dex publicly; the backend reaches it
only over the internal `rise_default` network. Exposing the demo IdP at
`dex.${RISE_DOMAIN}` is opt-in via the `docker-compose.standalone.demo-idp.yaml`
overlay, intended only for a throwaway demo/evaluation stack:

```bash
docker compose -f docker-compose.standalone.yaml \
               -f docker-compose.standalone.demo-idp.yaml up -d
```

If you do run the **bundled Dex in production** (demo overlay layered), two
config changes in `dev/dex/config.yaml` are **both** required — Dex enforces
each independently:

1. **Issuer.** Dex validates that the issuer it *serves* equals its configured
   `issuer`. The shipped config advertises `http://rise-dex:5556/dex`; when
   `DEX_ISSUER` points at `https://dex.${RISE_DOMAIN}` (the base file's default),
   change the served `issuer` in `dev/dex/config.yaml` to match — otherwise Dex
   rejects the discovery/token requests.
2. **Redirect URI.** The backend builds the OAuth callback as
   `{public_url}/api/v1/auth/callback`, i.e.
   `https://rise.${RISE_DOMAIN}/api/v1/auth/callback` in production. Dex rejects
   any `redirect_uri` not in `staticClients[rise-backend].redirectURIs`, which
   only lists the local hosts. **Add** `https://rise.${RISE_DOMAIN}/api/v1/auth/callback`
   (and `https://rise.${RISE_DOMAIN}/.rise/auth/callback` if you use the `/.rise`
   control-plane callback) to that list. Without this, login fails with
   `invalid redirect_uri` even after fixing the issuer.

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

The Rise backend mounts the **host** Docker socket, so every image pull is
executed by the **host's** Docker daemon — not from inside the Compose network.
That daemon resolves `registry_url`'s host with the **host's** resolver, which
does **not** consult Docker's embedded DNS on `rise_default`. The default
`registry_url=rise-registry:5000` therefore does **not** work out of the box on a
production host: `rise-registry` is only resolvable inside the Compose network,
and `:5000` is plain HTTP, which the daemon rejects unless told otherwise. Pick
one of:

- **Internal path (default `registry_url=rise-registry:5000`).** Two host-daemon
  prerequisites:
  1. Make `rise-registry` resolvable by the host daemon — add a host entry
     (e.g. `127.0.0.1 rise-registry` in `/etc/hosts`, or an `extra_hosts` /
     published-port mapping) so it reaches the registry container, **and**
  2. Mark it insecure (plain HTTP): add `"rise-registry:5000"` to the daemon's
     `insecure-registries` in `/etc/docker/daemon.json` and restart Docker.

  No auth is needed on this path — it never crosses the authenticated Traefik
  edge.
- **Host-published loopback.** Publish the registry on a host loopback port (as
  the local overlay does, `127.0.0.1:5000:5000`) and set
  `RISE_REGISTRY_URL=127.0.0.1:5000`. Still requires the matching
  `insecure-registries` entry for `127.0.0.1:5000`.
- **Public path.** Point `RISE_REGISTRY_URL` at the public
  `registry.${RISE_DOMAIN}` (TLS, no insecure-registry needed). The **daemon**
  then needs credentials too: run `docker login registry.${RISE_DOMAIN}` on the
  Rise host, or add a matching entry to the daemon's `~/.docker/config.json`.

Because all three URLs reference the same registry content, an image pushed to
`registry.${RISE_DOMAIN}` is the exact image the daemon pulls.

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
