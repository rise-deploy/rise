---
title: "Authenticating End Users"
description: "Add user login to a Rise app using the Generic OAuth extension, the rise_jwt cookie, or a DIY approach."
---

import { Aside } from '@astrojs/starlight/components';

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

### 1. Encrypt the upstream client secret

```bash
ENCRYPTED=$(rise encrypt "your_upstream_client_secret")
echo "$ENCRYPTED"
```

You can also pipe via stdin: `echo "your_upstream_client_secret" | rise encrypt`.

### 2. Create the OAuth extension

```bash
rise extension create oauth-google -p my-app \
  --type oauth \
  --spec '{
    "provider_name": "Google",
    "description": "Sign in with Google",
    "client_id": "123456789.apps.googleusercontent.com",
    "client_secret_encrypted": "'"$ENCRYPTED"'",
    "issuer_url": "https://accounts.google.com",
    "scopes": ["openid", "email", "profile"]
  }'
```

<Aside type="note" title="Verified spec fields">
Required fields (validated in `src/server/extensions/providers/oauth/provider.rs`):
- `provider_name` — display name
- `client_id` — upstream OAuth client ID
- `client_secret_encrypted` — output of `rise encrypt` (the `client_secret_ref` field is deprecated)
- `issuer_url` — OIDC issuer URL
- `scopes` — at least one scope required

Optional fields:
- `authorization_endpoint` — only needed for non-OIDC providers (auto-discovered for OIDC-compliant providers like Google, Auth0, Dex)
- `token_endpoint` — same: only for non-OIDC providers (e.g., GitHub, Snowflake)
</Aside>

For a **non-OIDC provider** like GitHub, specify endpoints manually:

```bash
rise extension create oauth-github -p my-app \
  --type oauth \
  --spec '{
    "provider_name": "GitHub",
    "client_id": "Iv1.abc123...",
    "client_secret_encrypted": "'"$ENCRYPTED"'",
    "issuer_url": "https://github.com",
    "authorization_endpoint": "https://github.com/login/oauth/authorize",
    "token_endpoint": "https://github.com/login/oauth/access_token",
    "scopes": ["read:user", "user:email"]
  }'
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
