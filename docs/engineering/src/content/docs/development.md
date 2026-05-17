---
title: "Local Development"
---

# Local Development

## Prerequisites

- Docker and Docker Compose
- Rust 1.91+
- [mise](https://mise.jdx.dev/) — task runner and tool version manager
- [direnv](https://direnv.net/) (optional) — auto-loads `.envrc`

## First-Time Setup

```bash
# Install mise-managed tools (minikube, helm, kubectl, etc.)
mise install

# Configure /etc/hosts and Docker daemon (idempotent, requires sudo).
# On WSL with Docker Desktop, setup:docker skips cleanly if /etc/docker is not present.
mise setup:hosts
mise setup:docker

# Start a local Kubernetes cluster (pick one)
mise minikube:up   # or: mise k3s:up
```

The cluster setup task writes pod-reachable host URLs to `.env`. If you use
direnv, run `direnv allow` once and `.envrc` will load those values
automatically.

## Day-to-Day

```bash
mise dev
```

This single command:

1. **Checks prerequisites** — verifies `/etc/hosts` entries, Docker daemon, and Kubernetes connectivity. If anything is missing it tells you exactly what to run.
2. **Starts Docker Compose services** — PostgreSQL, Dex (OIDC), container registry.
3. **Runs database migrations.**
4. **Starts the Vite frontend dev server** (background).
5. **Starts the backend server.**

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
mise backend:run   # (alias: mise br) — starts deps + migrations + backend
mise frontend:dev  # Vite dev server only
```

## Mise Tasks Reference

### Checks (run automatically by `mise dev`)

| Task | Purpose |
|------|---------|
| `check:hosts` | Verify `/etc/hosts` has `rise-registry` and `rise.local` |
| `check:docker` | Verify Docker is running and insecure registries are configured |
| `check:k8s` | Verify a Kubernetes cluster is reachable via `kubectl` |

### Setup (one-time, idempotent)

| Task | Purpose |
|------|---------|
| `setup:hosts` | Add `rise-registry` and `rise.local` to `/etc/hosts` |
| `setup:docker` | Configure Docker daemon insecure registries |
| `minikube:up` | Start Minikube with registry access and ingress port-forwarding |
| `minikube:down` | Stop and delete Minikube |
| `k3s:up` / `k3s:down` | Alternative: K3s instead of Minikube |

### Development

| Task | Purpose |
|------|---------|
| `dev` | Full dev stack (checks + services + frontend + backend) |
| `backend:run` | Backend only (starts deps + migrates) |
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

## Troubleshooting

**`http: server gave HTTP response to HTTPS client`** — insecure registries not configured. Run `mise setup:docker`.

**BuildKit can't push to registry** — verify `RISE_MANAGED_BUILDKIT_NETWORK_NAME=rise_default` is set in your environment (should be in `.envrc`).

**OAuth redirects fail** — ensure `rise.local` is in `/etc/hosts` (`mise check:hosts` will tell you).

**Minikube pods `ImagePullBackOff`** — verify registry access from inside Minikube:
```bash
minikube ssh -- curl http://rise-registry:5000/v2/
# Should return: {}
```
If it fails, re-run `mise minikube:up`.

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
