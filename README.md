# Rise <img src="static/assets/favicon-32x32.png" height="32px" align="right"/> 

<p align="center">Rise is a Kubernetes-based platform for deploying containerized apps via a simple CLI and web dashboard.</p>
<p align="center"><small><em>DISCLAIMER: Rise is an early work-in-progress project that mostly uses AI-generated code.</em></small></p>

<p align="center">
  <img src="docs/screenshots/dashboard-projects.png" width="100%" alt="Projects dashboard"/>
</p>
<p align="center">
  <img src="docs/screenshots/dashboard-project-detail.png" width="49%" alt="Project detail view"/>
  <img src="docs/screenshots/dashboard-teams.png" width="49%" alt="Teams view"/>
</p>

[Go to Documentation → ](https://niklasrosenstein.github.io/rise/)

## What is Rise?

  [pack]: https://buildpacks.io/docs/for-platform-operators/how-to/integrate-ci/pack/
  [railpack]: https://railpack.com/

Rise simplifies container deployment by providing:

- **Simple CLI** for building and deploying apps
    - **Buildpack support** with [pack] and [railpack]
    - **Enterprise ready** with support for corparate MITM proxies (handles `SSL_CERT_FILE` and `HTTPS_PROXY` forwarding)
- **Web dashboard** for monitoring deployments
- **Project & Team Management**: Organize apps and collaborate with teams
- **OAuth2/OIDC Authentication**: Secure authentication for Rise and deployed apps
- **Multi-tenant projects** with team collaboration
- **Automatic OCI repository provisioning**: Push images to AWS ACR with secure temporary credentials without per-project infrastructure setup
- **Service Accounts**: Workload identity for GitHub Actions, GitLab CI, etc. to deploy from CI/CD

## Install CLI

```bash
# Download the latest pre-built binary from GitHub Releases
mise use -g ubi:rise-deploy/rise

# Verify installation
rise --version
```

You can also download a binary directly from the
[GitHub Releases](https://github.com/rise-deploy/rise/releases) page.

## Local Development

### Prerequisites

- Docker (or Docker Desktop on macOS) and Docker Compose
- Rust 1.91+
- [mise](https://mise.jdx.dev/) (recommended for development)

### Start Services

```bash
direnv allow
# or else use `. .envrc`

# Install development tools (both paths need this)
mise install
```

Rise supports two deployment backends, and you can run either one locally as a
first-class dev environment. Choose the backend that fits what you're working
on — both are fully supported.

#### Option A — Kubernetes backend

The original backend. Pick it to work on the Kubernetes controller / Helm /
metacontroller, or if you already run a cluster.

```bash
# One-stop dev setup (cross-platform: Linux + macOS). Configures /etc/hosts
# (sudo), Docker insecure registries, and brings up a local cluster (minikube
# by default; k3s on Linux). Run interactively so the sudo prompt works.
mise setup

# Terminal (1): frontend
mise frontend:dev

# Terminal (2): host backend against the Kubernetes dev config (starts deps)
mise backend:run   # alias: mise br
```

`mise setup` invokes `./scripts/dev-setup.sh`; you can run individual steps
with `mise setup <hosts|docker|minikube|k3s|preflight>` (mise passes the
subcommand through, or run `./scripts/dev-setup.sh <...>` directly). Apps are
served at `*.rise.local` (the `/etc/hosts` + host-IP wiring that `mise setup`
provisions).

#### Option B — Docker backend

A lighter single-host setup with no cluster; great for backend/frontend feature
work and for the Docker deployment backend itself.

```bash
# Adds only the /etc/hosts aliases (incl. rise-dex); no cluster, no minikube.
mise setup hosts

# Runs the compose support services + Rise on the host with the Docker
# backend (reuses config/docker.yaml; no Rise image build).
mise backend:run docker   # alias: mise br docker

# Optionally, in another terminal:
mise frontend:dev
```

Apps are served at `*.rise.localhost` (loopback per RFC 6761 — no `/etc/hosts`
edits needed). For the full reference see the operator guide's
[Local development](https://niklasrosenstein.github.io/rise/operator-docs/docker/#local-development-run-rise-on-the-host-no-image)
section.

### Services and credentials

Services will be available at:
- **Rise server**: http://localhost:3000
- **PostgreSQL**: localhost:5432
- **Vite.js Frontend Server**: http://localhost:5731
- **Minikube HTTP/HTTPS Ingress** (Kubernetes path only): http://localhost:8080, https://localhost:8443

On the Kubernetes path you may also need to add entries for deployed projects to
`/etc/hosts` (the Docker path uses `*.rise.localhost`, which needs no edits):

```
127.0.0.1 {project}.rise.local # One for each Rise-deployed project you want to access
```

**Default credentials**:
- Email: `admin@example.com`, `dev@example.com` or `user@example.com`
- Password: `password`

## Deploy your first app

```bash
# Build the CLI
cargo build
# `rise` binary should be available from direnv, otherwise use `cargo run`
```

On the **Kubernetes backend**:

```bash
rise login --url http://rise.local:3000

cd examples/hello-world
rise project create hello-world
rise deploy
# Reachable at http://hello-world.rise.local
```

On the **Docker backend**:

```bash
rise login --url http://rise.localhost:3000

cd examples/hello-world
rise project create hello-world
rise deploy
# Reachable at http://hello-world.rise.localhost
```

(Add `--url ...` to `rise login` to switch between backends you've used before.)

## Releasing

**Prerequisites:**
- [GitHub CLI (`gh`)](https://cli.github.com/) - authenticated via `gh auth login`
- [Claude CLI](https://github.com/anthropics/anthropic-tools) - for AI-generated release notes (optional)

**Create a new release:**

```bash
# Preview release notes
./scripts/tag-version.sh --dry-run 0.14.0

# Preview release notes with extra Claude guidance
./scripts/tag-version.sh --dry-run --claude-guidance "Emphasize operator-facing changes" 0.14.0

# Create and publish release
./scripts/tag-version.sh 0.14.0
```

The script validates prerequisites, generates release notes, shows a plan, and after confirmation performs all git operations (commit, tag, push) and creates a GitHub release. CI then builds release binaries and Docker images.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
