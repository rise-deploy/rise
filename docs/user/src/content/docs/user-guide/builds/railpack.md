---
title: "Railpack"
---

The Railpack backend builds container images using [Railway Railpacks](https://railpack.io/) with BuildKit — no Dockerfile needed. Railpack auto-detects your application type and generates an optimized build plan.

See [Building Images](../builds) for the backend comparison and general options (build args, platform, cache).

## Prerequisites

Install the `railpack` CLI:

```bash
mise use -g ubi:railwayapp/railpack
```

## Basic Usage

```bash
rise build myapp:latest --backend railpack
rise deploy --backend railpack:buildctl
```

Railpack is the default backend when no Dockerfile or Containerfile is present.

## Backend Variants

- **`railpack` / `railpack:buildx`** — uses `railpack prepare` then `docker buildx build`
- **`railpack:buildctl`** — uses `railpack prepare` then `buildctl build` (no Docker daemon needed)

## How It Works

Railpack builds are two steps:

1. **Prepare**: `railpack prepare` generates a build plan (JSON) from your application code. Environment variables and secrets are declared at this stage.
2. **Build**: The plan is built using either `docker buildx build` (with the `railpack-frontend` BuildKit frontend) or `buildctl build`. Environment variables are passed as BuildKit secrets, not build args.

:::note
If builds fail with `requested experimental feature mergeop has been disabled`, the default buildx builder is too old for Railpack. Rise's managed BuildKit daemon resolves this — see [SSL & Proxy Configuration](../../ssl-proxy#managed-buildkit-daemon) to enable it, or force it with `rise deploy --managed-buildkit`.
:::
