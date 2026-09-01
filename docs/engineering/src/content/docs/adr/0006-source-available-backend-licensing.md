---
title: "ADR-0006: Source-Available Backend Licensing"
---

## Status

**In Progress** — Date: 2026-09-01.

Shipped: the licensing split itself (D1–D5), the ECS backend relicensed, the
per-crate license metadata, `LICENSING.md`, the DCO requirement.

Outstanding: the Kubernetes backend still lives in `src/server/deployment/` and
cannot carry its own license until it is extracted into
`crates/rise-backend-kubernetes/` (D3). Until that lands, the Kubernetes code is
still under MIT OR Apache-2.0 in fact, whatever `LICENSING.md` says about the
intended end state.

## Context

Rise was uniformly `MIT OR Apache-2.0`. The engineering effort and the
commercial value are not uniformly distributed: a single-node Docker host is
what an individual or a small team runs, while the Kubernetes and Amazon ECS
backends are the machinery for running Rise across a fleet, and they are what an
organization would pay for.

A license that is uniform across the repository cannot express that. Making the
whole project restrictive would break the low-friction adoption path that the
CLI and the Docker backend exist to provide. Leaving it wholly permissive leaves
no commercial position at all.

Two structural facts made a split practical:

1. **The released CLI already links no backend code.** `dist` builds with
   default features and `default = ["cli"]`; every backend crate is an optional
   dependency behind `backend`. Whatever the backends are licensed as, the
   binary users download is unaffected.
2. **The backends were already crate-shaped** — `rise-backend-docker`,
   `rise-backend-ecs` on the `rise-backend-core` seam (ADR-0005 D1). The
   decomposition work done for testability turned out to also be the work needed
   for a license boundary. Kubernetes is the exception, and that is the whole of
   the outstanding work.

## Decision

**D1 — Split the repository across two licenses.** The core, the CLI and the
Docker backend stay `MIT OR Apache-2.0`. The Kubernetes and ECS backends, the
Helm chart and the ECS Terraform module become `BUSL-1.1`. `LICENSING.md` is
the authoritative map.

**D2 — The change license is `MIT OR Apache-2.0`, four years out.** Each release
of the BSL code converts to the same dual license the rest of the project uses,
four years after it is published. Converting to the project's own license rather
than to bare Apache-2.0 keeps the repository at two licenses in the long run
instead of three.

**D3 — The license boundary is a crate boundary.** A file-header split through
`src/` would be unreadable and unenforceable by tooling. This obliges extracting
`crates/rise-backend-kubernetes/` — ~7,400 lines across `resource_builder.rs`,
`webhook.rs`, `crd.rs`, `pods.rs`, `identity_refresh.rs` and `ip_validator.rs`,
plus roughly fifteen call sites in shared handlers that reach directly into
`crd::` and `webhook::`. Those call sites move behind default-bodied methods on
`DeploymentBackend` and a new `OrganizationView` trait in `rise-backend-core`.

**D4 — The Additional Use Grant is scoped in prose, not enforced at build
time.** The grant states that running a Rise distribution with neither the
Kubernetes nor the ECS controller enabled is not use of the Licensed Work. A
Docker-backend operator is therefore in the clear while running the ordinary
`--all-features` server image, and no per-backend cargo feature split is needed.

**D5 — Contributions carry the license of the path they touch**, with a DCO
sign-off required. Contributions predating this ADR were made under the prior
inbound=outbound clause and remain MIT OR Apache-2.0; this ADR does not
retroactively relicense them.

## Consequences

`rise-backend-traefik` must stay permissive: the Docker backend depends on it.
Its `@ecs` provider handling stays with it — that is a provider-name string, not
ECS business logic. The same reasoning keeps `sanitize_ecs_name` and
`EventSource::Ecs` in `rise-backend-core`, and keeps `modules/rise-aws` (IAM,
also needed for backend-agnostic ECR registry provisioning) permissive.

`AccessClass` stays in the root crate despite its nginx-flavoured fields,
because Docker and ECS both consume `access_classes` maps and moving it would
create a Docker → BSL dependency edge.

D4 has a cost that D1 does not remove: automated license scanners read the
dependency tree, not the runtime configuration, and will flag the server image
as BUSL for any organization with a policy gate — including one that only ever
runs the Docker backend. Splitting the `backend` cargo feature into
`backend-docker` / `backend-kubernetes` / `backend-ecs` is the remedy if that
becomes a real adoption blocker. The D3 extraction keeps that a cheap change,
which is part of why it is worth doing even without build-time enforcement.

Rise is now **source-available**, not open source, when described as a whole.
Descriptions of the project should say which part they mean.

## Alternatives considered

**Whole-project BSL.** Rejected: it would put the CLI — which every user runs,
including people deploying to someone else's Rise install — under a restrictive
license for no commercial gain, and would cost distro and package-manager
distribution.

**AGPL instead of BSL.** Rejected: AGPL restricts the wrong axis for this
product. The concern is a competitor operating Rise as a managed service, which
AGPL permits so long as they publish their modifications, while AGPL
simultaneously deters ordinary corporate users the project wants.

**Per-file SPDX headers, no crate extraction.** Rejected under D3.

**Build-time enforcement via per-backend features.** Deferred, not rejected —
see the D4 consequences.

## References

- [LICENSING.md](https://github.com/rise-deploy/rise/blob/develop/LICENSING.md)
- [Business Source License 1.1](https://mariadb.com/bsl11/)
- ADR-0005 — the `rise-backend-core` seam this split relies on
