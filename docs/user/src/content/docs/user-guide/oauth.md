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
- Token refresh through the Rise token endpoint, with opaque refresh-token handles in place of the upstream refresh token.
- OIDC discovery and JWKS proxying when the upstream provider supports it.
- Encrypted storage for upstream provider client secrets.
- Local development support through `rise run --project`.

Rise is a proxy, not a session store for your application. After the code exchange, your application owns the upstream tokens and decides how to store them.

## Supported Flows

| Flow | Best for | Client authentication | Notes |
| --- | --- | --- | --- |
| Authorization code with PKCE | Browser apps and public clients | `client_id` plus `code_verifier` | No client secret in the browser. Uses RFC 7636 PKCE. |
| Authorization code with client secret | Backend apps | `client_id` plus `client_secret` | Token exchange happens server-side. Store resulting tokens in an HttpOnly session or backend store. |
| Refresh token | Backend apps or trusted clients | `client_id` plus `client_secret` | Exchanges a Rise refresh-token handle for fresh tokens by refreshing at the upstream provider. |

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
| `token_endpoint_auth_method` | Optional, `client_secret_post` by default | Optional, `client_secret_post` by default |
| `scopes` | Provider-specific | Provider-specific; `[]` for providers without scopes |

`token_endpoint_auth_method` sets how Rise authenticates to the upstream token endpoint: `client_secret_post` sends `client_id` and `client_secret` in the request body, `client_secret_basic` sends them as an HTTP Basic `Authorization` header (RFC 6749 §2.3.1). Query parameters in `authorization_endpoint` are kept on the authorization request.

Examples:

| Provider | OIDC compliant | `issuer_url` | `authorization_endpoint` | `token_endpoint` | Typical scopes |
| --- | --- | --- | --- | --- | --- |
| Google | Yes | `https://accounts.google.com` | Auto-discovered | Auto-discovered | `openid`, `email`, `profile` |
| Dex | Yes | Your Dex issuer URL | Auto-discovered | Auto-discovered | `openid`, `email`, `profile`, `groups` |
| Auth0 | Yes | `https://<tenant>.auth0.com` | Auto-discovered | Auto-discovered | `openid`, `email`, `profile` |
| GitHub | No | `https://github.com` | `https://github.com/login/oauth/authorize` | `https://github.com/login/oauth/access_token` | `read:user`, `user:email` |
| Snowflake | No | Your Snowflake account URL | Provider-specific authorization endpoint | Provider-specific token endpoint | Provider-specific |
| Notion | No | `https://api.notion.com` | `https://api.notion.com/v1/oauth/authorize?owner=user` | `https://api.notion.com/v1/oauth/token` | None (`[]`); needs `client_secret_basic` |

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

Notion requires HTTP Basic client authentication and has no scopes:

```json
{
  "provider_name": "Notion",
  "client_id": "<notion-oauth-client-id>",
  "client_secret_encrypted": "<encrypted>",
  "issuer_url": "https://api.notion.com",
  "authorization_endpoint": "https://api.notion.com/v1/oauth/authorize?owner=user",
  "token_endpoint": "https://api.notion.com/v1/oauth/token",
  "token_endpoint_auth_method": "client_secret_basic",
  "scopes": []
}
```

The **Configure** tab in the Rise UI offers these providers as templates.

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
| `refresh_token` | Yes | The refresh-token handle from a previous token response. |
| `client_id` | Yes | Rise client ID. |
| `client_secret` | Confidential clients | Rise client secret. Public clients refresh with `client_id` alone. |

Token responses use the RFC 6749 shape:

```json
{
  "access_token": "eyJhbGc...",
  "token_type": "Bearer",
  "expires_in": 3600,
  "refresh_token": "rrt1.AbCd...",
  "scope": "email profile",
  "id_token": "eyJhbGc..."
}
```

#### Refresh-token handles

The `refresh_token` Rise returns is an opaque handle (prefix `rrt1.`), not the upstream provider's refresh token. The upstream token is sealed inside the handle with Rise's encryption provider and bound to the project, to that incarnation of the extension and to the upstream it talks to, so a handle works only at that extension's Rise token endpoint and cannot be used directly against the upstream provider. Treat it as an opaque string and do not parse it.

- Both grants return the upstream's response with only the `refresh_token` replaced: the status, the `Content-Type`, every other member (including `scope`) and the member order are the upstream's own. A successful response whose body is not a JSON object is relayed as-is, unless it appears to carry a refresh token (for example a form-encoded `refresh_token=` pair), in which case Rise answers `502` rather than return it.
- A refresh response contains a new handle only if the upstream provider rotated its refresh token. Otherwise it has no `refresh_token`; keep using the handle you already have.
- An upstream error (any non-2xx other than `invalid_grant`) is passed through with its status and body, so retry or back off as you would against the provider. If the upstream answers a 4xx with a JSON `error` of `invalid_grant`, Rise answers `invalid_grant`: the refresh token is dead and the user must sign in again. If the upstream cannot be reached or times out, Rise answers `502`.
- A handle that does not unseal for this extension (tampered, or from another project or extension) is `invalid_grant`. So is a handle issued before the extension was deleted and created again under the same name, and one issued before the extension's `issuer_url`, `token_endpoint` or `client_id` changed: the handle names the upstream it was issued for, and Rise refuses it, without contacting any upstream, once the extension no longer points there. Users with such handles sign in again. Editing anything else (`scopes`, `authorization_endpoint`, `description`, the client authentication method) leaves handles valid. The comparison is exact, so even a trailing slash added to `issuer_url` counts as a change. If the encryption provider is temporarily unavailable (for example AWS KMS throttling), Rise answers `503` `server_error` and the request can be retried.
- Handles do not expire on their own and are not revoked by Rise; the upstream provider still decides whether the wrapped token is valid.

:::note[Refresh tokens issued before handles]
Refresh tokens issued before handles were introduced are raw upstream tokens. For now the token endpoint still accepts them and refreshes them upstream. The response to that first refresh carries a handle, wrapping the upstream's rotated token or, if it did not rotate, the token you presented, so clients move to handles on their first refresh. This compatibility path will be removed in a later release, after which only handles are accepted.

A raw token names no upstream, so Rise cannot tell whether the extension has been repointed since it was issued. Until the compatibility path is removed, a raw token is sent to whatever upstream the extension currently uses.
:::

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
- Applications never receive the upstream refresh token. They hold a sealed handle that only the Rise token endpoint can redeem, for the project and extension it was issued for.
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

**"Token exchange failed with status 401"** with `invalid_client`

- The provider may reject client credentials in the request body. Set `"token_endpoint_auth_method": "client_secret_basic"`.

**"No cached state found for state token"**

- The state token may have expired. Restart the OAuth flow.

**"Invalid or expired authorization code"**

- Authorization codes are single-use and expire after 5 minutes. Restart the OAuth flow.
