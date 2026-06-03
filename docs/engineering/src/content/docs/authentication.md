---
title: "Authentication & Tokens"
---

Rise both **issues** its own JWTs and **accepts** JWTs from external identity
providers. This page is the operator's map of that token model: what each token
is for, which configuration and endpoints govern it, and the security
implications you need to plan for. For the exact key names and YAML, see the
[Configuration Guide](/operator-docs/configuration/); for the internal
algorithm/claim disambiguation rules, see the engineering reference in
`crates/rise-backend-auth/README.md`.

## At a glance

| Token | Direction | Algorithm | Audience (`aud`) | Verified by | Governed by |
|---|---|---|---|---|---|
| **[Session](#session-hs256)** | Issued | HS256 | Rise `public_url` | Rise API middleware | `server.jwt_signing_secret`, `server.jwt_expiry_seconds` |
| **[Access](#access-hs256--token-exchange)** | Issued | HS256 | Rise `public_url` | Rise API middleware | `server.auth_token_max_ttl_seconds`, `auth.allow_raw_external_tokens` |
| **[Ingress](#ingress-rs256)** | Issued | RS256 | Project URL | Nginx/ingress via Rise JWKS | `server.rs256_private_key_pem` |
| **[Workload identity](#workload-identity-rs256)** | Issued | RS256 | Caller-supplied (e.g. `sts.amazonaws.com`) | External system via Rise JWKS | `server.rs256_private_key_pem`, `deployment_controller.identity_token_ttl_seconds` |
| **[User login (OIDC)](#user-login-oidc)** | Accepted | per IdP | — | Rise (JWKS of `auth.issuer`) | `auth.issuer`, `auth.client_id`, `auth.client_secret` |
| **[Service account](#service-accounts-cicd)** | Accepted | RS256 (JWKS) | project-scoped | Rise (JWKS of the SA issuer) | per-project (CLI/API managed) |
| **[Controller](#controllers)** | Accepted | RS256 (JWKS) | per identity | Rise (JWKS of controller issuer) | `auth.controllers[]` |

## Tokens Rise issues

Rise acts as an OIDC issuer for the RS256 tokens below. External systems verify
them against Rise's public key, published at:

- **JWKS:** `GET {public_url}/api/v1/auth/jwks`
- **OIDC discovery:** `GET {public_url}/.well-known/openid-configuration` (served at
  the root, per the OIDC spec)

### Session (HS256)

The token a user (or the CLI) holds after logging in; it authenticates requests
to the Rise API and UI. It is symmetric (HS256), signed with
`server.jwt_signing_secret` (base64, ≥ 32 bytes), scoped to the Rise
`public_url` audience, and carries the claims listed in `server.jwt_claims`.
Lifetime is `server.jwt_expiry_seconds` (default 24h). Session tokens are an
internal concern — they are never verified outside Rise.

### Access (HS256) — token exchange

A short-lived, Rise-issued token that encodes a **fully-resolved principal** (a
service account or a controller). It is minted by the RFC 8693 token-exchange
endpoint and is symmetric (HS256, `aud` = Rise `public_url`), so — like Session
tokens — it is verified only inside Rise and never by external parties. It is
distinguished from a Session token by its JWT header `typ` (`rise-access+jwt`).

**Why it exists.** Without it, a CI service account presents its external OIDC
token on *every* request and Rise re-resolves the service account per request
(claim matching + DB lookups in the hot path). With token exchange, the caller
resolves once, up front, and every subsequent request is a snap decision on the
embedded principal.

**Exchange endpoint:** `POST /api/v1/auth/token` (public — the subject token is
the credential). It follows RFC 8693:

```jsonc
// request
{
  "grant_type": "urn:ietf:params:oauth:grant-type:token-exchange",
  "subject_token": "<external OIDC JWT>",
  "subject_token_type": "urn:ietf:params:oauth:token-type:jwt",
  "rise_project": "my-project"   // required for project service-account exchange;
                                  // "resource" is accepted as an alias
}
// response
{
  "access_token": "<rise HS256 jwt>",
  "token_type": "Bearer",
  "issued_token_type": "urn:ietf:params:oauth:token-type:jwt",
  "expires_in": 600
}
```

The exchange verifies the inner OIDC token exactly as the accept paths below do
(issuer guard → JWKS signature + `exp` → per-identity claim matching), then mints
the access token. With `rise_project` it resolves a **project service account**;
without it, a **controller** identity. Lifetime is clamped to
`server.auth_token_max_ttl_seconds` (default 600s) — deliberately short, because
an exchanged token cannot be revoked mid-life (deleting the SA or tightening its
environment restrictions only takes effect once it expires).

> This STS-style endpoint is **distinct** from the OIDC `token_endpoint` (the
> authorization-code exchange at `/api/v1/auth/code/exchange`); the discovery
> document continues to advertise the latter.

**Migrating off raw tokens (`auth.allow_raw_external_tokens`).** Defaults to
`true`: a service account may still present its raw external OIDC token directly
to project-scoped endpoints (the legacy per-request path), which Rise resolves as
before. Set it to `false` to require pre-exchange — callers must obtain an access
token from `/api/v1/auth/token` first. While it is `true`, every raw-token
request is logged as deprecated (keyed by issuer) so you can see who still needs
migrating; leaving it `true` forfeits the security benefits above. It is slated
to default to `false` in a future release.

### Ingress (RS256)

Issued for **private project** access enforcement at the ingress layer (see
[Private Project Authentication](/operator-docs/kubernetes/private-auth/)). It is
asymmetric (RS256) so the ingress can verify it via Rise's JWKS without holding a
shared secret, and its audience is the project URL. Minted by `GET /api/v1/auth/ingress`.

### Workload identity (RS256)

Issued to **deployed apps** so they can federate identity to external systems
(AWS STS, GCP Workload Identity Federation, HashiCorp Vault, …) without
long-lived secrets. The subject describes the *Rise* identity
(`rise:proj:<project>:env:<environment>`), the audience is supplied per request
by the caller, and the token is signed with the same RS256 key as ingress
tokens — so the JWKS/discovery endpoints above already cover verification.

Two paths produce these tokens:

- The controller **auto-mints** them and projects them into pods under
  `/var/run/secrets/rise/tokens/`. Lifetime is
  `deployment_controller.identity_token_ttl_seconds` (default 3600s); the controller
  re-mints once a token passes half its lifetime.
- Apps may also call `POST /api/v1/identity/token` directly, exchanging their
  deployment bootstrap credential (`Authorization: Bearer <credential>`) for a
  token with a requested audience.

### The RS256 key is operationally load-bearing

`server.rs256_private_key_pem` signs **both** ingress and workload tokens and
backs the JWKS. If you do **not** configure it, Rise generates a fresh key pair
on every start — which silently invalidates all previously issued ingress and
workload tokens and rotates the JWKS out from under any external verifier.

- **Always** set `rs256_private_key_pem` (and optionally `rs256_public_key_pem`,
  otherwise derived) in any non-ephemeral deployment.
- Treat it as a secret; store it via your secret manager, not in plain config.
- Rotating it invalidates outstanding ingress/workload tokens and changes the
  published JWKS — coordinate with anything that has cached the keys.

## Tokens Rise accepts

When Rise receives a bearer token whose issuer is **not** Rise itself, it
discovers that issuer's JWKS via `{issuer}/.well-known/openid-configuration`
(SSRF-validated) and verifies the RS256 signature, `iss`, and `exp` before
applying per-identity claim constraints.

### User login (OIDC)

Interactive login federates to your IdP via `auth.issuer` / `auth.client_id` /
`auth.client_secret` / `auth.scopes`. On success Rise mints a **Session** token
(above). This is how operators wire Rise to Dex, Okta, Entra ID, Google, etc.

### Service accounts (CI/CD)

Project-scoped identities for automation (e.g. pipelines) that present an OIDC
JWT from a trusted CI issuer. They are managed per project through the CLI/API
rather than static config; Rise validates the token against the issuer's JWKS
and the service account's configured claim constraints. See the service-account
workflow in the Rise user documentation.

### Controllers

Trusted external controllers authenticate with OIDC JWTs declared under
`auth.controllers[]` — each entry binds a stable `id`, an `issuer`, and required
`claims` (`aud` is mandatory; add `sub`/`scope`/etc. as needed; `*` wildcards are
supported). Use a dedicated issuer or audience per controller to keep identities
unambiguous.

> Controllers authenticate against the generic resource API
> (`/api/v1/resources`). Today, built-in resources do not yet grant controllers
> write access (their `allowed_status_controller_ids` is empty), so configuring
> controllers is currently forward-looking — it is safe to set up ahead of time.

## Security checklist

- [ ] `server.jwt_signing_secret` is a unique, ≥ 32-byte base64 secret (rotating
      it invalidates all sessions).
- [ ] `server.rs256_private_key_pem` is set and persisted (see
      [above](#the-rs256-key-is-operationally-load-bearing)).
- [ ] `server.cookie_secure: true` and an `https://` `public_url` in production.
- [ ] Every accepted issuer (`auth.issuer`, service accounts, controllers) is one
      you control or explicitly trust; constrain `aud` (and `sub` where possible).
- [ ] Workload `aud` values are the specific external systems you intend (avoid
      broad audiences); `deployment_controller.identity_token_ttl_seconds` is as short as
      your federation tolerates.
