# rise.toml Reference

Field reference for `rise.toml` (also `.rise.toml`). Verified against `src/rise_toml.rs`.

A [JSON Schema](https://rise.dev/docs/schemas/rise-toml-v1.schema.json) is available for editor validation. Add this as the first line for Taplo support:

```toml
#:schema https://rise.dev/api/v1/schema/rise-toml/v1
```

## Top-Level Structure

| Field | Type | Description |
|-------|------|-------------|
| `version` | `u32` (optional) | Must be `1` if present |
| `[project]` | table | Project metadata |
| `[build]` | table | Build configuration (single-container or top-level defaults) |
| `[deploy]` | table | Deployment resource configuration (single-container or top-level defaults) |
| `[identity]` | table | Workload identity configuration |
| `[environments.<name>]` | table map | Per-environment overrides |
| `[containers.<name>]` | table map | Multi-container definitions |
| `[routes."<path>"]` | table map | Path-based ingress routing |

## `[project]`

| Field | Type | Description |
|-------|------|-------------|
| `name` | `String` | Project name (used for URLs, registry paths, `-p` default) |
| `env` | `map<String, String>` | Plain-text environment variables (non-secret) |

```toml
[project]
name = "my-app"

[project.env]
LOG_LEVEL = "info"
```

## `[build]`

| Field | Type | Description |
|-------|------|-------------|
| `backend` | `String` | Build backend: `docker`, `docker:build`, `docker:buildx`, `docker:buildctl`, `buildctl`, `pack`, `railpack`, `railpack:buildx`, `railpack:buildctl` |
| `dockerfile` | `String` | Path to Dockerfile (default: `Dockerfile` or `Containerfile`) |
| `build_context` | `String` | Default build context path (docker/podman only) |
| `build_contexts` | `map<String, String>` | Named build contexts for multi-stage builds (`{ "name" = "path" }`) |
| `builder` | `String` | Buildpack builder image (pack only) |
| `buildpacks` | `[String]` | Buildpacks to use (pack only) |
| `args` | `[String]` | Build-time args (`KEY=VALUE` or `KEY` to read from shell). Alias: `env` |
| `container_cli` | `String` | Container CLI: `docker` or `podman` |
| `managed_buildkit` | `bool` | Enable managed BuildKit daemon (auto-enables when `SSL_CERT_FILE` is set) |
| `platform` | `String` | Target platform (default: `linux/amd64`) |
| `no_cache` | `bool` | Disable build cache |

```toml
[build]
backend = "pack"
builder = "heroku/builder:24"
args = ["NODE_ENV=production", "BUILD_VERSION"]
```

## `[deploy]`

| Field | Type | Description |
|-------|------|-------------|
| `replicas` | `u32` | Number of replicas |
| `cpu` | `String` | CPU: fixed (`"500m"`, `"1"`) or request-limit range (`"128m-500m"`) |
| `memory` | `String` | Memory: fixed (`"256Mi"`) or range (`"64Mi-256Mi"`) |
| `health_check` | see below | Health probe configuration |

### CPU/Memory Range Syntax

A fixed value sets both the K8s request and limit. A range `"request-limit"` sets them independently:

```toml
[deploy]
cpu = "128m-500m"       # request 128m, limit 500m
memory = "64Mi-256Mi"   # request 64Mi, limit 256Mi
```

Request may not exceed limit.

### `health_check`

Set `false` to disable probes entirely, or a config block to enable:

```toml
# Disable probes
[deploy]
health_check = false

# Customise probes
[deploy]
health_check = { path = "/healthz", initial_delay_seconds = 15, period_seconds = 10 }
```

| Sub-field | Type | Default | Description |
|-----------|------|---------|-------------|
| `path` | `String` | `/` | HTTP path to probe |
| `initial_delay_seconds` | `i32` | 10 | Seconds before first probe |
| `period_seconds` | `i32` | — | Seconds between probes |
| `timeout_seconds` | `i32` | — | Probe timeout |
| `failure_threshold` | `i32` | — | Failures before considered failed |
| `liveness_enabled` | `bool` | `true` | Enable liveness probe |
| `readiness_enabled` | `bool` | `true` | Enable readiness probe |

`health_check = true` is rejected. Omit the key for defaults, or set `false` to disable.

## `[identity]`

| Field | Type | Description |
|-------|------|-------------|
| `audiences` | `map<String, String>` | Map of in-pod filename → token audience |

```toml
[identity.audiences]
aws = "sts.amazonaws.com"
vault = "https://vault.example.com"
```

The controller mints one token per audience and mounts it at:
```
/var/run/secrets/rise/identity/tokens/<filename>
```
e.g. `/var/run/secrets/rise/identity/tokens/aws` → JWT with `aud=sts.amazonaws.com`.

Always re-read the file at runtime (kubelet keeps it refreshed).

## `[environments.<name>]`

| Field | Type | Description |
|-------|------|-------------|
| `default` | `bool` | If `true`, used when `--environment` is not specified. At most one. |
| `env` | `map<String, String>` | Environment-scoped plain-text variables |
| `deploy` | `DeployConfig` | Environment-specific deploy overrides (replicas, cpu, memory, health_check) |

```toml
[environments.staging]
default = true
env = { LOG_LEVEL = "debug" }

[environments.production]
env = { LOG_LEVEL = "warn" }
```

## Multi-Container

### Single vs Multi-Container

- **Single-container**: top-level `[build]` / `[deploy]` define an implicit `app` container. No `[containers]` block.
- **Multi-container**: define `[containers.<name>]` entries. The top-level `[build]` / `[deploy]` become per-field defaults that each container inherits.

### `[containers.<name>]`

| Field | Type | Description |
|-------|------|-------------|
| `image` | `String` | Pre-built image reference. **Exclusive with `build`**. |
| `build` | `BuildConfig` | Build config for this container. **Exclusive with `image`**. Inherits top-level `[build]` per field. |
| `port` | `u16` | Port the container listens on. Required for ingress / sibling discovery. Omit for workers (no Service, no probes). |
| `env` | `map<String, String>` | Container-scoped env vars. Merged on top of project-level. Container wins on conflict. |
| `deploy` | `DeployConfig` | Per-container deploy overrides. Inherits top-level `[deploy]` per field. |

Container names must match `^[a-z][a-z0-9-]{0,14}$` (max 15 chars, no trailing dash).

### Inheritance Rules

- **`build`**: field-by-field merge of container's `[build]` over top-level `[build]`. A container with `image` takes no build at all.
- **`deploy`**: each field falls back to top-level default. `replicas`, `cpu`, `memory`, `health_check` each inherit independently.
- **`health_check`**: only inherits to containers that have a `port`. A port-less worker silently skips an inherited default probe.

### `[routes."<path>"]`

| Field | Type | Description |
|-------|------|-------------|
| `container` | `String` | Target container name (must exist and have `port`) |

```toml
[routes]
"/api" = { container = "api" }
"/" = { container = "frontend" }
```

- **Longest-prefix wins** — `/api` shadows `/`.
- The route's port is always the target container's `port`.
- If no explicit `[routes]` and exactly one container has a `port`, it is auto-exposed at `/`.
- Route paths must start with `/`, must not use the reserved `/.rise` prefix.

### Cross-Container Discovery

Each container with a `port` gets an auto-injected env var exposing sibling addresses:

```
RISE_CONTAINER_HOST__<NAME> = <group>-<name>:<port>
```

`<NAME>` is the container name uppercased with dashes → underscores. Each container sees its own entry too. Only injected when ≥2 containers exist.

Example: a container named `redis` with port `6379` → `RISE_CONTAINER_HOST__REDIS=default-redis:6379`.

## Complete Examples

### Minimal Single-Container

```toml
version = 1

[project]
name = "hello-world"

[project.env]
APP_ENV = "production"

[environments.production]
env = { LOG_LEVEL = "warn" }
```

Build backend is auto-detected: Railpack unless a Dockerfile exists. No `[build]` needed for the default case.

### Multi-Container (web + api + worker + redis)

```toml
version = 1

[project]
name = "multi-container"

# Top-level defaults inherited per-field by each container
[build]
backend = "docker"

[deploy]
replicas = 1
cpu = "128m-500m"

# Static frontend
[containers.frontend]
port = 8080
[containers.frontend.build]
dockerfile = "frontend/Dockerfile"
build_context = "frontend"

# JSON API — scales independently
[containers.api]
port = 8080
[containers.api.build]
dockerfile = "api/Dockerfile"
build_context = "api"
[containers.api.deploy]
replicas = 2
health_check = { path = "/api/health" }

# Background worker — no port, no Service, no probes
[containers.worker]
env = { WORK_MS = "2000" }
[containers.worker.build]
dockerfile = "worker/Dockerfile"
build_context = "worker"
[containers.worker.deploy]
cpu = "256m"                    # fixed: request == limit

# Redis — pre-built image, no build block
[containers.redis]
image = "redis:7-alpine"
port = 6379

# Path-based routing (longest-prefix wins)
[routes]
"/api" = { container = "api" }
"/" = { container = "frontend" }
```

## Configuration Precedence

1. CLI flags (e.g. `--backend pack`)
2. Project config file (`rise.toml` / `.rise.toml`)
3. Environment variables (`RISE_CONTAINER_CLI`, `RISE_MANAGED_BUILDKIT`)
4. Global config (`~/.config/rise/config.json`)
5. Auto-detection / defaults

For array fields (`buildpacks`, `args`), CLI values are **appended** to config file values.
