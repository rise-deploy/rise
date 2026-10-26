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
  `helm/rise/`
- **MIT OR Apache-2.0** — everything else

See [LICENSING.md](LICENSING.md) for the full map and the reasoning.

## Before you open a pull request

At minimum, format, lint and test:

```bash
cargo fmt --all
cargo clippy --workspace --all-features --all-targets -- -D warnings
cargo test --workspace --all-features        # needs `mise run db:migrate` once
```

What else to run depends on what you changed: `tests/e2e` is a separate
workspace, several generated files are checked for drift, and dependency
changes go through `cargo audit` and `cargo deny`. The full list lives in one
place, [CLAUDE.md § Before Commit & Push](CLAUDE.md#before-commit--push);
`mise run lint` runs most of the CI sweep.

## Branching

`develop` is the default branch. Target the branch your feature branch was
created from — for most work that is `develop`, not `main`.
