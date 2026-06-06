---
title: "Production deployment"
---

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
`src/server/auth/handlers.rs`) against `server.public_url`'s host **and the
specific project's own resolved hosts** (its canonical ingress host plus any
active-deployment/custom-domain hosts), so an app host like `secret.${RISE_DOMAIN}`
is accepted and the user returns to the exact page they started on with no extra
setup. The match is scoped to *that* project rather than the whole parent domain,
so a redirect to a **different** project's host is rejected (no cross-project
open redirect).

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
| `REGISTRY_BASIC_AUTH` | empty | htpasswd users for the public registry (see [Container registry](/operator-docs/docker/registry/)). |
| `DEX_ISSUER` | `https://dex.${RISE_DOMAIN}` | OIDC issuer (see [Authentication / Dex](/operator-docs/docker/authentication/)). |
| `RISE_IMAGE_TAG` | `0.23.0` | **Manual** image pin — bump on upgrade. There is no automatic "latest released" resolution. |
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

`admin_users` (`ADMIN_EMAIL`) and `operator_users` (`RISE_OPERATOR_EMAIL`)
always bypass this check. `operator_users` defaults to none — no operator is
configured unless you set `RISE_OPERATOR_EMAIL` (a single email; mount a config
override for several). Operators have full access to the generic resource API
(`/api/v1/resources`), so grant the role deliberately.

To run a restricted stack via env, set `RISE_PLATFORM_ACCESS_POLICY=restrictive`
and grant one user with `RISE_PLATFORM_ALLOWED_EMAIL=you@example.com`. To grant
several users (or IdP groups via `allowed_idp_groups`), mount a config override
that replaces the `auth.platform_access` block instead of relying on the single
env var.
