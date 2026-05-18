---
title: "Pack (Cloud Native Buildpacks)"
---

The `pack` backend builds container images using [Cloud Native Buildpacks](https://buildpacks.io/) via the `pack` CLI — no Dockerfile needed.

See [Building Images](../builds) for the backend comparison and general options (build args, platform, cache).

## Prerequisites

Install the `pack` CLI:

```bash
mise use -g ubi:buildpacks/pack
```

## Basic Usage

```bash
rise build myapp:latest --backend pack
rise deploy --backend pack --builder heroku/builder:24
```

## How It Works

Rise runs `pack build` with `--docker-host inherit --network host`. Environment variables are passed via `--env KEY=VALUE`. SSL certificates are volume-mounted to all common distro paths (Debian, RedHat, Alpine, etc.) with matching SSL environment variables set (`SSL_CERT_FILE`, `NODE_EXTRA_CA_CERTS`, etc.).

## Configuration

```toml
[build]
backend = "pack"
builder = "heroku/builder:24"
buildpacks = ["heroku/nodejs", "heroku/procfile"]
```

| Field | Description |
|-------|-------------|
| `builder` | Buildpacks builder image (default: determined by `pack`) |
| `buildpacks` | Explicit list of buildpacks to use |

## SSL Compatibility

SSL certificate injection depends on the builder image. `heroku/builder:24` correctly respects `SSL_CERT_FILE`. Paketo builders do not — their buildpack binaries use statically-linked Go TLS that doesn't read system CA bundles.

See [SSL & Proxy Configuration](../../ssl-proxy) for details.
