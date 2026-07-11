---
title: "ADR-0002: Authorization Code Structure"
---

## Status

**Proposed** (under review). Date: 2026-07-11.

Realizes the permission model of [ADR-0001](/operator-docs/adr/0001-unified-permission-model/). ADR-0001 owns *what* the model is; this ADR owns *where the code lives*.

## Context

The permission model is security-critical, and the value of a small, auditable core is highest exactly there. The goal of this carve-up is that the evaluation logic — union, Deny-wins, the subset check, wildcard replacement, ceiling intersection, the label-write gate — can be read, fuzzed, and tested **without a database and without any Rise product concept**. What a `Deployment` is, what `rise.dev/` means, and how rows reach Postgres must never leak into that logic.

One fact drives most of the structure: the RBAC objects — `Role`, `RoleBinding`, `PlatformRole`, `PlatformRoleBinding`, `OrganizationPolicy`, `InstancePolicy` — are all **resources** in the generic store (ADR-0001 §3, §5). Reading a subject's bindings or an org's ceiling is therefore an ordinary `ResourceStore` read, not a bespoke authorization data path. The store already exposes a `ResourceStore` trait (`crates/rise-resource-store/src/store.rs`) with a Postgres implementation; the engine consumes that trait, grown with the hierarchy/label operations the model needs.

## Decision

Three layers, cut so that security decisions, fact-retrieval, and product meaning are separated.

**Layer 0 — pure policy algebra** (new crate, e.g. `rise-authz-policy`; ~zero deps). The Allow/Deny evaluator, ceiling composition (pointwise `∩`, `min()`), the Deny-aware subset check and the intensional `(Scope, LabelSelector)` domain lattice, wildcard replacement with Deny-preservation, `${ref.name}` substitution. All pure functions over its **own** small `Verb`/`Kind`/`Statement`/`Policy` types — no store, no I/O, no reserved-key constants. This is the highest-value, most-auditable code and carries the pure-logic acceptance scenarios as unit tests.

**Layer 1 — the evaluation engine** (new crate, e.g. `rise-authz`). The ADR-0001 §4 algorithm, membership expansion, `effectiveLabels` diffing (§6.6), the recipient-boundary intersection, and `list` filtering. It depends on `rise-authz-policy`, on `rise-resource-api`'s envelope types, and on two traits it evaluates against — `ResourceStore` (facts from the tree) and a small new `MembershipResolver` (which it defines). It contains no SQLX, no Axum, no token signing, and is testable end-to-end against in-memory fake stores.

**Layer 2 — Rise wiring** (`rise-deploy`). The `MembershipResolver` implementation, the seed data (reserved label keys; the `system-admin`/`resource-owner`/`org-admin` contents and their deployment variants; the seeded bindings), the operator allowlist source, the authz choke point (`src/server/resources/authz.rs`, replacing today's `require_operator`), the HTTP handlers, the `list` metadata-vs-full projection and the 403-vs-masked-empty mapping.

**Where the facts come from — the store crate, not scattered in `rise-deploy`.** The tree/binding/ceiling reads are the *existing* `ResourceStore` trait, grown with generic hierarchy/label operations implemented in `rise-resource-store`'s Postgres store: ancestor chain, the K-inheriting subtree (`WITH RECURSIVE` over `parent_uid`), `effectiveLabels` resolution, and list-by-kind-under-scope. These are product-agnostic operations over a labeled hierarchical store and belong to the store. This also matches the repo's SQLX split: `rise-resource-store` owns resource-store SQLX; `rise_deploy::db` owns typed-table SQLX.

**The one product-specific seam: `MembershipResolver`.** Team membership (`team_members`), org membership (Team ties, per ADR-0001 §1), and operator status (config allowlist) are the Rise-specific inputs, and Teams are still typed-table-backed. So the engine defines the `MembershipResolver` trait and `rise-deploy` implements it over `rise_deploy::db` + config. When the typed tables migrate onto the generic store, most of this dissolves back into `ResourceStore` reads; the operator allowlist stays config-sourced regardless.

**Prerequisite refactor.** The `ResourceStore` trait currently lives in `rise-resource-store`, which carries `sqlx`. Move the trait (and its Row/Params model types) down into the dep-light `rise-resource-api`, leaving only `PgStore` + `sqlx` in `rise-resource-store`, so the engine compiles against the trait without transitively pulling a database driver.

**Two clean splits along existing seams.** Token issuance (§7): the *authorization* half — the `mintToken` verb check and the one-hop `token_class` rule — is engine logic; the *issuance* half — signing, `act`/`token_class` claims, TTL, trust-policy match — is `rise-backend-auth`. `list` (§4): per-item filtering is engine; the metadata-vs-full projection is a `rise-resource-api`/server serialization concern calling the engine per item. `effectiveLabels`: the plain read is a store op (also used by `list` output); the §6.6 before/after *simulation with a hypothetical value* is engine logic over store-provided ancestor labels — the store stays free of authorization semantics.

```
rise-authz-policy   (pure algebra; own Verb/Kind/Statement types; ~zero deps)
        ▲
rise-authz (engine) ──► rise-resource-api  (envelope types + the ResourceStore
   defines MembershipResolver              & MembershipResolver traits, no sqlx)
        ▲                        ▲
        │                        │ impl
rise-deploy ──► rise-resource-store (PgStore: ancestors, K-inheriting subtree,
  impl MembershipResolver         effectiveLabels, list-by-kind — the sqlx home)
  over team/org tables + config;
  seed data; authz.rs choke point; HTTP; list projection; token wiring
  (mintToken authz here, issuance via rise-backend-auth)
```

## Consequences

- The engine and pure algebra are testable with in-memory fakes and no Postgres, so the ADR-0001 conformance suite partitions three ways: pure-logic scenarios → Layer 0 unit tests; tree/membership scenarios → Layer 1 with fake stores; wiring scenarios (masking, `list` projection, token endpoint) → server integration.
- The most security-sensitive code has the fewest dependencies and the highest coverage; product churn (new kinds, new labels) cannot reach it.
- The `ResourceStore`-trait move is a prerequisite refactor, small but required before the engine can stay driver-free.
- `MembershipResolver` is the designed shrink-point: it narrows as typed tables converge onto the generic store, and is the only place team/org/operator semantics enter the engine.

## Alternatives considered

- **Leave the `ResourceStore` trait in `rise-resource-store` and let the engine take the transitive `sqlx` dependency.** Bloats the security core's dependency graph and couples it to the store crate's evolution, undercutting fake-testability. Rejected in favor of moving the trait to the dep-light `rise-resource-api`.
- **Bespoke `BindingStore` / `CeilingSource` / `ResourceTree` traits in `rise-deploy`.** Unnecessary: bindings, ceilings, and policy objects are all resources, so their reads are the existing `ResourceStore` trait grown with hierarchy/label ops in the store crate. A separate authz data path would duplicate the store and scatter SQLX. Rejected.
- **One combined `rise-authz` crate (modules `policy` + `engine`) vs. two crates.** Leaning **one crate with a hard internal module boundary** initially, splitting the pure-policy layer into its own crate once it earns it — matching how the repo extracted `rise-backend-docker` only as the seam matured (#377). Revisitable.
- **Layer 0 reusing `rise-resource-api`'s `Verb`/`Kind` types vs. its own.** Leaning **own types** so the pure policy library is standalone and portable (verb/kind as opaque strings), at the cost of a thin mapping layer in the engine — the isolation is what makes the security core auditable. Revisitable.

## References

- [ADR-0001](/operator-docs/adr/0001-unified-permission-model/) — the permission model this structure realizes.
- `crates/rise-resource-api`, `crates/rise-resource-store` — the envelope types and the `ResourceStore` trait/impl this builds on.
