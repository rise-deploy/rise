---
title: "Authentication"
---

Rise uses JWT tokens for user authentication, service accounts for CI/CD workload identity, and app users for controlling access to deployed applications.

## User Authentication

### Browser Flow (Default)

```bash
rise login --url https://rise.example.com
```

This starts a local HTTP server on `127.0.0.1`, trying ports 8765, 8766, then 8767 in order, opens your browser to the OAuth2/OIDC provider, and exchanges the auth code for a Rise JWT token using PKCE. The CLI stores your token and the server URL, so subsequent commands don't need `--url`.

The identity provider's Rise client must allow these exact redirect URIs (in Keycloak, add them to **Valid redirect URIs**):

- `http://127.0.0.1:8765/callback`
- `http://127.0.0.1:8766/callback`
- `http://127.0.0.1:8767/callback`

The callback listener and redirect both use the IPv4 loopback address so the browser reaches the port reserved by the CLI.

If `RISE_URL` is already set in your environment, you can omit `--url`:

```bash
rise login
```

### Device Flow

Where the CLI can't open a browser or receive the local callback — an SSH
session, a container, a remote VM — log in with a code instead:

```bash
rise login --device
```

The CLI prints a URL on your Rise instance (`https://rise.example.com/device?user_code=…`)
and a code such as `BCDF-GHJK`. Open the URL in any browser, sign in, check that the
page shows the same code, and click **Approve**. The CLI picks up the token on its next
poll.

- The code is confirmed on Rise itself, so this works with any identity provider.
- Approval needs a recent sign-in in the browser: if your browser session is
  older than a few minutes, the page asks you to sign in again first. A CLI
  login can't approve a device login.
- Codes expire after 10 minutes and work once. Only approve a code you requested
  yourself — approving signs that terminal in as you.

### Restricting a Login

A login has your full access by default. Add `--scope` to restrict it to a
project or one of its environments, for example before handing the CLI to a
coding agent:

```bash
rise login --device --scope my-app/staging=deploy
rise login --scope my-app=read --scope my-app/staging=develop
```

Each `--scope` is `<project>[/<environment>]=<access>`, where access is a preset
or a comma-separated list of permissions:

| Preset | Permissions | On |
|---|---|---|
| `read` | `view` | project, environment |
| `deploy` | `view`, `logs`, `deploy` | project, environment |
| `develop` | `view`, `logs`, `deploy`, `env-vars` | project, environment |
| `admin` | all of them | project |

The permissions are `view`, `logs`, `deploy`, `env-vars`, `secrets` (read
retrievable secret values), and the project-only `configure` (environments,
domains, extensions, app users), `service-accounts` and `project-admin` (rename,
re-own, change access class, delete). An environment grant reaches that
environment, its deployments and its own env vars. It never reaches the
project's other environments, project-wide env vars, or settings that would let
it redirect another environment's deployments.

- With `--device`, the scope is a request: the approval page shows it, and the
  person approving can narrow it, widen it, or grant full access. The CLI prints
  the access it was actually granted.
- A restricted login can't create projects, manage teams, or use the resource
  API. Admins' restricted logins are restricted too.
- `--full-access` asks for full access explicitly; it's the same as no `--scope`.
- A restricted login replaces the stored token for that Rise URL, including a
  full-access one. The CLI says so first.
- A `deploy` grant can reach secrets in practice: a deployment can print its own
  environment.

### Token Storage

Tokens are stored as private JSON files under `~/.config/rise/credentials/`, with
one token per backend URL. All profiles pointing to that URL share the token;
logging in again updates it for every alias.

### Multiple Profiles

To give a backend URL a local alias, log in with a named `--profile`:

```bash
rise login --profile work --url https://rise.work.example.com
```

`rise login --profile <name>` registers the profile automatically if it
doesn't already exist. Subsequent commands pick it up via `--profile <name>`
or the `RISE_PROFILE` environment variable; `rise profile list` shows every
registered profile. See [Login Profiles](../configuration#login-profiles) for
details.

### Environment Variables

- `RISE_URL` — default backend URL
- `RISE_PROFILE` — login profile to use (see [Login Profiles](../configuration#login-profiles))
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
- **"The login code expired or was already used"** (`--device`) — run `rise login --device` again and approve within 10 minutes
- **Token expired** — run `rise login` (tokens expire after 24 hours by default)
- **"The 'aud' claim is required"** — add `--claim aud=https://rise.example.net` to service account
- **"No service account matched"** — check claims match exactly (case-sensitive), verify issuer URL has no trailing slash
- **"Multiple service accounts matched"** — make claims more specific to avoid ambiguity
- **"403 Forbidden"** (service account) — service accounts can only deploy, not manage projects

See [Troubleshooting](../troubleshooting) for more.
