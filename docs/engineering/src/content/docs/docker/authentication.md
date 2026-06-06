---
title: "Authentication & ingress auth"
---

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

> The `http://rise:3000` host shown here is the value for the **standalone
> Compose stack**, where the backend is a service named `rise`. It is whatever
> `deployment_controller.auth_backend_url` resolves to — on the host-dev path
> (`mise br docker`) that is `http://host.docker.internal:3000` (see
> [Local development](/operator-docs/docker/quick-start/#local-development-run-rise-on-the-host-no-image)), not
> `rise:3000`.

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
every managed app container, mapping the configured alias host
(`RISE_APP_BACKEND_HOST_ALIAS=rise.localhost`, which populates
`deployment_controller.app_backend_host_aliases`) to the backend. There are two
cases, depending on where the backend runs:

1. **Containerized backend (standalone Compose):** the alias is mapped to the
   backend's IP on the shared `rise_default` network, resolved at reconcile
   startup from the `auth_backend_url` host (e.g. `rise`) via Docker DNS
   (`resolve_backend_ip` in `src/server/state.rs`). The captured IP is fixed at
   container-create time; if the backend restarts with a new IP, existing app
   containers keep the old entry until recreated — acceptable for local dev.
2. **Host-run backend (`mise br docker`):** the task sets
   `RISE_APP_BACKEND_IP=host-gateway` (the `deployment_controller.app_backend_ip`
   setting), so the controller stamps `rise.localhost:host-gateway` verbatim,
   skipping DNS. Docker resolves the special `host-gateway` value to the host
   gateway **per container** at create time, on both Docker Desktop and Linux —
   so there is no captured IP and no staleness.

**Production needs nothing here** — and the alias list is empty by default
(`app_backend_ip` is unset too). Public DNS resolves `rise.${RISE_DOMAIN}` to
Traefik, which terminates TLS and forwards to the backend; injecting an
`extra_hosts` override in production would wrongly bypass Traefik and break TLS.

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
   to that list — this is the only redirect URI the backend registers, including
   for the custom-domain `/.rise` flow (the IdP callback always lands here, after
   which the backend redirects internally to `/.rise/auth/complete`). Without
   this, login fails with `invalid redirect_uri` even after fixing the issuer.
