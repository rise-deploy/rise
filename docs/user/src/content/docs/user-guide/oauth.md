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

Rise receives the upstream callback and forwards the user back to the `redirect_uri` that started the flow.

```text
App URL -> Rise authorize -> OAuth provider
                                  |
App URL <- Rise callback  <-------+
```

## What Rise Provides

- A stable provider callback URL per project and extension.
- OAuth 2.0 authorization-code flow for backend applications (confidential clients) and browser applications (public clients with PKCE).
- Refresh through the Rise token endpoint. Your app receives Rise refresh-token handles and never the provider's refresh token.
- OIDC discovery and JWKS proxying when the upstream provider supports it.
- Encrypted storage for upstream provider client secrets.
- Local development support through `rise run --project`.

Rise is a proxy, not a session store for your application. After the code exchange, your application owns the access and ID tokens and decides how to store them.

## Client Types

`client_type` says how your application authenticates to the Rise token endpoint. It is independent of the upstream provider's client.

| | `confidential` (default) | `public` |
| --- | --- | --- |
| For | Apps with a backend | Browser and native apps that cannot keep a secret |
| Client secret | Rise issues one and injects `{EXTENSION}_CLIENT_SECRET`. It is required on **every** token request, code and refresh. | None is issued or injected. Sending one is rejected with `invalid_client`. |
| PKCE | Optional, and enforced when used. A `code_verifier` for a code issued without a `code_challenge` is rejected. | Required (`S256`). |
| Refresh tokens | `sealed` (default) or `rotating` | Always `rotating` |

The Rise client ID is deterministic for both types:

```text
{project-name}-{extension-name}
```

For project `my-app` and extension `oauth-google`, the Rise client ID is `my-app-oauth-google`.

### Refresh Tokens

The `refresh_token` in a token response is a Rise handle. Raw provider refresh tokens are never returned to your app, cannot be presented to the Rise token endpoint, and cannot be revoked or introspected through Rise.

`refresh_token_mode` selects the handle type:

| Mode | Handle | Behavior |
| --- | --- | --- |
| `sealed` (confidential default) | `rrt1.…` | Stateless. It contains the provider's token, encrypted by Rise. It keeps working until the provider's token expires. If the provider rotates its refresh token, the response carries a new handle. |
| `rotating` (always for `public`) | `rrt2.…` | Single use. Every refresh returns a successor handle, so always store the latest one. Presenting a used handle within 30 seconds returns the same successor (for tabs and retries). Presenting it later revokes the whole session, so the user signs in again. A session also ends 30 days after the original sign-in, even if the provider's refresh token lives longer. |

Rotating handles are stored in Rise's database until they expire, so storage grows with the number of signed-in users. Setting `public` together with `refresh_token_mode: sealed` is rejected.

Changing `client_type` or `refresh_token_mode` does not require resealing the upstream secret. Switching `sealed` to `rotating` keeps existing users signed in. Switching `rotating` back to `sealed` signs them out. An app switched to `public` must stop sending its client secret, which is removed from its environment on the next deployment.

Deleting and recreating an extension, or changing its `issuer_url`, `client_id`, or `token_endpoint`, invalidates every refresh token issued before, and users sign in again. Other edits, including scopes and the upstream secret, keep them valid.

## Supported Flows

| Flow | Best for | Client authentication | Notes |
| --- | --- | --- | --- |
| Authorization code with PKCE | Browser apps and public clients | `client_id`, `redirect_uri`, `code_verifier` | `client_type: public`. No client secret in the browser. Uses RFC 7636 PKCE. |
| Authorization code with client secret | Backend apps | `client_id`, `client_secret`, `redirect_uri` | `client_type: confidential`. Token exchange happens server-side. Store resulting tokens in an HttpOnly session or backend store. |
| Refresh token | Both | Same as the code exchange, without `code_verifier` | Presents a Rise handle. Confidential clients still send the secret. |

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
| `client_type` | Optional, `confidential` by default | Optional, `confidential` by default |
| `refresh_token_mode` | Optional, `sealed` by default | Optional, `sealed` by default |

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

Rise sends the provider's client credentials only to the token endpoint the secret was sealed for. Authorization requests may carry the provider's extra parameters, such as `prompt`, `login_hint`, and `nonce`, which Rise forwards. The spec's `scopes` are the maximum: an app may request a subset with `scope`, but never more.

## Test the Extension

Open the extension in the Rise UI and choose **Test OAuth Flow**. It signs you in through the provider as yourself, then marks the extension configured or shows the provider's error. Only a signed-in project member can run the test, and only the test updates the extension's status; end-user logins do not.

## Application Usage

Your application starts login by redirecting the user to Rise. `redirect_uri` is required: it is where Rise sends the user back, and it must be repeated unchanged at the token request.

```text
GET {EXTENSION}_ISSUER/authorize?redirect_uri=https://my-app.example.com/callback&state=<state>
```

Public clients also send a PKCE challenge:

```text
code_challenge=<challenge>&code_challenge_method=S256
```

Rise accepts these `redirect_uri` values:

- The origin of any active deployment of the project: default URLs, custom domains, and all deployment groups, with any path.
- `http` on `127.0.0.1`, `[::1]`, `localhost`, and `*.localhost`, with any port and path.
- `http` deployment origins, only when Rise itself is served over `http` (local development setups).

Anything else, including Rise's own URL, is refused.

After the upstream provider authenticates the user, Rise redirects back to your app with an authorization code:

```text
https://my-app.example.com/callback?code=<authorization-code>&state=<state>
```

Authorization codes are single-use, expire after 5 minutes, and are several kilobytes long, so make sure no proxy in front of your app limits URL length below that. Your app exchanges the code at:

```text
POST {EXTENSION}_ISSUER/token
```

A backend app sends `client_id`, `client_secret`, and `redirect_uri`. A browser app sends `client_id`, `redirect_uri`, and `code_verifier`. For example, a backend:

```bash
curl -X POST "$OAUTH_GOOGLE_ISSUER/token" \
  -d grant_type=authorization_code \
  -d code="$CODE" \
  -d redirect_uri=https://my-app.example.com/callback \
  -d client_id="$OAUTH_GOOGLE_CLIENT_ID" \
  -d client_secret="$OAUTH_GOOGLE_CLIENT_SECRET"
```

Refresh with `grant_type=refresh_token` and the latest `refresh_token`. Confidential apps send the secret again.

## Local Development

`rise run --project my-app` injects the same OAuth extension environment variables your deployed app receives:

| Variable | Purpose |
| --- | --- |
| `{EXTENSION}_CLIENT_ID` | Rise client ID, for example `OAUTH_GOOGLE_CLIENT_ID`. |
| `{EXTENSION}_CLIENT_SECRET` | Rise client secret. Set for confidential clients only. |
| `{EXTENSION}_ISSUER` | Rise OIDC proxy URL for this extension. |
| `RISE_ISSUER` | Rise server URL used to build authorize and token URLs. |

Use `http://localhost:<port>/<path>` as `redirect_uri` during development. The same provider registration then supports production, preview, and local development.

## API Reference

### Authorization Endpoint

```text
GET /oidc/{project}/{extension}/authorize
```

Query parameters:

| Parameter | Required | Description |
| --- | --- | --- |
| `redirect_uri` | Yes | Where Rise redirects after OAuth. See [Application Usage](#application-usage) for the allowed targets. |
| `code_challenge` | Public clients | Base64url-encoded SHA-256 hash of the code verifier. Optional for confidential clients. |
| `code_challenge_method` | With `code_challenge` | Only `S256` is supported. |
| `state` | Recommended | Application state, returned unchanged. |
| `scope` | No | A subset of the extension's `scopes`. Defaults to all of them. |

Other parameters, such as `prompt`, `login_hint`, and `nonce`, are forwarded to the provider. `request`, `request_uri`, and a `response_mode` other than `query` are rejected. A refused request answers with a plain `400` and no description.

### Callback Endpoint

```text
GET /oidc/{project}/{extension}/callback?code=...&state=...
```

This is the callback URL registered with the upstream provider. Rise handles it and redirects to the app with a Rise authorization code. Provider errors are returned to the validated `redirect_uri`.

### Token Endpoint

```text
POST /oidc/{project}/{extension}/token
Content-Type: application/x-www-form-urlencoded
```

Confidential clients authenticate with `client_id` and `client_secret` in the body, or with an HTTP Basic `Authorization` header, never both. Wrong credentials answer `401 invalid_client`. Public clients identify themselves with `client_id` only.

Authorization-code parameters:

| Parameter | Required | Description |
| --- | --- | --- |
| `grant_type` | Yes | `authorization_code` |
| `code` | Yes | Authorization code from the callback. |
| `redirect_uri` | Yes | The value sent to the authorize endpoint. |
| `client_id` | Yes | Rise client ID. |
| `client_secret` | Confidential clients | Rise client secret. |
| `code_verifier` | Public clients, and whenever a `code_challenge` was sent | Original PKCE verifier. |

Refresh-token parameters:

| Parameter | Required | Description |
| --- | --- | --- |
| `grant_type` | Yes | `refresh_token` |
| `refresh_token` | Yes | The latest Rise handle (`rrt1.` or `rrt2.`). Provider tokens are rejected with `invalid_grant`. |
| `client_id` | Yes | Rise client ID. |
| `client_secret` | Confidential clients | Rise client secret. |

Token responses are the provider's response with only `refresh_token` replaced by a Rise handle, so provider-specific fields are preserved:

```json
{
  "access_token": "eyJhbGc...",
  "token_type": "Bearer",
  "expires_in": 3600,
  "refresh_token": "rrt1.v1....",
  "scope": "email profile",
  "id_token": "eyJhbGc..."
}
```

Browser apps can call the token endpoint cross-origin from any allowed `redirect_uri` origin.

### Discovery and JWKS

Rise exposes a discovery document for each OAuth extension:

```text
GET /oidc/{project}/{extension}/.well-known/openid-configuration
```

The document starts from the provider's own metadata. Rise replaces the authorization and token endpoints with its own and describes what the relay accepts (`code` flow, `query` response mode, `S256`, and the client authentication methods of the extension's client type). Provider endpoints that the relay cannot honor, such as `revocation_endpoint` and `introspection_endpoint`, are removed.

```json
{
  "issuer": "https://accounts.google.com",
  "authorization_endpoint": "https://rise.example.com/oidc/my-app/oauth-google/authorize",
  "token_endpoint": "https://rise.example.com/oidc/my-app/oauth-google/token",
  "jwks_uri": "https://www.googleapis.com/oauth2/v3/certs"
}
```

- `issuer` is the **upstream** issuer, because that is who signs ID tokens. It is not the `{EXTENSION}_ISSUER` URL. A client that requires the discovery `issuer` to equal the URL it fetched the document from fails that check; configure such clients with the authorization and token endpoints manually and the upstream issuer for ID-token validation.
- Providers without OIDC discovery (GitHub, Notion, Snowflake) get a document with only the Rise endpoints and no `jwks_uri`.

JWKS is proxied from the upstream provider:

```text
GET /oidc/{project}/{extension}/jwks
```

## Security

- Upstream client secrets are encrypted at rest and never exposed to browser clients.
- Authorization codes are encrypted, single-use, and expire after 5 minutes.
- State tokens protect against CSRF and expire after 10 minutes.
- Refresh tokens are Rise handles. The provider's refresh token never reaches your app, and `rotating` handles detect theft through reuse.
- Redirects go only to the project's deployment origins and loopback addresses. Rise's own origin is not a target.
- PKCE is enforced for public clients. A confidential client that sends a `code_verifier` for a code issued without a challenge is rejected, so a downgrade cannot go unnoticed.
- Secret validation uses constant-time comparison.
- Applications own access-token storage after exchange.

## Troubleshooting

**"Failed to resolve OAuth endpoints"** or **"No authorization_endpoint in spec or OIDC discovery"**

- For OIDC-compliant providers, check that `issuer_url` is correct and supports OIDC discovery.
- For non-OIDC providers, set `authorization_endpoint` and `token_endpoint`.
- Test discovery with `curl {issuer_url}/.well-known/openid-configuration`.

**"Invalid issuer_url URL"**

- Use a valid HTTPS URL.
- Avoid trailing slashes and paths unless the provider documents them as part of the issuer.

**Authorize answers `400`**

- Send `redirect_uri` and make sure it is a deployment origin of this project or a loopback URL.
- Public clients need `code_challenge` with `code_challenge_method=S256`.
- `scope` may only narrow the extension's `scopes`.

**Token request answers `invalid_grant`**

- The code expired (5 minutes) or was already used. Restart the flow.
- `redirect_uri` differs from the authorize request, or `code_verifier` does not match the challenge.
- The refresh token is not a current Rise handle: a raw provider token, an already-used `rotating` handle, a session older than 30 days, or a token issued before the extension was recreated or its `issuer_url`, `client_id`, or `token_endpoint` changed. The user must sign in again.

**Token request answers `401 invalid_client`**

- Confidential clients must send the client secret on code and refresh requests. Public clients must not send one.

**"Token exchange failed with status 400" in the extension status**

- Verify `client_id` and `client_secret_encrypted`.
- Check that the provider redirect URI matches the Rise callback URL.
- Review provider logs for the upstream OAuth error.

**Upstream `401 invalid_client` in the extension status**

- The provider may reject client credentials in the request body. Set `"token_endpoint_auth_method": "client_secret_basic"`.
