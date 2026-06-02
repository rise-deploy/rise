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

# Top-level [build] and [deploy] act as defaults that every container inherits.
# [build] merges field-by-field (set the backend once, override only the
# Dockerfile per container); [deploy] fields fall back individually.
[build]
backend = "docker"

[deploy]
replicas = 1
cpu = "128m-500m"   # request 128m, cap the limit at 500m (see "CPU & memory" below)

# Each container declares exactly one of `image` (pre-built reference) or
# a `[containers.<name>.build]` block (the CLI builds and pushes for you).

[containers.frontend]
port = 8080
[containers.frontend.build]
dockerfile = "frontend/Dockerfile"     # inherits backend = "docker"
[containers.frontend.deploy]
replicas = 2                           # override just the replica count

[containers.backend]
port = 9090
# Per-container env vars. Project-level env vars also apply.
env = { LOG_LEVEL = "info" }
[containers.backend.build]
dockerfile = "backend/Dockerfile"
[containers.backend.deploy]
replicas = 3
health_check = { path = "/health", initial_delay_seconds = 5 }

[containers.worker]
# Pre-built image — the CLI won't try to build or push this one, and it ignores
# the top-level [build] default. Workers have no port → no Service, no probes.
image = "registry.example.com/my-app/worker:1.2.3"
[containers.worker.deploy]
replicas = 4

[routes]
"/api" = { container = "backend" }
"/" = { container = "frontend" }
```

When you run `rise deploy`, the backend allocates a deployment ID and returns one image tag for every container (a freshly minted registry tag for each `[build]` container — shared registry, distinct tags — and an immutable digest-pinned reference for each pre-built `image = ...` container). The credentials it returns are scoped to cover every container's tag, and the CLI builds and pushes only the `[build]` containers in turn (re-minting credentials per container so a long build can't outlast the token). Containers with a pre-built `image` are not rebuilt; their reference is resolved to an immutable `repo@sha256:…` digest so the running image can't drift if the upstream tag is later re-pushed.

Notes:

- The top-level `[build]` and `[deploy]` tables are **per-field defaults** for every container. `[build]` merges field-by-field under each container's own `[build]` (so a container with `image` ignores it); each `[deploy]` field (`replicas`, `cpu`, `memory`, `health_check`) falls back to the top-level value when the container omits it. A single-container project — top-level `[build]`/`[deploy]` with no `[containers]` — is internally just one container named `app`.
- A container's resource settings live in `[containers.<name>.deploy]`, mirroring the top-level `[deploy]` table exactly (same fields, same syntax).
- Containers without `port` get no `Service` and no HTTP probes — exactly what you want for workers / batch jobs.
- HTTP probes are **disabled by default**. Set a `[containers.<name>.deploy].health_check` block to enable them (with a path and optional timing), or `health_check = false` to explicitly mark them disabled. A `health_check` default set at the top level only applies to containers that have a `port`; port-less workers never get a probe. Containers with a `port` but no `health_check` get a `Service` but no probe.
- Routes are matched longest-prefix-first by the ingress, so `/api` shadows `/` correctly. If `[routes]` is omitted and exactly one container has a `port`, Rise synthesises `/` → that container.
- Platform and environment **replica limits apply to the deployment's total** replicas summed across all containers, not per container — e.g. with a cap of 10, `frontend = 4` + `backend = 3` + `worker = 4` (sum 11) is rejected. CPU and memory limits, by contrast, are enforced per container.
- Every container receives a `RISE_CONTAINER` env var set to its own name (e.g. `"frontend"`, `"api"`).
- Each container's `PORT` env var is set to **that container's own `port`** (e.g. `frontend` gets `PORT=8080`, `backend` gets `PORT=9090`) — not a single deployment-wide value. Containers without a `port` (workers) keep whatever deployment-wide `PORT` was set, if any.
- When a deployment has two or more containers, each is also given a `RISE_CONTAINER_HOST__<NAME>` env var for every sibling that exposes a `port` (including a port-having container that isn't routed, such as a database) — pointing at that sibling's in-cluster Service. See [Environment Variables](../environment-variables#auto-injected-variables).

To run a multi-container project locally, use `rise compose up` (or `rise run --container <name>` for a single container). See [Local Development](../local-development#multi-container-projects).

## CPU & Memory

The `cpu` and `memory` fields in any `[deploy]` table (top-level or per-container) accept two forms:

| Form | Example | Effect |
|---|---|---|
| Fixed | `cpu = "256m"`, `memory = "512Mi"` | Sets the Kubernetes request **and** limit to the same value. |
| Range | `cpu = "128m-1"`, `memory = "256Mi-1Gi"` | `request-limit` — the first value is the request, the second the limit. |

CPU is expressed in cores (`1`, `2.5`) or millicores (`500m`); memory in bytes or binary-suffixed units (`512Mi`, `1Gi`). The request may not exceed the limit, and both are validated against the allowed range for the target environment/platform: `min ≤ request ≤ limit ≤ max`. A limit above the platform/environment maximum, or a request below its minimum, is rejected at deploy time with a clear message. Ranges let you reserve a small guaranteed amount (the request) while allowing bursts up to the limit.

## CI/CD Deployments

For automated deployments from CI/CD pipelines, use service accounts with OIDC workload identity. See [Service Accounts](../service-accounts) for setup instructions and examples for GitLab CI and GitHub Actions.
