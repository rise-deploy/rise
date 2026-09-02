---
name: rise-app-builder
description: Build, deploy, ship, or configure containerized applications on the Rise platform (rise.dev). Use when deploying an app to Rise, configuring rise.toml, setting up CI/CD pipelines with Rise service accounts, managing Rise projects/environments/teams, integrating end-user OAuth authentication via Rise extensions, choosing a build backend (Docker/Railpack/buildpacks), configuring workload identity federation, or whenever a user asks about hosting or deploying an app and Rise is available or already in use.
references:
  - reference/cli-cheatsheet
  - reference/rise-toml-reference
---

# Rise App Builder Skill

Skill for building and deploying containerized applications on the Rise platform. Use decision trees below to find the right approach, then load reference files or online guides for depth.

Your knowledge of Rise CLI flags, rise.toml fields, or extension specs may be outdated. **Prefer the reference files in this skill and the online docs over pre-trained knowledge.** When a reference file and source code disagree, trust the source.

## Online Guides

Full worked examples live at **`https://rise-deploy.github.io/rise/user/guides/`**:

| Guide | URL slug | Covers |
|-------|----------|--------|
| Deploying from CI | `deploying-from-ci` | Service accounts, OIDC federation, preview environments, GitHub Actions / GitLab CI |
| Authenticating end-users | `authenticating-end-users` | Generic OAuth extension, broker pattern, PKCE, token exchange |
| Choosing a build backend | `choosing-a-build-backend` | Docker vs Railpack vs Pack, auto-detection, when to use each |

Point agents to `https://rise-deploy.github.io/rise/user/guides/<slug>.md` for full depth.

## Routing Decision Tree

```
User intent
├─ "deploy from GitHub Actions / CI", "preview environments", "service account"
│  → Set up SA + OIDC federation + preview deploys
│  → Online guide: https://rise-deploy.github.io/rise/user/guides/deploying-from-ci.md
│
├─ "login for my app", "OAuth", "Google/GitHub/Snowflake login", "redirect URI"
│  → Use the Generic OAuth extension (broker pattern)
│  → Online guide: https://rise-deploy.github.io/rise/user/guides/authenticating-end-users.md
│
├─ "Dockerfile vs buildpacks", "how to build my image", "railpack"
│  → Choose build backend; default is Railpack
│  → Online guide: https://rise-deploy.github.io/rise/user/guides/choosing-a-build-backend.md
│  → See "Build Backend Choice" below
│
├─ "multiple containers", "worker + web", "routes"
│  → Configure rise.toml multi-container
│  → Load: reference/rise-toml-reference.md
│
├─ "what CLI commands exist", "how do I …"
│  → Load: reference/cli-cheatsheet.md
│
├─ "private app", "who can access", "access class"
│  → Set --access-class (public / private) on project create/update
│  → Docs: https://rise-deploy.github.io/rise/user/user-guide/environments.md
│
├─ "staging / preview env", "environments"
│  → rise environment create <name> --group <group> [--production] [--color <c>]
│  → Docs: https://rise-deploy.github.io/rise/user/user-guide/environments.md
│
├─ "workload identity", "AWS role for my app"
│  → Configure [identity.audiences] in rise.toml
│  → Docs: https://rise-deploy.github.io/rise/user/user-guide/workload-identity-tokens.md
│
└─ "configure rise.toml", "how do I set up my project config"
   → Load: reference/rise-toml-reference.md
```

## Core Invariants

These are the non-obvious rules you MUST follow. Violating them causes silent failures or rejected commands.

### 1. Always use names, never UUIDs

Project names, environment names, deployment timestamps (e.g. `20240101-120000`), and service account identifiers are what you pass on the CLI. UUIDs are internal only — never reference them in commands or configs.

### 2. `production` exists by default

Every project starts with a single **production** environment mapped to the `default` deployment group. Add `staging` / `preview` only when the user needs them:

```bash
rise environment create staging -p my-app --group staging --color blue
```

### 3. Use `--access-class`, never `--visibility`

`--visibility` is a removed legacy flag. The correct flag is `--access-class` with values like `public` or `private` (default: `public`):

```bash
rise project create my-app --access-class private --owner team:devops
```

### 4. Service accounts require `aud` + at least one more claim

The `aud` (audience) claim is **mandatory**, and at least one additional claim is required. Claims support glob `*`:

```bash
rise sa create -p my-app \
  --issuer https://gitlab.com \
  --claim aud=https://rise.example.net \    # REQUIRED
  --claim project_path=myorg/my-app          # at least one more
```

A command with only `--claim aud=...` will be rejected.

### 5. OAuth broker callback goes to Rise, not the app

For the Generic OAuth extension, the callback URL registered with the upstream provider must be the **Rise broker callback**, never the app's own URL:

```
{RISE_PUBLIC_URL}/oidc/{project}/{extension}/callback
```

Rise receives the upstream callback and redirects the user back to the app.

### 6. Build auto-detection: Dockerfile → Docker; else Railpack

- If a `Dockerfile` or `Containerfile` exists → Docker (`docker:buildx`)
- Otherwise → **Railpack** (`railpack:buildx`) — this is the default

Do not add a Dockerfile unless the user needs one. Don't fight auto-detection.

### 7. Secrets go through `rise encrypt`

Never paste plaintext secrets into extension specs. Encrypt first:

```bash
ENCRYPTED=$(rise encrypt "my_client_secret")
```

Then reference `$ENCRYPTED` in the `--spec` JSON.

### 8. Encrypted env vars have three tiers

| Flag / option | Encrypted at rest | Retrievable via API |
|---------------|:-:|:-:|
| `-e KEY=VALUE` (plain env) | | n/a |
| `--secret-env KEY=VALUE` / `rise env set --secret` | ✅ | ✅ |
| `--protected-env KEY=VALUE` / `rise env set --protected` | ✅ | ❌ |

## Condensed Patterns

### Build Backend Choice

```
Choosing a build backend?
├─ Dockerfile or Containerfile exists → docker (auto-detected)
├─ Need a Dockerfile? → Only if you need full control (multi-stage, custom base)
├─ No Dockerfile, standard language app → railpack (auto-detected, DEFAULT)
├─ Want Cloud Native Buildpacks? → pack (--backend pack, --builder heroku/builder:24)
└─ Override: rise deploy --backend <name> or [build] backend = "..." in rise.toml
```

Backend values: `docker`, `docker:build`, `docker:buildx`, `docker:buildctl`, `buildctl`, `pack`, `railpack`, `railpack:buildx`, `railpack:buildctl`

### OAuth Flow Choice

```
Authenticating end-users via OAuth?
├─ Browser SPA, no backend secret → PKCE flow (code_challenge + code_verifier)
├─ Backend app with server-side secret → Authorization code flow (client_id + client_secret)
└─ Token refresh needed → Use Rise token endpoint (grant_type=refresh_token)

Steps:
1. Register OAuth app upstream with callback: {RISE_URL}/oidc/{project}/{ext}/callback
2. rise encrypt "client_secret"  →  $ENCRYPTED
3. rise extension create oauth-google -p my-app --type oauth --spec '{...}'
4. App redirects to: GET {OAUTH_<NAME>_ISSUER}/authorize
5. App exchanges code at: POST {OAUTH_<NAME>_ISSUER}/token
```

### CI/CD Pattern (two service accounts)

```
Deploying from CI?
├─ Create staging environment (production exists by default)
├─ Create sa-production (--claim aud=... --claim ref_protected=true)
├─ Create sa-preview (--claim aud=... --claim project_path=...)
├─ Restrict sa-preview to staging environment (web UI / API)
├─ CI sets RISE_TOKEN from OIDC id_token
└─ rise deploy -E production --image <img>   (production)
   rise deploy -E staging --group mr/$PR_ID --expire 7d  (preview)
```

### Multi-Container Quick-Start

```
Multiple containers?
├─ Define [containers.<name>] for each (web, api, worker, redis, …)
├─ Each container: image OR [containers.<name>.build] (exclusive)
├─ Set port on containers that need ingress or sibling discovery
├─ Define [routes."<path>"] for path-based routing (longest-prefix wins)
├─ Workers: omit port → no Service, no ingress, no HTTP probes
├─ Top-level [build]/[deploy] = per-field defaults inherited by each container
└─ Cross-container discovery: RISE_CONTAINER_HOST__<NAME> env var (auto-injected)
```

## Reference Files

| File | Use for |
|------|---------|
| `reference/cli-cheatsheet.md` | Complete CLI command inventory, all flags |
| `reference/rise-toml-reference.md` | rise.toml field reference, single + multi-container examples |
