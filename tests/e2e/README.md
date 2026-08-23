# `rise-e2e` — cross-backend end-to-end harness

A typed Rust harness that runs end-to-end scenarios. Backend scenarios run
against a real Rise stack through the [`Backend`](src/backend/mod.rs) driver seam
on either backend, so a parity gap surfaces as a **declared skip** (printed with
a reason) instead of silent drift. Standalone suites, such as local
`rise compose`, stay in this crate but run without provisioning a backend.

## Running

This is a **standalone Cargo workspace** (excluded from the root workspace so the
production image build never compiles it), so invoke it with `--manifest-path
tests/e2e/Cargo.toml` from the repo root.

The suite is a **binary you run** (`cargo run`), not a `cargo test` target — its
output is its own timed report, with no libtest scaffolding around it. It is
gated on `RISE_E2E_BACKEND` or `RISE_E2E_SUITE` (**both unset → prints a skip
and exits 0**) and exits non-zero on any scenario failure. Unit tests for the
harness's own helpers (e.g. CI-token minting) are plain `#[test]`s run by
`cargo test`.

```bash
# Docker backend (self-provisions its own compose stack).
RISE_E2E_BACKEND=docker \
RISE_IMAGE_TAG=<tag> \
RISE_IMAGE_REPOSITORY=ghcr.io/rise-deploy/rise \
  cargo run --manifest-path tests/e2e/Cargo.toml

# Minikube backend (self-provisions its own cluster: minikube start + helm
# install + port-forwards). Needs minikube/kubectl/helm on PATH.
RISE_E2E_BACKEND=minikube RISE_IMAGE_TAG=<tag> \
  cargo run --manifest-path tests/e2e/Cargo.toml

# ECS backend (self-provisions the whole AWS-side stack — see "ECS Backend"
# below for what the account and the caller need first).
AWS_PROFILE=<sandbox> AWS_REGION=eu-central-1 \
RISE_E2E_BACKEND=ecs RISE_IMAGE_TAG=<published-tag> \
  cargo run --manifest-path tests/e2e/Cargo.toml

# Local compose suite (no Rise backend; needs Docker and a rise CLI).
RISE_E2E_SUITE=compose \
RISE_BIN=./target/debug/rise \
  cargo run --manifest-path tests/e2e/Cargo.toml

# Upgrade suite: bring the stack up on an OLDER released version, seed a project,
# then upgrade in place to RISE_IMAGE_TAG. (Docker support exists in the harness
# but has no released stack to upgrade from yet — see "Upgrade Suite" below.)
RISE_E2E_BACKEND=minikube \
RISE_E2E_UPGRADE_FROM=0.22.1 \
RISE_IMAGE_TAG=<target-tag> \
RISE_IMAGE_REPOSITORY=ghcr.io/rise-deploy/rise \
  cargo run --manifest-path tests/e2e/Cargo.toml

# Unit tests for the harness's own helpers (no backend stood up).
cargo test --manifest-path tests/e2e/Cargo.toml
```

Backend scenarios run in-order as one suite (they share the single backend
bring-up). Standalone suites run separately via `RISE_E2E_SUITE`.

## ECS Backend

Unlike the other two backends this one runs against **real AWS** — LocalStack
puts ECS behind a paid plan, Cloud Map behind its top one, and publishes task
ports randomly enough that the Traefik ECS provider cannot discover them. There
is setup to do once per account before the first run.

### On the machine

`aws` (v2), `jq`, `curl`, and a working **Docker daemon** — the harness extracts
the `rise` CLI out of the image with `docker cp`, and the
`registry-build-push-pull` scenario builds a fixture image locally and pushes it
to ECR.

Your public IP must be stable for the run: the security group admits ports 80
and 8080 from `<your-ip>/32` only, resolved once at bring-up. A VPN that
reconnects mid-run will look like a hung scenario.

### In the account

Use a **sandbox account**. The stack creates IAM roles that carry `ecs:*`, so
the caller needs IAM write permissions; that is not a credential to hold in an
account you care about.

1. **A default VPC with at least one subnet** in the region. The script takes
   `Vpcs[?is-default]` and the first subnet under it, and fails if there is
   none — a region where the default VPC has been deleted needs one recreated
   (`aws ec2 create-default-vpc`).
2. **Fargate on-demand vCPU quota raised.** A fresh account gets **6**. The
   control plane alone takes 1.5 (Traefik 0.25 + Postgres 0.5 + Dex 0.25 +
   Rise 0.5), and every app task rounds up to 0.5 — with five scenarios leaving
   their deployments running, a full suite peaks around 4 vCPU on top of that.
   Ask for **16** (Service Quotas → Amazon ECS → *Fargate On-Demand vCPU
   resource count*); at the default the later scenarios fail as tasks that never
   leave `PROVISIONING`.
3. **A published `RISE_IMAGE_TAG`.** ECS pulls the control-plane image from
   GHCR, so a local build is not visible to it. Same constraint the Docker
   driver already has.

Nothing else is pre-created: cluster, security group, IAM roles (execution,
task, and the ECR push role), Cloud Map namespace, log group and ECR
repositories are all made at bring-up and removed on exit.

### Caller permissions

`ecs:*`, `ecr:*`, `ec2:*` (describe, plus security-group create/authorize/
delete), `logs:*`, `servicediscovery:*`, `sts:GetCallerIdentity`, and on IAM:
`CreateRole`, `DeleteRole`, `PutRolePolicy`, `DeleteRolePolicy`,
`AttachRolePolicy`, `DetachRolePolicy`, `ListRolePolicies`,
`ListAttachedRolePolicies`, `PassRole`.

### Registry

The stack runs with `registry: ecr` under a run-scoped repository prefix
(`rise-e2e/`), so the suite exercises the real path: Rise provisions a
repository per project, the CLI pushes with the STS-scoped credentials Rise
mints, and the task execution role pulls. Repositories are deleted on teardown
— including their images, which are otherwise billed.

### Cost, and what teardown does not cover

A run is a handful of 0.25–0.5 vCPU tasks for roughly 20 minutes, no load
balancer and no NAT gateway: cents. Everything is tagged `rise-e2e` and torn
down on exit, and `tests/e2e/aws/ecs-stack.sh down` is idempotent if a run is
killed. **`KEEP=1` deliberately leaves the stack up** for inspection (the
Traefik dashboard is on `:8080`) — that one you pay for until you tear it down.

You can drive the AWS side on its own, without the harness:

```bash
AWS_PROFILE=<sandbox> AWS_REGION=eu-central-1 RISE_IMAGE_TAG=<tag> \
  tests/e2e/aws/ecs-stack.sh up      # writes /tmp/rise-e2e-stack.env
tests/e2e/aws/ecs-stack.sh down
```

## Layout

- `src/backend/` — the `Backend` driver seam. Both backends self-provision their
  own stack: `DockerBackend` via `docker compose` (CLI extraction via `docker cp`,
  Traefik reach); `MinikubeBackend` via `minikube start` + `helm upgrade --install`
  + the JFrog/Vault registry stack + background `kubectl port-forward`s (server,
  Dex, per-app reach); `EcsBackend` by shelling out to `aws/ecs-stack.sh`, which
  runs Traefik, Postgres, Dex and Rise as ECS services in the cluster under test
  (CLI extraction via `docker cp`, reach via Traefik's public IP and `nip.io`).
- `aws/ecs-stack.sh` — the ECS backend's AWS-side bring-up and teardown. Also
  runnable on its own; see "ECS Backend" above.
- `src/scenario.rs` — backend-agnostic scenarios + the matrix runner. Each
  `Scenario::applies_to(backend)` returns `Run` or `Skip(reason)`.
- `src/compose.rs` — standalone `rise compose` suite. It runs the CLI directly
  against `example/multi-container`, starts Docker Compose, and asserts the
  frontend/API/worker/Redis path without a Rise backend.
- `src/{cli,http,token,dex}.rs` — process/HTTP/token helpers. `token` reuses
  `rise-backend-auth` to mint the CI bearer; `dex` mints OIDC id_tokens via the
  resource-owner password grant.
- `src/main.rs` — the binary entrypoint (`cargo run`): the suite's own runner +
  timed report.

## Scenarios

| id                  | docker | minikube | asserts |
|---------------------|--------|----------|---------|
| `public-deploy`     | Run    | Run      | deploy a sample app → Healthy; HTTP 200 (+ body marker) |
| `sa-token-exchange` | Run    | Run      | SA + Dex password-grant id_token + `RISE_IDENTITY` → `project list` returns the SA's project; un-exchanged token rejected |
| `loki-log-retention`| Skip   | Run      | stop deployment, pods gone, `/logs/volume` total>0 + `rise deployment logs` returns backlog (served by Loki) |
| `helm-idempotency`  | Skip   | Run      | re-run `helm upgrade` applies cleanly |
| `workload-identity` | Skip   | Run¹     | build fixture from source; `/identity` reports valid file+exchanged tokens, project-bound sub, matching iss; file token re-mints (new jti) |

¹ minikube only in `jfrog-vault` registry mode (source build needs a cluster-pullable registry); otherwise `Skip`.

## Standalone Suites

| suite     | backend required | asserts |
|-----------|------------------|---------|
| `compose` | No               | `rise compose up` builds and starts `example/multi-container`; frontend and API routes respond; API reaches Redis; worker completes a Redis-backed job |

## Upgrade Suite

Setting `RISE_E2E_UPGRADE_FROM=<old-tag>` (alongside `RISE_E2E_BACKEND`) switches
the harness from the scenario matrix to the in-place upgrade suite
([`src/upgrade.rs`](src/upgrade.rs)): it brings the stack up on the old version,
seeds a project + deployment, upgrades the control plane to `RISE_IMAGE_TAG`
(running its DB migrations against the seeded data), then asserts the seeded
project + its deployment history survived and a fresh deploy still works.

What each backend faithfully versions differs (a documented limitation, not a
parity bug): Docker recreates the `rise` service on the new image against the
existing Postgres volume — keeping the in-repo compose topology — so it exercises
the **image + DB-migration** upgrade; minikube builds the **old chart from its
source** at the release tag (`git archive v<old> helm/rise`, with that version's
own `values-ci.yaml` and a `helm dependency update`), pins it to the old image,
then `helm upgrade`s to the in-repo chart on the new image — a full **chart +
image + DB-migration** upgrade. The old chart is built from source (not the
published OCI artifact) because the published chart is renamed `rise-helm` while
the values files assume the in-repo name `chart`; installing from source keeps the
chart name — and the `rise-ci-chart-*` resource names — consistent across the
upgrade.

In CI, only the **minikube** upgrade runs (`e2e-upgrade-minikube`), from the
latest published stable release on every develop push / trusted PR. There is no
Docker upgrade job **yet**: the Docker deployment backend
(`docker-compose.standalone.yaml` + `config/docker.yaml`) landed after the latest
stable release, so no released version ships a Docker stack to upgrade *from* (the
old image can't even boot under the current compose, which expects the baked-in
`/etc/rise/docker.yaml`). The harness keeps the Docker `upgrade()` path, so the
job can be added once the Docker backend ships in a stable release — a tracked
parity gap, not a missing capability.
