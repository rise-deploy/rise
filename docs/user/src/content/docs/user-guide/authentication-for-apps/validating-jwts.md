---
title: Validating JWTs
description: How Rise apps validate the `rise_jwt` cookie.
---

Rise exposes the public keys needed to validate application JWTs through the standard OpenID Connect Discovery flow.

## Discovery flow

Applications should use OpenID Connect Discovery to find the JWKS endpoint:

1. Fetch OpenID configuration from `${RISE_ISSUER}/.well-known/openid-configuration`.
2. Extract `jwks_uri` from the configuration response.
3. Fetch JWKS from the `jwks_uri` endpoint.
4. Cache the JWKS. One hour is a reasonable default.
5. Use the JWKS to validate JWT signatures.

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

## Required validation checks

Always validate:

- Signature: verify the JWT using Rise's JWKS.
- Algorithm: expect `RS256`.
- Issuer: expect `RISE_ISSUER`.
- Audience: expect the URL of your app, usually `RISE_APP_URL`.
- Expiration: reject expired tokens.

Do not trust cookie contents without verification.

## Environment variables

Rise automatically injects these environment variables into deployed applications:

| Variable | Description |
| --- | --- |
| `RISE_ISSUER` | Rise server URL and JWT issuer for validation. |
| `RISE_APP_URL` | Canonical URL where your app is accessible. |
| `RISE_APP_URLS` | JSON array of all URLs where your app is accessible, including custom domains. |
| `PORT` | HTTP port your container should listen on. Defaults to `8080`. |

## Troubleshooting

### Token validation fails

- Check that your library expects `RS256`, not `HS256`.
- Confirm your app can reach `${RISE_ISSUER}/.well-known/openid-configuration`.
- Validate that the `aud` claim matches `RISE_APP_URL`.
- Check token expiration. Tokens expire after 24 hours by default.

### No cookie is present

- The user may not be logged in.
- The project may not have application authentication enabled.

### Groups are missing

- Confirm groups are available from your identity provider.
- Confirm group sync is enabled in Rise.
- Confirm the user is a member of the expected Rise teams.

## Next

See [Example Code](examples) for TypeScript middleware that validates `rise_jwt`.
