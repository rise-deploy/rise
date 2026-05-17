---
title: "Getting Started"
---

# Getting Started

This guide walks you through everything you need to deploy your first application with Rise, from installation to a running deployment.

## Prerequisites

- **Docker or Podman** — required for building container images
  - If using Podman Desktop behind a corporate proxy (Zscaler, Cloudflare), you may need to configure SSL certificates. See [SSL & Proxy Configuration](ssl-proxy.md).
- **Rise CLI** — the `rise` binary (obtain from your platform team or build from source with `cargo build --bin rise`)

**Optional build tools** (only needed if you use the corresponding build backend):

- **pack CLI** — for Cloud Native Buildpacks builds. [Install docs](https://buildpacks.io/docs/for-platform-operators/how-to/integrate-ci/pack/). With mise: `mise use -g ubi:buildpacks/pack`
- **railpack CLI** — for Railway Railpacks builds. With mise: `mise use -g ubi:railwayapp/railpack`

## Logging In

```bash
rise login
```

This opens your browser to complete OAuth2 authentication. After login, the CLI stores your token locally.

To connect to a specific Rise instance:

```bash
rise login --url https://rise.example.com
```

You can also set these via environment variables:

- `RISE_URL` — default backend URL
- `RISE_TOKEN` — authentication token (useful for CI/CD; see [Authentication](authentication.md))

## Creating a Project

A project represents a deployable application. Create one with:

```bash
rise project create my-app
```

This creates the project on the backend and writes a `rise.toml` file in your current directory. If a `rise.toml` already exists, only the backend project is created.

You can set the access class and owner:

```bash
rise project create my-app --access-class private --owner team:backend
```

The `rise.toml` file ties your local directory to the project, so subsequent commands don't need `-p my-app`:

```toml
[project]
name = "my-app"
```

See [Project Configuration](configuration.md) for all options.

## Deploying

The primary command for deploying is `rise deploy`:

```bash
rise deploy
```

This builds a container image from your application, pushes it to the registry, and deploys it. Rise auto-detects the build method based on your project files (Dockerfile, Containerfile, or falls back to buildpacks).

After creating a deployment, Rise automatically follows its progress until it reaches a terminal state.

### Deploying a Pre-Built Image

Skip the build step entirely:

```bash
rise deploy --image nginx:latest --http-port 80
```

### Deploying to an Environment

Deploy to a specific environment:

```bash
rise deploy -E staging
```

Or deploy to a custom group (e.g., for merge request previews):

```bash
rise deploy --group mr/123 --expire 7d
```

See [Deployments](deployments.md) for the full lifecycle and [Environments](environments.md) for URL routing, variable scoping, and more.

## Environment Variables

Set runtime environment variables for your project:

```bash
rise env set my-app DATABASE_URL postgres://db.example.com/mydb
rise env set my-app API_KEY s3cret --secret
```

List current variables:

```bash
rise env list my-app
# Or with rise.toml: rise env list
```

Import from a `.env` file:

```bash
rise env import my-app .env
```

Rise also auto-injects variables like `PORT`, `RISE_ISSUER`, `RISE_APP_URL`, and `RISE_APP_URLS` into every deployment.

See [Environment Variables](environment-variables.md) for secrets, protected secrets, and build-time vs runtime details.

## Teams

Create teams and transfer project ownership:

```bash
rise team create backend-team --owners alice@example.com --members bob@example.com
rise project update my-app --owner team:backend-team
```

List teams:

```bash
rise team list
```

## Custom Domains

Add a custom domain to your project:

```bash
rise domain add my-app example.com
```

Configure a DNS CNAME record pointing to your Rise instance, and Rise handles TLS.

See [Custom Domains](custom-domains.md) for details.

## Local Development

Build and run your application locally with project environment variables:

```bash
rise run --project my-app --http-port 3000
```

This builds the image, loads non-secret env vars from the project, and runs the container interactively.

See [Local Development](local-development.md) for port configuration and runtime overrides.

## CI/CD

For automated deployments from CI/CD pipelines, use service accounts with OIDC workload identity:

```bash
# Create a service account for GitLab CI
rise sa create my-app \
  --issuer https://gitlab.com \
  --claim aud=rise-project-my-app \
  --claim project_path=myorg/my-app \
  --claim ref_protected=true
```

The CI pipeline authenticates with a short-lived OIDC token — no long-lived secrets needed.

See [Authentication](authentication.md#service-accounts-workload-identity) for GitLab CI and GitHub Actions examples.

## Next Steps

- **[Project Configuration](configuration.md)** — `rise.toml` format, build config, precedence rules
- **[Deployments](deployments.md)** — lifecycle, groups, rollback, logs
- **[Environments](environments.md)** — named deployment targets (production, staging, dev)
- **[Building Images](builds.md)** — Docker, Pack, Railpack backends
- **[Environment Variables](environment-variables.md)** — secrets, imports, auto-injected vars
- **[Custom Domains](custom-domains.md)** — DNS setup, primary domain
- **[Local Development](local-development.md)** — `rise run`, port config
- **[Authentication](authentication.md)** — login, service accounts, app users
- **[SSL & Proxy Configuration](ssl-proxy.md)** — corporate proxy and certificate handling
- **[CLI Reference](cli-reference.md)** — complete command table
- **[Troubleshooting](troubleshooting.md)** — common issues and solutions
