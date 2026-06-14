# `rise-e2e` — cross-backend end-to-end harness

A typed Rust harness that runs end-to-end scenarios against a real Rise stack.
Scenarios are written **once** against the [`Backend`](src/backend/mod.rs) driver
seam and run on either backend, so a parity gap surfaces as a **declared skip**
(printed with a reason) instead of silent drift. It is the sole end-to-end test
suite for the Docker and Kubernetes deployment backends.

## Running

This is a **standalone Cargo workspace** (excluded from the root workspace so the
production image build never compiles it), so invoke it with `--manifest-path
tests/e2e/Cargo.toml` from the repo root.

The harness is gated on `RISE_E2E_BACKEND`. **Unset → every test is an instant
skip**, so a plain `cargo test --manifest-path tests/e2e/Cargo.toml` only runs the
pure unit tests and never tries to stand up a backend.

```bash
# Docker backend (self-provisions its own compose stack).
RISE_E2E_BACKEND=docker \
RISE_IMAGE_TAG=<tag> \
RISE_IMAGE_REPOSITORY=ghcr.io/rise-deploy/rise \
  cargo test --manifest-path tests/e2e/Cargo.toml -- --nocapture --test-threads=1

# Minikube backend (self-provisions its own cluster: minikube start + helm
# install + port-forwards). Needs minikube/kubectl/helm on PATH.
RISE_E2E_BACKEND=minikube RISE_IMAGE_TAG=<tag> \
  cargo test --manifest-path tests/e2e/Cargo.toml -- --nocapture --test-threads=1
```

`--test-threads=1` because a backend owns a single shared stack.

## Layout

- `src/backend/` — the `Backend` driver seam. Both backends self-provision their
  own stack: `DockerBackend` via `docker compose` (CLI extraction via `docker cp`,
  Traefik reach); `MinikubeBackend` via `minikube start` + `helm upgrade --install`
  + the JFrog/Vault registry stack + background `kubectl port-forward`s (server,
  Dex, per-app reach).
- `src/scenario.rs` — backend-agnostic scenarios + the matrix runner. Each
  `Scenario::applies_to(backend)` returns `Run` or `Skip(reason)`.
- `src/{cli,http,token,dex}.rs` — process/HTTP/token helpers. `token` reuses
  `rise-backend-auth` to mint the CI bearer; `dex` mints OIDC id_tokens via the
  resource-owner password grant.
- `tests/e2e.rs` — the `#[test]` entrypoint.

## Scenarios

| id                  | docker | minikube | asserts |
|---------------------|--------|----------|---------|
| `public-deploy`     | Run    | Run      | deploy a sample app → Healthy; HTTP 200 (+ body marker) |
| `sa-token-exchange` | Run    | Run      | SA + Dex password-grant id_token + `RISE_IDENTITY` → `project list` returns the SA's project; un-exchanged token rejected |
| `loki-log-retention`| Skip   | Run      | stop deployment, pods gone, `/logs/volume` total>0 + `rise deployment logs` returns backlog (served by Loki) |
| `helm-idempotency`  | Skip   | Run      | re-run `helm upgrade` applies cleanly |
| `workload-identity` | Skip   | Run¹     | build fixture from source; `/identity` reports valid file+exchanged tokens, project-bound sub, matching iss; file token re-mints (new jti) |

¹ minikube only in `jfrog-vault` registry mode (source build needs a cluster-pullable registry); otherwise `Skip`.
