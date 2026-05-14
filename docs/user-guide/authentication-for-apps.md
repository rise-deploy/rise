# Authentication for Rise-Deployed Applications

Rise provides built-in authentication for your deployed applications using JWT tokens. When users authenticate to access your application, Rise issues a signed JWT token that your application can validate to identify the user.

## Overview

When a user logs into a Rise-deployed application:

1. Rise authenticates the user via OAuth2/OIDC (e.g., through Dex)
2. Rise issues an RS256-signed JWT token with user information
3. The JWT is stored in the `rise_jwt` cookie
4. Your application can access this cookie to identify the user

> **Token types**: Rise issues HS256-signed JWTs for API/CLI authentication (audience = Rise server)
> and RS256-signed JWTs for application ingress authentication (audience = application URL).
> Applications only see RS256 tokens. The different signing algorithms ensure tokens cannot be
> used across contexts.

## The `rise_jwt` Cookie

The `rise_jwt` cookie contains a JWT token with the following structure:

### JWT Header
```json
{
  "alg": "RS256",
  "typ": "JWT",
  "kid": "<key-id>"
}
```

### JWT Claims (Example)
```json
{
  "sub": "CiQwOGE4Njg0Yi1kYjg4LTRiNzMtOTBhOS0zY2QxNjYxZjU0NjYSBWxvY2Fs",
  "email": "admin@example.com",
  "name": "admin",
  "groups": [],
  "iat": 1768858875,
  "exp": 1768945275,
  "iss": "http://rise.local:3000",
  "aud": "http://test.rise.local:8080"
}
```

### Claim Descriptions

- **sub**: Unique user identifier from the identity provider (typically a base64-encoded UUID)
- **email**: User's email address
- **name**: User's display name (optional, included if available from IdP)
- **groups**: Array of Rise team names the user belongs to (empty array if user has no team memberships)
- **iat**: Issued at timestamp (Unix epoch seconds)
- **exp**: Expiration timestamp (Unix epoch seconds, default: 24 hours from issue time)
- **iss**: Issuer (Rise backend URL, e.g., `http://rise.local:3000`)
- **aud**: Audience (your deployed application's URL, e.g., `http://test.rise.local:8080`)

**Note**: The JWT expiration time is configurable via the `jwt_expiry_seconds` server setting (default: 86400 seconds = 24 hours).

## Validating the JWT

Rise provides the public keys needed to validate JWTs through the standard OpenID Connect Discovery endpoint.

### OpenID Connect Discovery

Applications should use the OpenID Connect Discovery 1.0 specification to discover the JWKS endpoint:

1. **Fetch OpenID configuration** from `${RISE_ISSUER}/.well-known/openid-configuration`
2. **Extract `jwks_uri`** from the configuration response
3. **Fetch JWKS** from the `jwks_uri` endpoint
4. **Cache the JWKS** (recommended: 1 hour) to avoid excessive requests
5. **Use the JWKS** to validate JWT signatures

Example discovery response:
```json
{
  "issuer": "https://rise.example.com",
  "jwks_uri": "https://rise.example.com/api/v1/auth/jwks",
  "id_token_signing_alg_values_supported": ["RS256", "HS256"],
  "subject_types_supported": ["public"],
  "claims_supported": ["sub", "email", "name", "groups", "iat", "exp", "iss", "aud"]
}
```

### Environment Variables

Your deployed application automatically receives:

- **RISE_ISSUER**: Rise server URL (base URL for all Rise endpoints) and JWT issuer for validation (e.g., `http://rise.local:3000`)
- **RISE_APP_URL**: Canonical URL where your app is accessible (primary custom domain or default project URL)
- **RISE_APP_URLS**: JSON array of all URLs your app is accessible at (primary ingress + custom domains), e.g., `["http://myapp.rise.local:8080", "https://myapp.example.com"]`
- **PORT**: The HTTP port your container should listen on (default: 8080)

### Example: TypeScript/Node.js

Using the `jose` library which handles OIDC discovery and JWKS automatically:

```typescript
import { jwtVerify, createRemoteJWKSet } from 'jose';
import type { Request, Response, NextFunction } from 'express';

const RISE_ISSUER = process.env.RISE_ISSUER || 'http://rise.local:3000';
const RISE_APP_URL = process.env.RISE_APP_URL;

// Create JWKS fetcher (automatically handles caching and discovery)
const JWKS = createRemoteJWKSet(
  new URL(`${RISE_ISSUER}/api/v1/auth/jwks`)
);

interface RiseClaims {
  sub: string;
  email: string;
  name?: string;
  groups?: string[];
}

// Express middleware to verify Rise JWT
async function verifyRiseJwt(req: Request, res: Response, next: NextFunction) {
  const token = req.cookies.rise_jwt;

  if (!token) {
    return res.status(401).send('No authentication token');
  }

  try {
    const { payload } = await jwtVerify<RiseClaims>(token, JWKS, {
      issuer: RISE_ISSUER,
      audience: RISE_APP_URL, // Validates the aud claim
    });

    req.user = {
      id: payload.sub,
      email: payload.email,
      name: payload.name,
      groups: payload.groups || [],
    };

    next();
  } catch (err) {
    return res.status(401).send('Invalid token');
  }
}
```

Install: `npm install jose cookie-parser`

## Authorization Based on Groups

You can use the `groups` claim to implement team-based authorization:

```typescript
import type { Request, Response, NextFunction } from 'express';

function requireTeam(teamName: string) {
  return (req: Request, res: Response, next: NextFunction) => {
    if (!req.user) {
      return res.status(401).send('Not authenticated');
    }

    if (!req.user.groups.includes(teamName)) {
      return res.status(403).send('Access denied - not a member of required team');
    }

    next();
  };
}

// Protect routes by team membership
app.get('/admin', requireTeam('admin'), (req: Request, res: Response) => {
  res.send('Admin panel');
});
```

## Best Practices

1. **Always Validate the JWT**: Don't trust the cookie contents without verification
2. **Verify Audience**: Always validate the `aud` claim matches `RISE_APP_URL`
3. **Use Modern Libraries**: Use `jose` (Node.js) or `authlib` (Python) - they handle OIDC discovery automatically
4. **Use HTTPS**: The `rise_jwt` cookie is marked as Secure in production
5. **Handle Missing Tokens**: Users may not be authenticated - handle gracefully
6. **Let Libraries Cache**: Modern JWT libraries automatically cache JWKS with appropriate TTLs

## Troubleshooting

### Token Validation Fails

- **Check Algorithm**: Ensure you're using RS256, not HS256
- **Verify JWKS**: Ensure your library can reach `${RISE_ISSUER}/.well-known/openid-configuration`
- **Check Audience**: The `aud` claim must match `RISE_APP_URL`
- **Check Expiration**: Tokens expire after 24 hours by default (configurable)

### No Cookie Present

- **Check Authentication**: User may not be logged in
- **Check Access Class**: Ensure your project has authentication enabled
- **Check Cookie Domain**: For custom domains, cookies may not be shared

### Groups Missing

- **Check IdP Configuration**: Groups come from your identity provider
- **Check Team Sync**: Ensure IdP group sync is enabled in Rise
- **Check Team Membership**: User must be a member of Rise teams

## Security Considerations

- The `rise_jwt` cookie is **HttpOnly** - JavaScript cannot access it (XSS protection)
- The `rise_jwt` cookie is **application-scoped** — the `aud` claim is set to the application's URL, making the token unusable for Rise API access. Rise's API rejects tokens with non-matching audiences. Applications **must** validate the `aud` claim to ensure the token was issued for them specifically
- The JWT is signed with RS256 - public keys fetched via OIDC discovery verify authenticity
- Tokens expire after 24 hours by default - users must re-authenticate periodically

## Additional Resources

- [Authentication](authentication.md) — user login, service accounts, app users
- [OAuth Extensions](oauth.md) — OAuth proxy for third-party providers
- [Environment Variables](environment-variables.md) — auto-injected variables reference
- [JWT.io](https://jwt.io/) — JWT debugger and documentation
- [JWKS Specification](https://datatracker.ietf.org/doc/html/rfc7517) — JSON Web Key Set standard
