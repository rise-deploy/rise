---
title: "Environments"
---

Environments give semantic names (like "production", "staging", "dev") to deployment targets, with URL routing, variable scoping, and access control.

Deployment groups are just labels — typically reflecting the source of a deployment (e.g., the Git branch name). Environments layer on top of groups to control which deployments receive production traffic, get environment-specific URLs, and use scoped variables. The environment marked as **production** determines which deployments are served at the project's main URL.

## Default Setup

Every new project starts with a single **production** environment mapped to the `default` deployment group. This environment is both the production environment (gets the production URL) and the only environment, so deployments without `-E` will automatically use it.

```bash
rise environment list
```

```
╭────────────┬───────────────┬────────────┬───────┬────────────────╮
│ NAME       │ PRIMARY GROUP │ PRODUCTION │ COLOR │ MAX EXPIRATION │
├────────────┼───────────────┼────────────┼───────┼────────────────┤
│ production │ default       │ yes        │ green │ -              │
╰────────────┴───────────────┴────────────┴───────┴────────────────╯
```

## Creating Environments

```bash
rise environment create staging -p my-app --group staging --color blue
rise environment create dev -p my-app --group dev --color yellow
```

| Flag | Description |
|------|-------------|
| `--group`, `-g` | Primary deployment group for this environment |
| `--production` | Set as the production environment (one per project) |
| `--color` | Badge color: `green`, `blue`, `yellow`, `red`, `purple`, `orange`, `gray` (default: `green`) |
| `--max-expiration` | Caps the lifetime of deployments into any non-primary group, e.g. `7d`, `12h`, `30m` (default: no cap) |

Names must be lowercase alphanumeric with hyphens, no consecutive `--` (same rules as deployment groups).

## Listing and Viewing

```bash
rise environment list -p my-app
rise environment show staging -p my-app
```

With a `rise.toml` in your directory, you can omit `-p`:

```bash
rise environment list
rise environment show staging
```

Aliases: `rise envs ls`, `rise envs s`.

## Updating Environments

```bash
# Rename
rise environment update staging --rename qa

# Change primary group
rise environment update staging --group staging-v2

# Transfer the production flag
rise environment update staging --production true

# Change color
rise environment update staging --color purple

# Set (or change) the max expiration for non-primary groups
rise environment update staging --max-expiration 7d

# Clear the max expiration
rise environment update staging --max-expiration ""
```

Setting `--production true` automatically transfers the flag from the environment that previously held it.

## Deleting Environments

```bash
rise environment delete dev
```

You cannot delete the production environment. Transfer the production flag to another environment first:

```bash
rise environment update staging --production true
rise environment delete production
```

## Deploying to Environments

Use the `-E` flag on `rise deploy`:

```bash
rise deploy -E staging
```

### Environment Resolution

When `-E` is omitted, Rise resolves the target environment in order:

1. **`rise.toml` default** — if a `rise.toml` is present and one of its `[environments.<name>]` sections has `default = true`, that environment name is sent to the server.
2. **Server auto-resolve** — if no environment is specified by the client, the server picks one based on how many environments the project has:
   - **0 environments**: deploys to the `default` group with no environment association.
   - **1 environment**: uses that environment and its primary group.
   - **2 or more environments**: returns an error — you must pass `-E` explicitly.

To set a local default in `rise.toml`:

```toml
[environments.staging]
default = true
```

### Resolution Table

The environment and deployment group are resolved together:

| `-E` (environment) | `--group` | Result |
|----|---------|--------|
| set | set | Uses both as specified |
| set | omitted | Uses the environment's primary deployment group |
| omitted | set | Finds the environment whose primary group matches; auto-resolves from project environments if no match |
| omitted | omitted | Auto-resolves from `rise.toml` default or server-side (see above) |

If an environment is specified but has no primary deployment group, you must also pass `--group`.

### Maximum Expiration for Non-Primary Groups

An environment's `--max-expiration` caps how long a deployment can live once it lands in a group other than that environment's primary deployment group — the preview deployments created by `--group mr/123`, for example. An environment with no primary group has no group exempt from the cap, so it applies to every deployment created into it.

A deployment created without `--expire` gets the max as its expiration. One created with a longer `--expire` is clamped down to the max; a shorter one is left as requested. Either way, when the cap changes what a deployment's `expires_at` would otherwise have been, that is recorded on the deployment's creation event alongside what was actually requested.

Changing `--max-expiration` only affects deployments created afterward — it does not reach back and change `expires_at` on deployments that already exist.

See [Deployments](../deployments) for the full deployment lifecycle.

## URL Routing

Each deployment gets a URL based on how it is associated with environments and groups:

| Deployment type | URL pattern | Example |
|----------------|-------------|---------|
| Production environment | Project's main URL (and any custom domain) | `my-app.apps.rise.example.com` |
| Non-production environment | Environment-specific URL | `staging--my-app.preview.rise.example.com` |
| No environment (group only) | Group-specific staging URL | `mr--123--my-app.preview.rise.example.com` |

The exact URL patterns depend on how your Rise platform is configured — contact your operator if the URLs don't match what you expect. Custom domains only apply to deployments in the production environment.

## Environment-Scoped Variables

Scope environment variables to a specific environment with the `-E` flag:

```bash
# Set a variable only for staging
rise env set DATABASE_URL postgres://staging-db/mydb --plain -E staging

# List variables for staging (shows merged global + scoped)
rise env list -p my-app -E staging

# Get a scoped variable
rise env get -p my-app DATABASE_URL -E staging

# Delete a scoped variable
rise env delete -p my-app DATABASE_URL -E staging

# Import variables scoped to an environment
rise env import -p my-app .env.staging -E staging
```

When listing with `-E`, scoped variables override global variables with the same key. Without `-E`, only global variables are shown.

Environment-scoped variables can also be defined declaratively in `rise.toml` under `[environments.<name>.env]`. See [Environment Variables](../environment-variables#per-environment-variables-in-risetoml) for details.

## Auto-Injected Variable

Rise injects `RISE_ENVIRONMENT` into every deployment that has an associated environment. The value is the environment name (e.g., `"production"`, `"staging"`).

See the full list of auto-injected variables in [Environment Variables](../environment-variables#auto-injected-variables).

## Kubernetes ServiceAccounts

Each environment gets a dedicated Kubernetes ServiceAccount named `env-{name}` (e.g., `env-staging`). This enables cloud IAM integrations like AWS IRSA or GCP Workload Identity, where different environments can assume different IAM roles.

By default, production environments use the namespace's `default` ServiceAccount to preserve existing IAM bindings. Your Rise operator can change this if your organization uses per-environment Kubernetes service accounts.

## Service Account Restrictions

Service accounts can optionally be restricted to deploy only to specific environments. When configured, the service account can only create deployments targeting one of its allowed environments.

This is managed through the web UI or API when creating or updating a service account. See [Service Accounts](../service-accounts) for more.

## Recommended CI/CD Setup

See [CI/CD Setup](../ci-cd) for a step-by-step guide to setting up separated production and staging deployments with environment-restricted service accounts.
