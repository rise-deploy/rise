---
title: "OAuth Provider Extension"
---

The `oauth` extension makes Rise act as an OAuth/OIDC proxy between your application and an upstream provider (Google, GitHub, Snowflake, Notion, custom SSO).

## The Problem It Solves

OAuth providers require pre-registering every allowed redirect URI. A Rise project can have many URLs that aren't all known in advance:

- **Production**: `my-app.apps.rise.example.com`
- **Staging environment**: `staging--my-app.preview.rise.example.com`
- **Preview deployments**: `my-app--mr--26.preview.rise.example.com` (created dynamically per branch/MR)
- **Local development**: `http://localhost:3000`

Registering all of these at the OAuth provider is impractical, especially for preview URLs that are generated on demand. The OAuth extension solves this by making Rise the single registered redirect URI. Your application redirects users to Rise's authorize endpoint; Rise redirects to the upstream provider using Rise's own callback URL; after authentication, Rise forwards back to whichever app URL initiated the request.

You register **one URL** at the provider:

```
https://<rise-url>/oidc/<project>/<extension>/callback
```

Rise allows forwarding to the origin of any active deployment of the project (default URLs, custom domains, all deployment groups) and to `localhost` for local development. Every authorize request must carry a `redirect_uri`.

## What It Does

- Stores provider credentials securely (client secret encrypted at rest, never in client environments).
- Exposes Rise OAuth endpoints (`authorize`, `token`, `callback`, OIDC discovery, JWKS).
- Generates client credentials injected into app environments: a client ID, and a client secret for confidential clients.
- Returns Rise refresh-token handles instead of the provider's refresh token.
- Proxies OIDC discovery and JWKS so apps work identically in local dev and production.

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
- `token_endpoint_auth_method`: `client_secret_post` (default) sends the client credentials in the token request body; `client_secret_basic` sends them as an HTTP Basic `Authorization` header, as Notion requires.
- `scopes`: may be `[]` for providers without scopes; the authorization request then carries no `scope` parameter. Apps may request a subset, never more.
- `client_type`: `confidential` (default) apps send the Rise client secret on every token request; `public` apps (SPAs) use PKCE, receive no secret, and always get rotating refresh tokens.
- `refresh_token_mode`: `sealed` (default for confidential) returns stateless `rrt1.` handles; `rotating` returns single-use `rrt2.` handles with reuse detection. Public clients cannot use `sealed`.

## Setup Checklist

1. Register your app at the OAuth provider.
2. Set callback URL to:
   `https://<rise-url>/oidc/<project>/<extension>/callback`
3. Add provider values to extension config.
4. Test the flow with **Test OAuth Flow** on the extension detail page. Only a signed-in project member (or a Rise admin) can run it, and only the test updates the extension's status.

## Local Development Redirects

For local development, send users to the authorize endpoint with a `redirect_uri` pointing at your localhost app callback (`http` on `localhost`, `127.0.0.1` or `[::1]`, any port and path). The upstream provider still redirects to Rise, and Rise then forwards to your local callback URL. Send the same `redirect_uri` again with the token request.

Example:

`/oidc/<project>/<extension>/authorize?redirect_uri=http%3A%2F%2Flocalhost%3A3000%2Fcallback`

## See Also

- [OAuth Extensions User Guide](../../user-guide/oauth)
