---
title: "Deployments"
---

# Deployments

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

A deployment group is a label that identifies a set of related deployments. Only one deployment per group can be active at a time — when a new deployment in a group reaches `Healthy`, the previous one is `Superseded`. Group names typically reflect the source of the deployment (e.g., the Git branch or merge request it was deployed from).

```bash
# Deploy to the default group
rise deploy

# Deploy to a named group
rise deploy --group mr/123 --expire 7d
rise deploy --group feature/login
```

Group names must match `[a-z0-9][a-z0-9/-]*[a-z0-9]` (no consecutive hyphens `--`, normalized length max 63 characters). The default group is named `default`.

### Environments

[Environments](environments.md) give semantic meaning to deployment groups. Each environment has a primary deployment group and controls URL routing, variable scoping, and access. The environment marked as **production** determines which deployments receive production traffic and the project's main URL — not the deployment group name itself.

New projects start with a `production` environment mapped to the `default` group. You can create additional environments for staging, dev, etc. See [Environments](environments.md) for details.

### Auto-Expiration

Set deployments to expire automatically:

```bash
rise deploy --group mr/123 --expire 7d   # Days
rise deploy --group preview --expire 24h  # Hours
rise deploy --group temp --expire 1w      # Weeks
```

Expired deployments are automatically cleaned up.

## Monitoring Deployments

### Following a Deployment

`rise deploy` follows automatically. You can also follow an existing deployment:

```bash
rise deployment show my-app:20241205-1234 --follow
rise d s my-app:latest --follow --timeout 10m
```

### Listing Deployments

```bash
rise deployment list my-app
rise d ls my-app --group staging
```

### Viewing Deployment Details

```bash
rise deployment show my-app:20241205-1234
rise d s my-app:latest
```

### Deployment Logs

```bash
# Show recent logs
rise deployment logs my-app 20241205-1234

# Follow logs in real-time
rise deployment logs my-app 20241205-1234 --follow

# Show last 100 lines
rise deployment logs my-app 20241205-1234 --tail 100

# Show logs since a duration ago
rise deployment logs my-app 20241205-1234 --since 5m

# Show timestamps
rise deployment logs my-app 20241205-1234 --timestamps
```

Note that logs are currently only available for active deployments (`Healthy` or `Unhealthy`) and can not be accessed
for past deployments.

## Rollback

Rollback creates a new deployment using the same image as a previous one:

```bash
rise deployment rollback my-app:20241205-1234
```

This fetches the target deployment's image digest and creates a new deployment with it. The original deployment is not modified.

## Stopping Deployments

Stop all deployments in a group:

```bash
rise deployment stop my-app --group default
rise d stop my-app --group mr/123
```

Stopped deployments remain in the database for rollback purposes.

## Auto-Injected Environment Variables

Rise automatically injects these variables into every deployment:

| Variable | Description | Example |
|----------|-------------|---------|
| `PORT` | HTTP port the container should listen on | `8080` |
| `RISE_ISSUER` | Rise server URL and JWT issuer | `https://rise.example.com` |
| `RISE_APP_URL` | Canonical URL where your app is accessible | `https://myapp.example.com` |
| `RISE_APP_URLS` | JSON array of all URLs where your app is accessible | `["https://myapp.app.example.com", "https://myapp.example.com"]` |
| `RISE_ENVIRONMENT` | Environment name (if the deployment has an associated environment) | `staging` |

`PORT` defaults to 8080 and can be overridden per-deployment with `--http-port`, or set permanently with `rise env set`. `RISE_APP_URL` is your primary custom domain if set, otherwise the default project URL.

For JWT validation using `RISE_ISSUER`, see [Authentication for Applications](authentication-for-apps.md).

## CI/CD Deployments

For automated deployments from CI/CD pipelines, use service accounts with OIDC workload identity. See [Authentication](authentication.md#service-accounts-workload-identity) for setup instructions and examples for GitLab CI and GitHub Actions.
