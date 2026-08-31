---
title: "Snowflake OAuth Provisioner Extension"
---

The `snowflake-oauth-provisioner` extension provisions Snowflake OAuth integration and creates a paired `oauth` extension.

## What It Does

- Creates Snowflake `SECURITY INTEGRATION` resources.
- Retrieves OAuth credentials and stores them securely.
- Creates/manages a linked `oauth` extension instance.

## Provider Configuration

The Snowflake OAuth provisioner is configured at the operator level under `extensions.providers`. Add it to your backend config:

```yaml
extensions:
  providers:
    - type: snowflake-oauth-provisioner
      account: "myorg.us-east-1"        # Snowflake account identifier
      user: "RISE_SERVICE_USER"         # User with CREATE INTEGRATION privilege
      role: "ACCOUNTADMIN"              # Role with CREATE INTEGRATION ON ACCOUNT privilege
      warehouse: "RISE_WH"             # Warehouse for executing SQL
      auth_type: password               # "password" or "private_key"
      password: "${SNOWFLAKE_PASSWORD}"
      # For private key auth:
      # auth_type: private_key
      # private_key_path: "/etc/rise/snowflake_rsa_key.p8"
      # private_key_password: "${SNOWFLAKE_KEY_PASSWORD}"  # optional; omit for unencrypted keys
      integration_name_prefix: "rise"   # prefix for SECURITY INTEGRATION names
      # default_blocked_roles: ["ACCOUNTADMIN", "ORGADMIN", "SECURITYADMIN"]
      # default_scopes: ["refresh_token"]
      # refresh_token_validity_seconds: 7776000  # 90 days
```

Private-key authentication accepts encrypted PKCS#8, unencrypted PKCS#8, and
unencrypted RSA PKCS#1 PEM keys. Rise converts unencrypted keys to encrypted
PKCS#8 in memory for the Snowflake connector; the generated passphrase is not
persisted.

## Project Extension Spec

Users configure the extension per-project:

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
