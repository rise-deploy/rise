---
title: "OAuth Extension"
---

Rise's OAuth extension makes Rise act as an OAuth/OIDC proxy between your application and an upstream provider such as Google, GitHub, Snowflake, or a custom SSO service.

## Why Use It

OAuth providers usually require every allowed redirect URI to be registered ahead of time. A Rise project can have many URLs: production, staging, branch previews, merge request previews, custom domains, and localhost during development.

The OAuth extension gives the provider one stable callback URL:

```text
https://<rise-url>/oidc/<project>/<extension>/callback
```

Rise receives the upstream callback and forwards the user back to the original app URL that started the flow.

```text
App URL -> Rise authorize -> OAuth provider
                                  |
App URL <- Rise callback  <-------+
```

## What Rise Provides

- A stable provider callback URL per project and extension.
- OAuth 2.0 authorization-code flow for backend applications.
- PKCE support for browser applications and public clients.
- Token refresh through the Rise token endpoint.
- OIDC discovery and JWKS proxying when the upstream provider supports it.
- Encrypted storage for upstream provider client secrets.
- Local development support through `rise run --project`.

Rise is a proxy, not a session store for your application. After the code exchange, your application owns the upstream tokens and decides how to store them.

## Supported Flows

| Flow | Best for | Client authentication | Notes |
| --- | --- | --- | --- |
| Authorization code with PKCE | Browser apps and public clients | `client_id` plus `code_verifier` | No client secret in the browser. Uses RFC 7636 PKCE. |
| Authorization code with client secret | Backend apps | `client_id` plus `client_secret` | Token exchange happens server-side. Store resulting tokens in an HttpOnly session or backend store. |
| Refresh token | Backend apps or trusted clients | `client_id` plus `client_secret` | Proxies `grant_type=refresh_token` to the upstream provider. |

The Rise client ID is deterministic:

```text
{project-name}-{extension-name}
```

For project `my-app` and extension `oauth-google`, the Rise client ID is `my-app-oauth-google`.

## Create an OAuth Extension

First register an OAuth application with the upstream provider. Use the Rise callback URL as the provider redirect URI:

```text
https://<rise-url>/oidc/<project>/<extension>/callback
```

Then encrypt the upstream provider client secret and create the extension:

```bash
ENCRYPTED=$(rise encrypt "your_client_secret_here")

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

You can also encrypt via stdin:

```bash
echo "your_client_secret_here" | rise encrypt
```

The `rise encrypt` command is rate-limited to 100 requests per hour per user.

## Provider Configuration

OIDC-compliant providers expose an OpenID configuration document, so Rise can discover the authorization, token, and JWKS endpoints from `issuer_url`.

| Field | OIDC-compliant provider | Non-OIDC provider |
| --- | --- | --- |
| `provider_name` | Required | Required |
| `client_id` | Required | Required |
| `client_secret_encrypted` | Required | Required |
| `issuer_url` | Required | Required |
| `authorization_endpoint` | Auto-discovered | Required |
| `token_endpoint` | Auto-discovered | Required |
| `scopes` | Provider-specific | Provider-specific |

Examples:

| Provider | OIDC compliant | `issuer_url` | `authorization_endpoint` | `token_endpoint` | Typical scopes |
| --- | --- | --- | --- | --- | --- |
| Google | Yes | `https://accounts.google.com` | Auto-discovered | Auto-discovered | `openid`, `email`, `profile` |
| Dex | Yes | Your Dex issuer URL | Auto-discovered | Auto-discovered | `openid`, `email`, `profile`, `groups` |
| Auth0 | Yes | `https://<tenant>.auth0.com` | Auto-discovered | Auto-discovered | `openid`, `email`, `profile` |
| GitHub | No | `https://github.com` | `https://github.com/login/oauth/authorize` | `https://github.com/login/oauth/access_token` | `read:user`, `user:email` |
| Snowflake | No | Your Snowflake account URL | Provider-specific authorization endpoint | Provider-specific token endpoint | Provider-specific |

For non-OIDC providers, add the manual endpoints to the same extension spec:

```json
{
  "provider_name": "GitHub",
  "client_id": "Iv1.abc123...",
  "client_secret_encrypted": "<encrypted>",
  "issuer_url": "https://github.com",
  "authorization_endpoint": "https://github.com/login/oauth/authorize",
  "token_endpoint": "https://github.com/login/oauth/access_token",
  "scopes": ["read:user", "user:email"]
}
```

## Application Usage

Your application starts login by redirecting the user to Rise:

```text
GET {RISE_ISSUER}/oidc/{project}/{extension}/authorize
```

For browser applications using PKCE, include:

```text
code_challenge=<challenge>&code_challenge_method=S256
```

For local development or custom callback paths, include:

```text
redirect_uri=http://localhost:3000/callback
```

After the upstream provider authenticates the user, Rise redirects back to your app with an authorization code:

```text
https://my-app.example.com/callback?code=<authorization-code>&state=<state>
```

Your app exchanges that code at:

```text
POST {RISE_ISSUER}/oidc/{project}/{extension}/token
```

Use `client_secret` for backend apps or `code_verifier` for PKCE clients.

## Local Development

`rise run --project my-app` injects the same OAuth extension environment variables your deployed app receives:

| Variable | Purpose |
| --- | --- |
| `{EXTENSION}_CLIENT_ID` | Rise client ID, for example `OAUTH_GOOGLE_CLIENT_ID`. |
| `{EXTENSION}_CLIENT_SECRET` | Rise client secret for confidential clients. |
| `{EXTENSION}_ISSUER` | Rise OIDC proxy URL for this extension. |
| `RISE_ISSUER` | Rise server URL used to build authorize and token URLs. |

Rise allows `redirect_uri` values that point to localhost, so the same provider registration can support production, preview, and local development.

## API Reference

### Authorization Endpoint

```text
GET /oidc/{project}/{extension}/authorize
```

Query parameters:

| Parameter | Required | Description |
| --- | --- | --- |
| `code_challenge` | PKCE clients only | Base64url-encoded SHA-256 hash of the code verifier. |
| `code_challenge_method` | No | Only `S256` is supported. |
| `redirect_uri` | No | Where Rise should redirect after OAuth. Allows localhost and project domains. |
| `state` | No | Application state passed through the OAuth flow. |

### Callback Endpoint

```text
GET /oidc/{project}/{extension}/callback?code=...&state=...
```

This is the callback URL registered with the upstream provider. Rise handles it and redirects to the app with a Rise authorization code.

### Token Endpoint

```text
POST /oidc/{project}/{extension}/token
Content-Type: application/x-www-form-urlencoded
```

Authorization-code parameters:

| Parameter | Required | Description |
| --- | --- | --- |
| `grant_type` | Yes | `authorization_code` |
| `code` | Yes | Authorization code from the callback. |
| `client_id` | Yes | Rise client ID. |
| `client_secret` | Confidential clients | Rise client secret. |
| `code_verifier` | PKCE clients | Original PKCE verifier. |

Refresh-token parameters:

| Parameter | Required | Description |
| --- | --- | --- |
| `grant_type` | Yes | `refresh_token` |
| `refresh_token` | Yes | Refresh token from a previous token response. |
| `client_id` | Yes | Rise client ID. |
| `client_secret` | Yes | Rise client secret. |

Token responses use the RFC 6749 shape:

```json
{
  "access_token": "eyJhbGc...",
  "token_type": "Bearer",
  "expires_in": 3600,
  "refresh_token": "eyJhbGc...",
  "scope": "email profile",
  "id_token": "eyJhbGc..."
}
```

### Discovery and JWKS

Rise exposes an OIDC discovery endpoint for each OAuth extension:

```text
GET /oidc/{project}/{extension}/.well-known/openid-configuration
```

The response rewrites upstream URLs to Rise proxy URLs:

```json
{
  "issuer": "https://rise.example.com/oidc/my-app/oauth-google",
  "authorization_endpoint": "https://rise.example.com/oidc/my-app/oauth-google/authorize",
  "token_endpoint": "https://rise.example.com/oidc/my-app/oauth-google/token",
  "jwks_uri": "https://rise.example.com/oidc/my-app/oauth-google/jwks"
}
```

JWKS is proxied from the upstream provider:

```text
GET /oidc/{project}/{extension}/jwks
```

## Security

- Upstream client secrets are encrypted at rest and never exposed to browser clients.
- Authorization codes are single-use and expire after 5 minutes.
- OAuth state tokens protect against CSRF and expire after 10 minutes.
- PKCE proves that a public client initiated the authorization flow.
- Secret validation uses constant-time comparison.
- Applications own token storage and refresh behavior after exchange.

## Troubleshooting

**"Failed to resolve OAuth endpoints"** or **"No authorization_endpoint in spec or OIDC discovery"**

- For OIDC-compliant providers, check that `issuer_url` is correct and supports OIDC discovery.
- For non-OIDC providers, set `authorization_endpoint` and `token_endpoint`.
- Test discovery with `curl {issuer_url}/.well-known/openid-configuration`.

**"Invalid issuer_url URL"**

- Use a valid HTTPS URL.
- Avoid trailing slashes and paths unless the provider documents them as part of the issuer.

**"Token exchange failed with status 400"**

- Verify `client_id` and `client_secret_encrypted`.
- Check that the provider redirect URI matches the Rise callback URL.
- Review provider logs for the upstream OAuth error.

**"No cached state found for state token"**

- The state token may have expired. Restart the OAuth flow.

**"Invalid or expired authorization code"**

- Authorization codes are single-use and expire after 5 minutes. Restart the OAuth flow.
