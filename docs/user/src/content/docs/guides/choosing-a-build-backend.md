---
title: "Choosing a Build Backend"
description: "Decide between Railpack, Docker, and Pack (buildpacks) for building container images in Rise."
---

## When to use this

You're deploying an app to Rise and need to pick a build backend — or you've hit a limitation with the default and want to know when to switch. This guide covers auto-detection rules, the decision matrix, and configuration for each backend.

For reference material, see [Building Images](../user-guide/builds.md) and the per-backend docs: [Docker](../user-guide/builds/docker.md), [Pack](../user-guide/builds/pack.md), [Railpack](../user-guide/builds/railpack.md).

## Auto-detection (what Rise picks if you do nothing)

Verified in `src/build/method.rs` — `select_build_method`:

```
if a Dockerfile exists  →  Docker (docker:buildx if buildx is available, else docker:build)
else if a Containerfile exists  →  Docker
else  →  Railpack
```

If `--backend` is specified, auto-detection is bypassed entirely.

## Decision matrix

| Backend | Best for | When to choose | CLI flag | rise.toml |
|---------|----------|----------------|----------|-----------|
| **Railpack** | No Dockerfile; fast Nix-based auto-detection of many languages | **Default starting point** for new apps without a Dockerfile | `--backend railpack` | `[build] backend = "railpack"` |
| **Docker** (`docker:buildx`) | You have or want a Dockerfile; multi-stage builds; pinned base images; full control | You already maintain a Dockerfile, or need control Railpack can't provide | `--backend docker:buildx` (or just have a Dockerfile) | `[build] backend = "docker:buildx"` |
| **Pack** (buildpacks) | Heroku-style builds; org-standard buildpacks; Heroku parity | Your org uses Cloud Native Buildpacks, or you want Heroku-style builds | `--backend pack --builder <image>` | `[build] backend = "pack"` |

:::tip[Recommendation]
Start with Railpack (the auto-default when there's no Dockerfile). Switch to a custom Dockerfile when you need multi-stage builds, pinned versions, custom system dependencies, or a stack Railpack doesn't support.
:::

## When to switch from Railpack to Docker

Switch to Docker when any of these apply:

| Signal | Why Docker is better |
|--------|---------------------|
| You need **multi-stage builds** | Docker supports `COPY --from=` and named build contexts |
| You need **pinned base images** (e.g., `python:3.12.1-slim`) | Full control over exact versions |
| You need **custom system packages** (`apt-get install …`) | Add them in your Dockerfile's `RUN` steps |
| You need a **language/stack Railpack doesn't support** | Any Dockerfile works |

## Docker variants

| Variant | Command | When to use |
|---------|---------|-------------|
| `docker:buildx` | `docker buildx build` | **Recommended.** Supports SSL cert injection, build secrets, multi-platform. |
| `docker:build` | `docker build` | Simple and compatible, but no BuildKit features (no SSL, no secrets). |
| `docker:buildctl` / `buildctl` | `buildctl build` | When Docker is unavailable but a BuildKit daemon is. |

Use `--managed-buildkit` (or `[build] managed_buildkit = true` in rise.toml) for a managed BuildKit daemon with SSL certificate support — required behind corporate proxies.

## rise.toml examples

**Railpack (default, no Dockerfile):**

```toml
[build]
backend = "railpack"
```

**Docker with a Dockerfile:**

```toml
[build]
backend = "docker:buildx"
```

**Docker with a custom Dockerfile path:**

```toml
[build]
backend = "docker"
dockerfile = "Dockerfile.prod"
```

**Pack with a specific builder:**

```toml
[build]
backend = "pack"
builder = "heroku/builder:24"
buildpacks = ["heroku/nodejs", "heroku/procfile"]
```

## Common mistakes

- **Adding a Dockerfile you don't need** — once a `Dockerfile` exists, auto-detection switches to Docker and you must maintain it. If you want Railpack, delete the Dockerfile or set `--backend railpack` explicitly.
- **Using `pack` without considering the builder** — Rise defaults to `heroku/builder:24`. Set `--builder` (or `builder` in rise.toml) when your stack requires another builder.
- **Forgetting `--platform linux/amd64` when deploying from Apple Silicon** — if you build locally with `rise build` (no backend hint) on an ARM machine, the image targets `linux/arm64`. For `rise deploy`, the backend advertises its cluster arch automatically, but standalone `rise build` uses your host arch. Use `--platform linux/amd64` or set `RISE_PLATFORM=linux/amd64`.
- **Using `docker:build` when you need SSL cert injection** — `docker:build` has no BuildKit features. Use `docker:buildx` with `--managed-buildkit` for corporate proxy / custom CA setups. See [SSL & Proxy Configuration](../user-guide/ssl-proxy.md).
