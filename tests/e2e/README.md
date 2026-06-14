# `rise-e2e` — cross-backend end-to-end harness

A typed Rust harness that runs end-to-end scenarios against a real Rise stack.
Scenarios are written **once** against the [`Backend`](src/backend/mod.rs) driver
seam and run on either backend, so a parity gap surfaces as a **declared skip**
(printed with a reason) instead of silent drift between two bash scripts.

It exists to replace the drift-prone `scripts/ci/e2e-*.sh` suites; that migration
is incremental (see `ROADMAP.md` → *Cross-backend E2E test consistency*). The bash
suites still run in CI today.

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
  + background `kubectl port-forward`s (server, Dex, per-app reach), a Rust port of
  `scripts/ci/e2e-minikube.sh`.
- `src/scenario.rs` — backend-agnostic scenarios + the matrix runner. Each
  `Scenario::applies_to(kind)` returns `Run` or `Skip(reason)`.
- `src/{cli,http,token,dex}.rs` — process/HTTP/token helpers. `token` reuses
  `rise-backend-auth` to mint the CI bearer; `dex` mints OIDC id_tokens via the
  resource-owner password grant.
- `tests/e2e.rs` — the `#[test]` entrypoint.

## Scenarios

| id                  | docker | minikube | asserts |
|---------------------|--------|----------|---------|
| `public-deploy`     | Run    | Run      | deploy `traefik/whoami` → Healthy; HTTP 200 (+ `Hostname:` body on Docker) |
| `sa-token-exchange` | Run    | Run      | SA + Dex password-grant id_token + `RISE_IDENTITY` → `project list` returns the SA's project; un-exchanged token rejected |
