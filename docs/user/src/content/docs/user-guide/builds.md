---
title: "Building Container Images"
---

Rise supports multiple build backends for creating container images from your application code. Building happens automatically as part of `rise deploy`, or you can build standalone with `rise build`.

## Build Backends

| Backend | Tool | Details |
|---------|------|---------|
| `docker`, `docker:build`, `docker:buildx`, `docker:buildctl` / `buildctl` | docker / buildctl | [Docker](builds/docker) |
| `pack` | pack CLI | [Pack](builds/pack) |
| `railpack`, `railpack:buildx`, `railpack:buildctl` | railpack + buildx/buildctl | [Railpack](builds/railpack) |

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

:::note[Native `--push`]
"Partial" means some CLI frontends (e.g., Podman buildx) don't support the `--push` flag. Rise detects this and falls back to building with `--load` followed by a separate push step. Either way, images always get pushed when deploying — this only affects the internal mechanism.
:::

Use `--separate-push` to force the fallback mechanism even when native push is available. This builds/loads the image locally first and pushes it in a separate step, which is useful for long CI builds where short-lived registry credentials should be refreshed immediately before pushing.

## Auto-Detection

When `--backend` is not specified, Rise detects the build method automatically:

- If `Dockerfile` or `Containerfile` exists → `docker:buildx` (or `docker:build` if buildx is unavailable)
- Otherwise → `railpack:buildx`

Override with `--backend` or in `rise.toml`:

```bash
rise deploy --backend railpack
```

```toml
[build]
backend = "pack"
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

Build args are for build configuration only (compiler flags, tool versions). For runtime variables, use `-e` / `--env` on `rise deploy`, or `rise env set`. See [Environment Variables](../environment-variables) for the distinction.

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

Rise normally picks the right build platform for you:

- **`rise deploy`**: the backend tells the CLI what architecture its cluster expects (read from the controller's `node_selector["kubernetes.io/arch"]`). A production backend pinning amd64 will produce amd64 images even when you deploy from an ARM Mac.
- **`rise build` / `rise run` / no backend hint**: the CLI builds for your host architecture (so an ARM Mac builds `linux/arm64`, an Intel machine builds `linux/amd64`).

You only need to specify a platform explicitly to override the inference — for example, building an amd64 image on an ARM Mac for sharing with a colleague:

```bash
rise build myapp:latest --platform linux/amd64
```

Or in `rise.toml`:

```toml
[build]
platform = "linux/amd64"
```

Or via environment variable:

```bash
RISE_PLATFORM=linux/amd64 rise build myapp:latest
```

Precedence (highest to lowest): `--platform` flag → `RISE_PLATFORM` env var → `rise.toml` → backend-advertised target → host architecture.

When the platform is inferred rather than explicitly set, the CLI prints a one-line notice so the choice isn't silent.

## SSL and Proxy

If you're behind a corporate proxy or have custom CA certificates, see [SSL & Proxy Configuration](../ssl-proxy) for managed BuildKit daemon setup, certificate injection, and proxy variable handling.
