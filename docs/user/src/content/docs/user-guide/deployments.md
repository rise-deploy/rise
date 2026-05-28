---
title: "Deployments"
---

A deployment is an immutable, timestamped instance of your application running in the container runtime. Each deployment has a unique ID (e.g., `my-app:20241205-1234`), tracks its own status, and can be rolled back to.

## Creating a Deployment

The primary command is `rise deploy`:

```bash
# Deploy from current directory (builds, pushes, deploys)
rise deploy

# Deploy from a specific directory
rise deploy ./path/to/app

# Specify a project explicitly
rise deploy -p my-app

# Deploy to a specific environment
rise deploy -E staging
```

`rise deploy` is a shortcut for `rise deployment create` (`rise d c`). After creating the deployment, Rise automatically follows its progress.

**Environment and group selection** — when you don't pass `-E` or `--group`, Rise uses the project's default environment and its primary deployment group (typically `production` → `default`). You can deploy to a named environment with `-E <name>`, which automatically selects that environment's primary group. See [Environments](../environments#deploying-to-environments) for the full resolution rules.

### Pre-Built Images

Skip the build step by providing an image directly:

```bash
rise deploy --image nginx:latest --http-port 80
rise deploy --image myregistry.io/my-app:v1.2.3
```

When using `--image`, no build occurs and `--http-port` is required.

> **Note:** Private images from external registries may not be pullable by the container runtime due to missing credentials. Contact your Rise platform administrator for guidance.

### Deploying from an Existing Deployment

Reuse the image from a previous deployment:

```bash
rise deploy --from 20241205-1234
```

By default, the new deployment uses the project's current environment variables. To copy environment variables from the source deployment instead:

```bash
rise deploy --from 20241205-1234 --use-source-env-vars
```

## Deployment Lifecycle

Deployments progress through the following states:

### Build & Deploy States

| Status | Description |
|--------|-------------|
| `Pending` | Deployment created, waiting to start |
| `Building` | Container image is being built |
| `Pushing` | Image is being pushed to the registry |
| `Pushed` | Image pushed; handoff to the deployment controller |
| `Deploying` | Controller is creating the container in the runtime |

### Running States

| Status | Description |
|--------|-------------|
| `Healthy` | Running and passing health checks |
| `Unhealthy` | Running but failing health checks |

Once a deployment enters a running state it stays there indefinitely — Rise does **not** automatically terminate or fail a deployment that has been `Unhealthy` for a long time. An unhealthy deployment continues to receive traffic (or not, depending on your ingress health-check configuration) until you explicitly stop it or a new deployment supersedes it in the same group.

### Cancellation States (Before Infrastructure)

| Status | Description |
|--------|-------------|
| `Cancelling` | Being cancelled before infrastructure was provisioned |
| `Cancelled` | Cancelled before infrastructure was provisioned (terminal) |

### Termination States (After Infrastructure)

| Status | Description |
|--------|-------------|
| `Terminating` | Being gracefully terminated |
| `Stopped` | User-initiated termination (terminal) |
| `Superseded` | Replaced by a newer deployment in the same group (terminal) |

### Other Terminal States

| Status | Description |
|--------|-------------|
| `Failed` | Could not reach Healthy state (terminal) |
| `Expired` | Auto-deleted after reaching Healthy (terminal) |

## Deployment Groups

A deployment group is a label that identifies a set of related deployments. Only one deployment per group can be active at a time — when a new deployment in a group reaches `Healthy`, the previous one is `Superseded`.

**Group names are just labels — they carry no intrinsic meaning on their own.** A group name only acquires routing, URL, and variable-scoping semantics when it is set as the primary group of an [Environment](../environments). Without a matching environment, deployments in a custom group still run but receive the generic staging URL pattern and no environment-scoped variables.

```bash
# Deploy to the default group
rise deploy

# Deploy to a named group
rise deploy --group mr/123 --expire 7d
rise deploy --group feature/login
```

Group names must match `[a-z0-9][a-z0-9/-]*[a-z0-9]` (no consecutive hyphens `--`, normalized length max 63 characters). The default group is named `default`.

### Environments

[Environments](../environments) give semantic meaning to deployment groups. Each environment has a primary deployment group and controls URL routing, variable scoping, and access. The environment marked as **production** determines which deployments receive production traffic and the project's main URL — not the deployment group name itself.

New projects start with a `production` environment mapped to the `default` group. You can create additional environments for staging, dev, etc. See [Environments](../environments) for details.

### CI Source Information

When `rise deploy` runs inside a CI pipeline, the CLI automatically attaches deployment source metadata by reading well-known environment variables:

| Metadata | GitLab CI | GitHub Actions | Other CI |
|----------|-----------|---------------|----------|
| Job URL | `CI_JOB_URL` | `GITHUB_SERVER_URL` + `GITHUB_REPOSITORY` + `GITHUB_RUN_ID` | `CIRCLE_BUILD_URL`, `BUILDKITE_BUILD_URL`, `DRONE_BUILD_LINK`, `BUILD_URL` (Jenkins) |
| MR/PR URL | `CI_MERGE_REQUEST_URL` | `GITHUB_SERVER_URL` + `GITHUB_REPOSITORY` + `GITHUB_REF` (on `pull_request` events) | — |
| Git repository | `CI_PROJECT_URL` | `GITHUB_SERVER_URL` + `GITHUB_REPOSITORY` | `BITBUCKET_GIT_HTTP_ORIGIN`, `CIRCLE_REPOSITORY_URL`, `BUILD_REPOSITORY_URI` (Azure), `BUILDKITE_REPO`, `DRONE_GIT_HTTP_URL`, `GIT_URL` (Jenkins) |

This metadata is stored with the deployment and displayed in the Rise web UI, making it easy to trace a running deployment back to the pipeline job and merge/pull request that created it. No extra configuration is needed — detection is fully automatic.

When no CI environment is detected, the Git repository falls back to the local `origin` remote. Repository URLs in `ssh://`, `git://`, and `git@host:owner/repo` form are normalized to `https://` (credentials, ports, and the trailing `.git` are stripped). You can override detection with `rise deploy --git-repository <url>`.

A project also has a configurable `source` URL (set with `rise project create --source-url` or `rise project update`). When it is left unset, the web UI shows the repository resolved from the project's active deployment, falling back to its most recent deployment.

### Auto-Expiration

Set deployments to expire automatically:

```bash
rise deploy --group mr/123 --expire 7d   # Days
rise deploy --group preview --expire 24h  # Hours
rise deploy --group temp --expire 7d      # Days (max unit; no weeks shorthand)
```

Expired deployments are automatically cleaned up.

## Monitoring Deployments

### Following a Deployment

`rise deploy` follows automatically. You can also follow an existing deployment:

```bash
rise deployment show -p my-app 20241205-1234 --follow
rise d s -p my-app latest --follow --timeout 10m
```

### Listing Deployments

```bash
rise deployment list -p my-app
rise d ls -p my-app --group staging
```

### Viewing Deployment Details

```bash
rise deployment show -p my-app 20241205-1234
rise d s -p my-app latest
```

### Deployment Logs

```bash
# Show recent logs
rise deployment logs -p my-app 20241205-1234

# Follow logs in real-time
rise deployment logs -p my-app 20241205-1234 --follow

# Show last 100 lines
rise deployment logs -p my-app 20241205-1234 --tail 100

# Show logs since a duration ago
rise deployment logs -p my-app 20241205-1234 --since 5m

# Show timestamps
rise deployment logs -p my-app 20241205-1234 --timestamps
```

When Rise is configured with the Kubernetes log backend, runtime logs are available only while deployment Pods still exist. When Rise is configured with the Loki log backend, runtime logs can also be shown for past deployments until the Loki retention policy removes them. If no historical logs are found, Rise will indicate whether they may have expired based on the operator-configured retention hint.

## Rollback

The primary rollback mechanism is redeploying from a previous deployment's image:

```bash
rise deploy --from 20241205-1234
```

This fetches the target deployment's image digest and creates a new deployment with it. The original deployment is not modified.

> **Warning — database migrations:** Rollback only restores the container image, not the database schema. If a migration ran between the current and target deployment, rolling back may leave your application running against a schema it doesn't understand, causing errors or data corruption. Before rolling back, verify whether migrations are involved and plan accordingly (e.g., ensure migrations are backward-compatible, or restore a database snapshot alongside the image rollback).

## Stopping Deployments

Stop all deployments in a group:

```bash
rise deployment stop -p my-app --group default
rise d stop -p my-app --group mr/123
```

Stopped deployments remain in the database for rollback purposes.

## Auto-Injected Environment Variables

Rise injects variables like `PORT`, `RISE_ISSUER`, `RISE_APP_URL`, `RISE_APP_URLS`, and `RISE_ENVIRONMENT` into every deployment. See [Environment Variables](../environment-variables#auto-injected-variables) for the full list.

For JWT validation using `RISE_ISSUER`, see [Validating JWTs](../authentication-for-apps/validating-jwts).

## Multi-Container Deployments

A single Rise deployment can run multiple containers — for example a frontend, an HA backend, and a worker — each as its own Kubernetes Deployment so replica counts scale independently. Configure them under `[containers.<name>]` in `rise.toml` and route HTTP traffic across them via `[routes]`:

```toml
[project]
name = "my-app"

[containers.frontend]
image = "registry.example.com/my-app/frontend:1.2.3"
http_port = 8080
replicas = 2

[containers.backend]
image = "registry.example.com/my-app/backend:1.2.3"
http_port = 9090
replicas = 3
# Override the default HTTP probe (defaults to GET / when http_port is set):
health_check = { path = "/health", initial_delay_seconds = 5 }
# Per-container env vars. Project-level env vars also apply.
env = { LOG_LEVEL = "info" }

[containers.worker]
image = "registry.example.com/my-app/worker:1.2.3"
replicas = 4
# Workers have no http_port → no Service, no HTTP probes.

[routes]
"/api" = { container = "backend" }
"/" = { container = "frontend" }
```

Notes:

- `[containers]` is mutually exclusive with the top-level `[build]` and `[deploy]` sections. Existing single-container projects keep working unchanged — the top-level `[build]`/`[deploy]` is treated as an implicit `app` container.
- Each container must set `image = "..."` in the current CLI release. Per-container build orchestration is a planned follow-up; for now, build and push each container's image out-of-band (e.g., from CI).
- Containers without `http_port` get no `Service` and no HTTP probes — exactly what you want for workers / batch jobs.
- `health_check = false` disables probes entirely on a container.
- Routes are matched longest-prefix-first by the ingress, so `/api` shadows `/` correctly. If `[routes]` is omitted and exactly one container has `http_port`, Rise synthesises `/` → that container.

## CI/CD Deployments

For automated deployments from CI/CD pipelines, use service accounts with OIDC workload identity. See [Service Accounts](../service-accounts) for setup instructions and examples for GitLab CI and GitHub Actions.
