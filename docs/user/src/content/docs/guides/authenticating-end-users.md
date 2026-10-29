---
title: "Authenticating End Users"
description: "Add user login to a Rise app using the Generic OAuth extension, the rise_jwt cookie, or a DIY approach."
---

## When to use this

Your deployed app needs to authenticate end users, but you're unsure whether to use Rise's built-in identity (the `rise_jwt` cookie), the Generic OAuth extension (broker for third-party providers), or implement OAuth yourself. This guide helps you choose and set up the right approach.

For reference material, see [OAuth Extensions](../user-guide/oauth.md), [Authentication for Applications](../user-guide/authentication-for-apps/index.md), and the [OAuth Provider Extension](../extensions/oauth.md).

## The problem

OAuth providers (Google, GitHub, Snowflake, etc.) require every redirect URI to be registered ahead of time. A Rise app can have many URLs — production, staging, per-PR previews, custom domains, and localhost during development. You can't register all of them with the upstream provider. Additionally, you don't want the upstream client secret exposed in your application.

## The solution: Rise as an OAuth broker

The Generic OAuth extension makes Rise act as a reverse proxy between your app and the upstream provider. Rise exposes **one stable callback URL**, holds the upstream client secret (encrypted), and injects Rise-issued client credentials into your app as environment variables.

```text
App URL → Rise /authorize → Upstream OAuth provider
                                   │
App URL ← Rise /callback  ←────────┘
```

## Decision tree: which approach?

| If… | Use | Why |
|------|-----|-----|
| Rise is already the identity gateway (your app's access class uses authenticated/member access), and you just need the user's identity in your app | [**rise_jwt cookie**](../user-guide/authentication-for-apps/rise-jwt-cookie.md) | Rise authenticates the user and passes a signed JWT cookie — no extra setup |
| Your app needs to talk to a third-party OAuth provider (Google, Snowflake, GitHub) and the redirect-URI problem applies | [**Generic OAuth extension**](../user-guide/oauth.md) | One stable Rise callback URL; Rise holds the secret; preview URLs work without re-registration |
| You have your own backend, dynamic redirect URIs are acceptable, and you want full control | **DIY** | Register your app's URL directly with the provider; handle the flow yourself |

## The two OAuth extension flows

| Flow | Best for | Client auth | Token storage |
|------|----------|-------------|---------------|
| **Authorization code with PKCE** | SPAs and public clients (no backend secret) | `client_id` + `code_verifier` | Browser/session storage |
| **Confidential authorization code** | Server-rendered apps (confidential clients) | `client_id` + `client_secret` | Server-side session or HttpOnly cookie |

:::tip[Choosing a flow]
- **SPAs / mobile apps** → PKCE. Your app generates a `code_verifier` and `code_challenge`. Requires HTTPS or localhost for Web Crypto.
- **Server-rendered apps** → Confidential authorization code. Your backend presents the `client_secret` to Rise's token endpoint and stores tokens in a server-side session or HttpOnly cookie.
:::

## Step-by-step: Generic OAuth extension

Create the project first. The extension can be sealed before it exists. Put the
upstream configuration in `oauth.json`:

```json
{
  "provider_name": "Google",
  "client_id": "123456789.apps.googleusercontent.com",
  "issuer_url": "https://accounts.google.com",
  "scopes": ["openid", "email", "profile"]
}
```

Seal the upstream secret for this project, extension name, slot, and OAuth
configuration, then create the extension:

```bash
ENCRYPTED=$(printf '%s' "your_client_secret" | rise encrypt \
  --project my-app --extension-type oauth --extension oauth-google \
  --slot /spec/client_secret_encrypted --oauth-spec oauth.json)

rise extension create oauth-google -p my-app --type oauth \
  --spec "$(jq --arg secret "$ENCRYPTED" '.client_secret_encrypted = $secret' oauth.json)"
```

`--oauth-spec` resolves the effective token endpoint through Rise and binds the
secret to the upstream client ID, issuer, explicit endpoints, authentication
method, and resolved token destination. Token redirects are refused; use the
provider's canonical final token URL. Re-enter and seal the secret when any
of those change. A ciphertext sealed for another project, extension, or slot
cannot be used here. Restoring the same extension name within the same project
permits reuse when its OAuth configuration matches; recreating the project does
not. The generic resource API carries ciphertext as an opaque value.

`rise encrypt` reads plaintext from stdin when its optional plaintext argument
is omitted. It permits 100 encryptions per hour per user and requires an
unrestricted login plus access to the enclosing project.

:::note[Verified spec fields]
Required fields (validated in `src/server/extensions/providers/oauth/provider.rs`):
- `provider_name` — display name
- `client_id` — upstream OAuth client ID
- `client_secret_encrypted` — a `rise:enc:v2:` envelope sealed for this OAuth configuration
- `issuer_url` — OIDC issuer URL
- `scopes` — provider-specific; use an empty array for providers without scopes

Optional fields:
- `authorization_endpoint` — only needed for non-OIDC providers (auto-discovered for OIDC-compliant providers like Google, Auth0, Dex)
- `token_endpoint` — same: only for non-OIDC providers (e.g., GitHub, Snowflake)
:::

For a **non-OIDC provider** such as GitHub, add both endpoints to `oauth.json`
before sealing for the intended extension name:

```json
{
  "provider_name": "GitHub",
  "client_id": "Iv1.abc123...",
  "issuer_url": "https://github.com",
  "authorization_endpoint": "https://github.com/login/oauth/authorize",
  "token_endpoint": "https://github.com/login/oauth/access_token",
  "scopes": ["read:user", "user:email"]
}
```


### 3. Register the Rise callback URL upstream

Add this **exact** URL to your provider's allowed redirect URIs (in the Google/GitHub/Snowflake console):

```text
https://rise.example.net/oidc/my-app/oauth-google/callback
```

The pattern is: `{RISE_PUBLIC_URL}/oidc/{project}/{extension}/callback`

:::caution
Register the **Rise broker URL**, not your app's URL. Your app never talks directly to the upstream provider — Rise does, on your behalf.
:::

### 4. Use the auto-injected environment variables

Rise injects these into every deployment (and `rise run`) for each OAuth extension. The extension name is uppercased with hyphens replaced by underscores — verified in `src/server/extensions/providers/oauth/provider.rs`:

| Variable | Contains | Example (extension `oauth-google`) |
|----------|----------|------------------------------------|
| `{EXT}_CLIENT_ID` | Rise-issued client ID (deterministic: `{project}-{extension}`) | `OAUTH_GOOGLE_CLIENT_ID` = `my-app-oauth-google` |
| `{EXT}_CLIENT_SECRET` | Rise-issued client secret (for confidential clients) | `OAUTH_GOOGLE_CLIENT_SECRET` |
| `{EXT}_ISSUER` | Rise OIDC proxy URL for this extension | `OAUTH_GOOGLE_ISSUER` = `https://rise.example.net/oidc/my-app/oauth-google` |

Your app uses these to start the OAuth flow:

```text
# Redirect user to authorize:
GET {OAUTH_GOOGLE_ISSUER}/authorize?code_challenge=...&state=...

# Exchange the authorization code:
POST {OAUTH_GOOGLE_ISSUER}/token
  client_id=$OAUTH_GOOGLE_CLIENT_ID
  client_secret=$OAUTH_GOOGLE_CLIENT_SECRET   # confidential-client flow
  # OR
  code_verifier=...                           # PKCE flow
```

Rise handles the upstream provider exchange internally and returns the upstream tokens to your app. Your app then owns those tokens and decides how to store them.

## Common mistakes

- **Registering the wrong redirect URI** — register the Rise broker URL (`/oidc/{project}/{extension}/callback`), not your app's URL. Rise forwards the user back to your app after the upstream callback.
- **Choosing PKCE for a server-rendered app** — use the confidential-client flow instead. Your backend holds the `client_secret` and stores tokens outside browser JavaScript.
- **Forgetting HTTPS for browser PKCE** — generating the S256 challenge with `crypto.subtle.digest` requires a secure context (HTTPS or `localhost`).
- **Putting the upstream secret directly in the app** — use `rise encrypt` and store it in the extension spec. The app receives Rise-issued credentials, never the upstream secret.
- **Omitting `authorization_endpoint`/`token_endpoint` for non-OIDC providers** — Google, Auth0, and Dex auto-discover these. GitHub and Snowflake do not — you must specify them manually.
