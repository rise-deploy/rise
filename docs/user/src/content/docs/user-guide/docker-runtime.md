---
title: "Docker Runtime"
---

The Docker runtime deploys your app as a container on a single Docker host, with
[Traefik](https://traefik.io/) handling routing. It is a lighter-weight alternative to
the Kubernetes runtime, well suited to local development and small, single-host
deployments. From your point of view, `rise deploy`, `rise stop`, `rise logs`, and
status all work exactly the same — only the operator's runtime configuration differs.

## Prerequisites

- A Docker daemon.
- A running Traefik instance with the Docker provider enabled (configured by your
  operator). For local development, bring it up with the bundled compose stack:

  ```bash
  docker compose up -d traefik
  ```

  The Traefik dashboard is then available at <http://localhost:8090>.

## URLs

Apps are served at hostnames derived from the operator's URL templates, e.g.
`https://<project>.<domain>`. Locally, the default templates use the `*.localhost`
suffix, which resolves to `127.0.0.1` automatically — no `/etc/hosts` edits needed:

- Production: `http://<project>.rise.localhost`
- Staging group: `http://<group>--<project>.rise.localhost`
- Per-environment: `http://<env>--<project>.rise.localhost`

## TLS

When your operator configures a Traefik certresolver, Traefik issues and renews TLS
certificates automatically and apps are served over HTTPS. Local development typically
uses plain HTTP (no certresolver).

## Differences vs. Kubernetes

- **Single host** — there is no cluster; everything runs on one Docker host.
- **`replicas=1`** — each deployment runs as exactly one container; horizontal scaling
  is not available on this runtime.
- **Env vars** — environment variables are passed as plain values and are visible via
  `docker inspect <container>`.
- **Routing** — handled by Traefik labels on the container instead of a Kubernetes
  Ingress.

## Example

```bash
rise deploy
# ...
# Deployment successful! Your app is now running at
# http://my-app.rise.localhost
```

Open <http://my-app.rise.localhost> in your browser to reach the deployed app.
