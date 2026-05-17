---
title: "Authentication"
---

# Authentication

Rise uses JWT tokens for user authentication, service accounts for CI/CD workload identity, and app users for controlling access to deployed applications.

## User Authentication

### Browser Flow (Default)

```bash
rise login
```

This starts a local HTTP server (ports 8765-8767), opens your browser to the OAuth2/OIDC provider, and exchanges the auth code for a Rise JWT token using PKCE.

Connect to a specific Rise instance:

```bash
rise login --url https://rise.example.com
```

### Token Storage

Tokens are stored in `~/.config/rise/config.json` (plain JSON).

### Environment Variables

- `RISE_URL` — default backend URL
- `RISE_TOKEN` — authentication token (bypasses interactive login)

### API Usage

Protected endpoints require `Authorization: Bearer <token>`. Missing or invalid tokens return 401.

## Service Accounts (Workload Identity)

CI/CD systems authenticate using OIDC JWT tokens from their identity provider. No long-lived secrets are needed.

**How it works:** CI generates JWT → Rise validates signature against OIDC issuer → matches claims against service account → grants project-scoped access.

### Quick Start

**GitLab CI:**

```bash
rise sa create my-project \
  --issuer https://gitlab.com \
  --claim aud=rise-project-my-project \
  --claim project_path=myorg/myrepo \
  --claim ref_protected=true
```

Add to `.gitlab-ci.yml`:

```yaml
deploy:
  stage: deploy
  id_tokens:
    RISE_TOKEN:
      aud: rise-project-my-project
  script:
    - rise deploy --image $CI_REGISTRY_IMAGE:$CI_COMMIT_TAG
  only:
    - tags
```

**GitHub Actions:**

```bash
rise sa create my-app \
  --issuer https://token.actions.githubusercontent.com \
  --claim aud=rise-project-my-app \
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
      - name: Get OIDC token
        run: |
          TOKEN=$(curl -H "Authorization: bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN" \
                       "$ACTIONS_ID_TOKEN_REQUEST_URL&audience=rise-project-my-app" | jq -r .value)
          echo "RISE_TOKEN=$TOKEN" >> $GITHUB_ENV
      - name: Deploy
        run: rise deploy --image ghcr.io/myorg/my-app:$GITHUB_SHA
```

### Creating Service Accounts

```bash
rise sa create <project> \
  --issuer <issuer-url> \
  --claim aud=<value> \
  --claim <key>=<value>
```

Requirements: an `aud` claim and at least one additional claim for authorization.

### Common Use Cases

**Protected branches only (production):**

```bash
rise sa create prod \
  --issuer https://gitlab.com \
  --claim aud=rise-project-prod \
  --claim project_path=myorg/app \
  --claim ref_protected=true
```

**Specific branch (staging):**

```bash
rise sa create staging \
  --issuer https://gitlab.com \
  --claim aud=rise-project-staging \
  --claim project_path=myorg/app \
  --claim ref=refs/heads/staging
```

**Deploy from tags (releases):**

```bash
rise sa create releases \
  --issuer https://gitlab.com \
  --claim aud=rise-project-releases \
  --claim project_path=myorg/app \
  --claim ref_type=tag
```

### Available Claims

**GitLab CI**: `project_path`, `ref`, `ref_type`, `ref_protected`, `environment`, `pipeline_source` — [Docs](https://docs.gitlab.com/ee/ci/secrets/id_token_authentication.html)

**GitHub Actions**: `repository`, `ref`, `workflow`, `environment`, `actor` — [Docs](https://docs.github.com/en/actions/deployment/security-hardening-your-deployments/about-security-hardening-with-openid-connect)

### Wildcard Patterns in Claims

Claims support glob-style `*` wildcards:

```bash
# Match all merge request environments
rise sa create my-app \
  --issuer https://gitlab.com \
  --claim aud=rise-project-my-app \
  --claim project_path=myorg/myrepo \
  --claim environment=app-mr/*

# Match all feature branches
--claim ref=refs/heads/feature/*
```

`*` matches any sequence of characters (including `/` and `-`). Design patterns carefully — `app*` matches `app`, `app-staging`, and `application`.

### Managing Service Accounts

```bash
rise sa list <project>
rise sa show <project> <service-account-id>
rise sa delete <project> <service-account-id>
```

Service accounts can create, view, list, stop, and rollback deployments. They cannot manage projects, teams, or other service accounts.

Service accounts can optionally be restricted to specific [environments](environments.md). When configured, the service account can only create deployments targeting one of its allowed environments. This is managed through the web UI or API.

### Local Testing

To test service accounts locally, use [`@oidc.pub/cli`](https://oidc.pub) to run a dev OIDC issuer:

```bash
npx @oidc.pub/cli login
npx @oidc.pub/cli dev issuer --service <id>
```

The dev issuer runs locally but publishes its JWKS to a public URL (e.g. `https://<id>.oidc.pub`),
so the Rise backend can verify tokens without needing to reach your machine.

Create a service account using the public issuer URL:

```bash
rise sa create my-project \
  --issuer https://<id>.oidc.pub \
  --claim aud=test \
  --claim sub=dev
```

Mint a token and use it:

```bash
export RISE_TOKEN=$(curl -s http://localhost:9229/token \
  -d '{"aud": "test", "sub": "dev"}' | jq -r .access_token)
rise deploy --image my-image:latest
```

## App Users

App users grant view-only access to deployed applications. This controls who can access private projects through the ingress.

### Adding App Users

```bash
# Add a user by email
rise project app-user add my-app user:alice@example.com

# Add an entire team
rise project app-user add my-app team:backend
```

### Listing App Users

```bash
rise project app-user list my-app
```

### Removing App Users

```bash
rise project app-user remove my-app user:alice@example.com
```

Aliases: `rise project app-user rm`, `rise project app-user del`

## Troubleshooting

- **"Failed to start local callback server"** — ports 8765-8767 are in use
- **"Code exchange failed"** — check that the backend and identity provider are running
- **Token expired** — run `rise login` (tokens expire after 1 hour by default)
- **"The 'aud' claim is required"** — add `--claim aud=<value>` to service account
- **"No service account matched"** — check claims match exactly (case-sensitive), verify issuer URL has no trailing slash
- **"Multiple service accounts matched"** — make claims more specific to avoid ambiguity
- **"403 Forbidden"** (service account) — service accounts can only deploy, not manage projects

See [Troubleshooting](troubleshooting.md) for more.
