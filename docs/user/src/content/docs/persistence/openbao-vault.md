---
title: "OpenBao / Vault"
description: Store secrets and small config values in OpenBao or HashiCorp Vault, authenticated by your app's workload identity.
---

If your Rise operator has connected [OpenBao](https://openbao.org) or
[HashiCorp Vault](https://www.vaultproject.io) to Rise, your app logs in with
its [workload identity token](../../user-guide/workload-identity-tokens/) and
gets its own KV v2 path for each environment:

```
secret/rise/<project>/<environment>/*
```

No per-project setup or stored credentials are needed, and no app can reach
another project's or environment's path. The examples below use the values
from the operator [setup guide](/operator-docs/persistence/openbao-vault/) —
ask your operator for the ones on your install.

## Request a token

Add the Vault audience to `rise.toml`:

```toml
[identity.audiences]
bao = "https://bao.example.com"
```

Rise then keeps a token for it at `/var/run/secrets/rise/identity/tokens/bao`.

## Log in and read

Log in with the token file and work under your own path. Rise injects
`RISE_ENVIRONMENT`; the project name is your own:

```bash
export BAO_ADDR=https://bao.example.com
export BAO_TOKEN=$(bao write -field=token auth/rise/login \
  role=app jwt=@/var/run/secrets/rise/identity/tokens/bao)

bao kv put -mount=secret "rise/my-app/$RISE_ENVIRONMENT/config" api_key=s3cr3t
bao kv get -mount=secret "rise/my-app/$RISE_ENVIRONMENT/config"
```

With Vault, use `vault` instead of `bao` — the arguments are the same.

Any Vault client library does the same: `POST /v1/auth/rise/login` with
`{"role": "app", "jwt": "<file contents>"}`, then use the returned
`auth.client_token`. Re-read the token file on every login — Rise refreshes it
before it expires — and log in again when the Vault token runs out.

## Without Vault code in your app

Run [OpenBao Agent](https://openbao.org/docs/agent-and-proxy/agent/) or Vault
Agent alongside your app with JWT auto-auth pointed at the same file:

```hcl
auto_auth {
  method "jwt" {
    mount_path = "auth/rise"
    config = {
      role                     = "app"
      path                     = "/var/run/secrets/rise/identity/tokens/bao"
      remove_jwt_after_reading = false # the file is read-only and refreshed by Rise
    }
  }
}
```

## Notes

- Values are not exposed as environment variables; read them at runtime.
  Rise's own [environment variables](../../user-guide/environment-variables/)
  remain the place for configuration Rise should inject at deploy time.
