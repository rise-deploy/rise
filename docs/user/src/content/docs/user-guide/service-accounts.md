---
title: "Service Accounts"
---

Service accounts let CI/CD pipelines authenticate with Rise using short-lived OIDC tokens. Set `RISE_IDENTITY` to the service account's email so the CLI exchanges the CI provider's JWT for a short-lived Rise access token.

Rise validates the provider token against the service account's issuer and claim configuration during exchange. Subsequent API requests use the Rise-issued token with that service account's project permissions.

:::caution[Deprecated: direct external CI tokens]
Sending a CI provider's OIDC JWT directly to Rise, without exchanging it for a Rise access token, is deprecated. With `RISE_IDENTITY` unset, the CLI passes its selected token through unchanged; for an external CI token, this uses the deprecated per-request service-account authentication path.

This path still works when the operator enables `auth.allow_raw_external_tokens`, whose current default is `true`. Disabling it by default is **planned for 0.25.0**; the path is not removed today. Set `RISE_IDENTITY` in CI jobs to use token exchange.

Interactive `rise login` and Rise-issued tokens used directly without `RISE_IDENTITY` are unaffected. See the [deprecation register](https://rise-deploy.github.io/rise/operator/changelog/deprecations/) for status and release history.
:::

For the recommended two-SA setup (production + preview with environment restrictions), see [CI/CD Setup](../ci-cd).

## Quick Start

**GitLab CI:**

```bash
rise sa create -p my-project \
  --issuer https://gitlab.com \
  --claim aud=https://rise.example.net \
  --claim project_path=myorg/myrepo \
  --claim ref_protected=true
```

Add to `.gitlab-ci.yml`:

Use the service account's `Email` from `rise sa create` or `rise sa list` as `RISE_IDENTITY`. The addresses below are examples; use the email for the account you created.

```yaml
deploy:
  stage: deploy
  variables:
    RISE_URL: https://rise.example.net
    RISE_IDENTITY: my-project+0@sa.rise.local
  id_tokens:
    RISE_TOKEN:
      aud: https://rise.example.net
  script:
    - rise deploy --image $CI_REGISTRY_IMAGE:$CI_COMMIT_TAG
  only:
    - tags
```

**GitHub Actions:**

```bash
rise sa create -p my-app \
  --issuer https://token.actions.githubusercontent.com \
  --claim aud=https://rise.example.net \
  --claim repository=myorg/my-app
```

Add to `.github/workflows/deploy.yml`:

```yaml
name: Deploy
on:
  push:
    branches: [develop]

permissions:
  id-token: write
  contents: read

jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Deploy
        env:
          RISE_URL: https://rise.example.net
          RISE_IDENTITY: my-app+0@sa.rise.local
          # The CLI auto-detects GitHub Actions and mints an OIDC token on demand,
          # re-minting after about 2/3 of its lifetime so long builds can't outlast it.
          # Set the audience to the Rise server URL (`https://rise.example.net`).
          RISE_GHA_AUDIENCE: https://rise.example.net
        run: rise deploy --image ghcr.io/myorg/my-app:$GITHUB_SHA
```

Because GitHub Actions OIDC auto-detection ranks above the stored login token (see [Token source precedence](./cli-reference#token-source-precedence)), a workflow with `id-token: write` and no `RISE_TOKEN`/`RISE_TOKEN_COMMAND` set will **error if `RISE_GHA_AUDIENCE` is unset** rather than fall back to another token. Setting `RISE_TOKEN` overrides this.

## Exchanging the OIDC token (`RISE_IDENTITY`)

To **exchange** the external OIDC token for a short-lived, Rise-issued access
token, set `RISE_IDENTITY` to the service
account's email (shown by `rise sa list` / `rise sa show`, of the form
`<project>+<n>@sa.rise.local`):

```yaml
env:
  RISE_URL: https://rise.example.net
  RISE_GHA_AUDIENCE: https://rise.example.net
  RISE_IDENTITY: my-app+0@sa.rise.local   # exchange the OIDC token for this SA
```

This works with any token source (`RISE_TOKEN`, `RISE_TOKEN_COMMAND`, or GitHub
Actions OIDC) — it's the explicit, channel-agnostic signal that you want an
exchange. `RISE_IDENTITY` only *selects* the identity; your OIDC token still has
to prove (issuer + claims) that it may assume that service account. The resulting
access token carries the resolved principal, so it works across every command
(including `rise project list`) without a per-command `--project`.

## Creating Service Accounts

```bash
rise sa create -p <project> \
  --issuer <issuer-url> \
  --claim aud=https://rise.example.net \
  --claim <key>=<value>
```

Requirements: an `aud` claim and at least one additional claim to narrow authorization.

## Common Use Cases

**Protected branches only (production):**

```bash
rise sa create -p my-app \
  --issuer https://gitlab.com \
  --claim aud=https://rise.example.net \
  --claim project_path=myorg/app \
  --claim ref_protected=true
```

**Specific branch:**

```bash
rise sa create -p my-app \
  --issuer https://gitlab.com \
  --claim aud=https://rise.example.net \
  --claim project_path=myorg/app \
  --claim ref=refs/heads/staging
```

**Deploy from tags (releases):**

```bash
rise sa create -p my-app \
  --issuer https://gitlab.com \
  --claim aud=https://rise.example.net \
  --claim project_path=myorg/app \
  --claim ref_type=tag
```

## Available Claims

**GitLab CI**: `project_path`, `ref`, `ref_type`, `ref_protected`, `environment`, `pipeline_source` — [Docs](https://docs.gitlab.com/ee/ci/secrets/id_token_authentication.html)

**GitHub Actions**: `repository`, `ref`, `workflow`, `environment`, `actor` — [Docs](https://docs.github.com/en/actions/deployment/security-hardening-your-deployments/about-security-hardening-with-openid-connect)

## Wildcard Patterns

Claims support glob-style `*` wildcards:

```bash
# Match all merge request environments
rise sa create -p my-app \
  --issuer https://gitlab.com \
  --claim aud=https://rise.example.net \
  --claim project_path=myorg/myrepo \
  --claim environment=app-mr/*

# Match all feature branches
--claim ref=refs/heads/feature/*
```

`*` matches any sequence of characters including `/` and `-`. Design patterns carefully — `app*` matches `app`, `app-staging`, and `application`.

## Environment Restrictions

Service accounts can be restricted to deploy only to specific environments. When configured, any attempt to deploy to a non-allowed environment is rejected server-side — even if the pipeline is misconfigured.

Restrictions are managed through the web UI or API when creating or updating a service account. See [CI/CD Setup](../ci-cd) for the recommended pattern using two service accounts with separate environment restrictions.

## Managing Service Accounts

```bash
rise sa list -p <project>
rise sa show -p <project> <service-account-id>
rise sa delete -p <project> <service-account-id>
```

Service accounts can create, view, list, stop, and roll back deployments. They cannot manage projects, teams, or other service accounts.

## Local Testing

To test service account authentication locally, use [`@oidc.pub/cli`](https://oidc.pub) to run a dev OIDC issuer:

```bash
npx @oidc.pub/cli login
npx @oidc.pub/cli dev issuer --service <id>
```

The dev issuer runs locally but publishes its JWKS to a public URL (`https://<id>.oidc.pub`) so Rise can verify tokens without reaching your machine.

Create a service account using the public issuer URL:

```bash
rise sa create -p my-project \
  --issuer https://<id>.oidc.pub \
  --claim aud=https://rise.example.net \
  --claim sub=dev
```

Mint a token and use it:

```bash
export RISE_URL=https://rise.example.net
export RISE_IDENTITY=my-project+0@sa.rise.local
export RISE_TOKEN=$(curl -s http://localhost:9229/token \
  -d '{"aud": "https://rise.example.net", "sub": "dev"}' | jq -r .access_token)
rise deploy --image my-image:latest
```
