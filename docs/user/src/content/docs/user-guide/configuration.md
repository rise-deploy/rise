---
title: "Project Configuration"
---

Rise projects are configured through a `rise.toml` file in your project directory and through CLI flags.

## rise.toml

The `rise.toml` file defines your project metadata and build settings. Both `rise.toml` and `.rise.toml` are supported — if both exist, `rise.toml` takes precedence (with a warning).

A [JSON Schema](https://rise.example.com/api/v1/schema/rise-toml/v1) is available for editor auto-completion and validation. To enable it in editors that support the [Taplo](https://taplo.tamasfe.dev/) TOML language server, add this comment as the first line of your `rise.toml`:

```toml
#:schema https://rise.example.com/api/v1/schema/rise-toml/v1
```

### `[project]` Section

```toml
[project]
name = "my-app"

[project.env]
LOG_LEVEL = "info"
```

| Field | Type | Description |
|-------|------|-------------|
| `name` | String | Project name (used for URLs, registry paths, and as default for `-p` flag) |
| `env` | Object | Plain-text environment variables applied as deployment overrides (source: `toml`) |

### `[build]` Section

```toml
[build]
backend = "docker"
dockerfile = "Dockerfile.prod"
args = ["NODE_ENV=production", "BUILD_VERSION"]
```

| Field | Type | Description |
|-------|------|-------------|
| `backend` | String | Build backend: `docker`, `docker:build`, `docker:buildx`, `buildctl`, `docker:buildctl`, `pack`, `railpack`, `railpack:buildctl` |
| `dockerfile` | String | Path to Dockerfile, relative to `rise.toml` (default: `Dockerfile` or `Containerfile`) |
| `build_context` | String | Default build context path for Docker builds, relative to `rise.toml` |
| `build_contexts` | Object | Named build contexts for multi-stage Docker builds (format: `{ "name" = "path" }`) |
| `builder` | String | Buildpack builder image (pack backend only) |
| `buildpacks` | Array | Buildpacks to use (pack backend only) |
| `args` | Array | Build-time arguments (format: `KEY=VALUE` or `KEY` to read from shell). Alias: `env` for backward compat. |
| `container_cli` | String | Container CLI: `docker` or `podman` |
| `managed_buildkit` | Boolean | Enable/disable managed BuildKit daemon (auto-enables when `SSL_CERT_FILE` is set) |
| `no_cache` | Boolean | Disable build cache |

### `[environments.<name>]` Section

Define per-environment settings. Set `default = true` on one environment to auto-select it when deploying without `--environment`.

```toml
[environments.staging]
default = true
env.DATABASE_URL = "postgres://staging-db/mydb"
env.LOG_LEVEL = "debug"

[environments.production]
env.DATABASE_URL = "postgres://prod-db/mydb"
```

| Field | Type | Description |
|-------|------|-------------|
| `default` | Boolean | If `true`, this environment is used when `--environment` is not specified. At most one environment may be default. |
| `env` | Object | Plain-text environment variables scoped to this environment (applied as deployment overrides) |

See [Environment Variables](../environment-variables#per-environment-variables-in-risetoml) for details.

### Full Example

```toml
[project]
name = "my-app"

[project.env]
LOG_LEVEL = "info"
APP_MODE = "production"

[build]
backend = "pack"
builder = "heroku/builder:24"
buildpacks = ["heroku/nodejs", "heroku/procfile"]
args = ["BP_NODE_VERSION=20"]

[environments.staging]
default = true
env.DATABASE_URL = "postgres://staging-db/mydb"
env.LOG_LEVEL = "debug"

[environments.production]
env.DATABASE_URL = "postgres://prod-db/mydb"
```

## Project Creation

```bash
# Create project on backend and write rise.toml
rise project create my-app

# Create project on backend only (no rise.toml written)
rise project create my-app --no-rise-toml

# If rise.toml already exists, the project name is read from it
rise project create
```

If a `rise.toml` already exists, it is never overwritten — the project is created on the backend using the name from the file.

## Configuration Precedence

Settings are resolved in this order (highest to lowest priority):

1. **CLI flags** (e.g., `--backend pack`)
2. **Project config file** (`rise.toml` / `.rise.toml`)
3. **Environment variables** (e.g., `RISE_CONTAINER_CLI`, `RISE_MANAGED_BUILDKIT`)
4. **Global config** (`~/.config/rise/config.json`)
5. **Auto-detection / defaults**

For array fields (`buildpacks`, `args`), CLI values are **appended** to config file values rather than replacing them.

## Global CLI Config

The CLI stores global configuration in `~/.config/rise/config.json`, including:

- Authentication token (set by `rise login`)
- Backend URL
- Container CLI preference (`docker` or `podman`)
- Managed BuildKit setting

This file is created automatically on first `rise login`.
