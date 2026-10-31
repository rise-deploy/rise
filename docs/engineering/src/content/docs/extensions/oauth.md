---
title: "OAuth Provider Extension"
---

The `oauth` extension makes Rise act as an OAuth/OIDC proxy between your application and an upstream provider (Google, GitHub, Snowflake, custom SSO).

## The Problem It Solves

OAuth providers require pre-registering every allowed redirect URI. A Rise project can have many URLs that aren't all known in advance:

- **Production**: `my-app.apps.rise.example.com`
- **Staging environment**: `staging--my-app.preview.rise.example.com`
- **Preview deployments**: `my-app--mr--26.preview.rise.example.com` (created dynamically per branch/MR)
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

Rise allows forwarding to the origin of any active deployment of the project (all deployment group and environment URLs, and custom domains) and to loopback addresses (`localhost`, `*.localhost`, `127.0.0.1`, `[::1]`) for local development. `redirect_uri` is required on every authorize request. When Rise's own `server.public_url` is `http`, `http` deployment origins are accepted too; behind an `https` Rise they are not.

## What It Does

- Stores provider credentials securely (client secret encrypted at rest, never in client environments).
- Exposes Rise OAuth endpoints (`authorize`, `token`, `callback`, OIDC discovery, JWKS), served in-process by the embedded oauthrelay core on every deployment backend. URLs stay `/oidc/{project}/{extension}/…`.
- Generates client credentials injected into app environments: a client ID, and a client secret for `confidential` extensions only.
- Returns Rise refresh-token handles (`rrt1.` sealed, `rrt2.` rotating) instead of the provider's refresh token.
- Proxies OIDC discovery and JWKS so apps work identically in local dev and production. The discovery document's `issuer` is the upstream issuer.

## Configuration

```json
{
  "provider_name": "Google",
  "description": "Sign in with Google",
  "client_id": "your-client-id",
  "client_secret_encrypted": "rise:enc:v2:...",
  "issuer_url": "https://accounts.google.com",
  "scopes": ["openid", "email", "profile"]
}
```

Seal credentials with `rise encrypt --project PROJECT --extension-type oauth
--extension NAME --slot /spec/client_secret_encrypted --oauth-spec oauth.json`.
The OAuth configuration in `oauth.json` must match the destination and client
settings used by the extension. Changing those settings requires resealing the
secret. See the [OAuth user guide](/docs/user-guide/oauth/) for the complete
stdin workflow.

## Optional Overrides

- `authorization_endpoint`: explicit authorization endpoint for non-OIDC providers.
- `token_endpoint`: explicit token endpoint for non-OIDC providers.
- `client_type`: `confidential` (default) or `public`. Public clients require PKCE, get no secret (`{EXT}_CLIENT_SECRET` is not injected), and always use rotating refresh tokens.
- `refresh_token_mode`: `sealed` (default for confidential) or `rotating`. `public` with `sealed` is rejected.

## Operating Notes

- **Secrets and keys.** Authorization codes, state and sealed handles are encrypted with a data key that the configured encryption provider (local AES or KMS) wraps; the wrapped key travels in each value. Rotating handles and single-use markers live in the database and are cleaned up when they expire, so storage grows with active sessions.
- **Invalidation.** Deleting and recreating an extension, or changing its `issuer_url`, `client_id` or `token_endpoint`, invalidates its refresh tokens. A soft-deleted extension stops serving immediately.
- **Upgrading.** Refresh tokens and in-flight codes issued before the upgrade are rejected once, so users sign in again. See the changelog entry for app-side changes.
- **Status.** Only the "Test OAuth" button (`GET /api/v1/projects/{project}/extensions/{extension}/oauth/test`, project members only) updates the extension's status. End-user logins do not.
- **Snowflake provisioner.** Snowflake's 90-day refresh-token validity is cut to 30 days if the linked extension uses `rotating` or `public`.

## Setup Checklist

1. Register your app at the OAuth provider.
2. Set callback URL to:
   `https://<rise-url>/oidc/<project>/<extension>/callback`
3. Add provider values to extension config.
4. Test the flow from the extension detail page.

## Local Development Redirects

For local development, send users to the authorize endpoint with a `redirect_uri` query parameter pointing at your localhost app callback, and repeat it at the token request. The upstream provider still redirects to Rise, and Rise then forwards to your local callback URL.

Example:

`/oidc/<project>/<extension>/authorize?redirect_uri=http%3A%2F%2Flocalhost%3A3000%2Fcallback`

## See Also

- OAuth Extensions User Guide in the bundled Rise user docs.
