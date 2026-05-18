---
title: Authentication for Rise Apps
description: Understand how Rise authenticates users for deployed applications.
---

Rise can protect deployed applications and pass the authenticated user identity to the app as a signed JWT.

## How it works

When a user opens a protected Rise-deployed application:

1. Rise authenticates the user via OAuth2/OIDC, for example through Dex.
2. Rise issues an RS256-signed JWT token with user information.
3. The JWT is stored in the `rise_jwt` cookie.
4. Your application validates the cookie before trusting the user identity.

> **Token types:** Rise issues HS256-signed JWTs for API/CLI authentication and RS256-signed JWTs for application ingress authentication. Applications should only accept RS256 app tokens whose `aud` claim matches the app URL.

## Read next

- [`rise_jwt` Cookie](rise-jwt-cookie) describes the cookie, JWT header, claims, expiration, and security properties.
- [Validating JWTs](validating-jwts) explains OIDC discovery, JWKS lookup, validation checks, environment variables, and troubleshooting.
- [Example Code](examples) shows Express middleware and group-based authorization in TypeScript.

## Additional resources

- [Authentication](../authentication) — user login, service accounts, app users
- [OAuth Extensions](../oauth) — OAuth proxy for third-party providers
- [Environment Variables](../environment-variables) — auto-injected variables reference
