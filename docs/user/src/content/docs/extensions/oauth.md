---
title: "OAuth Provider Extension"
---

# OAuth Provider Extension

The `oauth` extension configures Rise as an OAuth/OIDC proxy for your application.

## What It Does

- Stores provider credentials securely.
- Exposes Rise OAuth endpoints for auth flows.
- Injects generated client credentials into app environments.
- Supports local development redirects (for example `http://localhost:*`) via `redirect_uri` override, even when the upstream provider only allows the Rise callback URL.

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

- [OAuth Extensions User Guide](../user-guide/oauth.md)
