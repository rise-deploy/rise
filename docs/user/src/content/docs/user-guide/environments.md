---
title: "Environments"
---

# Environments

Environments give semantic names (like "production", "staging", "dev") to deployment targets, with URL routing, variable scoping, and access control.

Deployment groups are just labels — typically reflecting the source of a deployment (e.g., the Git branch name). Environments layer on top of groups to control which deployments receive production traffic, get environment-specific URLs, and use scoped variables. The environment marked as **production** determines which deployments are served at the project's main URL.

## Default Setup

Every new project starts with a single **production** environment mapped to the `default` deployment group. This environment is both the default (fallback for deployments without an explicit environment) and the production environment (gets the production URL).

```bash
rise environment list
```

```
╭────────────┬───────────────┬─────────┬────────────┬───────╮
│ NAME       │ PRIMARY GROUP │ DEFAULT │ PRODUCTION │ COLOR │
├────────────┼───────────────┼─────────┼────────────┼───────┤
│ production │ default       │ yes     │ yes        │ green │
╰────────────┴───────────────┴─────────┴────────────┴───────╯
```

## Creating Environments

```bash
rise environment create staging -p my-app --group staging --color blue
rise environment create dev -p my-app --group dev --color yellow
```

| Flag | Description |
|------|-------------|
| `--group`, `-g` | Primary deployment group for this environment |
| `--default` | Set as the default environment (one per project) |
| `--production` | Set as the production environment (one per project) |
| `--color` | Badge color: `green`, `blue`, `yellow`, `red`, `purple`, `orange`, `gray` (default: `green`) |

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

# Transfer the default flag
rise environment update staging --default true

# Transfer the production flag
rise environment update staging --production true

# Change color
rise environment update staging --color purple
```

Setting `--default true` or `--production true` automatically transfers the flag from the environment that previously held it.

## Deleting Environments

```bash
rise environment delete dev
```

You cannot delete the default or production environment. Transfer the flag to another environment first:

```bash
rise environment update staging --default true
rise environment delete production
```

## Deploying to Environments

Use the `-E` flag on `rise deploy`:

```bash
rise deploy -E staging
```

The environment and deployment group are resolved together:

| `-E` (environment) | `--group` | Result |
|----|---------|--------|
| set | set | Uses both as specified |
| set | omitted | Uses the environment's primary deployment group |
| omitted | set | Finds the environment whose primary group matches; falls back to default environment |
| omitted | omitted | Uses the default environment and its primary group (or `default` group) |

If an environment is specified but has no primary deployment group, you must also pass `--group`.

See [Deployments](deployments.md) for the full deployment lifecycle.

## URL Routing

The environment's **production** flag controls which deployments get the project's main URL. When a deployment is in an environment's primary deployment group, the controller creates an ingress with an environment-specific URL:

- **Production environment** → production URL (e.g., `my-app.apps.rise.dev`). Custom domains also apply to these deployments.
- **Non-production environments** → environment URL (e.g., `staging--my-app.preview.rise.dev`)

The deployment group name itself does not determine URL routing — only the environment flags do.

Non-production environment URLs require the operator to configure `environment_ingress_url_template` in the backend settings. Contact your Rise operator if environment URLs do not match the patterns used by your organization.

## Environment-Scoped Variables

Scope environment variables to a specific environment with the `-E` flag:

```bash
# Set a variable only for staging
rise env set DATABASE_URL postgres://staging-db/mydb -E staging

# List variables for staging (shows merged global + scoped)
rise env list my-app -E staging

# Get a scoped variable
rise env get my-app DATABASE_URL -E staging

# Delete a scoped variable
rise env delete my-app DATABASE_URL -E staging

# Import variables scoped to an environment
rise env import my-app .env.staging -E staging
```

When listing with `-E`, scoped variables override global variables with the same key. Without `-E`, only global variables are shown.

Environment-scoped variables can also be defined declaratively in `rise.toml` under `[environments.<name>.env]`. See [Environment Variables](environment-variables.md#per-environment-variables-in-risetoml) for details.

## Auto-Injected Variable

Rise injects `RISE_ENVIRONMENT` into every deployment that has an associated environment. The value is the environment name (e.g., `"production"`, `"staging"`).

See the full list of auto-injected variables in [Deployments](deployments.md#auto-injected-environment-variables) and [Environment Variables](environment-variables.md#auto-injected-variables).

## Kubernetes ServiceAccounts

Each environment gets a dedicated Kubernetes ServiceAccount named `env-{name}` (e.g., `env-staging`). This enables cloud IAM integrations like AWS IRSA or GCP Workload Identity, where different environments can assume different IAM roles.

By default, production environments use the namespace's `default` ServiceAccount to preserve existing IAM bindings. Your Rise operator can change this if your organization uses per-environment Kubernetes service accounts.

## Service Account Restrictions

Service accounts can optionally be restricted to deploy only to specific environments. When configured, the service account can only create deployments targeting one of its allowed environments.

This is managed through the web UI or API when creating or updating a service account. See [Authentication](authentication.md#service-accounts-workload-identity) for more on service accounts.
