---
title: "Local Development"
---

## Prerequisites

- Docker and Docker Compose
- Rust 1.91+
- [mise](https://mise.jdx.dev/) — task runner and tool version manager
- [direnv](https://direnv.net/) (optional) — auto-loads `.envrc`

## First-Time Setup

```bash
# Install mise-managed tools (minikube, helm, kubectl, etc.)
mise install

# One-stop dev environment setup. Cross-platform (Linux + macOS), idempotent,
# and interactive. Configures /etc/hosts (sudo), Docker insecure registries,
# and brings up a local cluster (minikube by default; offers k3s on Linux).
mise setup
```

`mise setup` is a thin wrapper around `./scripts/dev-setup.sh`. Run individual
steps with `./scripts/dev-setup.sh <hosts|docker|minikube|k3s|preflight>` if
you only need one part. The script writes pod-reachable host URLs to `.env`;
if you use direnv, run `direnv allow` once and `.envrc` will load them.

On macOS, Docker Desktop's daemon config lives at `~/.docker/daemon.json` and
the script updates it there. The script offers to restart Docker Desktop for
you so the new insecure-registries take effect.

## Day-to-Day

```bash
mise dev
```

This single command:

1. **Checks prerequisites** — verifies `/etc/hosts` entries, Docker daemon, and Kubernetes connectivity. If anything is missing it lists the issues and asks whether to continue anyway.
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
| `mise setup minikube` | Bring up Minikube + ingress port-forward + Helm install (**preferred** on most dev machines) |
| `mise setup minikube-down` | `minikube delete` |
| `mise setup k3s` | Bring up k3s (Linux only — use when Minikube doesn't work, or in ephemeral/dedicated Rise dev environments) |
| `mise setup k3s-down` | Uninstall k3s |
| `mise setup pf` | Start the ingress port-forward (`localhost:8080` + `:8443`) in the background |
| `mise setup pf-down` | Stop the ingress port-forward |
| `mise setup preflight` | hosts + docker only, no cluster |

**Minikube vs K3s**: Minikube is preferred on most developer machines — it runs a single-node Kubernetes cluster inside a Docker container, starts quickly, and integrates well with the local Docker network so pods can reach `rise-registry:5000`. Use k3s (Linux only) when Minikube doesn't work on your machine (e.g., nested virtualization issues) or when setting up an ephemeral or dedicated environment solely for Rise development where the slight extra k3s overhead doesn't matter.

### Development

| Task | Purpose |
|------|---------|
| `dev` | Full dev stack (preflight prompt + services + frontend + backend) |
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
