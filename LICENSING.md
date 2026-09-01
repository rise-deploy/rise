# Licensing

Rise is split across two licenses. The core platform, the CLI and the
single-node Docker backend are permissively licensed and always will be. The
Kubernetes and Amazon ECS backends — the machinery for running Rise across a
fleet — are licensed under the Business Source License 1.1 and convert to the
permissive license four years after each release.

## The map

| Path | License |
|---|---|
| `src/` (the `rise-deploy` crate, CLI and server core) | MIT OR Apache-2.0 |
| `crates/rise-deployment-spec` | MIT OR Apache-2.0 |
| `crates/rise-backend-auth` | MIT OR Apache-2.0 |
| `crates/rise-backend-core` | MIT OR Apache-2.0 |
| `crates/rise-backend-traefik` | MIT OR Apache-2.0 |
| `crates/rise-backend-docker` | MIT OR Apache-2.0 |
| `crates/rise-authz` | MIT OR Apache-2.0 |
| `crates/rise-resource-api` | MIT OR Apache-2.0 |
| `crates/rise-resource-store-postgres` | MIT OR Apache-2.0 |
| `crates/rise-runtime-sync` | MIT OR Apache-2.0 |
| `frontend/`, `docs/`, `example/`, `tests/` | MIT OR Apache-2.0 |
| `modules/rise-aws` | MIT OR Apache-2.0 |
| **`crates/rise-backend-kubernetes`** | **BUSL-1.1** |
| **`crates/rise-backend-ecs`** | **BUSL-1.1** |
| **`helm/rise`** | **BUSL-1.1** |
| **`modules/rise-ecs`** | **BUSL-1.1** |

Full texts: [LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE),
[LICENSE-BSL](LICENSE-BSL).

## What this means in practice

**The `rise` CLI is entirely MIT/Apache-2.0.** Released CLI binaries are built
with default features and link no backend code at all, so nothing you download
from a release carries BSL terms.

**The Docker backend is free forever.** Running Rise on a single Docker host —
the whole path from `rise deploy` through the reconciler to Traefik routing —
touches no BSL-licensed code.

**The server image contains every backend.** It is built with `--all-features`,
so the Kubernetes and ECS backends are compiled in. Running it with neither of
those controllers enabled is explicitly *not* use of the Licensed Work under
the Additional Use Grant, so a Docker-backend operator is in the clear.

**Using the Kubernetes or ECS backend in production** is permitted by the
Additional Use Grant unless you are offering Rise, or something substantially
similar, to third parties as a hosted or managed service. Non-profit entities
may use them without limitation. If neither applies to you, you need a
commercial license.

**Everything converts.** Each release of the BSL code becomes MIT OR Apache-2.0
four years after it is published. The restriction is on the newest code, never
on all of it.

## Contributing

Contributions are accepted under the license of the path they touch: BSL-1.1
for the paths marked above, MIT OR Apache-2.0 for everything else. See
[CONTRIBUTING.md](CONTRIBUTING.md) — a `Signed-off-by` line (DCO) is required.

Contributions made before the DCO requirement was introduced were submitted
under the repository's prior inbound=outbound clause, which licensed them as
MIT OR Apache-2.0. This relicensing does not change the terms under which
those earlier contributions were given.
