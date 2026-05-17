---
title: "Building Container Images"
---

# Building Container Images

Rise supports multiple build backends for creating container images from your application code. Building happens automatically as part of `rise deploy`, or you can build standalone with `rise build`.

> **Note:** Rise defaults to `linux/amd64` for server compatibility. Override with `--platform`, `RISE_PLATFORM`, or `[build] platform` in `rise.toml`.

## Build Backends

| Backend | Build Tool | Invocation | Best For |
|---------|------------|------------|----------|
| `docker` / `docker:build` | docker build | `docker build` | Simple local builds, maximum compatibility |
| `docker:buildx` | docker buildx build | `docker buildx build` | BuildKit features (secrets, caching, multi-platform) |
| `docker:buildctl` / `buildctl` | buildctl | `buildctl build` | Dockerfile builds via buildctl (no Docker daemon needed) |
| `pack` | pack CLI | `pack build` | Cloud Native Buildpacks (no Dockerfile needed) |
| `railpack` / `railpack:buildx` | railpack + buildx | `railpack prepare` then `docker buildx build` | Railway Railpacks with BuildKit |
| `railpack:buildctl` | railpack + buildctl | `railpack prepare` then `buildctl build` | Railpacks with direct buildctl |

## Feature Matrix

| Feature | docker:build | docker:buildx | buildctl | pack | railpack:buildx | railpack:buildctl |
|---|:---:|:---:|:---:|:---:|:---:|:---:|
| Requires BuildKit | | x | x | | x | x |
| SSL cert injection | | x | x | x | x | x |
| Proxy support | x | x | x | x | x | x |
| Native `--push`* | | Partial | x | | Partial | x |
| Local output | Direct | `--load` | `docker load` pipe | Direct | `--load` | `docker load` pipe |
| Managed BuildKit | | x | x | N/A | x | x |
| Build contexts | x | x | | | | |

\*Native `--push`: Whether the build command supports pushing directly. "Partial" means some CLI frontends (e.g., Podman buildx) don't support the `--push` flag; Rise detects this and falls back to a separate push step. Either way, images always get pushed when deploying — this only affects the internal mechanism.

## Auto-Detection

When `--backend` is not specified, Rise detects the build method automatically:

- If `Dockerfile` or `Containerfile` exists in the project directory → `docker:buildx` (if buildx is available, otherwise `docker:build`)
- Otherwise → `railpack:buildx`

Override auto-detection with `--backend` or in `rise.toml`:

```bash
rise deploy --backend railpack
```

```toml
[build]
backend = "pack"
```

## Docker Backend

### Basic Usage

```bash
rise build myapp:latest --backend docker
rise deploy --backend docker:buildx
```

### How It Works

- **`docker:build`**: Runs `docker build` with `--build-arg` for environment variables and `--platform` set to the configured target platform. Does not support SSL certificate injection.
- **`docker:buildx`**: Runs `docker buildx build` via a managed BuildKit daemon. Adds `--platform` and uses `--load` (local) or `--push` (deploy). SSL certificates are injected by preprocessing the Dockerfile to add BuildKit bind mounts to each `RUN` step.

### Custom Dockerfile Path

```bash
rise build myapp:latest --dockerfile Dockerfile.prod
```

Or in `rise.toml`:

```toml
[build]
backend = "docker"
dockerfile = "Dockerfile.prod"
```

### Build Contexts (Multi-Stage Builds)

Use additional directories in multi-stage Docker builds:

```bash
rise build myapp:latest \
  --build-context mylib=../my-library \
  --build-context tools=../build-tools
```

Or in `rise.toml`:

```toml
[build]
backend = "docker"
build_context = "./app"  # Custom default build context

[build.build_contexts]
mylib = "../my-library"
tools = "../build-tools"
```

Reference in your Dockerfile:

```dockerfile
COPY --from=mylib /src /app/lib
```

Build contexts are supported by all Docker-based backends. Paths are relative to the `rise.toml` location.

## Pack Backend

Uses Cloud Native Buildpacks via the `pack` CLI:

```bash
rise build myapp:latest --backend pack
rise deploy --backend pack --builder heroku/builder:24
```

### How It Works

Runs `pack build` with `--docker-host inherit --network host`. Environment variables are passed via `--env KEY=VALUE`. SSL certificates are volume-mounted to all common distro paths (Debian, RedHat, Alpine, etc.) with matching SSL environment variables set.

Configure builder and buildpacks in `rise.toml`:

```toml
[build]
backend = "pack"
builder = "heroku/builder:24"
buildpacks = ["heroku/nodejs", "heroku/procfile"]
```

## Railpack Backend

Uses Railway Railpacks with BuildKit:

```bash
rise build myapp:latest --backend railpack
rise deploy --backend railpack:buildctl
```

### How It Works

Railpack builds are a two-step process:

1. **Prepare**: `railpack prepare` generates a build plan (JSON) from your application code. Environment variables and secrets are declared at this stage.
2. **Build**: The plan is built using either `docker buildx build` (with the `railpack-frontend` BuildKit frontend) or `buildctl build` (with the `gateway.v0` frontend). Environment variables are passed as BuildKit secrets, not build args.

If builds fail with the error `requested experimental feature mergeop has been disabled`, create a custom buildx builder:

```bash
docker buildx create --use
```

## Build-Time Arguments

Pass build arguments with `-b` / `--build-arg`:

```bash
rise build myapp:latest -b NODE_ENV=production -b BUILD_VERSION=1.2.3
```

Or in `rise.toml`:

```toml
[build]
args = ["NODE_ENV=production", "BUILD_VERSION"]
```

Using `KEY` without `=VALUE` reads the variable from your shell environment (useful for CI metadata like git SHAs).

**How backends use these variables:**

- **Docker**: Passed as `--build-arg` (requires `ARG` declaration in Dockerfile)
- **Pack**: Passed as `--env` to pack CLI
- **Railpack**: Passed as BuildKit secrets

Build args are for build configuration only (compiler flags, tool versions). For runtime variables, use `-e` / `--env` on `rise deploy`, or `rise env set`. See [Environment Variables](environment-variables.md) for the distinction.

## Build Cache Control

Force a complete rebuild:

```bash
rise deploy --no-cache
```

Or in `rise.toml`:

```toml
[build]
no_cache = true
```

## Target Platform

By default, Rise builds for `linux/amd64` (the server architecture). Override this for local development on other architectures (e.g., ARM Macs):

```bash
rise build myapp:latest --platform linux/arm64
```

Or in `rise.toml`:

```toml
[build]
platform = "linux/arm64"
```

Or via environment variable:

```bash
RISE_PLATFORM=linux/arm64 rise build myapp:latest
```

Precedence: `--platform` flag > `RISE_PLATFORM` env var > `rise.toml` > default (`linux/amd64`).

## SSL and Proxy

If you're behind a corporate proxy or have custom CA certificates, see [SSL & Proxy Configuration](ssl-proxy.md) for managed BuildKit daemon setup, certificate injection, and proxy variable handling.
