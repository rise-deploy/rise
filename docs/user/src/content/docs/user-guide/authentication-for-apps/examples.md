---
title: Example Code
description: TypeScript examples for validating Rise app JWTs.
---

These examples use the `jose` library, which handles JWKS fetching and caching.

Install:

```bash
npm install jose cookie-parser
```

## Express middleware

```typescript
import { jwtVerify, createRemoteJWKSet } from 'jose';
import type { Request, Response, NextFunction } from 'express';

const RISE_ISSUER = process.env.RISE_ISSUER || 'http://rise.local:3000';
const RISE_APP_URL = process.env.RISE_APP_URL;

const JWKS = createRemoteJWKSet(
  new URL(`${RISE_ISSUER}/api/v1/auth/jwks`)
);

interface RiseClaims {
  sub: string;
  email: string;
  name?: string;
  groups?: string[];
}

async function verifyRiseJwt(req: Request, res: Response, next: NextFunction) {
  const token = req.cookies.rise_jwt;

  if (!token) {
    return res.status(401).send('No authentication token');
  }

  try {
    const { payload } = await jwtVerify<RiseClaims>(token, JWKS, {
      issuer: RISE_ISSUER,
      audience: RISE_APP_URL,
    });

    req.user = {
      id: payload.sub,
      email: payload.email,
      name: payload.name,
      groups: payload.groups || [],
    };

    next();
  } catch {
    return res.status(401).send('Invalid token');
  }
}
```

## Group-based authorization

Use the `groups` claim to implement team-based authorization:

```typescript
import type { Request, Response, NextFunction } from 'express';

function requireTeam(teamName: string) {
  return (req: Request, res: Response, next: NextFunction) => {
    if (!req.user) {
      return res.status(401).send('Not authenticated');
    }

    if (!req.user.groups.includes(teamName)) {
      return res.status(403).send('Access denied');
    }

    next();
  };
}

app.get('/admin', requireTeam('admin'), (req: Request, res: Response) => {
  res.send('Admin panel');
});
```

## Library guidance

Use a mature JWT/OIDC library rather than parsing tokens by hand. Good defaults:

- Node.js: `jose`
- Python: `authlib`

## Additional resources

- [`rise_jwt` Cookie](rise-jwt-cookie)
- [Validating JWTs](validating-jwts)
- [JWT.io](https://jwt.io/)
- [JWKS Specification](https://datatracker.ietf.org/doc/html/rfc7517)
