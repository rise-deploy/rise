# Auth Token Exchange Plan

> **Status:** Design proposal (no code changes yet). This document reviews Rise's
> current request-time authentication patterns and proposes a token-exchange
> mechanism that turns Rise into the single trusted issuer for request-time auth.
>
> **Confirmed decisions:** all token parsing / verification / signing is centralized in a new
> **pure-core workspace crate** (`rise-backend-auth`) exposing exactly **two** verify entry points —
> one for arbitrary external JWTs, one for Rise-issued JWTs (§3); access tokens are HS256 and
> separate from existing claim types; migration is phased with a transparent fallback (no breakage
> for existing raw-token CI); controller identities are folded into the new model now; the exchange
> endpoint follows RFC 8693 token-exchange.

## 1. Motivation

Today Rise accepts several JWT types **directly at every request** and interprets
them inline in the auth middleware and handlers. The costly case is **service-account
(SA) authentication**, which is inherently *two-phase*:

1. The auth middleware can only JWKS-validate an external token's signature and
   expiry. It **cannot** decide *which* SA the token represents, because that
   requires the **project context**, which is not known until the request reaches
   the handler body (the project name travels in the request payload / path).
2. So each handler runs a second phase: look up service accounts by
   `(project_id, issuer)`, match the token's claims against each SA's expected
   claims (with glob support), handle the zero-match / multi-match cases, resolve a
   synthetic user, and then separately enforce environment restrictions.

This spreads non-trivial auth logic across roughly a dozen handlers, where it can
drift or develop bugs. It also forces `GET /api/v1/platform/capabilities` to be a
**public** endpoint: a project SA calling it has no project context, and recognizing
the SA would mean scanning every project's service accounts (with cross-project
collision risk) — explicitly called out in the handler today
(`src/server/platform/handlers.rs`).

**The principle we want:** a caller presents **one** source JWT (plus optional
project context) to a dedicated **exchange endpoint** and receives a **Rise-issued
access token** that fully encodes the resolved principal. After that, the middleware
and handlers make **snap decisions by inspecting the Rise token** — no DB lookups, no
context-gathering, no logic that can drift. As a bonus, `platform/capabilities` can
become authed again: any valid Rise access token may call it, and a deploy using a
project SA simply ships its project name as exchange context.

## 2. Current state (as-is)

> **⚠️ Stale code references (post-Phase-0).** Phase 0 has **landed**: the claim
> types, the two verify entry points, signing, the pure matchers, `is_rise_issued_jwt`,
> and `AuthError` now live in **`crates/rise-backend-auth/`** (`claims.rs`, `error.rs`,
> `signer.rs`, `verify.rs`, `matchers.rs`). Many `jwt_signer.rs:NNN` / `jwt.rs:NNN` /
> `controller.rs:NNN` line citations in §2–§5 below **predate that move** and now point into
> gutted shim files in `src/server/auth/`. Treat those line numbers as historical; the real
> code is in the crate. The narrative is kept verbatim only where it still describes
> rise-deploy-side wiring (middleware, extractors, DB resolution) that did not move.

### 2.1 Auth middleware — `src/server/auth/middleware.rs`

`auth_middleware` (line ~65) extracts the token (Rise JWT cookie first, then
`Authorization: Bearer`), then **peeks at the unvalidated `iss`** to choose a path:

- **Rise path** (`is_rise_issued_jwt(iss, public_url)`, middleware.rs:20-34 — exact match
  **plus** a port-stripping `starts_with` prefix match): `JwtSigner::verify_user_jwt`
  (HS256, audience-checked) → `find_or_create_with_default_organization` → inject
  `db::User` and the `groups` vector into request extensions.
- **External path** (any other issuer): a lightweight guard requires the issuer to be
  either a configured controller (`state.controllers_by_issuer`, in-memory, O(1)) or a
  known SA issuer (`service_accounts::issuer_exists`, a DB round-trip). Then
  `JwtValidator::validate_token` validates **signature + expiry only** via JWKS (no
  custom-claim checks) and injects a `VerifiedExternalToken { issuer, claims }`.

`platform_access_middleware` (line ~297) runs after auth on the platform tier. It
**skips external tokens entirely** (SAs are validated per-project later) and only gates
Rise `User`s against the email/group allowlist (admins/operators bypass).

`optional_auth_middleware` accepts only Rise user JWTs (never SA tokens).

### 2.2 Two-phase SA resolution — `src/server/auth/context.rs`

```rust
pub enum AuthContext { User(User), ExternalToken(VerifiedExternalToken) }
```

`AuthContext` is an Axum `FromRequestParts` extractor that reads whichever extension
the middleware injected. `resolve_for_project(pool, project, controllers_by_issuer)`
is **phase 2**:

- For `User`: returns `(user, is_sa=false)`.
- For `ExternalToken`:
  1. If the issuer matches a controller identity, **reject** (controller tokens are
     not SAs).
  2. `service_accounts::find_by_project_and_issuer(project_id, issuer)`.
  3. For each SA, deserialize its expected `claims` (`HashMap<String, String>`) and
     match against the token via `JwtValidator::validate_custom_claims` (glob `*`
     supported).
  4. Zero matches → `401`; more than one → `409` (ambiguous config); exactly one →
     look up the SA's synthetic user and return `(user, is_sa=true)`.

**Call sites:** ~10 in `src/server/deployment/handlers.rs` (create + list/show/rollback/
stop/etc.) and 1 in `src/server/registry/handlers.rs`. All but one receive the project **by name**;
the lone outlier — `update_deployment_status` — *discovers* its project from a `deployment_id`, which
changes how it must be migrated (§4.3, C1). Environment restrictions are
enforced **separately** in `create_deployment` (around `deployment/handlers.rs:1178`)
via `service_accounts::find_active_by_user_id` + `allowed_environment_ids`.

### 2.3 Controller identities — `src/server/auth/controller.rs`

`auth.controllers[]` config → `ControllerIdentity { id, issuer, claims }`.
`match_controller_identity` performs `aud`/claims glob matching (`Single` / `Multiple`
/ `Unmatched`). `ControllerAuthContext` (an extractor) and `AnyAuth` (an enum of
user-or-controller) are currently consumed **only** by the operator-gated generic
resource API (`src/server/resources/handlers.rs`). `ControllerAuthContext` depends on
the `VerifiedExternalToken` extension. **Stale-comment caveat (L1) — partially resolved by Phase 0:**
the old `controller.rs:9` module comment ("No HTTP route consumes the extractor yet") has been
**corrected** — it now references the resources API, so a Phase-3 reader will not mistake the live
resources path for dead code. The narrowly-justified `#[allow(dead_code)]` on
`VerifiedControllerToken` **remains** and should still be revisited when the type is removed in
Phase 3.

### 2.4 Token signing — `src/server/auth/jwt_signer.rs`

`JwtSigner` holds an HS256 symmetric key and an RS256 keypair (the RS256 public key is
exposed via JWKS for third parties to verify). Two claim shapes exist; their **behavior must be
preserved** by this work — they are relocated into the crate in Phase 0 (§3.3, §8), but neither their
shape nor their verification semantics change:

- `RiseClaims` (`sub`, `email`, `name`, `groups`, `iss`, `aud`, `iat`, `exp`): user/UI
  login uses HS256 (`aud = public_url`, via `sign_user_jwt` / `verify_user_jwt`);
  project ingress uses RS256 (`aud = project_url`, via `sign_ingress_jwt`).
  `verify_jwt_skip_aud` accepts both HS256 and RS256 for the ingress-auth handler.
- `WorkloadClaims` (RS256): **outbound** federation tokens issued to deployed apps.

`verify_user_jwt` explicitly rejects non-HS256 algorithms — a pattern we mirror below.

### 2.5 Workload exchange precedent — `src/server/workload_tokens/`

`POST /api/v1/identity/token` is the **outbound** analogue of what we want: a deployed
app presents a bootstrap credential (`Authorization: Bearer`, looked up by SHA-256 hash
in `deployments.identity_credential_hash`) plus an `audience`, and receives a Rise-signed
**RS256** `WorkloadClaims` token to federate to AWS STS / GCP / Vault. It is a **public**
route, rate-limited via `state.oauth_rate_limiter.increment_and_check`, with TTL clamped
to `workload_token_max_ttl_seconds`. This is the structural template for the new
**inbound** exchange endpoint — but the directions are opposite, so they stay separate
modules.

### 2.6 Router tiers — `src/server/mod.rs` (~line 211)

Three tiers nested under `/api/v1`:

- `public_routes`: `health`, `version`, schema redirect, **`platform::routes()`
  (capabilities)**, `auth::routes::public_routes()`, `workload_tokens::routes()`.
- `auth_only_routes`: `logs/capabilities` + auth-only auth routes, with the
  `auth_middleware` layer (authentication, **no** platform-access gate).
- `platform_routes`: projects, teams, deployments, service_accounts, registry,
  env_vars, environments, extensions, encryption, quickstart, resources — with
  `platform_access_middleware` + `auth_middleware` layers.

### 2.7 CLI token flow — `src/cli/token_source.rs`

The CLI resolves a `TokenProvider` (`resolve_token_provider`) rather than a fixed token:
a long build+push deploy can outlast a short-lived OIDC token, so the provider lazily
re-mints/refreshes. `CachedToken::is_fresh` refreshes at ~2/3 of token lifetime or within
a 60s skew window, driven off the JWT `exp`. A CI **service account presents its raw
external OIDC token directly** to `POST /api/v1/deployments`, with the project name in the
request body (`CreateDeploymentRequest.project`).

## 3. Target principle (to-be)

Rise becomes the **single trusted issuer at request time**. Every non-Rise token (SA
OIDC, controller) is **exchanged up front** into a short-lived Rise access token that
encodes the fully-resolved principal. The auth middleware then only ever validates
Rise-issued tokens and populates a rich principal; handlers inspect that principal and
decide instantly. All the claim-matching / DB-resolution / context-gathering logic lives
in exactly one place: the exchange endpoint.

Underpinning that, the **token logic itself** must have one home. Today verification is
scattered — `JwtValidator::validate_token` (external, jwt.rs), `verify_user_jwt` /
`verify_jwt_skip_aud` (Rise HS256/RS256, jwt_signer.rs), and workload verification — each a
separate path that can drift independently. We collapse this to **two non-negotiable code
paths**, owned by a dedicated crate (§3.3).

### 3.1 The two code paths (non-negotiable)

**Path A — arbitrary external JWT → typed claims.** Exactly one function turns an untrusted,
externally-issued JWT into a validated, typed claims structure usable for SA / controller
matching:

```rust
// the ONLY entry point for external tokens.
// As implemented (Phase 0), the issuer is passed explicitly (the caller has
// already peeked it for the issuer guard) and `keys` is a trait object:
async fn verify_external_jwt(token: &str, issuer: &str, keys: &dyn JwksKeySource)
    -> Result<ExternalClaims, AuthError>;

/// Opaque proof that signature + expiry were checked via JWKS. No public constructor —
/// the only way to obtain one is `verify_external_jwt`.
struct ExternalClaims { issuer: String, /* private */ claims: serde_json::Value }
```

`JwksKeySource` is a trait (JWKS discovery + key fetch); rise-deploy supplies the reqwest +
SSRF + cache implementation (§3.3). The matchers (`match_service_account_claims`,
`match_controller_identity`, glob) are pure functions over `&ExternalClaims` — they **cannot**
be called on un-verified input because the type can't be constructed any other way.

**Path B — Rise-issued JWT → typed token enum.** Exactly one function verifies a Rise-issued
JWT. Its **output type models only what the verifier can ever return** — and `Workload` is **not**
in it, because workload tokens are **sign-only**: Rise mints them (jwt_signer.rs:432) but **never
verifies them inbound** (confirmed — the only `decode::<WorkloadClaims>` in the tree is a unit test,
jwt_signer.rs:732; AWS STS / GCP / Vault verify them via the published JWKS, never Rise). So the
**signer** and the **verifier** have **different** kind sets:

```rust
// the ONLY entry point for Rise-issued tokens.
// Note the return enum has NO Workload variant — see below.
fn verify_rise_jwt(token: &str, keys: &RiseKeys) -> Result<RiseToken, AuthError>;

// Verifier output — only the kinds Rise actually classifies on the inbound path.
enum RiseToken {
    Session(RiseClaims),     // HS256, aud = public_url   (UI / CLI login)
    Ingress(RiseClaims),     // RS256, aud = project_url  (deployed-app ingress auth)
    Access(AccessClaims),    // HS256, aud = public_url   (exchanged SA / controller — §4)
}

// Signer input — the signer (RiseTokenSigner) can mint all four kinds, including
// the outbound-only Workload token that Rise never verifies.
enum RiseTokenKind {
    Session(RiseClaims),
    Ingress(RiseClaims),
    Access(AccessClaims),
    Workload(WorkloadClaims), // RS256, arbitrary aud — outbound federation, verified externally
}
```

`verify_rise_jwt` performs the disambiguation **once, centrally** (the §4.1 discriminator rules
live here, not smeared across call sites). The discriminators are **algorithm** and **header-`typ`**,
*not* `aud`:

- `Session` vs `Ingress` is decided by **algorithm** (HS256 → `Session`, RS256 → `Ingress`),
  matching today's `verify_jwt_skip_aud` which alg-branches and sets `validate_aud=false`
  (jwt_signer.rs:507-535). `verify_rise_jwt` does **not** validate the ingress `aud` — the per-request
  project_url is unknown to a context-free verifier, so project/aud binding stays an app-side concern
  (or is not enforced at this layer, exactly as today's ingress-auth handler, which only reads `email`
  and never checks `aud`, handlers.rs:1488-1499).
- `Session` vs `Access` (both HS256) is decided by the **header-`typ`** (§4.1).
- **The RS256 verify branch always yields `Ingress`.** `verify_rise_jwt` **never returns `Workload`**:
  workload tokens are sign-only, so nothing inbound classifies a token as `Workload`, and there is no
  inbound Ingress-vs-Workload decision to make. (`sign_workload_jwt` and `sign_ingress_jwt` both emit
  `Header::new(Algorithm::RS256)` + `kid` only today, jwt_signer.rs:415-416 / 460-461, so both carry the
  default `typ:"JWT"` and would be **indistinguishable** on an RS256 verify path anyway — which is moot
  precisely because that path is `Ingress`-only. If a future inbound workload-verify path is ever wanted,
  it would first require giving `sign_workload_jwt` a distinct custom `typ` — a behavior change, Phase 1+,
  explicitly out of scope here.)

**Workload↔Ingress separation is currently implicit (HIGH).** Today the **only** thing preventing a
Rise-issued **Workload** RS256 token (same RS256 key, same `iss`, default `typ:"JWT"` as an Ingress
token) from being accepted as `RiseToken::Ingress` on the RS256 verify branch is an implicit serde
fact: `RiseClaims.email` is a **required** field that `WorkloadClaims` **never sets**, so a workload
token fails to deserialize into `RiseClaims`. This is **fragile** — it silently breaks if `email` is
ever made optional on `RiseClaims`, or if a workload subject ever carries an `email` claim. The
Phase-1 `typ` discriminator (below) should make Rise-issued token-kind separation **explicit** rather
than relying on a missing serde field; the same `header.typ` branch that splits Session/Access should
be the mechanism that keeps Ingress and Workload distinguishable if a workload-verify path is ever
added.

Today's `verify_user_jwt`, `verify_jwt_skip_aud`, and workload verification become thin shims over this
single path (or are deleted). Callers `match` on `RiseToken` and get a **compile error** if they
forget a variant — the type system enforces exhaustive handling, so a new token kind can't
silently fall through a stale check. This structurally subsumes the earlier C2 "try-then-fallback"
hazard: there is no fallback, only one verifier returning a typed sum.

**Sync vs async split (H2).** Path B (`verify_rise_jwt`) is **synchronous and key-local**: it verifies
against in-process HS256/RS256 keys via `RiseKeys`/`RiseTokenSigner` — Rise's signing is fully local
(jwt_signer.rs:85-99), so there is no JWKS fetch and no `async`. Do **not** wire a `JwksKeySource` or
async into Path B. Path A (`verify_external_jwt`) is the **only** async path and the **only** one
taking a `JwksKeySource` (external IdP keys are fetched over the network).

### 3.2 The complete token model

Every JWT Rise issues or accepts, in one table (Verify = which of the two paths, or `n/a` for the
sign-only Workload token). The **Workload row is kept for completeness** — it is part of the full
token model the signer mints — but it is **not** a `verify_rise_jwt` output (§3.1): nothing inbound
ever classifies a token as `Workload`.

| Token (type) | Direction | Alg | Issuer | Audience | Verify | Purpose |
|---|---|---|---|---|---|---|
| Session (`RiseClaims`) | Rise-issued | HS256 | public_url | public_url | B → `Session` | UI / CLI user login |
| Ingress (`RiseClaims`) | Rise-issued | RS256 | public_url | project_url (alg-discriminated; aud not checked by the verifier, as today) | B → `Ingress` | deployed-app ingress auth |
| Access (`AccessClaims`) | Rise-issued | HS256 | public_url | public_url | B → `Access` | exchanged SA / controller principal (§4) |
| Workload (`WorkloadClaims`) | Rise-issued | RS256 | public_url | caller-supplied | n/a — sign-only; verified externally via JWKS (AWS/GCP/Vault), never by Rise | outbound federation (AWS/GCP/Vault) |
| External OIDC (SA) | inbound | RS256 | external IdP | SA-matched | A → `ExternalClaims` | CI service-account source token |
| External OIDC (controller) | inbound | RS256 | external IdP | controller-matched | A → `ExternalClaims` | controller source token |

Rise-issued tokens never flow through Path A and external tokens never through Path B; the
`is_rise_issued_jwt` issuer peek (§2.1, §5 step 1) is the *only* branch that routes a raw token
between the two.

**Single source of truth (M1).** This table is the public contract of `RiseToken` + the two verify
functions, so its **canonical home is `crates/rise-backend-auth/README.md`** (next to the types) — which
**now exists**. The plan (§3.2) and any operator/auth doc under `docs/` **link** to that README rather
than copy it; the table above is a convenience mirror, and the README is authoritative. The README
reflects reality **today** (Phase 0: HS256⇒`Session`, RS256⇒`Ingress`; **no** `Access` variant and **no**
`typ` discriminator yet) and flags the Phase-1 additions inline.

Anti-drift will be enforced by a **parametrized crate unit test** (call it
`rise_token_disambiguation_matrix`) that asserts the `(alg, header-typ, aud) → expected RiseToken variant
/ rejection` matrix against the real `verify_rise_jwt`, so the README table and the code cannot silently
diverge (this is the unit-testability §3.3 already promises, named as the enforcement mechanism).
**This test belongs to Phase 1, not Phase 0:** the matrix's discriminating rows are the `typ`-based
Session-vs-Access split, which does not exist until the Phase-1 `typ` discriminator lands. Until then the
matrix would only assert the trivial alg split. Land
`rise_token_disambiguation_matrix` **with** the `typ` discriminator + `RiseToken::Access` (the same
Phase-1 change), so the test is not left homeless.

### 3.3 The `rise-backend-auth` crate

The repo is **already a Cargo workspace** with multiple member crates, so a new member
`crates/rise-backend-auth` fits the existing structure — the "single consolidated crate" line in
`CLAUDE.md` is outdated and must be updated when this lands. The genuine **no-I/O precedent** is
`crates/rise-resource-api` (deps: `chrono`/`schemars`/`serde`/`serde_json`/`uuid` — no `axum`/`sqlx`/
`reqwest`); note `crates/rise-resource-store` is **not** a pure precedent (it depends on `sqlx`).

**Pure core — no I/O, no framework, no DB.** The crate depends on `jsonwebtoken`,
`serde`/`serde_json`, `uuid`, `schemars` (already a workspace dep — needed because
`ControllerIdentity` derives `JsonSchema` and is embedded in settings, controller.rs:30 /
settings.rs:475, and moves into the crate with the matchers, H1), `async-trait` (for the trait),
and `tracing` (lightweight, pure — the relocated `match_controller_identity` logs unmatched-claim
diagnostics, controller.rs:92/103). It does **not** depend on `reqwest`, `axum`, or `sqlx` — **nor
on `anyhow` or `regex`**, even though the matchers being relocated use them today:
- **`anyhow` is dropped via refactor.** Three of the relocated matchers —
  `build_controller_indexes`, `validate_controller_id` (controller.rs:293/146) and
  `validate_custom_claims` (jwt.rs:234-262) — currently return `anyhow::Result` and use
  `anyhow!`/`bail!`/`Context`. On relocation they are **refactored to return the crate's `AuthError`**
  (replacing `anyhow::Result`), so `anyhow` is not pulled into the pure core. **This is a small,
  deliberate behavior-adjacent refactor:** their signatures change from `anyhow::Result` to
  `AuthError`, and today's callers consume `anyhow::Result` — so it is a reviewed delta, **not** covered
  by the Phase-0 "byte-for-byte identical" guarantee that applies to the verifier shims (§7).
  `match_controller_identity` (controller.rs:236) is **already infallible** — it returns the
  `ControllerMatch { Single, Multiple, Unmatched }` enum and uses no `anyhow` — so it relocates
  **unchanged** (its internal call to `validate_custom_claims` simply consumes the new `AuthError`).
- **`regex` is dropped by hand-rolling.** `validate_controller_id` matches `CONTROLLER_ID_RE`
  (controller.rs:16/130-131/171). The relocated validator is **hand-rolled (no `regex`)**. The plain
  string ops at controller.rs:147-170 only cover the length caps; the regex itself carries the
  structural rules the hand-roll **must** reproduce exactly — **lowercase-only** host labels, the
  **mandatory dot** between host labels, **no leading/trailing hyphen** per label, and the
  case-sensitivity split between host (`[a-z0-9]`) and the optional `/name` segment (`[A-Za-z0-9]`).
  A crate unit test must port the existing `controller.rs` id-validation cases to lock equivalence;
  treat this as a reviewed delta (like the `anyhow` change), not byte-for-byte.
- JWKS fetching is abstracted behind the `JwksKeySource` trait; the reqwest + `ssrf` + cache
  implementation stays in rise-deploy (today's `JwtValidator` *becomes* that impl). SSRF/HTTP
  policy stays in the app.
- No SQLX in the crate (consistent with `CLAUDE.md`: SQLX only in `src/db`). DB resolution
  (finding SAs, synthetic users, projects) stays in rise-deploy; the crate's matchers are pure and
  take already-fetched rows / claims.
- The crate is **deterministically unit-testable** with no network or DB — the bulk of the
  high-value auth tests (glob matching, disambiguation, alg/typ/aud rejection, claim round-trips)
  live here.

**In the crate:** all claim types (`RiseClaims`, `AccessClaims` / `PrincipalClaims` / `Scope`,
`WorkloadClaims`, `ExternalClaims`), the two verify entry points + signing (`RiseTokenSigner`, plus
the `pub` helper `compute_key_id`), the pure matchers (`match_service_account_claims` /
`validate_custom_claims`, `match_controller_identity`, `audience_matches` / `matches_wildcard_pattern`,
`build_controller_indexes`, `validate_controller_id`, `validate_oidc_issuer`) **and the
`ControllerIdentity` config type they operate on** (it moves with them since they take
`&ControllerIdentity`; it keeps its `JsonSchema` derive and is re-exported for `settings.rs` to embed),
`is_rise_issued_jwt`, and `AuthError` (plus `JwtSignerError`).

**Stays in rise-deploy:** the axum `AccessPrincipal` extractor + middleware, the `JwksKeySource`
impl, OAuth handlers, cookie helpers, the exchange HTTP handler, and all DB access.

**Anti-drift guarantee ("parse, don't validate").** `ExternalClaims` and `RiseToken` have private
fields and no public constructor — the *only* way to obtain one is through the two verify
functions. rise-deploy therefore **cannot** fabricate a "verified" value or hand-roll a second
validation path; the compiler enforces that every auth decision flows through the crate. This is
the systemic property the centralization buys, and the direct answer to the two MUSTs.

## 4. The Rise access-token model

### 4.1 `AccessClaims` (new, HS256)

A **new** claim type, **separate** from `RiseClaims` and `WorkloadClaims`, signed with
**HS256**:

```rust
struct AccessClaims {
    iss: String,   // Rise public_url — satisfies the middleware's is_rise_issued_jwt branch
    aud: String,   // Rise public_url — verify path mirrors verify_user_jwt's aud check
    sub: String,   // stable principal id: user uuid, "rise:sa:<sa_id>", or "rise:ctrl:<id>"
    iat: u64,
    exp: u64,
    jti: String,   // audit id; room for a future revocation deny-list
    principal: PrincipalClaims,
}

#[serde(tag = "kind", rename_all = "snake_case")]
enum PrincipalClaims {
    User { email: String, groups: Option<Vec<String>> },
    ServiceAccount {
        service_account_id: Uuid,
        synthetic_user_id: Uuid,                      // the existing SA synthetic user.id
        project_id: Uuid,
        project_name: String,
        allowed_environment_ids: Option<Vec<Uuid>>,   // None = any; folds in today's separate check
        scopes: Vec<Scope>,
    },
    Controller { identity_id: String },
}
```

**Why a separate struct, not an extension of `RiseClaims`:** `RiseClaims` is consumed by
both `verify_user_jwt` (HS256+aud) and `verify_jwt_skip_aud` (HS256 session **and** RS256
ingress). Bolting SA/scope fields onto it risks an SA-shaped token being accepted on the
ingress path, or an RS256 ingress token deserializing into an SA principal. A sibling type
keeps those boundaries crisp.

**Why HS256, not RS256:** the access token is consumed **only** by Rise's own middleware,
which holds the symmetric secret. RS256 is reserved for tokens that *external* parties
verify via JWKS (`WorkloadClaims`, ingress tokens). Keeping the access token HS256 makes it
impossible for a third party to verify it and enforces the boundary in the key system.
`verify_access_jwt` must reject non-HS256 algorithms (mirroring `verify_user_jwt`) so an
RS256 ingress/workload token can never be parsed as an access token.

**Discriminator (mandatory, type-safe).** These rules are implemented **once**, inside
`verify_rise_jwt` (§3.1, Path B); no call site re-derives them. A body-level `principal.kind` tag
is *not* sufficient on its own: there is **no `#[serde(deny_unknown_fields)]` anywhere in
`src/server/auth/`**, `RiseClaims.email` is required while `name`/`groups` are optional, so an
`AccessClaims::User` carrying an `email` could deserialize cleanly as `RiseClaims` on any
fallback path. "Try `verify_access_jwt`, fall back to `verify_user_jwt`" is therefore unsafe.
The **primary** discriminator must be the **JWT-header `typ`** (e.g. `typ: "rise-access+jwt"`
for `AccessClaims`, distinct from the session token's `typ`): `verify_access_jwt` accepts only
its own `typ`, `verify_user_jwt`/`verify_jwt_skip_aud` reject it. **Implementation note (L4):**
jsonwebtoken's `Validation` does **not** check the header `typ`, and today `verify_user_jwt` /
`verify_jwt_skip_aud` only `decode_header` for the alg check (jwt_signer.rs ~481, ~508) and never
read `header.typ` — so this requires adding **explicit `header.typ` rejection** to those two verify
functions; it is not free from `Validation`. (Session tokens get the default `typ:"JWT"` via
`Header::new`, jwt_signer.rs ~348; the access token uses a distinct custom `typ`.) Additionally, add
`#[serde(deny_unknown_fields)]` to **both** `AccessClaims` and `RiseClaims` (note: adding it to
`RiseClaims` is a migration risk — any older session token carrying an extra claim would stop
verifying; roll it out behind a flag / session-key cutover, coordinated with the §9 key-rotation window).
**Ingress hardening:** `verify_jwt_skip_aud` (jwt_signer.rs ~507) accepts HS256 **and** RS256
with `validate_aud=false` and is used by the ingress-auth handler — it must explicitly **reject
any token whose header `typ` is the access-token `typ`, or that carries a `principal` claim**, so
an `AccessClaims` can never be honored on the ingress path.

### 4.2 Scopes

A coarse enum (`Deploy`, `RegistryPush`, `ReadProject`, …). SAs receive a fixed set per
principal-kind initially (matching what an SA can do today). Embedding scopes in the token
lets handlers gate with `has_scope()` and **zero DB work**; the source of truth at exchange
time remains the SA row. Per-SA configurable scopes are deferred (they'd need a new column +
migration), but putting `scopes` in the token now future-proofs the handler signatures.

### 4.3 Consumption — `AccessPrincipal` extractor

A new `AccessPrincipal` Axum extractor (read from a single extension the middleware injects)
exposes:

- `user()` → the resolved user (replaces `AuthContext::user()`).
- `is_service_account()`.
- `require_project(project_id)` → for SA tokens, asserts the embedded `project_id` matches
  (a snap decision, no DB); for user tokens, a no-op (user project-access still uses the
  existing `ensure_project_access_or_admin`).
- `allowed_environment_ids()` and `has_scope(scope)`.
- `is_controller()` and, for controllers, `controller_identity_id()` — required by the resources API's
  `enforce_controller_allowed` (§6, H3).
- For `User` principals, the extractor must expose enough to drive `require_operator` (the resources
  API does **not** let operator gating be skipped — admins are not operators).

`resolve_for_project(pool, project, controllers)` collapses to
`require_project(project.id)?` + `has_scope(...)` — no pool, no controllers map, no claim
matching at request time.

**The bound-project asymmetry (discover-from-deployment sites).** `require_project` only works
when the handler already holds the project the caller *named*. Today's `resolve_for_project`
re-resolves the SA against whatever project the handler **discovered**, so it transparently
handles handlers that reach the project indirectly. With pre-exchange, the SA token is bound to
project P **at exchange time**; the handler must compare P against the discovered project and
**404 on mismatch** (an SA may act only within its bound project). One current site does this
discovery:

- `update_deployment_status` (deprecated, **unscoped** `PATCH /deployments/{deployment_id}/status`,
  handlers.rs ~2121-2197): takes only `deployment_id`, then
  `find_by_deployment_id_unscoped` → `find_by_id` to reach the project. An SA token bound to
  P must **not** be allowed to act on a deployment owned by Q; `require_project(discovered.id)`
  enforces exactly that (mismatch → 404, consistent with the endpoint's existing
  not-found-on-auth-failure masking).

Every other deployment site (the project-scoped status/list/show/stop/logs/groups handlers) and
the registry site receive the project **by name** from the path/body and resolve via
`find_by_name`, so for them `require_project` is the direct, clean replacement. The deprecated
unscoped endpoint is the **only** outlier and should stay on the legacy resolution path through
Phase 2 (its callers are old CLIs that can't pre-exchange anyway), then be removed with the
legacy path in Phase 3.

**Note on users:** users are *already* exchanged — OIDC login mints an HS256 `RiseClaims`
today. We **keep the user login flow on `RiseClaims`** for this scope; the SA access token is
its analogue. Unifying user login onto `AccessClaims::User` is a larger change (cookies,
ingress, `verify_jwt_skip_aud`) and is deferred (§9). **`PrincipalClaims::User` is therefore
reserved (L5):** the §5 pipeline mints only `ServiceAccount` / `Controller` and **never** `User` —
the variant exists solely for that deferred user-login unification, so an implementer must **not**
wire a User-minting exchange path in this scope. **Field asymmetry that makes this safe:**
`require_operator` / the resources API needs only `user.email` (resources/handlers.rs ~88-90),
which an `AccessClaims::User` *would* carry; but the deployment/registry user path needs `user.id`
for `projects::user_can_access` (project/handlers.rs ~1175), which an access token does **not**
carry. This is fine **only because** user tokens never become `AccessClaims` in this scope — user
requests still arrive as `RiseClaims` and resolve to a `db::User` (with `id`) exactly as today.

## 5. The exchange endpoint (RFC 8693)

`POST /api/v1/auth/token`, a **public** route (the source JWT is the credential), in a new
`src/server/auth/exchange/` module that mirrors the structure of `workload_tokens/`.

**Request** (RFC 8693 token-exchange, with one Rise-specific field):

```jsonc
{
  "grant_type": "urn:ietf:params:oauth:grant-type:token-exchange",
  "subject_token": "<source OIDC JWT>",
  "subject_token_type": "urn:ietf:params:oauth:token-type:jwt",
  "rise_project": "my-project"   // optional; required for project-SA exchange.
                                  // "resource" accepted as an alias for strict 8693.
}
```

**Response:**

```jsonc
{
  "access_token": "<rise HS256 jwt>",
  "token_type": "Bearer",
  "issued_token_type": "urn:ietf:params:oauth:token-type:jwt",
  "expires_in": 600
}
```

**Pipeline** (reuses existing code rather than reimplementing it):

1. Peek `iss`. Reject a token already issued by Rise — using the **same**
   `is_rise_issued_jwt` helper the middleware uses (never a hand-rolled `iss == public_url`),
   so the exchange and middleware agree byte-for-byte on what counts as Rise-issued.
   **Latent hazard — fuzzy match (HIGH, assigned to Phase 1).** Phase 0 shipped
   `is_rise_issued_jwt` **verbatim** from the old middleware helper
   (`crates/rise-backend-auth/src/lib.rs`), which is correct for a behavior-preserving extraction
   (a crate test even pins the fuzzy behavior). The **precise** current behavior:
   `public_url.strip_suffix(|c| c.is_ascii_digit() || c == ':')` strips only **one** trailing
   character, so for a `public_url` ending in a port like `:8443` the prefix base is `…:844`, and
   **sibling ports** `…:844X` (e.g. `…:8440`, `…:8449`) all satisfy the `starts_with` check. It is
   **fail-closed** (a non-Rise issuer matching the fuzzy prefix still cannot forge a Rise
   *signature*, so verification fails), so it is not exploitable today — but it is a foot-gun for
   both call sites. **Assign the exact-match tightening to Phase 1**: it MUST land **before** the
   exchange endpoint reuses this helper to *reject* Rise-issued tokens (step 1 here), because the
   exchange relies on the predicate being precise. Require a **negative test for the sibling-port
   superset** case (`https://rise.example.com:8443` public_url must NOT treat
   `https://rise.example.com:8440` as Rise-issued).
2. Issuer guard: `controllers_by_issuer.contains_key(issuer)` OR
   `service_accounts::issuer_exists(issuer)` — same lightweight guard the middleware uses today.
3. `JwtValidator::validate_token(token, issuer)` — JWKS signature + expiry (unchanged). The exchange
   **reuses this method as-is** rather than re-implementing JWKS fetch: it already SSRF-validates both
   the discovery URL and the returned `jwks_uri` (jwt.rs ~103-129). Note `validate_token` is **RS256-only**
   (jwt.rs ~358) — see L3: HS256/ES256 source IdPs are unsupported by the exchange, as today, except
   that post-Phase-3 the exchange is the *only* ingestion path.
   **Typed-error gap (MEDIUM, Phase 1).** `JwtValidator::validate_token` / `validate` currently return
   `anyhow::Result`, which **re-flattens** the crate's typed `AuthError` back into an opaque error. The
   exchange must emit **distinct RFC 8693 error codes** — e.g. `invalid_grant` (bad signature / expiry /
   issuer-guard failure) vs `temporarily_unavailable` (JWKS fetch / network failure, §5.1, §9 M2) — which
   it cannot do reliably off a flattened `anyhow`. Phase 1 should therefore **change these methods to
   return `AuthError`** (or otherwise expose the typed variant) so the exchange can map error kind → code,
   rather than introducing a **second** JWKS verification path to recover the distinction.
4. If `rise_project` is present: `projects::find_by_name` → run the **relocated body of
   `resolve_for_project`** (controller rejection, `find_by_project_and_issuer`, per-SA
   `validate_custom_claims`, 0→401 / >1→409). On a single match, read the SA's
   `allowed_environment_ids`, compute scopes, and mint `AccessClaims::ServiceAccount`.
5. Else match the token against controller identities (`match_controller_identity`): `Single` →
   mint `AccessClaims::Controller` (this enables the capabilities-while-controller case);
   `Multiple` → ambiguous configuration, reject (`invalid_grant` / 409-equivalent, per the §5.1
   taxonomy), mirroring the ambiguous-SA handling and `resolve_for_project`'s current 409 on
   multi-controller match (context.rs ~81-92); `Unmatched` → fall through.
6. Otherwise → `401`.
7. On **every successful mint**, emit a structured audit log (M5) — see §5.1.

**Cross-cutting:**

- **Rate limiting:** reuse `state.oauth_rate_limiter.increment_and_check(&ip, None, &key)` exactly as
  `identity/token` does. **Key only on a server-trusted id computed *after* JWKS validation** — the
  matched **SA id** (project-SA exchange) or the controller **identity_id** — mirroring the workload
  precedent, which keys on the deployment id resolved *after* the credential lookup (handlers.rs ~47).
  Never key on the raw token `sub`/`iss`: at the point the limiter must fire they are
  attacker-controlled and unverified, so a key derived from them lets an attacker fan out across
  buckets (or collide a victim's). For the **pre-validation / unknown-issuer reject path** (steps 1-3,
  before any trusted id exists), fall back to **IP-only** keying (matching workload's
  `"invalid-credential"` placeholder bucket).
- **TTL:** short, clamped by a new `auth_token_max_ttl_seconds` setting (~600s default, ≤10
  min), mirroring `workload_token_max_ttl_seconds`.
- **Signing:** new `JwtSigner::sign_access_jwt` / `verify_access_jwt`, **HS256-only**.
- **Endpoint auth:** public; the source token is the credential. Keep today's posture of not
  leaking unknown-issuer vs no-match beyond what's necessary.

### 5.1 Error taxonomy & input limits

- **Error bodies** follow RFC 8693 / 6749 OAuth shapes, not Rise's ad-hoc strings:
  `invalid_request` (missing/duplicate `grant_type`/`subject_token`, unsupported `subject_token_type`),
  `invalid_grant` (signature/expiry/issuer-guard failure, 0-match SA, controller-token-as-SA),
  `invalid_target` (unknown `rise_project`). The >1-match SA case **and** the >1-match controller
  case (step 5, `ControllerMatch::Multiple`) both stay a `409`-equivalent (`invalid_grant` with a
  distinguishing description). Avoid leaking unknown-issuer vs no-match (above).
- **Audit log (M5).** The exchange is the **only** place an external CI identity (`iss`+`sub`) maps
  to a Rise principal: downstream, a deployment's `created_by_id` is the opaque SA synthetic-user
  UUID (deployment/handlers.rs ~662) and tracing logs only the synthetic email, so nothing links a
  synthetic user back to the originating CI identity. Therefore every **successful** mint MUST emit a
  structured audit log recording the resolved `service_account_id` / controller `identity_id`, the
  `project` (for SA exchanges), the source `iss`, the source-token `sub`, and the minted `jti`. This
  makes a later `created_by` synthetic user traceable to the CI identity that authorized it. `jti`
  need **not** be persisted in Phase 1 (the deny-list is deferred, §10) — but it must appear in this
  log now so an operator can correlate an issued token with the request that minted it.
- **Input bound:** cap `subject_token` length before any parse (the workload endpoint already bounds
  its `audience` to 1024 chars, handlers.rs:37 — set an analogous inbound-JWT bound, e.g. ≤8 KiB, to
  blunt oversized-token CPU/DoS). Reject empty/oversized before touching JWKS.
- **Idempotency:** exchange is naturally idempotent for a given valid `(subject_token, rise_project)`
  within the source token's life — each call mints a fresh short-TTL access token with a new `jti`;
  there is no server-side state to dedupe, so retries are safe.

### 5.2 Discovery & schema (M4)

Rise's OIDC discovery doc (`/.well-known/openid-configuration`, served by `openid_configuration` in
`auth/handlers.rs` ~1796) already advertises `token_endpoint = {public_url}/api/v1/auth/code/exchange`
— the **OAuth authorization-code** exchange for CLI login. The new `POST /api/v1/auth/token` is a
**second, semantically different** endpoint (RFC 8693 STS token-exchange) and is **NOT** the OIDC
`token_endpoint`; it must **not** overwrite or be conflated with it. Decision: leave the discovery
doc's `token_endpoint` pointing at `code/exchange`; document the STS endpoint separately (OpenAPI +
auth docs under `docs/`), and only advertise it via a discovery field if/when a standard one exists
(there is no OIDC discovery key for an STS endpoint).

## 6. Middleware & handler simplification (end-state)

- `auth_middleware` validates **only Rise tokens**. After `is_rise_issued_jwt`, it
  calls `verify_rise_jwt` (§3.1, Path B) and **matches the returned `RiseToken`**: `Session` →
  inject `db::User` (user/UI, unchanged); `Access` → inject `AccessPrincipal`. Disambiguation
  happens once inside the verifier (header-`typ`/alg/aud), *not* by trial deserialization at the
  call site. The external branch, the `issuer_exists` DB round-trip, and the `controllers_by_issuer`
  lookup all **leave the request hot path** — the core win.
- The `resolve_for_project` call sites collapse to `auth.require_project(project.id)?` plus a
  `has_scope(...)` check — **with one exception**: the deprecated unscoped
  `update_deployment_status` discovers its project from `deployment_id` (§4.3) and must compare
  the discovered project against the token's bound `project_id` (404 on mismatch); it stays on
  the legacy path until Phase 3 removes it. The env-restriction block in `create_deployment`
  becomes `auth.allowed_environment_ids()` read straight from the token (no
  `find_active_by_user_id`). **Fail-closed semantics must be preserved** (handlers.rs ~1180-1197):
  `None` = unrestricted (any environment); a restricted SA whose target env is **not** in the allowed
  list **403s**, and a restricted SA with **no** target environment specified **also 403s** (can't
  verify the target is allowed). The token's `allowed_environment_ids` snapshots the SA row at exchange
  time, preserving the existing **one-active-SA-per-synthetic-user** assumption that
  `find_active_by_user_id` relies on.
- `VerifiedExternalToken`, `AuthContext`, `AnyAuth`, and `ControllerAuthContext` are removed;
  `match_controller_identity` / `build_controller_indexes` / `ControllerIdentity` are **retained but
  relocated into the `rise-backend-auth` crate** in Phase 0 (§3.3), re-exported so the exchange handler
  and `settings.rs` can use them. The rich resolution tests in `context.rs` move to the exchange module
  (the pure-matcher tests move into the crate).
- `resources/handlers.rs` switches from `AnyAuth`/`ControllerAuthContext` to `AccessPrincipal`, but
  **a single `is_controller()` check would regress its authz** (H3). The real handler
  (`resources/handlers.rs` ~670/688/707) enforces three distinct rules that `AccessPrincipal` must keep:
  (a) **operator-user** items via `require_operator` (`update_resource`) — and per `CLAUDE.md` the
  resources API is operator-gated and **admins are NOT operators**, so the `User` principal must still
  carry enough to run `require_operator`, not bypass it; (b) controllers **may not** update items
  (the explicit `forbidden` at ~688 must survive); (c) controllers may only touch **status
  subresources**, gated per-resource by `enforce_controller_allowed(identity_id)` (~707) — so the
  `Controller` principal must surface its `identity_id`. These routes are `#[cfg(feature = "backend")]`,
  and `platform_access_middleware` skips external tokens today, so the access-token model is the only
  thing that brings controller identity to this handler post-Phase-3.
- In `mod.rs`, `platform::routes()` (capabilities) moves from `public_routes` to
  `auth_only_routes`. Project SAs reach it because their token now carries the project binding
  and is recognized by the middleware — the "can't recognize a project SA without scanning all
  projects" problem dissolves.
- **`platform_access_middleware` must recognize `AccessPrincipal` (H1 — PHASE-1 ordering
  dependency, not deferred to Phase 3).** Today the gate has two paths only: skip if a
  `VerifiedExternalToken` extension is present (middleware.rs:306), else require a `User` extension
  or **500** (middleware.rs:312-313). The moment the middleware can inject an `AccessPrincipal`
  (Phase 1), a Rise access token is **neither** a `VerifiedExternalToken` (that branch is gone in
  the end-state) **nor** a `User` — so every SA deploy and every controller resource-status call
  (deployments/registry/resources all stay under `platform_routes`, mod.rs:236-260) would **500**.
  The gate must inspect the `AccessPrincipal` extension: **SA and Controller principals bypass** the
  email/group allowlist (their authz is the embedded project binding / controller identity, just as
  external tokens bypass it today), while **`User` principals are gated exactly as now**. This edit
  ships with the Phase-1 middleware change that first injects `AccessPrincipal`, not later.

## 7. Phased migration (transparent fallback)

The risk: existing CI sends a **raw external OIDC token** directly to `POST /deployments`
(project in body) through a refreshing provider. A hard cutover would break every pipeline,
so we phase it.

- **Phase 0 — extract `rise-backend-auth`, behavior-preserving (two small reviewed deltas).** Move
  the claim types, the two verify entry points (§3.1), signing, and the pure matchers into the new
  crate; introduce the `JwksKeySource` trait and reimplement today's `JwtValidator` as its rise-deploy
  impl. Replace the scattered verifiers with shims over `verify_rise_jwt` / `verify_external_jwt`, and
  adopt them at all current call sites. **Each verifier shim must reproduce its legacy verifier's exact
  alg-set and aud posture byte-for-byte:** `verify_user_jwt` (HS256 only, `aud` checked,
  jwt_signer.rs:476-492); `verify_jwt_skip_aud` (HS256 **and** RS256, `aud` skipped,
  jwt_signer.rs:507-535); `validate_token` (RS256-only, `aud` skipped, jwt.rs:358-361). **Signing's
  JWK publication moves too:** the `jwks` and `openid_configuration` handlers call
  `jwt_signer.generate_jwks()` (handlers.rs:1780, 1796), so RS256-public-key → JWK generation relocates
  to the crate's `RiseTokenSigner`. The **verifier shims** are a **pure refactor** — identical behavior,
  no new endpoints, no `AccessClaims` yet. **Two deltas are deliberate and reviewed, NOT covered by the
  byte-for-byte guarantee** (which applies only to the verifier shims): (1) **three relocated matchers'
  signatures change** from `anyhow::Result` to the crate's `AuthError` (§3.3, C2) — a behavior-adjacent
  refactor of `build_controller_indexes` / `validate_controller_id` / `validate_custom_claims`, with
  `validate_controller_id` additionally hand-rolled off `regex` (`match_controller_identity` is already
  infallible — it returns `ControllerMatch` — and relocates unchanged); (2) the
  verifier's **RS256 branch is `Ingress`-only** and the output enum has **no `Workload` variant** (§3.1,
  C1/C3) — which matches today's behavior (nothing inbound ever verified a workload token), so it is a
  modeling clarification rather than a runtime change. The §4.1 hardening
  (`header.typ` rejection and `#[serde(deny_unknown_fields)]`) is **Phase 1, NOT Phase 0** — Phase 0
  preserves today's exact posture (no `typ` check, no `deny_unknown_fields`), so "pure refactor,
  identical behavior" does not contradict §4.1. **Scoped out:** the CLI's unverified client-side `exp`
  peek — `read_token_exp` (`src/cli/login/token_utils.rs:65`, used by `token_source.rs`) decodes `exp`
  via `insecure_decode` **without** signature verification; it is **not** one of the two verify paths
  and stays in the CLI. "Centralize all token parsing" means *verification*, not unverified client-side
  decode. Land and merge Phase 0 on its own (it touches many files but preserves request-time auth
  behavior — modulo the two reviewed deltas above — so it reviews cleanly and de-risks the later phases). The `Access` variant of `RiseToken` and
  `verify_external_jwt`'s use by the exchange arrive in Phase 1.
- **Phase 1 — additive, no removals.** Ship the exchange endpoint, `AccessClaims`, and the
  signer methods. Keep the middleware's external branch and `resolve_for_project` in place.
  The new `AccessPrincipal` extractor, when it finds **no** `AccessClaims`, **falls back** to
  the legacy `VerifiedExternalToken` resolution — i.e. the endpoint exchanges internally. Old
  raw-token clients keep working unchanged; clients that pre-exchange skip the per-request DB
  work. Capabilities stays public in this phase.

  **Hard prerequisite — the `header.typ` discriminator (HIGH, not optional).** Phase 0 ships a
  `verify_rise_jwt` that dispatches **only** on `alg` (HS256→`Session`, RS256→`Ingress`) and
  **never reads `header.typ`** (`crates/rise-backend-auth/src/verify.rs`). Because Session and the
  new Access token are **both HS256** and `RiseClaims` has no `#[serde(deny_unknown_fields)]`
  (§4.1), they would be indistinguishable on the HS256 branch. Therefore, as a **mandatory
  acceptance criterion of Phase 1**:
  - **Before** adding `RiseToken::Access` to the HS256 branch, `verify_rise_jwt` MUST branch on
    `header.typ` **first**. A missing/unknown `typ` on HS256 ⇒ `Session` **only** (do **NOT**
    require a specific `typ` on legacy Session tokens: `Header::new` emits the default
    `"JWT"`, so requiring a Session-specific `typ` would break every existing session). The
    Access token carries a distinct custom `typ` (e.g. `rise-access+jwt`) and is matched
    exclusively on it.
  - **Adding the `Access` variant and the `typ` check must land in the same change** — never the
    variant first. Introducing `RiseToken::Access` without the `typ` branch would let an Access
    token be accepted as a `Session` (and vice-versa). This is a hard ordering constraint, not a
    follow-up.

  **Forcing function (H4).** While `auth.allow_raw_external_tokens` is `true`, the legacy attack
  surface ships **on by default** and the headline benefits — no-DB-in-the-hot-path and
  authed `platform/capabilities` — are **NOT realized** (any request can still take the external
  branch). To avoid this becoming permanent: (a) emit a **deprecation metric + log** counting
  raw-token (non-exchanged) requests, keyed by issuer, so operators can see who still needs migrating;
  (b) commit to a **target version** at which the default flips to `false` (call it out in the
  changelog/settings doc when the toggle lands); (c) state plainly in the setting's doc-comment that
  leaving it `true` forfeits the security benefits above.
- **Phase 2 — CLI auto-exchange.** Add an `ExchangingTokenSource` decorator in
  `cli/token_source.rs` that wraps the existing provider: on `token()` it calls the exchange
  endpoint with the inner OIDC token + project name and caches the returned Rise access token,
  reusing the existing `CachedToken`/`is_fresh` machinery. Nested freshness: re-mint the inner
  OIDC token only when it's stale; re-exchange the outer Rise token when it's stale. The deploy
  command (which knows the project) constructs it. Pure CLI change — the server already supports
  it from Phase 1.
- **Phase 3 — remove the legacy path,** gated behind an operator toggle
  `auth.allow_raw_external_tokens` (default `true` → flip to `false`). Delete the middleware
  external branch, `resolve_for_project`, `VerifiedExternalToken`, and the extractor fallback;
  flip capabilities to auth-only. The toggle lets operators control the cutover instead of a
  hard version break.

## 8. Files to change (for the eventual implementation)

- **New crate** `crates/rise-backend-auth/` (Phase 0, **landed**) — claim types (`RiseClaims`,
  `AccessClaims` / `PrincipalClaims` / `Scope`, `WorkloadClaims`, `ExternalClaims`), the two entry
  points `verify_external_jwt` / `verify_rise_jwt` (`RiseToken`), `RiseTokenSigner` (+ `compute_key_id`),
  the pure matchers (`match_controller_identity`, `validate_custom_claims`, `audience_matches` /
  `matches_wildcard_pattern`, `build_controller_indexes`, `validate_controller_id`,
  `validate_oidc_issuer`), `is_rise_issued_jwt`, the `JwksKeySource` trait, and `AuthError` /
  `JwtSignerError`. Add it as a workspace member in the root `Cargo.toml`; `rise-deploy` depends on it.
  No `reqwest`/`axum`/`sqlx`. **Update the `CLAUDE.md` "single consolidated crate" note** to reflect
  the auth crate.
- `src/server/auth/jwt.rs` (Phase 0) — `JwtValidator` becomes the rise-deploy `JwksKeySource`
  implementation (reqwest + `ssrf` + JWKS cache) that backs `verify_external_jwt`; its pure
  claim-matching moves into the crate.
- **New module** `src/server/auth/exchange/` (`mod.rs`, `handlers.rs`, `models.rs`, `routes.rs`)
  — the `POST /api/v1/auth/token` handler; relocate `resolve_for_project`'s body here; reuse the
  crate's `verify_external_jwt` + matchers, plus `oauth_rate_limiter`, `extract_bearer_token`.
- **New** `src/server/auth/access.rs` — the axum `AccessPrincipal` extractor (the `AccessClaims` /
  `PrincipalClaims` / `Scope` *types* live in the crate; only the extractor is rise-deploy-side).
- Access-token signing/verification land **in the crate**: a `sign_access` method on
  `RiseTokenSigner` and the `RiseToken::Access` arm of `verify_rise_jwt` (HS256-only,
  `aud == public_url`). `src/server/auth/jwt_signer.rs` becomes a thin wrapper over the crate's
  signer (or is removed once Phase 0 relocates signing); `RiseClaims` / `WorkloadClaims` behavior is
  preserved, not changed.
- `src/server/auth/middleware.rs` — Phase 1 adds `AccessClaims` recognition; Phase 3 deletes the
  external branch and its guard. **Same Phase-1 edit (H1):** `platform_access_middleware` must
  recognize the `AccessPrincipal` extension (SA/Controller bypass the allowlist, `User` gated as
  today) or it 500s on every access-token request the moment the middleware injects one — see §6.
- `src/server/mod.rs` — register `auth::exchange::routes()` under `public_routes`; (Phase 3)
  move `platform::routes()` into `auth_only_routes`.
- **Mechanical handler edits** (one pattern, ~10 project-scoped sites): `src/server/deployment/handlers.rs`
  and `src/server/registry/handlers.rs` swap `AuthContext` → `AccessPrincipal` and
  `resolve_for_project(...)` → `require_project(project.id)?`; the env block becomes
  `auth.allowed_environment_ids()`. The one **discover-from-deployment** site
  (`update_deployment_status`, §4.3) is *not* mechanical — it stays on the legacy path until Phase 3.
  `src/server/resources/handlers.rs` is *not* a one-line swap either (§6, H3): it must preserve
  per-kind authz (operator-user `update_resource`, controller item-update prohibition,
  per-resource `enforce_controller_allowed` on status subresources).
- **Phase 3 removals:** `AuthContext` / `AnyAuth` / `VerifiedExternalToken` from `context.rs`;
  `ControllerAuthContext` / `VerifiedControllerToken` from `controller.rs`. (`match_controller_identity`
  / `build_controller_indexes` / `ControllerIdentity` are not removed — they were **relocated to the
  crate in Phase 0**, §3.3, and are re-exported for the exchange handler and `settings.rs`.)
- `src/server/settings.rs` — add `auth_token_max_ttl_seconds` and `auth.allow_raw_external_tokens`;
  regenerate the schema (`mise run config:schema:generate`).
- `src/cli/token_source.rs` — `ExchangingTokenSource` decorator (reuses `CachedToken`/`is_fresh`).
- **Docs** — update auth docs under `docs/` and keep this plan current. **Source of truth (M1):** the
  §3.2 token-model table's canonical home is `crates/rise-backend-auth/README.md` (next to the types,
  and **now created** in Phase 0); §3.2 and any operator/auth doc under `docs/` **link** to it rather
  than copy it. The `rise_token_disambiguation_matrix` crate unit test (§3.2) is the anti-drift
  enforcement keeping the README table and `verify_rise_jwt` in sync; it lands in **Phase 1** with the
  `typ` discriminator and `RiseToken::Access` (the matrix's discriminating rows do not exist before
  then).

**DB note:** Phase 1 needs **no new SQLX** — the exchange reuses
`service_accounts::find_by_project_and_issuer`, `projects::find_by_name`, and
`users::find_by_id`. Per `CLAUDE.md`, all SQLX must live in `src/db`; the exchange handler must
not embed raw queries. Per-SA configurable scopes (deferred) would add a column + migration +
a `src/db/service_accounts.rs` helper + `mise run sqlx:prepare`.

## 9. Risks & trade-offs

- **Revocation.** An exchanged Rise token can't be revoked mid-life: deleting an SA or
  tightening its `allowed_environment_ids` won't take effect until the token expires. Mitigate
  with a short TTL (≤10 min — the same exposure window the workload tokens already accept). The
  `jti` field leaves room for a future deny-list.
- **Config drift.** Scopes and env restrictions are snapshotted at exchange time. Short TTL
  bounds the staleness; this is inherent to the "snap decision" goal.
- **Clock / TTL skew.** Reuse the CLI's 60s skew + 2/3-lifetime refresh; keep server-side leeway
  consistent with `verify_user_jwt`.
- **Algorithm confusion.** `verify_access_jwt` must be HS256-only so an RS256 ingress/workload
  token can never be parsed as an access token.
- **Key rotation.** Access tokens reuse the **same HS256 symmetric secret** as user sessions
  (jwt_signer.rs ~162 holds a single `hs256_encoding_key`/`hs256_decoding_key`), so rotating it
  invalidates **all live sessions AND all live access tokens at once**. Two acceptable postures:
  (a) support an **overlapping previous-key verification window** — verify against `{current, previous}`
  while signing only with `current` — so rotation drains gracefully within one TTL; or (b) document
  rotation as a **hard dual cutover** (sessions + access tokens re-mint together). The short access-token
  TTL (≤10 min) bounds option (b)'s blast radius for SAs, but UI sessions are longer-lived, so (a) is
  preferred. This interacts with the §4.1 `deny_unknown_fields` rollout on `RiseClaims`, which also
  wants a session-key window.
- **Availability (M2).** Once Phase 3 removes the legacy path, the exchange endpoint is a **hard
  dependency on the deploy critical path** and a **public DoS target** (unauthenticated by design).
  Ops requirements: (a) treat it as a tier-1 endpoint with its own SLO/alerting (it does a JWKS fetch
  + a couple of DB reads — keep JWKS cached, as `validate_token` already does); (b) on `5xx` the CLI
  should **retry, reusing the existing `token_with_retry`** machinery (token_source.rs ~115), so a
  transient blip doesn't fail a deploy; (c) the `auth.allow_raw_external_tokens` toggle (§7) doubles as
  a **brief escape hatch** — operators can re-enable the legacy path if the exchange is degraded — but
  it is not a substitute for making the endpoint highly available.

## 10. Deferred / out of scope

- Per-SA configurable scopes (needs a DB column + migration).
- Unifying the user OIDC login flow onto `AccessClaims::User` (touches cookies, ingress,
  `verify_jwt_skip_aud`).
- A `jti` deny-list for hard revocation (reintroduces a DB lookup, partially defeating the
  no-DB-in-middleware goal — only if the short-TTL window proves insufficient).
- **De-duplicate the controller-id validator (tracked follow-up).** Post-Phase-0,
  `crates/rise-backend-auth` and `crates/rise-resource-api` **each** carry their own controller-id
  validation logic. They should be consolidated into a single shared validator to prevent the two
  copies silently drifting (e.g. one gaining a structural rule the other lacks).
