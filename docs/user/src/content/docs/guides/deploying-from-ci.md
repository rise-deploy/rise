---
title: "Deploying from CI"
description: "Set up Service Accounts and CI pipelines for automatic deploys, including preview environments."
---

## When to use this

You want your CI pipeline (GitHub Actions, GitLab CI, etc.) to push deployments to Rise automatically — without storing a long-lived token. This guide covers inbound OIDC federation via Service Accounts, the recommended exchange flow, and the preview-environments pattern.

For the reference material behind this guide, see [Service Accounts](../user-guide/service-accounts.md) and [CI/CD Setup](../user-guide/ci-cd.md).

## The model: OIDC federation, no stored secrets

Your CI provider mints a short-lived OIDC JWT on every job run. Rise validates that JWT against a Service Account's configured issuer and claims, then exchanges it for a short-lived Rise access token. **No long-lived secret is stored anywhere** — if a token leaks it expires in minutes, and you can revoke a Service Account instantly.

This is strictly better than storing any long-lived deployment credential because:

- There is no secret to rotate or accidentally commit.
- Each deployment is attributable to a specific CI job identity (issuer + claims), audited by Rise.
- You can restrict which environments a Service Account may deploy to, enforced server-side.

## Decision: how to provide a token

The CLI resolves a token in this precedence order (verified in `src/cli/token_source.rs`):

| Priority | Source | What it provides | Exchange? |
|----------|--------|------------------|-----------|
| 1 | `RISE_TOKEN` env var | An OIDC JWT minted by your CI (e.g. GitLab `id_tokens`) | Yes — set `RISE_IDENTITY` |
| 2 | `RISE_TOKEN_COMMAND` env var | A shell command that outputs an OIDC JWT (generic escape hatch) | Yes — set `RISE_IDENTITY` |
| 3 | GitHub Actions OIDC (auto-detected) | OIDC JWT minted on demand by GHA | Yes — set `RISE_IDENTITY` |
| 4 | Stored login (`rise login`) | A Rise access token from interactive login | No — used directly |

When `RISE_IDENTITY` is set, the resolved token is **exchanged** for a Rise access token bound to that Service Account (RFC 8693, `POST /api/v1/auth/token`). For CI, always set `RISE_IDENTITY` to the service-account email so the OIDC token is exchanged — an OIDC JWT is not a valid Rise access token on its own. The only token used directly (without exchange) is the interactive login token from `rise login`.

| Your CI provider | Token source | Exchange? | Env vars to set |
|-----------------|-------------|-----------|-----------------|
| **GitHub Actions** | GHA OIDC auto-detect (priority 3) | Yes — `RISE_IDENTITY` | `RISE_URL`, `RISE_GHA_AUDIENCE`, `RISE_IDENTITY` |
| **GitLab CI** | `RISE_TOKEN` via `id_tokens` | Yes — `RISE_IDENTITY` | `RISE_TOKEN` (auto-set by runner), `RISE_URL`, `RISE_IDENTITY` |
| **Other (generic)** | `RISE_TOKEN_COMMAND` | Yes — `RISE_IDENTITY` | `RISE_TOKEN_COMMAND`, `RISE_URL`, `RISE_IDENTITY` |

## Step-by-step: GitHub Actions

### 1. Create a Service Account locally

```bash
rise sa create -p my-app \
  --issuer https://token.actions.githubusercontent.com \
  --claim aud=https://rise.example.net \
  --claim repository=myorg/my-app \
  --claim ref=refs/heads/main
```

:::note[Verified requirements]
- `--issuer` for GitHub Actions is `https://token.actions.githubusercontent.com`.
- The `aud` claim is **mandatory** (validated both client-side and server-side).
- At least **one additional claim** beyond `aud` is required.
- Claims support glob wildcards (`*`). For example, `--claim ref=refs/heads/*` matches any branch.
:::

### 2. Note the SA identity email

The output shows a synthetic email like `my-app+0@sa.rise.local`. This is the value you'll set as `RISE_IDENTITY` in CI. You can also find it with:

```bash
rise sa list -p my-app
```

### 3. Create the GHA workflow

```yaml
name: Deploy

on:
  push:
    branches: [main]

permissions:
  id-token: write   # required to mint OIDC tokens
  contents: read

jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - uses: jdx/mise-action@v4

      - name: Install Rise CLI
        run: mise use -g github:rise-deploy/rise

      - name: Deploy
        env:
          RISE_URL: https://rise.example.net
          RISE_GHA_AUDIENCE: https://rise.example.net
          RISE_IDENTITY: my-app+0@sa.rise.local
        run: rise deploy -p my-app
```

The CLI auto-detects the GitHub Actions environment (from `ACTIONS_ID_TOKEN_REQUEST_URL` + `ACTIONS_ID_TOKEN_REQUEST_TOKEN`), mints OIDC tokens on demand, and exchanges each for a Rise access token bound to the Service Account. Tokens are re-minted automatically as they near expiry, so long builds are safe.

## Preview environments

Combine `--environment` and `--group` to create isolated preview deployments. Each group gets its own active deployment; a new deployment in the same group supersedes the previous one. Use `--expire` for automatic cleanup.

| Flag | Purpose | Example |
|------|---------|---------|
| `-E` / `--environment` | Shared env for variable scoping / URL routing | `-E staging` |
| `-g` / `--group` | Unique isolated deployment slot | `--group pr/42` |
| `--expire` | Auto-delete after a duration | `--expire 7d` |

### Per-PR preview job (GitHub Actions)

```yaml
name: Preview Deploy

on:
  pull_request:

permissions:
  id-token: write
  contents: read

jobs:
  preview:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - uses: jdx/mise-action@v4

      - name: Install Rise CLI
        run: mise use -g github:rise-deploy/rise

      - name: Deploy preview
        env:
          RISE_URL: https://rise.example.net
          RISE_GHA_AUDIENCE: https://rise.example.net
          RISE_IDENTITY: my-app+1@sa.rise.local   # preview SA (restricted to staging)
        run: |
          rise deploy -p my-app \
            -E staging \
            --group pr/${{ github.event.pull_request.number }} \
            --expire 7d
```

The `--job-url` and `--pull-request-url` metadata are auto-detected from GitHub Actions environment variables — no manual flags needed.

### Recommended: two Service Accounts

| Service Account | Allowed environments | CI trigger | Purpose |
|-----------------|---------------------|------------|---------|
| `my-app+0@sa.rise.local` | `production` | `main` branch | Production deploys |
| `my-app+1@sa.rise.local` | `staging` | All PRs | Preview deploys |

Environment restrictions are enforced server-side — a misconfigured pipeline using the preview SA **cannot** deploy to production. See [CI/CD Setup](../user-guide/ci-cd.md) for the recommended two-SA pattern.

## GitLab CI (brief)

GitLab CI injects the OIDC token directly into `RISE_TOKEN` via the `id_tokens` keyword:

```yaml
deploy-preview:
  except: [tags]
  id_tokens:
    RISE_TOKEN:
      aud: https://rise.example.net
  variables:
    RISE_URL: https://rise.example.net
    RISE_IDENTITY: my-app+1@sa.rise.local
  script:
    - rise deploy -p my-app -E staging --group mr/$CI_MERGE_REQUEST_IID --expire 7d
```

The `--job-url` and `--pull-request-url` metadata are auto-detected from `CI_JOB_URL` and `CI_MERGE_REQUEST_URL`.

## Common mistakes

- **Forgetting the `aud` claim** — `rise sa create` will reject the command. The `aud` value must match `RISE_GHA_AUDIENCE` (or the GitLab `id_tokens.aud`) in your CI config.
- **Manually fetching an OIDC token into `RISE_TOKEN` on GitHub Actions** — the CLI mints OIDC tokens itself via auto-detection (just set `id-token: write` and `RISE_GHA_AUDIENCE`). Use `RISE_TOKEN` only for CI providers like GitLab that inject the token via `id_tokens`.
- **Forgetting `permissions: id-token: write`** in your GitHub Actions workflow — the CLI cannot mint OIDC tokens without it.
- **Not setting `RISE_IDENTITY` in CI** — an OIDC token from CI is not a valid Rise access token on its own; it must be exchanged. Always set `RISE_IDENTITY` to the service-account email in CI jobs.
- **Deploying to production from a preview job** — prevent this by restricting the preview SA to the `staging` environment only (configured via the web UI or API after creation).
