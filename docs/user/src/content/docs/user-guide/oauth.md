---
title: "OAuth Extension"
---

# OAuth Extension

Rise's Generic OAuth 2.0 extension enables end-user authentication with any OAuth provider (Snowflake, Google, GitHub, custom SSO) without managing client secrets locally.

## Overview

**Key Features:**

- **Generic Provider Support**: Works with any OAuth 2.0 compliant provider
- **Multiple Flow Support**:
  - PKCE (SPAs, RFC 7636-compliant)
  - Token endpoint with client credentials (backend apps, RFC 6749-compliant)
- **Stateless OAuth Proxy**: Rise proxies OAuth flows, clients own their tokens after exchange
- **OIDC Discovery Proxy**: Rise proxies OIDC discovery and JWKS endpoints, making apps work in local dev
- **No Client Secret Exposure**: Secrets stored as encrypted environment variables on Rise
- **Standards Compliant**: RFC 6749 (OAuth 2.0) and RFC 7636 (PKCE) support

**Security Model:**

- Client secrets never leave Rise backend (both upstream OAuth and Rise client credentials)
- OAuth state tokens prevent CSRF attacks
- Authorization codes single-use with 5-minute TTL
- PKCE support for public clients (SPAs) prevents code interception attacks
- Constant-time comparison for all secret validation
- Clients manage token refresh via `/oidc/{project}/{extension}/token` with `grant_type=refresh_token`

## OAuth Flows

Rise supports multiple OAuth flows to accommodate different application architectures:

### PKCE Flow (For SPAs)

Best for single-page applications (React, Vue, Angular) using RFC 7636 Proof Key for Code Exchange (PKCE).

**Security:** PKCE prevents authorization code interception attacks by requiring the client to prove it initiated the OAuth flow. No client secret needed (SPAs can't securely store secrets).

**Configuration:**

Rise client IDs are deterministic and follow the pattern `{project-name}-{extension-name}`. You can construct the client ID directly or fetch it from the extension:

```bash
# Option 1: Construct directly (deterministic format)
# For project "my-app" and extension "oauth-google": my-app-oauth-google

# Option 2: Fetch from extension spec (requires Rise auth token)
rise extension show oauth-google -p my-app --output json | jq -r '.spec.rise_client_id'
# Output: "my-app-oauth-google"
```

Add to your build-time configuration:

```javascript
// config.js (or environment variables)
const CONFIG = {
  apiUrl: 'https://rise.example.com',
  projectName: 'my-app',
  extensionName: 'oauth-google',
  // Client ID is deterministic: {projectName}-{extensionName}
  get riseClientId() {
    return `${this.projectName}-${this.extensionName}`;
  }
};
```

**Usage Example:**

```bash
# Install OAuth library for PKCE helpers
npm install oauth4webapi
```

```javascript
import * as oauth from 'oauth4webapi';

// 1. Initiate OAuth login with PKCE
async function login() {
  // Generate PKCE code verifier and challenge
  const codeVerifier = oauth.generateRandomCodeVerifier();
  const codeChallenge = await oauth.calculatePKCECodeChallenge(codeVerifier);
  sessionStorage.setItem('pkce_verifier', codeVerifier);

  // Build authorization URL
  const authUrl = new URL(
    `https://rise.example.com/oidc/${CONFIG.projectName}/${CONFIG.extensionName}/authorize`
  );
  authUrl.searchParams.set('code_challenge', codeChallenge);
  authUrl.searchParams.set('code_challenge_method', 'S256');

  window.location.href = authUrl;
}

// 2. After callback, exchange code for tokens
async function handleCallback() {
  const urlParams = new URLSearchParams(window.location.search);
  const code = urlParams.get('code');
  const codeVerifier = sessionStorage.getItem('pkce_verifier');

  if (!code || !codeVerifier) {
    throw new Error('Missing code or verifier');
  }

  // Exchange code for tokens
  const tokenUrl = `https://rise.example.com/oidc/${CONFIG.projectName}/${CONFIG.extensionName}/token`;
  const response = await fetch(tokenUrl, {
    method: 'POST',
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body: new URLSearchParams({
      grant_type: 'authorization_code',
      code: code,
      client_id: CONFIG.riseClientId,
      code_verifier: codeVerifier
    })
  });

  if (!response.ok) {
    const error = await response.json();
    throw new Error(`OAuth error: ${error.error}`);
  }

  const tokens = await response.json();
  // { access_token, token_type, expires_in, refresh_token, scope, id_token }

  // Store tokens securely
  localStorage.setItem('oauth_tokens', JSON.stringify(tokens));
  sessionStorage.removeItem('pkce_verifier');

  return tokens;
}
```

### Token Endpoint Flow (For Backend Apps)

Best for server-rendered applications (Express, Django, Rails) where tokens should be handled server-side. The authorization code (5-min TTL, single-use) is passed in a query param; the backend exchanges it for tokens via Rise's token endpoint using `client_id` + `client_secret`.

```typescript
// Express example
app.get('/oauth/callback', async (req, res) => {
  const { code } = req.query;

  const tokens = await fetch(
    `${process.env.RISE_ISSUER}/oidc/my-app/oauth-google/token`,
    {
      method: 'POST',
      headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
      body: new URLSearchParams({
        grant_type: 'authorization_code',
        code: code as string,
        client_id: process.env.OAUTH_GOOGLE_CLIENT_ID!,
        client_secret: process.env.OAUTH_GOOGLE_CLIENT_SECRET!
      })
    }
  ).then(r => r.json());

  req.session.tokens = tokens;  // Store in HttpOnly session
  res.redirect('/');
});
```

## Configuration

### Prerequisites

**1. Register OAuth Application with Provider**

Obtain client credentials from your OAuth provider:
- **Client ID**: Public identifier
- **Client Secret**: Secret key (never expose in frontend)
- **Redirect URI**: Set to `https://api.{your-domain}/oidc/{project}/{extension}/callback`

**2. Determine Provider Type**

Rise supports two types of OAuth providers:

| Provider Type | issuer_url | Endpoints |
|--------------|-----------|-----------|
| **OIDC-compliant** (Google, Dex, Auth0) | Required | Auto-discovered via `{issuer_url}/.well-known/openid-configuration` |
| **Non-OIDC** (GitHub, Snowflake) | Required | Must set `authorization_endpoint` and `token_endpoint` manually |

**3. Store Client Secret in Rise**

Encrypt the secret and store it directly in the extension spec:

```bash
# Encrypt the secret
ENCRYPTED=$(rise encrypt "your_client_secret_here")

# Use in extension spec (assuming rise.toml has project = "my-app")
rise extension create oauth-provider \
  --type oauth \
  --spec '{
    "provider_name": "My OAuth Provider",
    "client_id": "your_client_id",
    "client_secret_encrypted": "'"$ENCRYPTED"'",
    "issuer_url": "https://accounts.google.com",
    "scopes": ["openid", "email", "profile"]
  }'

# Or specify project explicitly with -p flag:
# rise extension create oauth-provider -p my-app --type oauth --spec '{...}'
```

Or encrypt via stdin:

```bash
echo "your_client_secret_here" | rise encrypt
```

The `rise encrypt` command is rate-limited to 100 requests per hour per user.

### Creating OAuth Extension

**OIDC-Compliant Provider (Google, Dex, Auth0):**

For OIDC-compliant providers, only `issuer_url` is needed - endpoints are auto-discovered:

```bash
# Encrypt the client secret
ENCRYPTED=$(rise encrypt "your_client_secret_here")

# Create extension - endpoints auto-discovered via OIDC
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

**Non-OIDC Provider (GitHub, Snowflake):**

For non-OIDC providers, also set `authorization_endpoint` and `token_endpoint`:

```bash
# Encrypt the client secret
ENCRYPTED=$(rise encrypt "your_client_secret_here")

# Create extension with manual endpoints
rise extension create oauth-github -p my-app \
  --type oauth \
  --spec '{
    "provider_name": "GitHub",
    "description": "Sign in with GitHub",
    "client_id": "Iv1.abc123...",
    "client_secret_encrypted": "'"$ENCRYPTED"'",
    "issuer_url": "https://github.com",
    "authorization_endpoint": "https://github.com/login/oauth/authorize",
    "token_endpoint": "https://github.com/login/oauth/access_token",
    "scopes": ["read:user", "user:email"]
  }'
```

## Local Development

`rise run` automatically injects OAuth extension environment variables into your local container, so OAuth flows work out of the box during local development.

### How It Works

When you run `rise run --project my-app`, the CLI calls the preview endpoint to fetch the full set of environment variables your deployment would receive, including:

- `{EXTENSION}_CLIENT_ID` — Rise client ID (e.g., `OAUTH_GOOGLE_CLIENT_ID`)
- `{EXTENSION}_CLIENT_SECRET` — Rise client secret (decrypted)
- `{EXTENSION}_ISSUER` — Rise OIDC proxy URL
- `RISE_ISSUER` — Rise server URL

Your app uses the same OAuth credentials and OIDC proxy in both local dev and production. The Rise OIDC proxy handles the upstream provider interaction, so no provider-side redirect URI changes are needed.

### Redirect URI Handling

Pass a `redirect_uri` query parameter to redirect back to localhost after authentication:

```
GET {RISE_ISSUER}/oidc/my-app/oauth-google/authorize?redirect_uri=http://localhost:3000/callback
```

Rise allows redirects to:
- **Localhost URLs** (any port) — for local development
- **Project domain** (e.g., `https://my-app.app.example.com`) — for production

### Example Workflow

```bash
# Start your app locally with all project env vars (including OAuth)
rise run --project my-app --http-port 3000

# Your app now has OAUTH_GOOGLE_CLIENT_ID, OAUTH_GOOGLE_CLIENT_SECRET, etc.
# User visits http://localhost:3000 → app redirects to Rise OIDC proxy →
# Rise authenticates with upstream provider → callback to localhost
```

## Security

- **Client secrets**: Encrypted at rest (AES-GCM or AWS KMS), never exposed to frontends
- **Authorization codes**: Single-use, 5-minute TTL
- **CSRF protection**: Random state tokens validated on callback (10-minute TTL)
- **PKCE**: Required for public clients (SPAs) — proves the client that initiated the flow
- **Constant-time comparison**: All secret validation uses constant-time comparison

### Token Refresh

Clients manage token refresh via the `/oidc/{project}/{extension}/token` endpoint with `grant_type=refresh_token`. Rise proxies the request to the upstream provider.

## Troubleshooting

**"Failed to resolve OAuth endpoints"** or **"No authorization_endpoint in spec or OIDC discovery"**
- For OIDC-compliant providers: Ensure `issuer_url` is correct and provider supports OIDC discovery
- For non-OIDC providers (GitHub, Snowflake): Set `authorization_endpoint` and `token_endpoint` manually
- Test OIDC discovery: `curl {issuer_url}/.well-known/openid-configuration`

**"Invalid issuer_url URL"**
- Ensure URL is valid HTTPS endpoint
- Don't include trailing slash or paths (e.g., `https://accounts.google.com`, not `https://accounts.google.com/`)

**"Token exchange failed with status 400"**
- Verify `client_id` and `client_secret_encrypted` are correct
- Check redirect URI matches OAuth provider configuration
- Review OAuth provider logs for specific error

**"No cached state found for state token"**
- State token expired (10-minute TTL)
- Restart OAuth flow from beginning

**"Invalid or expired authorization code"**
- Authorization code already used (single-use)
- Authorization code expired (5-minute TTL)
- Request new authorization

## API Reference

### Authorization Endpoint

**PKCE Flow (SPA):**

```
GET /oidc/{project}/{extension}/authorize?code_challenge=...&code_challenge_method=S256
GET /oidc/{project}/{extension}/authorize?code_challenge=...&code_challenge_method=S256&redirect_uri=http://localhost:3000/callback
```

**Token Endpoint Flow (Backend):**

```
GET /oidc/{project}/{extension}/authorize
GET /oidc/{project}/{extension}/authorize?redirect_uri=http://localhost:3000/callback
```

**Query Parameters:**
- `code_challenge` (optional): PKCE code challenge (base64url-encoded SHA-256 hash of code_verifier)
- `code_challenge_method` (optional): Only `S256` is supported (and is the default)
- `redirect_uri` (optional): Where to redirect after OAuth (localhost or project domain)
- `state` (optional): Application state passed through OAuth flow

### Callback Endpoint

```
GET /oidc/{project}/{extension}/callback?code=...&state=...
```

**Response:**
```
HTTP/1.1 302 Found
Location: https://my-app.app.example.com/callback?code=abc123...
```

The `code` parameter is an authorization code that can be exchanged for tokens at the token endpoint.

### Token Endpoint (RFC 6749-compliant)

**Recommended:** This is the standards-compliant OAuth 2.0 token endpoint.

```
POST /oidc/{project}/{extension}/token
Content-Type: application/x-www-form-urlencoded
```

**Request Parameters (form-urlencoded or JSON):**

**For authorization_code grant (exchange code for tokens):**
- `grant_type` (required): Must be `"authorization_code"`
- `code` (required): Authorization code from callback
- `client_id` (required): Rise client ID from environment variable `{EXTENSION}_CLIENT_ID`
- **Client authentication (at least one required):**
  - `client_secret`: Rise client secret (confidential clients)
  - `code_verifier`: PKCE code verifier (proves client initiated the flow)
  - Both may be provided together for defense-in-depth (recommended by OAuth 2.1)

**For refresh_token grant (refresh access token):**
- `grant_type` (required): Must be `"refresh_token"`
- `refresh_token` (required): Refresh token from previous token response
- `client_id` (required): Rise client ID
- `client_secret` (required): Rise client secret (confidential clients)

**Response (RFC 6749 format):**
```json
{
  "access_token": "eyJhbGc...",
  "token_type": "Bearer",
  "expires_in": 3600,  // Seconds from now (not timestamp)
  "refresh_token": "eyJhbGc...",  // Optional
  "scope": "email profile",  // Optional, space-delimited
  "id_token": "eyJhbGc..."  // Optional, OIDC
}
```

**Error Response (RFC 6749 format):**
```json
{
  "error": "invalid_grant",
  "error_description": "Invalid or expired authorization code"
}
```

**Error Codes:**
- `invalid_request` (400): Missing or invalid parameters (e.g., neither `client_secret` nor `code_verifier` provided)
- `invalid_client` (401): Invalid client_id or client_secret
- `invalid_grant` (400): Invalid/expired code, or PKCE validation failed
- `unsupported_grant_type` (400): Unknown grant_type
- `server_error` (500): Internal server error

### OIDC Discovery Endpoint

Rise provides an OIDC-compliant discovery endpoint that proxies from the upstream provider and rewrites URLs to point to Rise's OIDC proxy.

```
GET /oidc/{project}/{extension}/.well-known/openid-configuration
```

**Response:** Standard OIDC discovery document with Rise URLs:

```json
{
  "issuer": "https://rise.example.com/oidc/my-app/oauth-google",
  "authorization_endpoint": "https://rise.example.com/oidc/my-app/oauth-google/authorize",
  "token_endpoint": "https://rise.example.com/oidc/my-app/oauth-google/token",
  "jwks_uri": "https://rise.example.com/oidc/my-app/oauth-google/jwks",
  "...": "other fields from upstream provider"
}
```

### JWKS Endpoint

Rise proxies the JWKS (JSON Web Key Set) from the upstream OAuth provider.

```
GET /oidc/{project}/{extension}/jwks
```

**Response:** JWKS from upstream provider for id_token signature validation.

**Client Credentials:**

Rise automatically generates client credentials when you create an OAuth extension:
- `{EXTENSION}_CLIENT_ID` - Client ID (plaintext, can be public for PKCE flows)
  - Format: `{project-name}-{extension-name}` (deterministic and predictable)
  - Example: For project `my-app` and extension `oauth-google` → `my-app-oauth-google`
  - Env var: `OAUTH_GOOGLE_CLIENT_ID` (for extension named `oauth-google`)
- `{EXTENSION}_CLIENT_SECRET` - Client secret (encrypted, random, for confidential clients)
  - Env var: `OAUTH_GOOGLE_CLIENT_SECRET` (for extension named `oauth-google`)
- `{EXTENSION}_ISSUER` - Rise OIDC proxy URL for id_token validation via JWKS discovery
  - Env var: `OAUTH_GOOGLE_ISSUER` (for extension named `oauth-google`)
  - Points to Rise's OIDC proxy: `{RISE_PUBLIC_URL}/oidc/{project}/{extension}`
  - Proxies OIDC discovery and JWKS from upstream provider with URLs rewritten to Rise endpoints

These are available as environment variables in your deployed applications. The extension name is normalized to uppercase with hyphens replaced by underscores.

**Rise URL Environment Variables:**

Rise injects URL environment variables into deployments for building OAuth URLs dynamically:

| Environment Variable | Purpose | Example |
|---------------------|---------|---------|
| `RISE_ISSUER` | Rise server URL (base URL for all Rise endpoints) | `http://localhost:3000` |

Use this for all Rise endpoints:

```javascript
// Browser redirect (PKCE authorize URL)
const authUrl = `${process.env.RISE_ISSUER}/oidc/my-app/oauth-google/authorize`;

// Backend token exchange
const tokenUrl = `${process.env.RISE_ISSUER}/oidc/my-app/oauth-google/token`;

// OpenID configuration (for JWT validation)
const configUrl = `${process.env.RISE_ISSUER}/.well-known/openid-configuration`;
```

**Security:**
- Client secret validated with constant-time comparison
- PKCE code_verifier validated against code_challenge (SHA-256 hash)
- Authorization codes are single-use with 5-minute TTL
- All tokens encrypted at rest
