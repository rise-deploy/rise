---
title: "Snowflake OAuth Provisioner Extension"
---

The `snowflake-oauth-provisioner` extension provisions Snowflake OAuth integration and creates a paired `oauth` extension.

## What It Does

- Creates Snowflake `SECURITY INTEGRATION` resources.
- Retrieves OAuth credentials and stores them securely.
- Creates/manages a linked `oauth` extension instance.

## Configuration

```json
{
  "blocked_roles": ["SYSADMIN"],
  "scopes": ["session:role:ANALYST"]
}
```

## Fields

- `blocked_roles` (optional): additional blocked roles merged with backend defaults.
- `scopes` (optional): additional scopes merged with backend defaults.

## Lifecycle

Typical states:

- `Pending`
- `TestingConnection`
- `CreatingIntegration`
- `RetrievingCredentials`
- `CreatingOAuthExtension`
- `Available`

## Notes

- Provisioning is usually fast (seconds).
- This extension manages resources for you, including linked OAuth setup.
