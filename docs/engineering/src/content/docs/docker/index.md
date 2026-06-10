---
title: "Docker deployment backend"
---

The Docker deployment backend runs applications as plain Docker containers on a
single host and routes traffic to them with [Traefik](https://traefik.io/)
(Docker provider). It is the lightweight counterpart to the
[Kubernetes backend](/operator-docs/kubernetes/): no cluster, no Helm — just a
Docker daemon and a Compose stack.

A complete, runnable reference stack ships in the repository as
[`docker-compose.standalone.yaml`](https://github.com/rise-deploy/rise/blob/develop/docker-compose.standalone.yaml)
with a local/dev overlay
[`docker-compose.standalone.local.yaml`](https://github.com/rise-deploy/rise/blob/develop/docker-compose.standalone.local.yaml).
This guide explains how the stack is wired and how to run it locally or in
production.

## Two run modes

The same base file serves both production and local/dev; the difference is
whether you layer the overlay, which `RISE_*` env vars are set, and which
Traefik entrypoints exist.

| | Production (base file) | Local / dev (base + overlay) |
|---|---|---|
| Traefik entrypoints | `web` (:80) + `websecure` (:443), 80→443 redirect | `web` (:80) only |
| TLS | Let's Encrypt via the `le` ACME resolver (HTTP-01) | none (plain HTTP) |
| Domain | `${RISE_DOMAIN}` | `rise.localhost` |
| Backend config | shipped `config/docker.yaml` (run_mode `docker`, baked into the image at `/etc/rise/docker.yaml`); nothing is mounted | same shipped file — its built-in defaults are already local |
| Config differences | prod `RISE_*` env on the `rise` service flip it to https / `le` / the real domain | overlay resets those env vars to the local HTTP defaults |
| Public registry / Dex routers | exposed via Traefik | dropped (`labels: !reset []`) |

## Guide contents

- [Architecture](/operator-docs/docker/architecture/) — how the standalone stack is wired, the rolling cutover, and health checks.
- [Quick start](/operator-docs/docker/quick-start/) — the fastest way to try the backend locally over plain HTTP, plus the host-run dev inner loop.
- [Production deployment](/operator-docs/docker/production/) — DNS, TLS/Let's Encrypt, production env vars, hardening, and platform access.
- [Authentication & ingress auth](/operator-docs/docker/authentication/) — access classes, forwardAuth, `/.rise` routing, and the OIDC/Dex setup.
- [Container registry](/operator-docs/docker/registry/) — the push/pull close-the-loop and how to expose, push to, and pull from the registry.
- [Troubleshooting](/operator-docs/docker/troubleshooting/) — common failure modes and how to diagnose them.
