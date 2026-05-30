---
title: "Environment Variables"
---

Rise manages environment variables at the project level. Variables are injected into containers at deployment time.

## Managing Variables

### Setting Variables

```bash
# Plain text variable
rise env set -p my-app LOG_LEVEL info

# Secret variable (encrypted at rest, masked in listings)
rise env set -p my-app DATABASE_URL postgres://user:pass@db/mydb --secret

# Protected secret (encrypted, cannot be retrieved via API)
rise env set -p my-app JWT_SECRET supersecret --secret --protected
```

With a `rise.toml` in your directory, you can omit `-p`:

```bash
rise env set LOG_LEVEL info
```

### Listing Variables

```bash
rise env list -p my-app

# List variables for a specific environment (shows global + scoped, merged)
rise env list -p my-app -E staging
```

Secret values are masked. Protected secrets cannot be decrypted.

### Getting a Single Variable

```bash
rise env get -p my-app LOG_LEVEL
```

### Deleting Variables

```bash
rise env delete -p my-app LOG_LEVEL
```

Aliases: `rise env unset`, `rise env rm`, `rise env del`

## Importing from a File

Import variables from a `.env`-style file:

```bash
rise env import -p my-app .env
```

File format:

```
# Comments are supported
LOG_LEVEL=info
DATABASE_URL=secret:postgres://user:pass@db/mydb
API_KEY=secret:s3cret
```

Prefix a value with `secret:` to store it as a secret variable.

## Environment-Scoped Variables

Variables can be scoped to a specific [environment](../environments) using the `-E` flag. Scoped variables override global variables with the same key when deploying to that environment.

```bash
# Set a variable only for staging
rise env set DATABASE_URL postgres://staging-db/mydb -E staging

# Get a scoped variable
rise env get -p my-app DATABASE_URL -E staging

# Delete a scoped variable
rise env delete -p my-app DATABASE_URL -E staging

# Import variables scoped to an environment
rise env import -p my-app .env.staging -E staging
```

Without `-E`, variables are global and apply to all environments.

## Deployment Snapshots

View the environment variables that were active for a specific deployment:

```bash
rise env show-deployment -p my-app 20241205-1234
```

This is a read-only view of the variables as they existed when the deployment was created.

## Auto-Injected Variables

Rise automatically injects these variables into every deployment:

| Variable | Description | Example |
|----------|-------------|---------|
| `PORT` | HTTP port the container should listen on | `8080` |
| `RISE_ISSUER` | Rise server URL and JWT issuer | `https://rise.example.com` |
| `RISE_APP_URL` | Canonical URL (primary custom domain or default URL) | `https://myapp.example.com` |
| `RISE_APP_URLS` | JSON array of all URLs for the app | `["https://myapp.app.example.com"]` |
| `RISE_DEPLOYMENT_GROUP` | Deployment group name | `default` |
| `RISE_DEPLOYMENT_GROUP_NORMALIZED` | Deployment group name normalized for URLs and K8s resource names (sequences of characters not in `[A-Za-z0-9-_.]` are replaced with `--`, and non-alphanumeric leading/trailing characters are trimmed) | `mr--123` |
| `RISE_ENVIRONMENT` | Environment name (if the deployment has an associated environment) | `staging` |
| `RISE_CONTAINER_HOST__<NAME>` | Resolvable address (`<host>:<port>`) of each sibling container in a [multi-container deployment](../deployments#multi-container-deployments) that exposes a `port` — one entry per such container, whether or not it is routed (a port-having database like Redis is included). `<NAME>` is the uppercase container name (dashes mapped to underscores). Workers (containers without a `port`) are excluded. Injected only when the deployment has two or more containers. Each container also sees its own entry. | `default-api:8080` |

`PORT` defaults to 8080. Override it per-deployment with `--http-port` on `rise deploy`, or set it permanently with `rise env set -p my-app PORT 3000`.

`RISE_CONTAINER_HOST__<NAME>` values are `<host>:<port>` (no scheme). Always address siblings through this variable rather than hardcoding a hostname — the exact host string depends on the deployment backend, but the variable resolves correctly on both:

- **Kubernetes** — the in-cluster Service host, prefixed with the deployment group (e.g. `default-api:8080`).
- **Docker** — the sibling's container name on the shared `rise_default` network (e.g. `rise_myapp_default_20260101-120000_api:8080`), resolved by Docker's embedded DNS.

Callers prepend their own protocol — e.g. `redis://${RISE_CONTAINER_HOST__REDIS}` or `http://${RISE_CONTAINER_HOST__API}`.

:::caution[The `RISE_` prefix is reserved]
The `RISE_` prefix is reserved for the auto-injected variables above. Setting your own variable whose name starts with `RISE_` — whether via `rise env set`, deploy-time overrides, or `[containers.<name>.env]` in `rise.toml` — is rejected. This prevents an injected value from silently shadowing your variable (or vice-versa).
:::

## Deploy-Time Environment Overrides

You can pass runtime environment variables directly when deploying:

```bash
# Plain text variable
rise deploy -e DATABASE_URL=postgres://user:pass@db/mydb

# Secret variable (encrypted, retrievable via `rise env get`)
rise deploy --secret-env API_KEY=sk-xxx

# Protected secret (encrypted, NOT retrievable)
rise deploy --protected-env MASTER_KEY=xxx

# From a file (same format as `rise env import`)
rise deploy --env-file .env.production
```

These overrides are applied after copying project env vars, so they take precedence over existing values. They are stored as deployment-level env vars.

## Build-Time vs Runtime Variables

| Aspect | Build-Time | Runtime |
|--------|-----------|---------|
| **Purpose** | Configure build process (compiler flags, tool versions) | Configure running application |
| **Set via** | `-b` / `--build-arg` flag or `[build] args` in `rise.toml` | `rise env set`, or `-e` / `--env` on deploy |
| **Available during** | Image build only | Container runtime |
| **Storage** | Ephemeral (not persisted) | Database (encrypted for secrets) |
| **Examples** | `NODE_ENV`, `BUILD_VERSION` | `DATABASE_URL`, `API_KEY` |

See [Building Images](../builds#build-time-arguments) for build-time variable details.

## Variables in rise.toml

You can define plain-text environment variables in `rise.toml`. These are applied as **deployment overrides** when you run `rise deploy` — they are not synced to the project-level env vars on the backend.

```toml
[project]
name = "my-app"

[project.env]
LOG_LEVEL = "info"
APP_MODE = "production"
```

### Per-Environment Variables in rise.toml

Environment-scoped variables can be defined under `[environments.<name>.env]`:

```toml
[project]
name = "my-app"

[project.env]
LOG_LEVEL = "info"
DATABASE_URL = "postgres://localhost/mydb"

[environments.staging]
default = true
env.DATABASE_URL = "postgres://staging-db/mydb"
env.LOG_LEVEL = "debug"

[environments.production]
env.DATABASE_URL = "postgres://prod-db/mydb"
```

When deploying, variables are merged in this order (later overrides earlier):

1. `[project.env]` variables (source: `toml`)
2. `[environments.<target>.env]` variables for the target environment (source: `toml`)
3. CLI deploy-time overrides (`-e`, `--secret-env`, `--protected-env`, `--env-file`) (source: `cli`)

Only plain-text variables can be managed in `rise.toml`. Secrets must be set via the CLI (`rise env set --secret`) or passed at deploy time (`--secret-env`, `--protected-env`).
