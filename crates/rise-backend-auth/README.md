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

> See [`../../AUTH_TOKEN_EXCHANGE_PLAN.md`](../../AUTH_TOKEN_EXCHANGE_PLAN.md) for
> the full design, phasing, and rationale.

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
| HS256 | any (incl. default `"JWT"`) | `RiseToken::Session(RiseClaims)` | UI / CLI user login, `aud = public_url` (checked by the API middleware, not here) |
| RS256 | any (incl. default `"JWT"`) | `RiseToken::Ingress(RiseClaims)` | deployed-app ingress auth, `aud = project_url` (not checked here) |
| anything else | — | **rejected** (`InvalidAlgorithm`) | only HS256 / RS256 are accepted |

**Today (Phase 0): dispatch is on `alg` only.** `verify_rise_jwt` does **not** read
`header.typ`, and there is no `RiseToken::Access` variant. Session vs. Ingress is
decided purely by algorithm.

Two notes on the **current** RS256 / HS256 boundaries:

- **Workload vs. Ingress (RS256).** Rise also mints **Workload** tokens (RS256,
  same key, default `typ:"JWT"`), but they are **sign-only** — `verify_rise_jwt`
  never returns a `Workload` variant. The *only* thing currently keeping a Workload
  token from deserializing as `Ingress` on the RS256 branch is that `RiseClaims.email`
  is **required** and `WorkloadClaims` never sets it. This is **fragile** (it breaks
  if `email` ever becomes optional, or a workload subject carries `email`); the
  Phase-1 `typ` discriminator should make this separation explicit.
- **Session vs. Access (HS256).** Once `RiseToken::Access` is added it will also be
  HS256, and `RiseClaims` has no `#[serde(deny_unknown_fields)]`, so the two are not
  serde-distinguishable. The Phase-1 `header.typ` discriminator is what keeps them
  apart (see "Roadmap").

### Sign-only Rise token (not a verifier output)

| Token | Alg | header `typ` | Verify | Purpose |
|---|---|---|---|---|
| Workload (`WorkloadClaims`) | RS256 | `"JWT"` | n/a — sign-only; verified **externally** via the published JWKS (AWS STS / GCP WIF / Vault), never by Rise | outbound federation |

### External tokens — `verify_external_jwt` (Path A)

| Token | Alg | Issuer | Verifier output | Purpose |
|---|---|---|---|---|
| External OIDC (service account) | RS256 | external IdP | `ExternalClaims` | CI service-account source token |
| External OIDC (controller) | RS256 | external IdP | `ExternalClaims` | controller source token |

`verify_external_jwt(token, issuer, keys)` fetches JWKS for `issuer` via the
`JwksKeySource`, verifies the RS256 signature against the matching `kid`, and
enforces `iss` + `exp`. Audience and any custom-claim constraints are validated
**separately** by the pure matchers ([`validate_custom_claims`],
[`match_controller_identity`]) over the resulting `ExternalClaims`.

## Roadmap (Phase 1+ additions)

The following are **not yet implemented** but are part of the token model the plan
describes. They will extend the matrix above:

- **`RiseToken::Access(AccessClaims)`** — HS256, `aud = public_url`, the exchanged
  service-account / controller principal (RFC 8693 token exchange).
- **`header.typ` discriminator (hard prerequisite for `Access`).** Because Session
  and Access are both HS256, `verify_rise_jwt` MUST branch on `header.typ` **before**
  `RiseToken::Access` is added — in the **same** change. The rules:
  - HS256 + Access `typ` (e.g. `rise-access+jwt`) ⇒ `Access`.
  - HS256 + missing/unknown `typ` ⇒ `Session` (do **not** require a specific `typ`
    on legacy Session tokens — `Header::new` emits the default `"JWT"`, so a
    Session-specific requirement would break existing sessions).
  - The same mechanism should also make Ingress vs. Workload separation explicit
    rather than relying on the missing-`email` serde accident.
- **`rise_token_disambiguation_matrix` test** — a parametrized unit test asserting
  this whole matrix against the real `verify_rise_jwt`, landing **with** the `typ`
  discriminator (its discriminating rows do not exist before then).

## Public surface (selected)

- Verify: [`verify_external_jwt`], [`RiseTokenSigner::verify_rise_jwt`].
- Sign: [`RiseTokenSigner`] (`sign_user_jwt`, `sign_ingress_jwt`,
  `sign_workload_jwt`, `generate_jwks`), [`compute_key_id`].
- Adapters (legacy, thin wrappers over `verify_rise_jwt`): `verify_user_jwt`
  (HS256 + `aud`), `verify_jwt_skip_aud` (HS256 **or** RS256, `aud` skipped).
- Claims: [`RiseClaims`], [`WorkloadClaims`], [`WorkloadSubjectInfo`],
  [`ExternalClaims`].
- Matchers / config: [`ControllerIdentity`], [`ControllerIndexes`],
  [`ControllerMatch`], `match_controller_identity`, `build_controller_indexes`,
  `validate_custom_claims`, `audience_matches`, `matches_wildcard_pattern`,
  `validate_controller_id`, `validate_oidc_issuer`.
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
[`ControllerIdentity`]: src/matchers.rs
[`ControllerIndexes`]: src/matchers.rs
[`ControllerMatch`]: src/matchers.rs
[`validate_custom_claims`]: src/matchers.rs
[`match_controller_identity`]: src/matchers.rs
[`AuthError`]: src/error.rs
[`JwtSignerError`]: src/error.rs
