---
title: "OAuth Provider Extension"
---

The `oauth` extension makes Rise act as an OAuth/OIDC proxy between your application and an upstream provider (Google, GitHub, Snowflake, custom SSO).

## The Problem It Solves

OAuth providers require pre-registering every allowed redirect URI. A Rise project can have many URLs that aren't all known in advance:

- **Production**: `my-app.apps.rise.example.com`
- **Staging environment**: `staging--my-app.preview.rise.example.com`
- **Preview deployments**: `my-app-mr--26.preview.rise.example.com` (created dynamically per branch/MR)
- **Local development**: `http://localhost:3000`

Registering all of these at the OAuth provider is impractical, especially for preview URLs that are generated on demand. The OAuth extension solves this by making Rise the single registered redirect URI. Your application redirects users to Rise's authorize endpoint; Rise redirects to the upstream provider using Rise's own callback URL; after authentication, Rise forwards back to whichever app URL initiated the request.

```
App (any URL) → Rise authorize → OAuth provider
                                       ↓
App (original URL) ← Rise callback ←──┘
```

You register **one URL** at the provider:

```
https://<rise-url>/oidc/<project>/<extension>/callback
```

Rise allows forwarding to any URL associated with the project (all deployment group and environment URLs) as well as `localhost` for local development.

## What It Does

- Stores provider credentials securely (client secret encrypted at rest, never in client environments).
- Exposes Rise OAuth endpoints (`authorize`, `token`, `callback`, OIDC discovery, JWKS).
- Generates scoped client credentials injected into app environments.
- Proxies OIDC discovery and JWKS so apps work identically in local dev and production.

## Configuration

```json
{
  "provider_name": "Google",
  "description": "Sign in with Google",
  "client_id": "your-client-id",
  "client_secret_encrypted": "rise_encrypted_secret",
  "issuer_url": "https://accounts.google.com",
  "scopes": ["openid", "email", "profile"]
}
```

## Optional Overrides

- `authorization_endpoint`: explicit authorization endpoint for non-OIDC providers.
- `token_endpoint`: explicit token endpoint for non-OIDC providers.

## Setup Checklist

1. Register your app at the OAuth provider.
2. Set callback URL to:
   `https://<rise-url>/oidc/<project>/<extension>/callback`
3. Add provider values to extension config.
4. Test the flow from the extension detail page.

## Local Development Redirects

For local development, send users to the authorize endpoint with a `redirect_uri` query parameter pointing at your localhost app callback. The upstream provider still redirects to Rise, and Rise then forwards to your local callback URL.

Example:

`/oidc/<project>/<extension>/authorize?redirect_uri=http%3A%2F%2Flocalhost%3A3000%2Fcallback`

## See Also

- OAuth Extensions User Guide in the bundled Rise user docs.
