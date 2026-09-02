# Rise CLI Cheatsheet

Complete inventory of `rise` CLI commands. Verified against `src/main.rs` clap definitions.

## Auth

| Command | Flags | Description |
|---------|-------|-------------|
| `rise login` | `--url <url>`, `--browser` (default), `--device` | Authenticate with the Rise backend. Default: OAuth2 authorization-code flow with PKCE via browser. `--device`: device authorization flow. |

## Projects (`rise project` / `rise p`)

| Command | Aliases | Key Flags |
|---------|---------|-----------|
| `rise project create [name]` | `c`, `new` | `--access-class <public\|private>` (default: public), `--owner user:email` / `team:name`, `--source-url <url>`, `--path <dir>`, `--no-rise-toml` |
| `rise project list` | `ls`, `l` | — |
| `rise project show [project]` | `s` | `--path <dir>` |
| `rise project update [project]` | `u`, `edit` | `--name <new>`, `--access-class <v>`, `--owner <user:email\|team:name>`, `--source-url <url>` (empty string to clear), `--path <dir>` |
| `rise project delete [project]` | `del`, `rm` | `--path <dir>` |

When project name is omitted, it is read from `[project] name` in `rise.toml` in the given `--path` (default: `.`).

### App Users (`rise project app-user`)

View-only access to deployed apps.

| Command | Aliases | Args |
|---------|---------|------|
| `rise project app-user add <identifier>` | `a` | `user:email` or `team:name`; `[project]`, `--path` |
| `rise project app-user remove <identifier>` | `rm`, `del` | `user:email` or `team:name`; `[project]`, `--path` |
| `rise project app-user list` | `ls`, `l` | `[project]`, `--path` |

## Teams (`rise team` / `rise t`)

| Command | Aliases | Key Flags |
|---------|---------|-----------|
| `rise team create <name>` | `c`, `new` | `--owners <email,email,…>` (default: current user), `--members <email,email,…>` |
| `rise team list` | `ls`, `l` | — |
| `rise team show <team>` | `s` | — |
| `rise team update <team>` | `u`, `edit` | `--name <new>`, `--add-owners <emails>`, `--remove-owners <emails>`, `--add-members <emails>`, `--remove-members <emails>` |
| `rise team delete <team>` | `del`, `rm` | — |

## Deploy (`rise deploy` / `rise deployment create`)

`rise deploy` is a shortcut for `rise deployment create`.

| Flag | Description |
|------|-------------|
| `-p`, `--project <name>` | Project name (optional if rise.toml has `[project]`) |
| `[path]` | App directory (default: `.`) |
| `-i`, `--image <ref>` | Pre-built image (skips build). Required with `--image`: `--http-port` |
| `--from <timestamp>` | Reuse image from existing deployment (e.g. `20240101-120000`) |
| `--use-source-env-vars` | With `--from`: copy env vars from source deployment |
| `-g`, `--group <name>` | Deployment group (e.g. `default`, `mr/27`) |
| `-E`, `--environment <name>` | Target environment (resolved from group if omitted) |
| `--expire <duration>` | Auto-cleanup after period (e.g. `7d`, `2h`, `30m`) |
| `-e`, `--env KEY=VALUE` | Runtime env var (repeatable) |
| `--secret-env KEY=VALUE` | Encrypted secret env var, retrievable (repeatable) |
| `--protected-env KEY=VALUE` | Encrypted secret env var, NOT retrievable (repeatable) |
| `--env-file <path>` | File with env vars (`KEY=value` or `KEY=secret:value`) |
| `--http-port <port>` | HTTP port the app listens on (default: 8080 for buildpack builds) |
| `--replicas <n>` | Override replica count |
| `--cpu <val>` | CPU allocation (e.g. `500m`, `1`) |
| `--memory <val>` | Memory allocation (e.g. `256Mi`, `1Gi`) |
| `--push-image` | Pull `--image` locally and push to Rise registry |
| `--job-url <url>` | CI job URL (auto-detected from GitLab/GitHub) |
| `--pull-request-url <url>` | PR/MR URL (auto-detected) |
| `--git-repository <url>` | Git repo URL (auto-detected) |

Plus all `BuildArgs` flags (see Build section below).

### Deployment Management (`rise deployment` / `rise d`)

| Command | Aliases | Key Flags |
|---------|---------|-----------|
| `rise deployment create` | `c`, `new` | Same as `rise deploy` |
| `rise deployment list` | `ls`, `l` | `-p`, `--path`, `-g/--group`, `-l/--limit` (default 10) |
| `rise deployment show <id>` | `s` | `-p`, `--path`, `-f/--follow`, `--timeout` (default `5m`) |
| `rise deployment stop` | — | `-p`, `--path`, `-g/--group` (required) |
| `rise deployment logs <id>` | — | `-p`, `--path`, `-f/--follow`, `--tail <n>`, `--timestamps`, `--since <dur>`, `--level <lvl>` (repeatable) |

Deployment IDs are timestamps in `YYYYMMDD-HHMMSS` format.

## Build (`rise build` / `rise run`)

### `rise build <tag> [path]`

Build a container image locally without deploying.

| Flag | Description |
|------|-------------|
| `<tag>` | Image tag (e.g. `myapp:latest`) |
| `[path]` | App directory (default: `.`) |
| `--push` | Push to registry after building |

### `rise run [path]`

Build and run a container locally for development.

| Flag | Description |
|------|-------------|
| `-p`, `--project <name>` | Project name (for loading env vars) |
| `--use-project-env <bool>` | Load Rise project env vars (non-secret only, default: true) |
| `-E`, `--environment <name>` | Target environment |
| `--http-port <port>` | App HTTP port, also sets `PORT` env (default: 8080) |
| `--expose <port>` | Host port to expose (default: same as `--http-port`) |
| `-e`, `--env KEY=VALUE` | Runtime env var (repeatable) |

### BuildArgs (shared by deploy, build, run)

| Flag | Description |
|------|-------------|
| `--backend <name>` | Build backend: `docker`, `docker:build`, `docker:buildx`, `docker:buildctl`, `buildctl`, `pack`, `railpack`, `railpack:buildx`, `railpack:buildctl` |
| `--builder <image>` | Buildpack builder image (pack only) |
| `-B`, `--buildpack <id>` | Buildpack to use (pack only, repeatable) |
| `-b`, `--build-arg KEY=VALUE` | Build-time arg (repeatable). `KEY` alone reads from shell env |
| `--container-cli <docker\|podman>` | Container CLI override |
| `--managed-buildkit [bool]` | Enable managed BuildKit daemon (auto-when SSL_CERT_FILE set) |
| `--dockerfile <path>` | Path to Dockerfile (default: `Dockerfile` or `Containerfile`) |
| `--context <path>` | Build context directory (docker/podman only) |
| `--build-context name=path` | Named build context for multi-stage builds (repeatable) |
| `--no-cache` | Disable build cache |
| `--platform <arch>` | Target platform override. Resolution order: CLI, `RISE_PLATFORM`, rise.toml, backend hint, host architecture. |
| `--separate-push` | Build locally first, then push separately (for long CI builds) |

## Environments (`rise environment` / `rise envs`)

| Command | Aliases | Key Flags |
|---------|---------|-----------|
| `rise environment create <name>` | `c`, `new` | `-p`, `--path`, `-g/--group`, `--production`, `--color` (default: green) |
| `rise environment list` | `ls`, `l` | `-p`, `--path` |
| `rise environment show <name>` | `s` | `-p`, `--path` |
| `rise environment update <name>` | `u`, `edit` | `-p`, `--path`, `--rename <new>`, `-g/--group`, `--production [bool]`, `--color` |
| `rise environment delete <name>` | `del`, `rm` | `-p`, `--path` |

Valid colors: `green`, `blue`, `yellow`, `red`, `purple`, `orange`, `gray`.

## Environment Variables (`rise env` / `rise e`)

| Command | Aliases | Key Flags |
|---------|---------|-----------|
| `rise env set <key> [value]` | `s` | `--plain` or `--secret` (required), `-p`, `--path`, `--protected <bool>`, `-E/--environment` |
| `rise env list` | `ls`, `l` | `-p`, `--path`, `-E/--environment` |
| `rise env get <key>` | `g` | `-p`, `--path`, `-E/--environment` |
| `rise env delete <key>` | `unset`, `rm`, `del` | `-p`, `--path`, `-E/--environment` |
| `rise env import <file>` | `i` | `-p`, `--path`, `-E/--environment`. Format: `KEY=value` or `KEY=secret:value` |
| `rise env export` | `x` | `-p`, `--path`, `-E/--environment`. Outputs shell-safe `export KEY=value` commands |

Without `-E`, variables are global (apply to all environments). With `-E`, scoped to that environment (merged on top of globals).

## Service Accounts (`rise service-account` / `rise sa`)

| Command | Aliases | Key Flags |
|---------|---------|-----------|
| `rise sa create` | `c`, `new` | `-p`, `--path`, `--issuer <url>` (required), `--claim key=value` (repeatable, required) |
| `rise sa list` | `ls`, `l` | `-p`, `--path` |
| `rise sa show <id>` | `s`, `get` | `-p`, `--path` |
| `rise sa delete <id>` | `del`, `rm` | `-p`, `--path` |

**Requirements**: `--claim aud=<value>` is mandatory. At least one additional `--claim` is required. Claims support glob `*`.

## Extensions (`rise extension` / `rise ext`)

| Command | Aliases | Key Flags |
|---------|---------|-----------|
| `rise ext create <name>` | `c`, `new` | `-p`, `--path`, `--type <handler>` (e.g. `oauth`), `--spec '<json>'` |
| `rise ext update <name>` | `u` | `-p`, `--path`, `--spec '<json>'` (full replace) |
| `rise ext patch <name>` | `p` | `-p`, `--path`, `--spec '<json>'` (partial update, null unsets) |
| `rise ext list` | `ls`, `l` | `-p`, `--path` |
| `rise ext show <name>` | `s` | `-p`, `--path` |
| `rise ext delete <name>` | `rm`, `del` | `-p`, `--path` |

## Domains (`rise domain` / `rise dom`)

| Command | Aliases | Key Flags |
|---------|---------|-----------|
| `rise domain add <domain>` | `a` | `-p`, `--path`, `-e/--environment` (default: production) |
| `rise domain list` | `ls`, `l` | `-p`, `--path` |
| `rise domain remove <domain>` | `rm`, `del` | `-p`, `--path` |

## Identity (`rise identity`)

| Command | Flags | Description |
|---------|-------|-------------|
| `rise identity token` | `--audience <aud>` (required), `--credential <path>`, `--ttl-seconds <n>` | Request a workload identity token. Run inside a Rise deployment. Default credential: `/var/run/secrets/rise/identity/credential`. |

## AI Skills (`rise skill`)

| Command | Key Flags | Description |
|---------|-----------|-------------|
| `rise skill install [name]` | `--git-ref <ref>`, `--target <dir>` | Install or atomically update a Rise skill from the repository. Defaults to `rise-app-builder` from `develop` in `~/.claude/skills`. |
| `rise skill list` | `--target <dir>` | List installed skills containing a `SKILL.md`. |
| `rise skill uninstall <name>` | `--target <dir>` | Remove an installed skill. Alias: `rm`. |

## Encrypt

| Command | Description |
|---------|-------------|
| `rise encrypt [plaintext]` | Encrypt a secret for use in extension specs. Reads from stdin if no argument. Rate-limited: 100 requests/hour/user. |

## CI / Token Environment Variables

| Variable | Purpose |
|----------|---------|
| `RISE_URL` | Backend URL (overrides config file) |
| `RISE_TOKEN` | OIDC JWT from CI (e.g. GitLab `id_tokens`); exchanged for a Rise token when `RISE_IDENTITY` is set (highest precedence token source) |
| `RISE_TOKEN_COMMAND` | Shell command whose stdout is a token (cached; TTL configurable via `RISE_TOKEN_COMMAND_TTL`) |
| `RISE_IDENTITY` | Service-account identity to exchange the token for (synthetic-user email) |
| `RISE_GHA_AUDIENCE` | Audience for GitHub Actions OIDC token exchange |
| `RISE_CONTAINER_CLI` | Container CLI override (`docker` / `podman`) |
| `RISE_MANAGED_BUILDKIT` | Enable/disable managed BuildKit daemon |

**Token source precedence**: `RISE_TOKEN` → `RISE_TOKEN_COMMAND` → GitHub Actions OIDC → stored login. If `RISE_IDENTITY` is set, the selected token is exchanged for that identity.
