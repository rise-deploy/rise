---
title: "Local Development"
---

## Prerequisites

- Docker and Docker Compose
- Rust 1.91+
- [mise](https://mise.jdx.dev/) — task runner and tool version manager
- [direnv](https://direnv.net/) (optional) — auto-loads `.envrc`

## First-Time Setup

Rise has two deployment backends, and either one can be your local dev
environment — both are first-class and fully supported. Choose the backend that
fits what you're working on. Install the mise-managed tools once (both paths
need them), then follow **Path A (Kubernetes)** or **Path B (Docker)**:

```bash
# Install mise-managed tools (minikube, helm, kubectl, etc.) — both paths
mise install
```

### Path A — Kubernetes backend

The original backend. Pick it to work on the Kubernetes controller / Helm /
metacontroller, or if you already run a cluster.

```bash
# One-stop dev setup. Cross-platform (Linux + macOS), idempotent, interactive.
# Configures /etc/hosts (sudo), Docker insecure registries, and brings up a
# local cluster (minikube by default; offers k3s on Linux).
mise setup

mise frontend:dev   # Vite dev server
mise backend:run    # alias: mise br — host backend against the K8s dev config
```

`mise setup` is a thin wrapper around `./scripts/dev-setup.sh`. Run individual
steps with `./scripts/dev-setup.sh <hosts|docker|minikube|k3s|preflight>` if
you only need one part. The script writes pod-reachable host URLs to `.env`;
if you use direnv, run `direnv allow` once and `.envrc` will load them.

On macOS, Docker Desktop's daemon config lives at `~/.docker/daemon.json` and
the script updates it there. The script offers to restart Docker Desktop for
you so the new insecure-registries take effect.

Apps deploy to the cluster and are reachable at `*.rise.local` (resolved via the
`/etc/hosts` + host-IP wiring `mise setup` provisions).

### Path B — Docker backend

A lighter single-host setup with no cluster — great for backend/frontend feature
work and for the Docker deployment backend itself.

```bash
# Adds ONLY the /etc/hosts aliases (incl. rise-dex → 127.0.0.1). No cluster,
# no minikube — the full `mise setup` is not needed for this path.
mise setup hosts

# Brings up the compose support services + runs Rise on the host with the
# Docker backend (reuses the env-driven config/docker.yaml; no Rise image build).
mise backend:run-docker   # alias: mise br-docker

mise frontend:dev         # optional, in another terminal
```

The registry uses `localhost:5000` (insecure-by-default in Docker — no
`daemon.json` change), and apps are reachable at `*.rise.localhost` (loopback
per RFC 6761, so no `/etc/hosts` edits for app hosts). The only setup `mise br-docker`
needs is the `rise-dex` host alias from `mise setup hosts`. See
[Docker backend (local)](#docker-backend-local) below and the operator guide's
[Local development](/operator-docs/docker/#local-development-run-rise-on-the-host-no-image)
section for the full reference.

## Day-to-Day

On the **Kubernetes** path, one command runs the full stack:

```bash
mise dev
```

This single command:

1. **Checks prerequisites** — verifies `/etc/hosts` entries, Docker daemon, and Kubernetes connectivity. If anything is missing it lists the issues and asks whether to continue anyway. (The cluster is only needed for the Kubernetes backend, `mise br` — a Docker-only dev can ignore that line.)
2. **Starts Docker Compose services** — PostgreSQL, Dex (OIDC), container registry.
3. **Runs database migrations.**
4. **Starts the Vite frontend dev server** (background).
5. **Starts the backend server.**

On the **Docker** path, run `mise br-docker` (plus `mise frontend:dev` if you
need the UI) instead — see [Docker backend (local)](#docker-backend-local).

Services are then available at:

| Service | URL |
|---------|-----|
| Backend API + Web UI | <http://rise.local:3000> |
| PostgreSQL | `localhost:5432` |
| Container registry | `localhost:5000` |
| Registry UI | <http://localhost:5001> |
| Kubernetes ingress (HTTP) | `localhost:8080` |
| Kubernetes ingress (HTTPS) | `localhost:8443` |

### Running Components Individually

```bash
mise backend:run         # (alias: mise br) — K8s backend: starts deps + migrations + backend
mise backend:run-docker  # (alias: mise br-docker) — Docker backend on the host (no image build)
mise frontend:dev        # Vite dev server only
```

## Mise Tasks Reference

### Setup (one-time, idempotent)

| Task | Purpose |
|------|---------|
| `setup` | One-stop dev setup: hosts + docker + cluster (interactive) |
| `down` | Undo what `setup` did (kills port-forward, deletes cluster, strips .env block + daemon.json registries + /etc/hosts block) |

Everything is driven by `./scripts/dev-setup.sh`. `mise setup` and `mise down` are convenience wrappers; positional args pass through, so any script subcommand works as `mise setup <subcmd>`:

| Invocation | What it does |
|------------|--------------|
| `mise setup hosts` | Rewrite the managed `/etc/hosts` block (base hosts + `*.rise.local` ingress hosts enumerated from the current cluster) |
| `mise setup hosts-clear` | Remove the managed `/etc/hosts` block |
| `mise setup docker` | Configure Docker insecure-registries (asks to restart Docker Desktop on macOS) |
| `mise setup docker-clear` | Remove rise registries from `daemon.json` |
| `mise setup minikube` | Bring up Minikube + ingress/Loki port-forwards + Helm install (**preferred** on most dev machines) |
| `mise setup minikube-down` | `minikube delete` |
| `mise setup k3s` | Bring up k3s (Linux only — use when Minikube doesn't work, or in ephemeral/dedicated Rise dev environments) |
| `mise setup k3s-down` | Uninstall k3s |
| `mise setup pf` | Start the ingress port-forward (`localhost:8080` + `:8443`) in the background |
| `mise setup pf-down` | Stop the ingress port-forward |
| `mise setup loki-pf` | Start the Loki port-forward (`localhost:3100`) in the background |
| `mise setup loki-pf-down` | Stop the Loki port-forward |
| `mise setup preflight` | hosts + docker only, no cluster |

**Minikube vs K3s**: Minikube is preferred on most developer machines — it runs a single-node Kubernetes cluster inside a Docker container, starts quickly, and integrates well with the local Docker network so pods can reach `rise-registry:5000`. Use k3s (Linux only) when Minikube doesn't work on your machine (e.g., nested virtualization issues) or when setting up an ephemeral or dedicated environment solely for Rise development where the slight extra k3s overhead doesn't matter.

### Development

| Task | Purpose |
|------|---------|
| `dev` | Full dev stack, Kubernetes backend (preflight prompt + services + frontend + backend) |
| `backend:run` (alias `br`) | Kubernetes-backend host server only (starts deps + migrates) |
| `backend:run-docker` (alias `br-docker`) | Docker-backend host server (support services + Rise on the host; no image build, no cluster) |
| `frontend:dev` | Vite frontend dev server |
| `db:migrate` | Run database migrations |
| `db:nuke` | Drop and recreate the database |
| `docs:serve` | Serve bundled user docs with Starlight (port 3001) |
| `docs:engineering:serve` | Serve engineering docs with Starlight (port 3002) |

### CI / Quality

| Task | Purpose |
|------|---------|
| `lint` | clippy + fmt check + sqlx check + helm lint |
| `sqlx:prepare` | Regenerate SQLX offline query cache |
| `sqlx:check` | Verify SQLX queries are valid |
| `config:schema:generate` / `check` | Backend settings JSON schema |
| `crd:generate` / `check` | CRD YAML from Rust definition |

## Development Workflow

**Backend** — edit code, then restart with `mise backend:run`.

**Frontend** — Vite hot-reloads automatically. The backend proxies frontend routes to `http://localhost:5173` when `server.frontend_dev_proxy_url` is configured.

**CLI:**
```bash
cargo build --bin rise
rise <command>
```

**Database schema:**
```bash
sqlx migrate add <migration_name>
# Edit the new migration in migrations/
sqlx migrate run
cargo sqlx prepare   # update offline query cache, commit the result
```

## Registry Configuration

The local setup uses two registry URLs:

- **`rise-registry:5000`** — used by deployment controllers (inside Docker/Kubernetes networks)
- **`localhost:5000`** — used by the CLI on the host for push operations

This is configured in `config/development.yaml`:

```yaml
registry:
  type: "oci-client-auth"
  registry_url: "rise-registry:5000"
  namespace: "rise-apps/"
  client_registry_url: "localhost:5000"
```

## Environment Variables

`.envrc` (loaded by direnv) sets: `DATABASE_URL`, `RISE_CONFIG_RUN_MODE`, `RISE_MANAGED_BUILDKIT_*`, and `PATH`.

Server configuration lives in `config/development.yaml`.

## Default Credentials

| Service | Credentials |
|---------|-------------|
| PostgreSQL | `postgres://rise:rise123@localhost:5432/rise` |
| Dex (OIDC) | `admin@example.com`, `dev@example.com`, `user@example.com` — password: `password` |

## Networking Overview

```
Host Machine (127.0.0.1)
├── rise.local:3000     → Rise Backend
├── localhost:5173      → Vite dev server
├── localhost:8080/8443 → K8s ingress (port-forward or hostPort)
├── localhost:3100      → Loki (port-forward)
│
├── Docker network: rise_default
│   ├── rise-postgres       (5432)
│   ├── rise-dex            (5556)
│   ├── rise-registry       (5000)
│   ├── rise-buildkit       (managed, joins this network)
│   └── minikube node       (connected to this network)
│
└── Kubernetes cluster
    └── Pods pull from rise-registry:5000 via network connectivity
```

- BuildKit connects to the `rise_default` Docker network (via `RISE_MANAGED_BUILDKIT_NETWORK_NAME`) so it can push to `rise-registry:5000`.
- Minikube joins the same network so pods can pull images.
- The local cluster setup writes `.env` values such as `RISE_AUTH_BACKEND_URL`
  and `RISE_K8S_HOST_IP` so Kubernetes pods can reach the host backend.
- Runtime logs use the local Loki backend by default. `mise setup` installs the
  dev Loki/Alloy chart components and forwards Loki to `localhost:3100`, which
  `config/development.yaml` reads through `RISE_LOKI_URL`.

## Docker backend (local)

The Docker backend deploys to a single Docker host with Traefik for routing — the
lightest local setup, with no cluster. Run it on the host with no Rise image build:

```bash
mise setup hosts        # one-time: adds /etc/hosts aliases (incl. rise-dex)
mise backend:run-docker # alias: mise br-docker
```

`mise br-docker` brings up only the compose support services (Postgres, Dex,
registry, Traefik — **no** Rise container) and runs Rise on the host against the
env-driven `config/docker.yaml` (run_mode `docker`). It is the Docker-backend
analog of `mise br`. You no longer hand-edit `config/development.yaml` to switch
controllers — the task selects the Docker controller via run_mode. See the
operator guide's
[Local development](/operator-docs/docker/#local-development-run-rise-on-the-host-no-image)
section for the full reference (gateway-IP / `extra_hosts` wiring, Docker Desktop
notes, etc.).

How it works:

- The backend runs an **in-process reconcile loop** (replacing Metacontroller): it
  enumerates projects, drives deployment state transitions, diffs the desired state
  against the actual Rise-labelled containers, creates/recreates/garbage-collects
  them, and HTTP-probes for health — all on the shared `rise_default` network. There
  is **no webhook listener** and no cluster.
- **`replicas=1` caveat**: the Docker runtime clamps every deployment to a single
  container; horizontal scaling is a follow-up.
- Env vars are passed as plain `KEY=VALUE` and are therefore visible via
  `docker inspect <container>` (Docker has no secret primitive) — acceptable for the
  single-host scope.
- Apps are reachable at `http://<project>.rise.localhost` (and the
  `<group>--<project>` / `<env>--<project>` variants) with no DNS or `/etc/hosts`
  edits, since `*.localhost` resolves to `127.0.0.1`.

## Troubleshooting

**`http: server gave HTTP response to HTTPS client`** — insecure registries not configured. Run `./scripts/dev-setup.sh docker`.

**BuildKit can't push to registry** — verify `RISE_MANAGED_BUILDKIT_NETWORK_NAME=rise_default` is set in your environment (should be in `.envrc`).

**OAuth redirects fail** — ensure `rise.local` is in `/etc/hosts` (`mise check:hosts` will tell you).

**Minikube pods `ImagePullBackOff`** — verify registry access from inside Minikube:
```bash
minikube ssh -- curl http://rise-registry:5000/v2/
# Should return: {}
```
If it fails, re-run `mise setup minikube`.

**Reset everything:**
```bash
docker compose down -v
cargo clean
mise install
mise dev
```

## Accessing the Database

```bash
docker compose exec postgres psql -U rise -d rise
# or: psql postgres://rise:rise123@localhost:5432/rise
```
