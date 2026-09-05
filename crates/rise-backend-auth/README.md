# rise-backend-auth

Pure-core token signing, verification, and matching for Rise — the single home
for Rise's auth-token logic. It is intentionally **pure**: no `reqwest`, `axum`,
`sqlx`, `tokio`, `anyhow`, or `regex`. JWKS fetching is abstracted behind the
[`JwksKeySource`] trait, implemented by `rise-deploy`.

The crate exposes exactly **two** verification entry points:

- [`RiseTokenSigner::verify_rise_jwt`] — turns a **Rise-issued** JWT into a typed
  [`RiseToken`] (synchronous, key-local: no JWKS / network).
- [`verify_external_jwt`] — turns an **arbitrary external** JWT into validated,
  opaque [`ExternalClaims`] (async; the only path that touches a `JwksKeySource`).

Both `RiseToken` and `ExternalClaims` have **private fields and no public
constructor** — the only way to obtain one is through the corresponding verify
function. A caller therefore cannot fabricate a "verified" value or hand-roll a
second validation path; the compiler enforces that every auth decision flows
through this crate ("parse, don't validate").

> See [`../../ROADMAP.md`](../../ROADMAP.md) § "Workstream 2 — Authentication
> & Token Exchange" for the phasing and rationale.

## Token-disambiguation matrix (canonical)

This table is the **canonical** `(alg, header-typ, aud) → RiseToken variant`
mapping. The plan (§3.2) and any operator/auth doc under `docs/` link here rather
than copy it. Anti-drift is (will be) enforced by the parametrized crate unit test
`rise_token_disambiguation_matrix` (see "Roadmap" below).

### Rise-issued tokens — `verify_rise_jwt` (Path B)

`verify_rise_jwt` verifies the signature and `iss`, then classifies the token.
It does **not** check `aud` (the per-request audience is unknown to a context-free
verifier; callers enforce audience per context).

| Alg | header `typ` | Verifier output | Notes |
|---|---|---|---|
| HS256 | `"rise-access+jwt"` | `RiseToken::Access(AccessClaims)` | exchanged SA / controller principal (RFC 8693), `aud = public_url` (checked by the API middleware, not here) |
| HS256 | any other (incl. default `"JWT"`, missing, unknown) | `RiseToken::Session(RiseClaims)` | UI / CLI user login, `aud = public_url` (checked by the API middleware, not here) |
| RS256 | any (incl. default `"JWT"`) | `RiseToken::Ingress(RiseClaims)` | deployed-app ingress auth, `aud = project_url` (not checked here) |
| anything else | — | **rejected** (`InvalidAlgorithm`) | only HS256 / RS256 are accepted |

**Dispatch:** RS256 → `Ingress`. HS256 branches on the header `typ` **first** — the
access `typ` (`rise-access+jwt`) → `Access`; anything else → `Session`. The access
`typ` is matched *exclusively*; legacy session tokens carry the default `"JWT"`, so a
session is never required to set a specific `typ`. The
`rise_token_disambiguation_matrix` unit test pins this table against the real
`verify_rise_jwt`.

Two notes on the RS256 / HS256 boundaries:

- **Session vs. Access (HS256).** Both are HS256, and `RiseClaims` has no
  `#[serde(deny_unknown_fields)]`, so they are not serde-distinguishable — the
  `header.typ` discriminator is what keeps them apart. `AccessClaims` *does* use
  `deny_unknown_fields`. The ingress adapter (`verify_jwt_skip_aud`) additionally
  rejects any token carrying a `principal` claim, so an access-shaped payload can
  never be honored on the ingress path.
- **Workload vs. Ingress (RS256).** Rise also mints **Workload** tokens (RS256,
  same key, default `typ:"JWT"`), but they are **sign-only** — `verify_rise_jwt`
  never returns a `Workload` variant. The *only* thing currently keeping a Workload
  token from deserializing as `Ingress` on the RS256 branch is that `RiseClaims.email`
  is **required** and `WorkloadClaims` never sets it. This is **fragile** (it breaks
  if `email` ever becomes optional, or a workload subject carries `email`); making the
  RS256 Ingress-vs-Workload split explicit via a distinct `typ` is still deferred (see
  "Roadmap").

### Sign-only Rise token (not a verifier output)

| Token | Alg | header `typ` | Verify | Purpose |
|---|---|---|---|---|
| Workload (`WorkloadClaims`) | RS256 | `"JWT"` | n/a — sign-only; verified **externally** via the published JWKS (AWS STS / GCP WIF / Vault), never by Rise | outbound federation |

The signer (`RiseTokenSigner`) mints all four kinds — `sign_user_jwt` (Session),
`sign_access_jwt` (Access), `sign_ingress_jwt` (Ingress), `sign_workload_jwt`
(Workload) — but the verifier classifies only the three inbound kinds above.

### External tokens — `verify_external_jwt` (Path A)

| Token | Alg | Issuer | Verifier output | Purpose |
|---|---|---|---|---|
| External OIDC (service account) | RS256 | external IdP | `ExternalClaims` | CI service-account source token |
| External OIDC (controller) | RS256 | external IdP | `ExternalClaims` | controller source token |

`verify_external_jwt(token, issuer, keys)` fetches JWKS for `issuer` via the
`JwksKeySource`, verifies the RS256 signature against the matching `kid`, and
enforces `iss` + `exp`. Audience and any custom-claim constraints are validated
**separately** by the pure matchers ([`validate_custom_claims`],
[`match_trust_candidates`]) over the resulting `ExternalClaims`.

## Roadmap (still deferred)

- **Explicit RS256 Ingress-vs-Workload `typ` split.** Today the separation relies on
  the missing-`email` serde accident (see above). Giving `sign_workload_jwt` a distinct
  `typ` and adding an inbound workload-verify path is out of scope.
- **`PrincipalClaims::User` minting.** The `User` variant exists but the exchange does
  not mint it; unifying the user OIDC login flow onto `AccessClaims::User` (cookies,
  ingress) is deferred.
- **Per-SA configurable scopes** (a DB column + migration) and a `jti` deny-list for
  hard revocation.

## Public surface (selected)

- Verify: [`verify_external_jwt`], [`RiseTokenSigner::verify_rise_jwt`].
- Sign: [`RiseTokenSigner`] (`sign_user_jwt`, `sign_access_jwt`, `sign_ingress_jwt`,
  `sign_workload_jwt`, `generate_jwks`), [`compute_key_id`], `RISE_ACCESS_TYP`.
- Adapters (legacy, thin wrappers over `verify_rise_jwt`): `verify_user_jwt`
  (HS256 + `aud`), `verify_jwt_skip_aud` (HS256 **or** RS256, `aud` skipped; rejects
  access tokens / `principal`-carrying tokens).
- Claims: [`RiseClaims`], [`AccessClaims`], [`PrincipalClaims`], [`Scope`],
  [`WorkloadClaims`], [`WorkloadSubjectInfo`], [`ExternalClaims`].
- Matchers: [`TrustCandidate`], [`TrustMatch`], [`match_trust_candidates`]
  (shape-agnostic claim matching a caller builds from its own trust-policy
  resources — Controller, ServiceAccount — since this crate cannot depend on
  `rise-resource-api`), `validate_custom_claims`, `audience_matches`,
  `matches_wildcard_pattern`, `validate_oidc_issuer`.
- Routing helper: `is_rise_issued_jwt`.
- Errors: [`AuthError`], [`JwtSignerError`].

[`JwksKeySource`]: src/verify.rs
[`RiseTokenSigner::verify_rise_jwt`]: src/verify.rs
[`verify_external_jwt`]: src/verify.rs
[`RiseToken`]: src/verify.rs
[`ExternalClaims`]: src/claims.rs
[`RiseClaims`]: src/claims.rs
[`WorkloadClaims`]: src/claims.rs
[`WorkloadSubjectInfo`]: src/claims.rs
[`RiseTokenSigner`]: src/signer.rs
[`compute_key_id`]: src/signer.rs
[`TrustCandidate`]: src/matchers.rs
[`TrustMatch`]: src/matchers.rs
[`validate_custom_claims`]: src/matchers.rs
[`match_trust_candidates`]: src/matchers.rs
[`AuthError`]: src/error.rs
[`JwtSignerError`]: src/error.rs
