---
title: "Authentication"
---

Rise uses JWT tokens for user authentication, service accounts for CI/CD workload identity, and app users for controlling access to deployed applications.

## User Authentication

### Browser Flow (Default)

```bash
rise login --url https://rise.example.com
```

This starts a local HTTP server (ports 8765-8767), opens your browser to the OAuth2/OIDC provider, and exchanges the auth code for a Rise JWT token using PKCE. The CLI stores your token and the server URL, so subsequent commands don't need `--url`.

If `RISE_URL` is already set in your environment, you can omit `--url`:

```bash
rise login
```

### Token Storage

Tokens are stored in `~/.config/rise/config.json` (plain JSON).

### Environment Variables

- `RISE_URL` — default backend URL
- `RISE_TOKEN` — authentication token (bypasses interactive login)

### API Usage

Protected endpoints require `Authorization: Bearer <token>`. Missing or invalid tokens return 401.

## Service Accounts (Workload Identity)

Service accounts let CI/CD pipelines authenticate with Rise using short-lived OIDC tokens — no long-lived secrets required. Each job presents a JWT from the CI provider; Rise validates it against the service account's claim configuration and grants project-scoped deployment access.

See [Service Accounts](../service-accounts) for full setup instructions, available claims, GitLab CI and GitHub Actions examples, and local testing.

See [CI/CD Setup](../ci-cd) for the recommended two-SA pattern with environment restrictions.

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
- **"The 'aud' claim is required"** — add `--claim aud=https://rise.example.net` to service account
- **"No service account matched"** — check claims match exactly (case-sensitive), verify issuer URL has no trailing slash
- **"Multiple service accounts matched"** — make claims more specific to avoid ambiguity
- **"403 Forbidden"** (service account) — service accounts can only deploy, not manage projects

See [Troubleshooting](../troubleshooting) for more.
