# Multi-Container Example — Redis-Backed Job Queue

A working four-container app that needs every piece to do its job:

| Container  | Build              | port      | Replicas | Role                                                |
|------------|--------------------|-----------|----------|-----------------------------------------------------|
| `frontend` | `frontend/`        | 8080      | 1        | Static page that submits text and polls job status  |
| `api`      | `api/`             | 8080      | 2        | Stateless JSON API; reads/writes job state in Redis |
| `worker`   | `worker/`          | —         | 1        | `BRPOP`s pending jobs, analyzes text, writes result |
| `redis`    | `redis:7-alpine`   | 6379      | 1        | Shared queue + job-state store                      |

```
browser ──ingress──► api ──┐
                           ├─► redis
              worker ──────┘
```

## Cross-container service discovery

The deployment controller injects one env var per routable sibling container:

```
RISE_CONTAINER_HOST__<NAME>=<host>:<port>
```

Both `api` and `worker` read `RISE_CONTAINER_HOST__REDIS` to find Redis —
no hardcoded service names, no Kubernetes-specific DNS knowledge. The format
is host:port (no scheme), so callers add their own protocol:
`redis://${RISE_CONTAINER_HOST__REDIS}` here,
`http://${RISE_CONTAINER_HOST__API}` if the worker needed to call the API.

## Path-based ingress

```toml
[routes]
"/api" = { container = "api" }
"/" = { container = "frontend" }
```

Longest-prefix wins, so `/api/jobs` reaches the `api` container with its
original path (no rewrite) and everything else falls through to `frontend`.
The `worker` container has no `port` — Deployment but no Service, no
ingress route, no HTTP probes. Redis has a `port` (it need not be HTTP — the
Service routes TCP fine) but isn't in `[routes]`, so it gets a Service for
discovery and no HTTP probes (only routed containers are probed by default).

## Deploy

```bash
rise project create multi-container
rise deployment create multi-container example/multi-container
```

The CLI builds and pushes `frontend`, `api`, and `worker` in `[containers]`
order using a single set of registry credentials whose scope covers every
tag. `redis` declares `image = "redis:7-alpine"`, so it's passed through
unchanged with no build step.

## Try it

```bash
open https://multi-container.rise.dev/

# Tail the worker as you submit jobs from the UI
rise deployment logs multi-container --container worker --follow
```

Submit text in the UI; watch a row appear as `pending`, flip to `processing`
(with the worker's PID), then `completed` with word counts and the top
words. The worker artificially sleeps for `WORK_MS` (default 2s) so the
state transitions are observable.

You can also drive it from the API directly:

```bash
curl -X POST https://multi-container.rise.dev/api/jobs \
  -H 'content-type: application/json' \
  -d '{"text": "the quick brown fox jumps over the lazy dog"}'

curl https://multi-container.rise.dev/api/jobs
```

## Notes

- `[containers]` is mutually exclusive with top-level `[build]` / `[deploy]`.
  Existing single-container projects keep working — their top-level
  `[build]` is treated as an implicit `app` container.
- Redis here has no persistent volume, so pending jobs are lost on Redis
  pod restart. Fine for a demo; a real app would attach a PVC or use
  managed Redis.
- The API can scale horizontally (replicas = 2 above) because all state
  lives in Redis. Worker stays at 1 for the demo, but BRPOP is safe to fan
  out — bump replicas to fan-out processing.
- Routes are not rewritten — the path you ingress on is the path the
  container receives.
