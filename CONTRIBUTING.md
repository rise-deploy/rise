# Contributing to Rise

## Developer Certificate of Origin

Every commit must carry a `Signed-off-by` line certifying that you wrote the
patch or otherwise have the right to submit it under the license of the files
you touched. `git commit -s` adds it for you:

```
Signed-off-by: Your Name <your.email@example.com>
```

The full text of what you are certifying is the
[Developer Certificate of Origin 1.1](https://developercertificate.org/).

To sign off a branch you have already written:

```bash
git rebase --signoff develop
```

## Which license your contribution falls under

Rise is split across two licenses, and a contribution is accepted under the
license of the path it touches:

- **BUSL-1.1** — `crates/rise-backend-kubernetes/`, `crates/rise-backend-ecs/`,
  `helm/rise/`, `modules/rise-ecs/`
- **MIT OR Apache-2.0** — everything else

See [LICENSING.md](LICENSING.md) for the full map and the reasoning.

## Before you open a pull request

Format and lint — CI rejects unformatted code:

```bash
cargo fmt --all
cargo clippy --workspace --all-features --all-targets -- -D warnings
cargo test --workspace --all-features        # needs `mise run db:migrate` once
```

If you touched `tests/e2e`, it is a **separate workspace** that `--all` does not
reach:

```bash
cargo fmt --manifest-path tests/e2e/Cargo.toml
cargo clippy --manifest-path tests/e2e/Cargo.toml --all-targets -- -D warnings
```

Several artifacts are generated and checked for drift in CI. Regenerate and
commit the result when you change their source:

| Changed | Run |
|---|---|
| SQLX queries in `rise-deploy` | `mise run sqlx:prepare` |
| `src/server/settings.rs` | `mise run config:schema:generate` |
| `crates/rise-deployment-spec/src/project_config.rs` | `mise run rise-toml:schema:generate` |
| `crates/rise-resource-api/` | `mise run resource:schema:generate` |
| The `RiseProject` CRD structs | `mise run crd:generate` |
| `Cargo.toml` / `Cargo.lock` | `cargo audit` |

`mise run lint` runs the CI-equivalent sweep.

## Branching

`develop` is the default branch. Target the branch your feature branch was
created from — for most work that is `develop`, not `main`.
