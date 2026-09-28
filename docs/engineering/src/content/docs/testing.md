---
title: "Testing"
description: "How Rise's test suites run, and where ADR-0001's conformance suite lives."
---

Rise's Rust tests are split by what they need to run:

- **Pure/fake-store crates** — no database. Run per crate, e.g.:

  ```bash
  cargo test -p rise-authz
  cargo test -p rise-backend-auth
  cargo test -p rise-resource-api
  ```

- **The rest of the workspace**, including `rise-deploy`,
  `rise-resource-store-postgres`, and `rise-runtime-sync` — needs a running
  Postgres and `DATABASE_URL` set:

  ```bash
  mise run db:migrate   # once, against a running Postgres
  cargo test --workspace --all-features
  ```

  `mise run lint` runs the pure/fake-store portion of the workspace tests as
  part of its checks; the three crates above are excluded there because they
  need a database, so run them separately as above.

- **`tests/e2e`** is its own Cargo workspace (excluded from the root one so
  production image builds never compile it), driving the `rise` CLI and HTTP
  API against a live backend. Its own unit tests run the ordinary way —
  `cargo test --manifest-path tests/e2e/Cargo.toml`; the end-to-end suite
  itself is a binary you run, not a `cargo test` target — `cargo run
  --manifest-path tests/e2e/Cargo.toml`, gated on `RISE_E2E_BACKEND` (skips
  and exits 0 when unset). See the header comment in `tests/e2e/Cargo.toml`
  and `tests/e2e/README.md` for the exact invocation per backend.

## ADR-0001 conformance suite

[ADR-0001](./adr/0001-unified-permission-model.md)'s appendix defines 61
acceptance scenarios. Scenarios 1-57 are covered across three test tiers:

1. **Pure fakes** — `crates/rise-authz/tests/{policy,engine,gate}.rs` and
   `crates/rise-resource-api/tests/*`, built on the fakes in
   `crates/rise-authz/tests/support/mod.rs` (`StoreBuilder`,
   `FakeMemberships`). No database.
2. **Pure signer** — `crates/rise-backend-auth/src/signer.rs`, covering the
   token signer's own invariants (e.g. the delegation depth limit) in
   isolation.
3. **Postgres/HTTP** — `src/server/resources/handlers/conformance.rs` and
   `src/server/resources/handlers/test_support.rs`, driving the resource
   API's dispatch functions against a real Postgres, plus
   `crates/rise-resource-store-postgres/tests`.

Every test that backs a scenario carries one doc-comment line per scenario it
claims, directly above the `#[test]` / `#[tokio::test]` / `#[sqlx::test]`:

```rust
/// ADR-0001 scenario 23
/// ADR-0001 scenario 13
async fn a_user_administers_two_organizations_independently(pool: sqlx::PgPool) {
    ...
}
```

`scripts/check-adr-conformance.sh` greps for these markers across the crates
above and fails if any of scenarios 1-57 is unclaimed, or if any of the
deferred scenarios below is claimed. Run it directly, or as part of lint:

```bash
mise run adr:conformance:check
mise run lint          # runs the check as one of its steps
```

CI runs the same check as a step in the Rust quality job.

**Exclusions:**

- Scenarios 58-60 (marked `§9 deferred` in the ADR) and 61
  (product-operation deferred) are intentionally out of scope and must not be
  claimed by any test.
- Scenario 10 (operator selectors, JIT login, and User tokens carrying
  `rise_uid`) is only partially covered: JIT login, session re-resolution,
  and the inactive-User/identity halves are tested
  (`src/server/auth/user_identity.rs`), but the operator-selector half waits
  on `operatorIdentities` (`ROADMAP.md` §1; `WORKLOG.md` increment 10b).
- No per-subject token count cap and no revocation list exist — the ADR
  accepts this (L1048-1050) — so neither is tested. Only the two caps the ADR
  does specify are: the platform-global maximum token TTL
  (`auth_token_max_ttl_seconds`) and the delegation depth limit
  (`MAX_DELEGATION_DEPTH`).
