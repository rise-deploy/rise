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
the `VerifiedExternalToken` extension. **Stale-comment caveat (L1):** `controller.rs:9` still says
"No HTTP route consumes the extractor yet" and the type carries `#[allow(dead_code)]` — both are
**stale**: the resources API *does* consume it now. Fix the comment/attribute so a Phase-3 reader
doesn't mistake `ControllerAuthContext` for dead code and delete the live resources path.

### 2.4 Token signing — `src/server/auth/jwt_signer.rs`

`JwtSigner` holds an HS256 symmetric key and an RS256 keypair (the RS256 public key is
exposed via JWKS for third parties to verify). Two claim shapes exist and **must stay
untouched** by this work:

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
// the ONLY entry point for external tokens
async fn verify_external_jwt(token: &str, keys: &impl JwksKeySource)
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
JWT and returns a value that **wholly models** every kind of Rise token:

```rust
// the ONLY entry point for Rise-issued tokens
fn verify_rise_jwt(token: &str, keys: &RiseKeys) -> Result<RiseToken, AuthError>;

enum RiseToken {
    Session(RiseClaims),     // HS256, aud = public_url   (UI / CLI login)
    Ingress(RiseClaims),     // RS256, aud = project_url  (deployed-app ingress auth)
    Access(AccessClaims),    // HS256, aud = public_url   (exchanged SA / controller — §4)
    Workload(WorkloadClaims),// RS256, arbitrary aud      (outbound federation)
}
```

`verify_rise_jwt` performs the alg / header-`typ` / `aud` disambiguation **once, centrally**
(the §4.1 discriminator rules live here, not smeared across call sites). Today's
`verify_user_jwt`, `verify_jwt_skip_aud`, and workload verification become thin shims over this
single path (or are deleted). Callers `match` on `RiseToken` and get a **compile error** if they
forget a variant — the type system enforces exhaustive handling, so a new token kind can't
silently fall through a stale check. This structurally subsumes the earlier C2 "try-then-fallback"
hazard: there is no fallback, only one verifier returning a typed sum.

### 3.2 The complete token model

Every JWT Rise issues or accepts, in one table (Verify = which of the two paths):

| Token (type) | Direction | Alg | Issuer | Audience | Verify | Purpose |
|---|---|---|---|---|---|---|
| Session (`RiseClaims`) | Rise-issued | HS256 | public_url | public_url | B → `Session` | UI / CLI user login |
| Ingress (`RiseClaims`) | Rise-issued | RS256 | public_url | project_url | B → `Ingress` | deployed-app ingress auth |
| Access (`AccessClaims`) | Rise-issued | HS256 | public_url | public_url | B → `Access` | exchanged SA / controller principal (§4) |
| Workload (`WorkloadClaims`) | Rise-issued | RS256 | public_url | caller-supplied | B → `Workload` | outbound federation (AWS/GCP/Vault) |
| External OIDC (SA) | inbound | RS256 | external IdP | SA-matched | A → `ExternalClaims` | CI service-account source token |
| External OIDC (controller) | inbound | RS256 | external IdP | controller-matched | A → `ExternalClaims` | controller source token |

Rise-issued tokens never flow through Path A and external tokens never through Path B; the
`is_rise_issued_jwt` issuer peek (§2.1, §5 step 1) is the *only* branch that routes a raw token
between the two.

### 3.3 The `rise-backend-auth` crate

The repo is **already a Cargo workspace** (`crates/rise-resource-store`,
`crates/rise-resource-api`), so a new member `crates/rise-backend-auth` fits the existing
structure — the "single consolidated crate" line in `CLAUDE.md` is outdated and must be updated
when this lands.

**Pure core — no I/O, no framework, no DB.** The crate depends only on `jsonwebtoken`,
`serde`/`serde_json`, `uuid`, and (for the trait) `async-trait`. It does **not** depend on
`reqwest`, `axum`, or `sqlx`:
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
`WorkloadClaims`, `ExternalClaims`), the two verify entry points + signing (`RiseTokenSigner`),
the pure matchers, `is_rise_issued_jwt`, and `AuthError`.

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
   so the exchange and middleware agree byte-for-byte on what counts as Rise-issued. **Latent
   hazard:** `is_rise_issued_jwt`'s port-stripping `starts_with` is a fuzzy predicate — a
   prefix-superset external issuer (e.g. `https://rise.example.com.evil/`) can satisfy it.
   Tighten it toward exact-match before this lands; the prefix branch is a foot-gun for both
   call sites.
2. Issuer guard: `controllers_by_issuer.contains_key(issuer)` OR
   `service_accounts::issuer_exists(issuer)` — same lightweight guard the middleware uses today.
3. `JwtValidator::validate_token(token, issuer)` — JWKS signature + expiry (unchanged). The exchange
   **reuses this method as-is** rather than re-implementing JWKS fetch: it already SSRF-validates both
   the discovery URL and the returned `jwks_uri` (jwt.rs ~103-129). Note `validate_token` is **RS256-only**
   (jwt.rs ~358) — see L3: HS256/ES256 source IdPs are unsupported by the exchange, as today, except
   that post-Phase-3 the exchange is the *only* ingestion path.
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
  `match_controller_identity` / `build_controller_indexes` / `ControllerIdentity` are **kept**
  (now called by the exchange handler). The rich resolution tests in `context.rs` move to the
  exchange module.
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

- **Phase 0 — extract `rise-backend-auth`, no behavior change.** Move the claim types, the two
  verify entry points (§3.1), signing, and the pure matchers into the new crate; introduce the
  `JwksKeySource` trait and reimplement today's `JwtValidator` as its rise-deploy impl. Replace the
  scattered verifiers (`verify_user_jwt`, `verify_jwt_skip_aud`, `validate_token`, workload
  verification) with shims over `verify_rise_jwt` / `verify_external_jwt`, and adopt them at all
  current call sites. This is a **pure refactor** — identical behavior, no new endpoints, no
  `AccessClaims` yet — establishing the single home *before* any new surface is built. Land and merge
  it on its own (it touches many files but changes no behavior, so it reviews cleanly and de-risks the
  later phases). The `Access` variant of `RiseToken` and `verify_external_jwt`'s use by the exchange
  arrive in Phase 1.
- **Phase 1 — additive, no removals.** Ship the exchange endpoint, `AccessClaims`, and the
  signer methods. Keep the middleware's external branch and `resolve_for_project` in place.
  The new `AccessPrincipal` extractor, when it finds **no** `AccessClaims`, **falls back** to
  the legacy `VerifiedExternalToken` resolution — i.e. the endpoint exchanges internally. Old
  raw-token clients keep working unchanged; clients that pre-exchange skip the per-request DB
  work. Capabilities stays public in this phase.

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

- **New crate** `crates/rise-backend-auth/` (Phase 0) — claim types (`RiseClaims`, `AccessClaims` /
  `PrincipalClaims` / `Scope`, `WorkloadClaims`, `ExternalClaims`), the two entry points
  `verify_external_jwt` / `verify_rise_jwt` (`RiseToken`), `RiseTokenSigner`, the pure matchers,
  `is_rise_issued_jwt`, the `JwksKeySource` trait, and `AuthError`. Add it as a workspace member in
  the root `Cargo.toml`; `rise-deploy` depends on it. No `reqwest`/`axum`/`sqlx`. **Update the
  `CLAUDE.md` "single consolidated crate" note** to reflect the auth crate.
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
  `ControllerAuthContext` / `VerifiedControllerToken` from `controller.rs` (keep
  `match_controller_identity` / `build_controller_indexes` / `ControllerIdentity`).
- `src/server/settings.rs` — add `auth_token_max_ttl_seconds` and `auth.allow_raw_external_tokens`;
  regenerate the schema (`mise run config:schema:generate`).
- `src/cli/token_source.rs` — `ExchangingTokenSource` decorator (reuses `CachedToken`/`is_fresh`).
- **Docs** — update auth docs under `docs/` and keep this plan current.

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
