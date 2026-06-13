# `rise-e2e` — cross-backend end-to-end harness

A typed Rust harness that runs end-to-end scenarios against a real Rise stack.
Scenarios are written **once** against the [`Backend`](src/backend/mod.rs) driver
seam and run on either backend, so a parity gap surfaces as a **declared skip**
(printed with a reason) instead of silent drift between two bash scripts.

It exists to replace the drift-prone `scripts/ci/e2e-*.sh` suites; that migration
is incremental (see `ROADMAP.md` → *Cross-backend E2E test consistency*). The bash
suites still run in CI today.

## Running

The harness is gated on `RISE_E2E_BACKEND`. **Unset → every test is an instant
skip**, so the normal `cargo test --workspace` never tries to stand up a backend.

```bash
# Docker backend (self-contained: brings up its own compose stack).
RISE_E2E_BACKEND=docker \
RISE_IMAGE_TAG=<tag> \
RISE_IMAGE_REPOSITORY=ghcr.io/rise-deploy/rise \
  cargo test -p rise-e2e -- --nocapture --test-threads=1

# Minikube backend (thin connector: expects the stack already up + a
# `kubectl port-forward svc/... 3000:3000`, as the bash CI job arranges).
RISE_E2E_BACKEND=minikube RISE_IMAGE_TAG=<tag> \
  cargo test -p rise-e2e -- --nocapture --test-threads=1
```

`--test-threads=1` because a backend owns a single shared stack.

## Layout

- `src/backend/` — the `Backend` driver seam. `DockerBackend` is fully
  self-contained (compose up/down, CLI extraction via `docker cp`, Traefik reach);
  `MinikubeBackend` is a thin connector (health-check + `docker run` the CLI; app
  HTTP reach is a declared gap → `reach_app` returns `Ok(None)`).
- `src/scenario.rs` — backend-agnostic scenarios + the matrix runner. Each
  `Scenario::applies_to(kind)` returns `Run` or `Skip(reason)`.
- `src/{cli,http,token,dex}.rs` — process/HTTP/token helpers. `token` reuses
  `rise-backend-auth` to mint the CI bearer; `dex` mints OIDC id_tokens via the
  resource-owner password grant.
- `tests/e2e.rs` — the `#[test]` entrypoint.

## Scenarios

| id                  | docker | minikube | asserts |
|---------------------|--------|----------|---------|
| `public-deploy`     | Run    | Run      | deploy `traefik/whoami` → Healthy; HTTP 200 + `Hostname:` body (Docker; Healthy-only on minikube) |
| `sa-token-exchange` | Run    | Skip     | SA + Dex password-grant id_token + `RISE_IDENTITY` → `project list` returns the SA's project; un-exchanged token rejected |

`sa-token-exchange` skips on minikube until its in-cluster Dex enables the password
grant and is reachable from the harness host.
