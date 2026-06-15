---
title: "Troubleshooting"
---

## Troubleshooting

**Traefik 404s every route / "client version 1.24 is too old".** Traefik must be
new enough to negotiate the host Docker daemon's API version. The reference stack
pins `traefik:v3.7.1`, which negotiates the API directly over the raw socket
(no socket-proxy needed) against Docker 29.x (API 1.54). Older v3.x ship a Docker
client pinned to API 1.24, which the daemon rejects, after which Traefik 404s
every route. Confirm the provider connected:

```bash
docker logs rise-traefik 2>&1 | grep "Provider connection established with docker"
```

**Private app redirect loop.** If an unauthenticated request to a private app
loops instead of landing on a login page, the session cookie is being set on the
wrong host. Verify the `/.rise` router is in place (so the signin page is served
on the app host) and that the `302` `Location` points at
`{app-host}/.rise/auth/signin` — not the control plane. Also confirm
`cookie_secure` matches the scheme (`false` for HTTP local, `true` for HTTPS).

**Backend refuses to start ("…access class(es) … require authentication … but
`auth_backend_url` is empty").** This is the fail-closed guard. Set
`deployment_controller.auth_backend_url` (e.g. `http://rise:3000`) or set the
offending access classes to `access_requirement: None`.

**OIDC issuer mismatch / Dex rejects login in production.** `DEX_ISSUER` and the
issuer Dex actually serves (`dev/dex/config.yaml`) must be identical. See the
[production caveat](/operator-docs/docker/authentication/#production-caveat-important).

**App `404` right after a deploy reports Healthy.** Traefik observes new
containers asynchronously via the Docker provider, so routing lags the API's
"Healthy" mark by a few seconds. A `404` immediately after Healthy usually just
means the route is not registered yet — retry briefly.

**App containers left running after `docker compose down`.** App containers are
created by the Rise reconciler, not by Compose, so `compose down` leaves them.
Remove them by their bookkeeping label:

```bash
docker rm -f $(docker ps -aq --filter "label=rise.dev/managed-by=rise")
```
