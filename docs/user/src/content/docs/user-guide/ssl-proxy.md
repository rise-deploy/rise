---
title: "SSL & Proxy Configuration"
---

When building behind corporate proxies (Zscaler, Cloudflare) or in environments with custom CA certificates, builds may fail with SSL certificate verification errors. Rise provides several mechanisms to handle this.

## Quick Start

For most corporate proxy setups, you need two things:

```bash
# Point to your custom CA certificate bundle
export SSL_CERT_FILE=/path/to/ca-bundle.crt

# Set your proxy (Rise auto-detects these)
export HTTPS_PROXY=http://proxy.corp.example:3128

# Deploy with managed BuildKit (recommended for SSL support)
rise deploy --managed-buildkit
```

Rise will automatically inject the certificate into builds and transform proxy URLs for container environments.

## Managed BuildKit Daemon

Rise can manage a BuildKit daemon container with SSL certificates automatically mounted.

### Auto-Detection (Default)

When no explicit setting is provided, managed BuildKit **automatically enables** if:

1. The build method requires BuildKit (`docker:buildx`, `buildctl`, `railpack`)
2. `SSL_CERT_FILE` is set in the environment
3. `BUILDKIT_HOST` is not already set

This means in most corporate/proxy environments, managed BuildKit "just works" without any flags.

### Disabling

If you don't need managed BuildKit (e.g., you manage your own daemon or don't need SSL certificate support), disable it explicitly:

```bash
# CLI flag
rise deploy --managed-buildkit=false

# Environment variable
export RISE_MANAGED_BUILDKIT=false

# rise.toml
[build]
managed_buildkit = false
```

### Force Enabling

To force managed BuildKit even without `SSL_CERT_FILE` (e.g., for insecure registries):

```bash
rise deploy --managed-buildkit
```

### How It Works

1. If `BUILDKIT_HOST` is already set, Rise uses your existing daemon
2. Otherwise, Rise creates a `rise-buildkit` container with:
   - SSL certificates mounted (if `SSL_CERT_FILE` is set)
   - `--add-host host.docker.internal:host-gateway` for host network access
   - Proxy environment variables passed through to the daemon
   - `--cgroupns=host` when running under Podman (see [Container Runtime Differences](#container-runtime-differences))
3. The daemon is automatically recreated when its configuration changes (see below)

### Daemon Lifecycle

The managed daemon is recreated when any of the following change:

- SSL certificate content
- Proxy variable values (`HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY` and lowercase variants)
- Network configuration (`RISE_MANAGED_BUILDKIT_NETWORK_NAME`)
- Insecure registry configuration (`RISE_MANAGED_BUILDKIT_INSECURE_REGISTRIES`)
- Internal Rise version updates (e.g., new daemon flags)

You don't need to manually stop or restart the daemon — Rise handles this automatically.

### Backend Compatibility

| Backend | Managed BuildKit | Notes |
|---------|:---:|-------|
| `pack` | N/A | SSL support depends on builder; heroku/builder:24 works, paketo builders don't respect `SSL_CERT_FILE` for buildpack-level downloads |
| `docker` / `docker:build` | No | Use `docker:buildx` for SSL support |
| `docker:buildx` | Yes | Full SSL via BuildKit secrets |
| `buildctl` | Yes | Full SSL via BuildKit secrets |
| `railpack` / `railpack:buildx` | Yes | Benefits from managed daemon |
| `railpack:buildctl` | Yes | Benefits from managed daemon |

## SSL Certificate Injection (Docker/Buildctl)

When using `docker:buildx` or `buildctl` with `SSL_CERT_FILE` set, Rise automatically injects certificates into Dockerfile `RUN` commands using BuildKit bind mounts.

Rise preprocesses your Dockerfile to mount certificates at standard system paths during each `RUN` command, and exports SSL environment variables (`SSL_CERT_FILE`, `NODE_EXTRA_CA_CERTS`, `REQUESTS_CA_BUNDLE`, etc.) so build tools can find the certificates.

Certificates are only available during build — they are **not** embedded in the final image.

The `docker:build` backend does not support this feature. Use `docker:buildx` instead.

## SSL Certificate Embedding (Railpack)

For Railpack builds, when `SSL_CERT_FILE` is set, Rise automatically embeds the certificate into the Railpack build plan. This ensures `RUN` commands during the build can access the certificate for SSL verification.

Unlike the Docker injection above, this **does** embed the certificate in the final image. In addition to embedding the cert file, this also injects SSL environment variables (`SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`, `NODE_EXTRA_CA_CERTS`, etc.) as build secrets so that build-time package managers can find the certificates.

When `SSL_CERT_FILE` is set, managed BuildKit also auto-enables, giving comprehensive SSL support at both the daemon and build level:

```bash
rise deploy --backend railpack
```

## Proxy Support

Rise automatically detects and injects HTTP/HTTPS proxy variables into all build backends:

- `HTTP_PROXY` / `http_proxy`
- `HTTPS_PROXY` / `https_proxy`
- `NO_PROXY` / `no_proxy`

Both uppercase and lowercase variants are detected from your environment.

### Localhost Transformation

Since builds run in containers, `localhost` and `127.0.0.1` addresses are automatically transformed to `host.docker.internal`:

- `http://localhost:3128` → `http://host.docker.internal:3128`
- `https://127.0.0.1:8080` → `https://host.docker.internal:8080`

`NO_PROXY` values are passed through unchanged.

No configuration is needed — proxy support is fully automatic.

### How Proxy Variables Are Injected Per Backend

Each backend uses a different mechanism to pass proxy variables into the build:

| Backend | Mechanism | Example |
|---------|-----------|---------|
| `docker` / `docker:build` | `--build-arg` | `--build-arg HTTPS_PROXY=http://...` |
| `docker:buildx` | `--build-arg` | `--build-arg HTTPS_PROXY=http://...` |
| `pack` | `--env` | `--env HTTPS_PROXY=http://...` |
| `railpack:buildx` | BuildKit secrets | `--secret id=HTTPS_PROXY,env=_RISE_SECRET_HTTPS_PROXY` |
| `railpack:buildctl` / `buildctl` | BuildKit secrets | `--secret id=HTTPS_PROXY,env=_RISE_SECRET_HTTPS_PROXY` |

The `_RISE_SECRET_` prefix in the railpack/buildctl backends exists because the `docker` CLI itself reads proxy variables from its own environment. Since `--secret id=KEY,env=KEY` reads from the subprocess environment, storing the *transformed* proxy value (with `host.docker.internal`) under the original name would interfere with the CLI's own proxy settings. The prefixed name avoids this conflict while BuildKit sees the secret under the original key name.

### Host Gateway Resolution

When using `docker:buildx` with a managed BuildKit daemon, Rise needs to resolve a concrete IP address for `host.docker.internal`. This is necessary because the buildx remote driver cannot resolve the `host-gateway` magic value.

Resolution order:

1. **Primary**: Read `/etc/hosts` from inside the BuildKit daemon container to find the `host.docker.internal` entry
2. **Fallback**: Use `NetworkSettings.Gateway` from `docker inspect`

The `/etc/hosts` method is preferred because it correctly handles VM layers. With Podman Machine, `NetworkSettings.Gateway` returns the `podman0` bridge IP *inside* the VM, which is not reachable from the host. The `/etc/hosts` file contains the actual host IP as resolved by the container runtime's `--add-host host.docker.internal:host-gateway` flag.

## Container Runtime Differences

### Docker Desktop

Works out of the box. `host.docker.internal` is natively supported, and BuildKit runs without additional flags.

### Podman / Podman Machine

Rise detects Podman and applies the following adjustments automatically:

- **`--cgroupns=host`** is added to the managed BuildKit daemon container. This is needed for cgroup v2 memory controller delegation — without it, runc can fail inside the BuildKit container.
- **`--push` fallback**: Some Podman buildx implementations don't support the `--push` flag. Rise detects this and falls back to building with `--load` followed by a separate `docker push` step.
- **Host gateway IP**: Resolved from `/etc/hosts` inside the daemon container rather than `NetworkSettings.Gateway`, which returns an unreachable VM-internal bridge IP when using Podman Machine (see [Host Gateway Resolution](#host-gateway-resolution)).
- **Container cleanup**: The managed daemon is removed with `rm -f` instead of `stop` because Podman does not always clean up `--rm` containers reliably on stop.

## BuildKit Network Connectivity

When using the managed BuildKit daemon with Docker Compose services (e.g., a local registry), connect BuildKit to your compose network:

```bash
export RISE_MANAGED_BUILDKIT_NETWORK_NAME=rise_default
rise deploy
```

The daemon is recreated if the network name changes.

## Insecure Registries (Local Development)

For local HTTP registries, configure BuildKit to allow insecure connections:

```bash
export RISE_MANAGED_BUILDKIT_INSECURE_REGISTRIES="rise-registry:5000,localhost:5000"
rise deploy --managed-buildkit
```

Note: `--managed-buildkit` is needed here since insecure registries typically don't involve `SSL_CERT_FILE`, so auto-detection won't enable it.

This generates a `buildkitd.toml` config at `~/.rise/buildkitd.toml`. For local development only.

## Manual BuildKit Setup

For manual control, create your own BuildKit daemon:

```bash
docker run --platform linux/amd64 --privileged --name my-buildkit --rm -d \
  --volume $SSL_CERT_FILE:/etc/ssl/certs/ca-certificates.crt:ro \
  moby/buildkit

export BUILDKIT_HOST=docker-container://my-buildkit
rise deploy --backend railpack
```

## Known Limitations

- **`docker:build` does not support SSL cert injection.** Use `docker:buildx` instead, which uses BuildKit secrets to mount certificates during `RUN` steps.
- **Paketo buildpacks ignore `SSL_CERT_FILE` for their own downloads.** Paketo buildpack binaries use statically-linked Go TLS which doesn't read system CA bundles. Use `heroku/builder:24` instead, which correctly respects `SSL_CERT_FILE`.
- **`buildctl` non-push builds pipe through `docker load`.** When building locally (not pushing directly to a registry), `buildctl` outputs a tar archive that is loaded via `docker load`, which is slower than `docker buildx` for local development.
