---
title: "Private Project Authentication"
---

The Kubernetes controller implements ingress-level authentication for private projects using Nginx auth annotations and Rise-issued JWTs.

## Overview

- **Public projects**: Accessible without authentication
- **Private projects**: Require user authentication AND project access authorization
- **Authentication method**: OAuth2 via configured identity provider (Dex)
- **Token security**: Rise-issued JWTs scoped to specific projects
- **Cookie isolation**: Separate cookies prevent projects from accessing Rise APIs

## Configuration

Private project authentication requires JWT signing configuration:

```yaml
server:
  # JWT signing secret for ingress authentication (base64-encoded, min 32 bytes)
  # Generate with: openssl rand -base64 32
  # REQUIRED: The backend will fail to start without this
  jwt_signing_secret: "YOUR_BASE64_SECRET_HERE"

  # Optional: JWT claims to include from IdP token (default shown)
  jwt_claims: ["sub", "email", "name"]

  cookie_secure: false          # Set to false for local development (HTTP)

kubernetes:
  # Internal cluster URL for Nginx auth subrequests
  auth_backend_url: "http://rise-backend.default.svc.cluster.local:3000"

  # Public backend URL for browser redirects during authentication
  auth_signin_url: "http://rise.local"  # Use http:// for local development
```

**Generate JWT signing secret**:
```bash
openssl rand -base64 32
```

## Authentication Flow

When a user visits a private project, the following flow occurs:

```
User → myapp.apps.rise.local (private)
  ↓
Nginx calls GET /api/v1/auth/ingress?project=myapp
  - 🍪 NO COOKIE or invalid JWT
  ↓ Returns 401 Unauthorized
  ↓
Nginx redirects to /api/v1/auth/signin?project=myapp&redirect=http://myapp.apps.rise.local
  ↓
GET /api/v1/auth/signin (Pre-Auth Page):
  - Renders auth-signin.html.tera
  - Shows: "Project 'myapp' is private. Sign in to access."
  - Button: "Sign In" → /api/v1/auth/signin/start?project=myapp&redirect=...
  ↓
User clicks "Sign In" button
  ↓
GET /api/v1/auth/signin/start (OAuth Start):
  - Stores project_name='myapp' in OAuth2State (PKCE state)
  - Redirects to Dex IdP authorize endpoint
  ↓
User completes OAuth at Dex
  ↓
Dex redirects to /api/v1/auth/callback?code=xyz&state=abc
  ↓
GET /api/v1/auth/callback (Token Exchange):
  - Retrieve OAuth2State (includes project_name='myapp')
  - Exchange code for IdP tokens
  - Validate IdP JWT
  - Extract claims (sub, email, name) and expiry
  - Issue Rise JWT scoped to project (aud=https://myapp.apps.rise.local)
  - Store JWT under a one-time completion token
  - Redirect browser to https://myapp.apps.rise.local/.rise/auth/complete?token=xxx
  ↓
GET https://myapp.apps.rise.local/.rise/auth/complete?token=xxx (Cookie Setting):
  - Exchange one-time token for the stored Rise JWT
  - 🍪 SET COOKIE: rise_jwt = <Rise JWT>
       (HttpOnly, SameSite=Lax — host-only, no Domain attribute)
  - Shows success page, JavaScript redirects to original URL
  ↓
Browser redirects to http://myapp.apps.rise.local
  ↓
Nginx calls GET /api/v1/auth/ingress?project=myapp
  - 🍪 READS COOKIE: rise_jwt (host-only, only sent to myapp.apps.rise.local)
  - Verifies Rise JWT signature (HS256)
  - Validates expiry
  - Checks user has project access via database query
  ↓ Returns 200 OK + headers (X-Auth-Request-Email, X-Auth-Request-User)
  ↓
Nginx serves app
  - 🍪 rise_jwt cookie is sent to app (but app cannot read it — HttpOnly)
```

## JWT Structure

Rise issues symmetric HS256 JWTs with the following claims:

```json
{
  "sub": "user-id-from-idp",
  "email": "user@example.com",
  "name": "User Name",
  "groups": ["backend-team", "devops"],
  "iat": 1234567890,
  "exp": 1234571490,
  "iss": "http://rise.local",
  "aud": "https://myapp.apps.rise.local"
}
```

- `groups` — list of all Rise teams the authenticated user is a member of. Useful for application-level authorization without additional API calls.

For an example of how to validate these JWTs in an application, see [`example/rise-jwt`](https://github.com/NiklasRosenstein/rise/tree/main/example/rise-jwt) in the repository.

**Key features**:
- **Project-scoped audience**: The `aud` claim is set to the project URL, so applications that validate the JWT themselves can verify scope. Rise's own ingress auth does not check the audience — project access is validated by database permissions instead.
- **Cookie isolation**: Cookies are host-only (no `Domain` attribute), so each app domain only receives its own `rise_jwt` cookie. A cookie set on `myapp.apps.rise.local` is never sent to `otherapp.apps.rise.local`.
- **Configurable claims**: Include only necessary user information
- **Expiry matching**: Token expiration matches IdP token (typically 1 hour)
- **Symmetric signing**: HS256 with shared secret for fast validation

## Cookie Security

The `rise_jwt` cookie is used for both Rise UI sessions and project ingress authentication. What distinguishes them is the host it is set on and the JWT audience:

| Context | Set on host | JWT `aud` | Access |
|---------|-------------|-----------|--------|
| Rise UI login | `rise.local` | Rise public URL | HttpOnly |
| Project ingress auth | `myapp.apps.rise.local` | `https://myapp.apps.rise.local` | HttpOnly |

**Security attributes**:
- `HttpOnly`: Prevents JavaScript access (XSS protection)
- `Secure`: HTTPS-only transmission (configurable for local development)
- `SameSite=Lax`: CSRF protection while allowing navigation
- **No `Domain` attribute**: Cookie is host-only; browsers only send it to the exact host it was set on
- `Max-Age`: Matches JWT expiration

## Access Control

For private projects, the ingress auth endpoint validates:

1. **JWT validity**: Signature, expiration, issuer, audience
2. **User permissions**: Database query to check if user is owner or team member

Access check logic:
```rust
// User can access if:
// - User is the project owner (owner_user_id), OR
// - User is a member of the team that owns the project (owner_team_id)
//
// NOTE: Rise validates project access via database permissions, not JWT claims.
// Cookie isolation (host-only, no Domain) ensures a cookie for one app is never
// sent to a different app — additional DB permission checks guard access within
// the same host (e.g. sub-path routing where multiple projects share a host).
```

## Ingress Annotations

For private projects, the controller adds these Nginx annotations:

```yaml
annotations:
  nginx.ingress.kubernetes.io/auth-url: "http://rise-backend.default.svc.cluster.local:3000/api/v1/auth/ingress?project=myapp"
  nginx.ingress.kubernetes.io/auth-signin: "http://rise.local/api/v1/auth/signin?project=myapp&redirect=$escaped_request_uri"
  nginx.ingress.kubernetes.io/auth-response-headers: "X-Auth-Request-Email,X-Auth-Request-User"
```

**How it works**:
- `auth-url`: Nginx calls this endpoint for every request to validate authentication
  - Returns 2xx (200): Access granted
  - Returns 401/403: Access denied, redirect to auth-signin
  - Returns 5xx or unreachable: **Access denied (fail-closed)** - ensures security even if auth service is misconfigured or down
- `auth-signin`: Where to redirect unauthenticated users
- `auth-response-headers`: Headers to pass from auth response to the application

The application receives authenticated requests with these additional headers:
- `X-Auth-Request-Email`: User's email address
- `X-Auth-Request-User`: User's ID

## Troubleshooting

**Infinite redirect loop**:
- Verify cookies are being set (check browser DevTools → Application → Cookies)
- Ensure `cookie_secure` is `false` for HTTP development environments

**Browser always redirects HTTP to HTTPS**:
- Some TLDs (e.g., `.dev`) are on the HSTS preload list and browsers will always force HTTPS
- Use `.local` TLD for local development to avoid HSTS issues
- The default configuration uses `rise.local` which works correctly with HTTP
- If you must use a different TLD, check if it's on the HSTS preload list at https://hstspreload.org/

**"Access denied" or 403 Forbidden error**:
- User is authenticated but not authorized for this project
- Check project ownership: `rise project show <project-name>`
- Add user to project's team if needed

**"No session cookie" error**:
- Cookie expired or not set
- Browser blocking third-party cookies

**Private projects accessible without authentication**:
- Check ingress controller logs for auth subrequest errors: `kubectl logs -n ingress-nginx <ingress-controller-pod>`
- Verify `auth_backend_url` in config includes the correct service URL and port
- Ensure the auth service is reachable from the ingress controller (test with `curl` from ingress pod)
- Check that ingress annotations are correctly set: `kubectl get ingress -n rise-<project> -o yaml`
- All auth endpoints are under `/api/v1` prefix (e.g., `/api/v1/auth/ingress`)

**Authentication succeeds but access denied**:
- User is authenticated but not authorized for this project
- Check project ownership: `rise project show <project-name>`
- Add user to project's team if needed

**JWT signing errors in logs**:
```
Error: Failed to initialize JWT signer: Invalid base64
```
- JWT signing secret is not valid base64
- Regenerate with: `openssl rand -base64 32`
- Ensure secret is at least 32 bytes when decoded
