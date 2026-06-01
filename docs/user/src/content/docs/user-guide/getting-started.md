---
title: "Getting Started"
---

This guide walks you through everything you need to deploy your first application with Rise, from installation to a running deployment.

## Prerequisites

- **Docker or Podman** — required for building container images
  - If using Podman Desktop behind a corporate proxy (Zscaler, Cloudflare), you may need to configure SSL certificates. See [SSL & Proxy Configuration](../ssl-proxy).
- **Rise CLI** — see installation instructions below

**Optional build tools** (only needed if you use the corresponding build backend):

- **pack CLI** — for Cloud Native Buildpacks builds. [Install docs](https://buildpacks.io/docs/for-platform-operators/how-to/integrate-ci/pack/). With mise: `mise use -g ubi:buildpacks/pack`
- **railpack CLI** — for Railway Railpacks builds. With mise: `mise use -g ubi:railwayapp/railpack`

## Installing the Rise CLI

**With mise (preferred)** — downloads a pre-built binary from GitHub releases, no Rust toolchain required:

```bash
mise use -g ubi:rise-deploy/rise
```

**With cargo** — builds from source (requires Rust):

```bash
cargo install rise-deploy
```

Your platform team may also distribute the `rise` binary directly — check your internal tooling documentation.

## Logging In

```bash
rise login --url https://rise.example.com
```

This opens your browser to complete OAuth2 authentication. After login, the CLI stores your token locally along with the URL, so subsequent commands don't need `--url`.

If `RISE_URL` is already set in your environment (e.g. via your shell profile or a `.envrc`), you can omit `--url`:

```bash
rise login
```

You can also set these via environment variables:

- `RISE_URL` — default backend URL
- `RISE_TOKEN` — authentication token (useful for CI/CD; see [Authentication](../authentication))

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

See [Project Configuration](../configuration) for all options.

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

See [Deployments](../deployments) for the full lifecycle and [Environments](../environments) for URL routing, variable scoping, and more.

## Environment Variables

Set runtime environment variables for your project:

```bash
rise env set -p my-app DATABASE_URL postgres://db.example.com/mydb
rise env set -p my-app API_KEY s3cret --secret
```

List current variables:

```bash
rise env list -p my-app
# Or with rise.toml: rise env list
```

Import from a `.env` file:

```bash
rise env import -p my-app .env
```

Rise also auto-injects variables like `PORT`, `RISE_ISSUER`, `RISE_APP_URL`, and `RISE_APP_URLS` into every deployment.

See [Environment Variables](../environment-variables) for secrets, protected secrets, and build-time vs runtime details.

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

> **Note:** Your Rise deployment may restrict team creation to administrators. If `rise team create` returns a permission error, contact your platform team to create a team or grant you the necessary permissions.

## Custom Domains

Add a custom domain to your project:

```bash
rise domain add my-app example.com
```

Configure a DNS CNAME record pointing to your Rise instance, and Rise handles TLS.

> **Note:** Custom domain support depends on how your Rise deployment is configured. Contact your platform team if the command is unavailable or if you need a domain provisioned.

See [Custom Domains](../custom-domains) for details.

## Local Development

**Run in a container** (builds the image, then runs it with project env vars injected):

```bash
rise run --project my-app --http-port 3000
```

**Run your app directly** with Rise env vars exported to your shell (no image build needed):

```bash
rise env export -p my-app > .env.rise
# then load with your preferred tool, e.g.:
export $(cat .env.rise | xargs)
# or: direnv, dotenv, etc.
```

`rise env export` fetches the resolved set of non-secret environment variables Rise would inject into a deployment, letting you run your app natively without Docker. This is useful when your local dev workflow builds and runs the app directly (e.g. `cargo run`, `npm run dev`).

Note that variables sourced from extensions (e.g., database credentials from the RDS extension) are not included — use a local database for development instead.

See [Local Development](../local-development) for port configuration and runtime overrides.

## CI/CD

For automated deployments from CI/CD pipelines, use service accounts with OIDC workload identity:

```bash
# Production: only protected branches/tags can deploy to production
rise sa create -p my-app \
  --issuer https://gitlab.com \
  --claim aud=https://rise.example.net \
  --claim project_path=myorg/my-app \
  --claim ref_protected=true

# Previews: any branch can deploy, but only to the staging environment
rise sa create -p my-app \
  --issuer https://gitlab.com \
  --claim aud=https://rise.example.net \
  --claim project_path=myorg/my-app
```

The CI pipeline authenticates with a short-lived OIDC token — no long-lived secrets needed.

Service accounts can be restricted to specific environments (e.g., limit the preview SA to `staging` so branch deployments never affect the production URL). This separation is strongly recommended: production deployments go to the `production` environment, while merge-request previews deploy to a `staging` environment with its own URL and scoped variables. See [CI/CD Setup](../ci-cd) for the full recommended setup.

See [Service Accounts](../service-accounts) for GitLab CI and GitHub Actions examples.

## Next Steps

- **[Project Configuration](../configuration)** — `rise.toml` format, build config, precedence rules
- **[Deployments](../deployments)** — lifecycle, groups, rollback, logs
- **[Environments](../environments)** — named deployment targets (production, staging, dev)
- **[Building Images](../builds)** — Docker, Pack, Railpack build backends
- **[CI/CD Setup](../ci-cd)** — recommended production + preview SA pattern
- **[Environment Variables](../environment-variables)** — secrets, imports, auto-injected vars
- **[Custom Domains](../custom-domains)** — DNS setup, primary domain
- **[Local Development](../local-development)** — `rise run`, port config
- **[Authentication](../authentication)** — login, service accounts, app users
- **[SSL & Proxy Configuration](../ssl-proxy)** — corporate proxy and certificate handling
- **[CLI Reference](../cli-reference)** — complete command table
- **[Troubleshooting](../troubleshooting)** — common issues and solutions
