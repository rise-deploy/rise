---
title: "Docker Build Backend"
---

The Docker backend builds container images from a Dockerfile using `docker build`, `docker buildx`, or `buildctl`.

See [Building Images](../builds) for the backend comparison and general options (build args, platform, cache).

## Basic Usage

```bash
rise build myapp:latest --backend docker
rise deploy --backend docker:buildx
```

If no `--backend` is specified and a `Dockerfile` or `Containerfile` is present, Rise uses `docker:buildx` automatically (or `docker:build` if buildx is unavailable).

## Backend Variants

- **`docker` / `docker:build`** — runs `docker build`. Simple and compatible, but no BuildKit features (no SSL cert injection, no secrets).
- **`docker:buildx`** — runs `docker buildx build` via a BuildKit daemon. Supports SSL certificate injection, build secrets, and multi-platform builds. Recommended over `docker:build`.
- **`docker:buildctl` / `buildctl`** — runs `buildctl build` directly. Useful when Docker is not available but a BuildKit daemon is.

## How It Works

- **`docker:build`**: Runs `docker build` with `--build-arg` for environment variables and `--platform` for the target architecture. Does not support SSL certificate injection.
- **`docker:buildx`**: Runs `docker buildx build` via a managed BuildKit daemon. SSL certificates are injected by preprocessing the Dockerfile to add BuildKit bind mounts to each `RUN` step.
- **`buildctl`**: Builds via buildctl. Images are output as a tar archive piped through `docker load` for local use, or pushed directly to the registry when deploying.

## Custom Dockerfile Path

```bash
rise build myapp:latest --dockerfile Dockerfile.prod
```

Or in `rise.toml`:

```toml
[build]
backend = "docker"
dockerfile = "Dockerfile.prod"
```

## Build Contexts (Multi-Stage Builds)

Use additional directories as named build contexts:

```bash
rise build myapp:latest \
  --build-context mylib=../my-library \
  --build-context tools=../build-tools
```

Or in `rise.toml`:

```toml
[build]
backend = "docker"
build_context = "./app"

[build.build_contexts]
mylib = "../my-library"
tools = "../build-tools"
```

Reference named contexts in your Dockerfile:

```dockerfile
COPY --from=mylib /src /app/lib
```

Paths are relative to the `rise.toml` location. Build contexts work with all Docker-based backends.

## SSL and Proxy

For corporate proxy or custom CA certificate setups, see [SSL & Proxy Configuration](../../ssl-proxy). The `docker:buildx` and `buildctl` backends support full SSL certificate injection via the managed BuildKit daemon; `docker:build` does not.
