---
title: "rise_jwt Cookie"
description: Cookie and JWT claim reference for Rise-protected applications.
---

The `rise_jwt` cookie contains the application authentication token issued by Rise.

## JWT header

```json
{
  "alg": "RS256",
  "typ": "JWT",
  "kid": "<key-id>"
}
```

Applications must validate `alg` and only accept `RS256` for app authentication.

## JWT claims

Example:

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

| Claim | Description |
| --- | --- |
| `sub` | Unique user identifier from the identity provider. |
| `email` | User email address. |
| `name` | User display name, included when available from the identity provider. |
| `groups` | Rise team names the user belongs to. Empty when the user has no team memberships. |
| `iat` | Issued-at timestamp as Unix epoch seconds. |
| `exp` | Expiration timestamp as Unix epoch seconds. |
| `iss` | Issuer, usually the Rise backend URL. |
| `aud` | Audience, set to the deployed application's URL. |

JWT expiration is controlled by the Rise platform. The default is 24 hours, but your platform operator may configure a different duration.

## Security properties

- The cookie is `HttpOnly`, so browser JavaScript cannot read it.
- The cookie is marked `Secure` in production.
- The JWT is application-scoped. The `aud` claim is set to the application URL, making the token unusable for Rise API access.
- Rise signs app tokens with RS256. Applications verify authenticity with public keys discovered from Rise.
- When cookies share a parent domain, an HS256 Rise API session cookie may also be present. Validating `alg` and `aud` prevents an app from accepting the wrong token type.

## Next

Use [Validating JWTs](validating-jwts) to validate the cookie in your application.
