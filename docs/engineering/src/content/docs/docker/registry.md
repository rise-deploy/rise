---
title: "Container registry"
---

## Container registry (push/pull close-the-loop)

The registry is the part most likely to trip operators up, because the push path
(a developer's machine) and the pull path (the Rise host's Docker daemon) can use
**different URLs for the same registry**.

The `oci-client-auth` registry provider **mints no credentials**
(`src/server/registry/providers/docker.rs` returns empty user/pass for both push
and pull). It assumes the relevant Docker client has already done `docker login`.
The config carries two URLs:

| Config key | Used by | Reference value |
|------------|---------|-----------------|
| `client_registry_url` | `rise deploy` on a developer host (**push**) | `registry.${RISE_DOMAIN}` |
| `registry_url` | the Rise host's Docker daemon (**pull**) | `rise-registry:5000` |

Both URLs point at the **same registry content** — only the network path differs.

### 1. Expose the registry (operator, once)

The base Compose file exposes the registry via Traefik at
`registry.${RISE_DOMAIN}` with TLS (`le`) and a basicauth middleware. Generate an
htpasswd entry and pass it as `REGISTRY_BASIC_AUTH` (escape `$` as `$$` in a
`.env` file):

```bash
htpasswd -nbB ci 's3cret'
# ci:$2y$05$....   →  REGISTRY_BASIC_AUTH='ci:$2y$05$....'  (or $$ in .env)
```

An internet-exposed registry **must** have auth — hence the basicauth
middleware. The internal pull path can stay unauthenticated within the trusted
host network.

### 2. Push (developer host, possibly remote)

Log in with the basicauth credentials, then deploy. The CLI uses the stored
`docker login` — `oci-client-auth` supplies no creds of its own.

```bash
docker login registry.${RISE_DOMAIN}      # basicauth user/pass
rise deploy --project myapp --image ...    # builds + pushes to client_registry_url
```

The push targets `client_registry_url` (`registry.${RISE_DOMAIN}`).

### 3. Pull (Rise host's Docker daemon)

The Rise backend mounts the **host** Docker socket, so every image pull is
executed by the **host's** Docker daemon — not from inside the Compose network.
That daemon resolves `registry_url`'s host with the **host's** resolver, which
does **not** consult Docker's embedded DNS on `rise_default`. The default
`registry_url=rise-registry:5000` therefore does **not** work out of the box on a
production host: `rise-registry` is only resolvable inside the Compose network,
and `:5000` is plain HTTP, which the daemon rejects unless told otherwise. Pick
one of:

- **Internal path (default `registry_url=rise-registry:5000`).** Two host-daemon
  prerequisites:
  1. Make `rise-registry` resolvable by the host daemon — add a host entry
     (e.g. `127.0.0.1 rise-registry` in `/etc/hosts`, or an `extra_hosts` /
     published-port mapping) so it reaches the registry container, **and**
  2. Mark it insecure (plain HTTP): add `"rise-registry:5000"` to the daemon's
     `insecure-registries` in `/etc/docker/daemon.json` and restart Docker.

  No auth is needed on this path — it never crosses the authenticated Traefik
  edge.
- **Host-published loopback.** Publish the registry on a host loopback port (as
  the local overlay does, `127.0.0.1:5000:5000`) and set
  `RISE_REGISTRY_URL=127.0.0.1:5000`. Still requires the matching
  `insecure-registries` entry for `127.0.0.1:5000`.
- **Public path.** Point `RISE_REGISTRY_URL` at the public
  `registry.${RISE_DOMAIN}` (TLS, no insecure-registry needed). The **daemon**
  then needs credentials too: run `docker login registry.${RISE_DOMAIN}` on the
  Rise host, or add a matching entry to the daemon's `~/.docker/config.json`.

Because all three URLs reference the same registry content, an image pushed to
`registry.${RISE_DOMAIN}` is the exact image the daemon pulls.
