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

Log in with the token file and work under your own path, built from the
`RISE_PROJECT` and `RISE_ENVIRONMENT` variables Rise injects:

```bash
export BAO_ADDR=https://bao.example.com
export BAO_TOKEN=$(bao write -field=token auth/rise/login \
  role=app jwt=@/var/run/secrets/rise/identity/tokens/bao)
APP_PATH="rise/$RISE_PROJECT/$RISE_ENVIRONMENT"

bao kv put -mount=secret "$APP_PATH/config" api_key=s3cr3t
bao kv get -mount=secret "$APP_PATH/config"
```

With Vault, use `vault` instead of `bao` — the arguments are the same.

Any Vault client library does the same: `POST /v1/auth/rise/login` with
`{"role": "app", "jwt": "<file contents>"}`, then use the returned
`auth.client_token`. Re-read the token file on every login — Rise refreshes it
before it expires — and log in again when the Vault token runs out.

## Without Vault code in your app

Make [OpenBao Agent](https://openbao.org/docs/agent-and-proxy/agent/) (or
Vault Agent) your image's entrypoint and let it start your app. The Agent logs
in with the token file, passes secrets to the app as environment variables, and
restarts the app when they change. This replaces the image's entrypoint, so it
needs a [Dockerfile build](../../user-guide/builds/docker/):

```dockerfile
FROM openbao/openbao:2.4 AS bao

FROM python:3.13-slim
COPY --from=bao /bin/bao /usr/local/bin/bao
COPY agent.hcl /etc/bao/agent.hcl
COPY . /app
WORKDIR /app
ENTRYPOINT ["bao", "agent", "-config=/etc/bao/agent.hcl"]
```

```hcl
# agent.hcl
vault {
  address = "https://bao.example.com"
}

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

# One block per variable; RISE_PROJECT and RISE_ENVIRONMENT pick this
# deployment's path.
env_template "API_KEY" {
  contents             = "{{ with secret (printf \"secret/data/rise/%s/%s/config\" (env \"RISE_PROJECT\") (env \"RISE_ENVIRONMENT\")) }}{{ .Data.data.api_key }}{{ end }}"
  error_on_missing_key = true
}

exec {
  command                   = ["python", "app.py"]
  restart_on_secret_changes = "always"
  restart_stop_signal       = "SIGTERM"
}

# How often changed values are picked up (default 5m).
template_config {
  static_secret_render_interval = "1m"
}
```

The same image works in every project and environment. Your app reads `API_KEY` like any
other environment variable and must handle `SIGTERM`, which the Agent sends
before restarting it with new values.

## Notes

- Rise itself does not inject Vault values; read them at runtime or through
  the Agent as above. Rise's own [environment variables](../../user-guide/environment-variables/)
  remain the place for configuration Rise should inject at deploy time.
