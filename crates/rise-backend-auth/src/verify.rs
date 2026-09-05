//! The two unified verification entry points.
//!
//! - [`RiseTokenSigner::verify_rise_jwt`] — the single path that turns a
//!   Rise-issued JWT into a typed [`RiseToken`].
//! - [`verify_external_jwt`] — the single path that turns an arbitrary external
//!   JWT into validated [`ExternalClaims`].

use std::collections::HashMap;

use async_trait::async_trait;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};

use crate::claims::{AccessClaims, ExternalClaims, RiseClaims};
use crate::error::AuthError;
use crate::signer::{RiseTokenSigner, RISE_ACCESS_TYP};

/// A verified Rise-issued token. The output type models only what the verifier
/// can ever return: `Session` (HS256 session, UI/CLI login), `Access` (HS256
/// exchanged SA / controller principal), and `Ingress` (RS256, deployed-app
/// ingress auth). Workload tokens are sign-only and never appear here.
///
/// The variants carry their claim types; there is no public constructor other
/// than [`RiseTokenSigner::verify_rise_jwt`], so a caller cannot fabricate a
/// verified token.
#[derive(Debug, Clone)]
pub enum RiseToken {
    /// HS256, `typ = "JWT"`, aud = public_url — UI / CLI user login.
    Session(RiseClaims),
    /// HS256, `typ = "rise-access+jwt"`, aud = public_url — exchanged SA /
    /// controller principal (RFC 8693 token exchange).
    Access(AccessClaims),
    /// RS256, aud = project_url — deployed-app ingress auth (aud not checked here).
    Ingress(RiseClaims),
}

impl RiseTokenSigner {
    /// Verify a Rise-issued JWT and classify it into a typed [`RiseToken`].
    ///
    /// Verifies the signature and `iss`, then disambiguates:
    /// - **RS256** → [`RiseToken::Ingress`].
    /// - **HS256** → branch on the header `typ`: the access `typ`
    ///   ([`RISE_ACCESS_TYP`]) → [`RiseToken::Access`]; anything else (the default
    ///   `"JWT"`, a missing or unknown `typ`) → [`RiseToken::Session`]. Legacy
    ///   session tokens carry the default `"JWT"`, so the access `typ` is matched
    ///   *exclusively* — never requiring a session-specific `typ` that would break
    ///   existing sessions.
    ///
    /// No `aud` check is performed here — callers enforce audience per context
    /// (the API middleware requires `aud == public_url` for `Session` and
    /// `Access`; ingress ignores `aud`, exactly as today).
    ///
    /// This is the single inbound verification path for Rise tokens. The legacy
    /// `verify_user_jwt` / `verify_jwt_skip_aud` methods are thin adapters over
    /// it.
    pub fn verify_rise_jwt(&self, token: &str) -> Result<RiseToken, AuthError> {
        let header = decode_header(token)?;

        match header.alg {
            Algorithm::HS256 => {
                let mut validation = Validation::new(Algorithm::HS256);
                validation.set_issuer(&[self.issuer()]);
                // No aud check at this layer.
                validation.validate_aud = false;

                // Branch on the header `typ` FIRST: the access token and the
                // session token are both HS256, so the `typ` is the only safe
                // discriminator (`RiseClaims` has no `deny_unknown_fields`).
                if header.typ.as_deref() == Some(RISE_ACCESS_TYP) {
                    let token_data =
                        decode::<AccessClaims>(token, self.hs256_decoding_key(), &validation)?;
                    Ok(RiseToken::Access(token_data.claims))
                } else {
                    let token_data =
                        decode::<RiseClaims>(token, self.hs256_decoding_key(), &validation)?;
                    Ok(RiseToken::Session(token_data.claims))
                }
            }
            Algorithm::RS256 => {
                let mut validation = Validation::new(Algorithm::RS256);
                validation.set_issuer(&[self.issuer()]);
                validation.validate_aud = false;

                let token_data =
                    decode::<RiseClaims>(token, self.rs256_decoding_key(), &validation)?;
                Ok(RiseToken::Ingress(token_data.claims))
            }
            _ => Err(AuthError::Jwt(
                jsonwebtoken::errors::ErrorKind::InvalidAlgorithm.into(),
            )),
        }
    }
}

/// Source of JWKS decoding keys for an external issuer.
///
/// rise-deploy supplies the concrete implementation (reqwest + SSRF validation +
/// cache); the pure crate only depends on this trait so it never touches the
/// network.
#[async_trait]
pub trait JwksKeySource: Send + Sync {
    /// Return the decoding keys (keyed by `kid`) for the given issuer.
    async fn decoding_keys(&self, issuer: &str) -> Result<HashMap<String, DecodingKey>, AuthError>;
}

/// Validate an arbitrary external JWT's signature and expiry against an issuer.
///
/// This is the single inbound verification path for external (non-Rise) tokens:
/// 1. Decode the header to get the key ID (`kid`).
/// 2. Fetch JWKS for the issuer via the [`JwksKeySource`].
/// 3. Verify the signature (RS256) against the matching key, with `iss` set and
///    audience validation disabled. Expiry (`exp`) is enforced by
///    [`Validation`]'s defaults (`validate_exp`).
///
/// Custom claim matching is performed separately (see
/// [`validate_custom_claims`](crate::validate_custom_claims)).
pub async fn verify_external_jwt(
    token: &str,
    issuer: &str,
    keys: &dyn JwksKeySource,
) -> Result<ExternalClaims, AuthError> {
    // Decode header to get key ID
    let header = decode_header(token)?;
    let kid = header.kid.ok_or(AuthError::MissingKid)?;

    // Get JWKS for this issuer
    let key_map = keys.decoding_keys(issuer).await?;

    // Get the decoding key
    let key = key_map.get(&kid).ok_or_else(|| AuthError::KeyNotFound {
        kid: kid.clone(),
        issuer: issuer.to_string(),
    })?;

    // Set up validation parameters
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[issuer]);
    // Disable built-in audience validation - validated separately via expected_claims
    validation.validate_aud = false;

    // Validate and decode the token. `exp` is enforced by `Validation`
    // (`validate_exp` is true by default), with jsonwebtoken's default 60s
    // clock-skew leeway.
    let token_data = decode::<serde_json::Value>(token, key, &validation)?;

    Ok(ExternalClaims::new(issuer.to_string(), token_data.claims))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::tests::create_test_signer;
    use jsonwebtoken::{encode, Header};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn test_verify_rise_jwt_rs256_is_ingress() {
        let signer = create_test_signer();
        let claims = RiseClaims {
            sub: "user456".to_string(),
            email: "user2@example.com".to_string(),
            name: None,
            groups: None,
            iat: now(),
            exp: now() + 3600,
            iss: "https://rise.test".to_string(),
            aud: "https://myapp.apps.rise.dev".to_string(),
        };
        let token = signer
            .sign_ingress_jwt(
                &serde_json::json!({"sub": claims.sub, "email": claims.email}),
                None,
                &claims.aud,
                Some(claims.exp),
            )
            .unwrap();

        match signer.verify_rise_jwt(&token).unwrap() {
            RiseToken::Ingress(c) => {
                assert_eq!(c.sub, "user456");
                assert_eq!(c.email, "user2@example.com");
                assert_eq!(c.aud, "https://myapp.apps.rise.dev");
            }
            other => panic!("expected Ingress, got {other:?}"),
        }
    }

    #[test]
    fn test_verify_rise_jwt_hs256_is_session() {
        let signer = create_test_signer();
        let token = signer
            .sign_user_jwt(
                &serde_json::json!({"sub": "user123", "email": "user@example.com"}),
                None,
                "https://rise.test",
                None,
            )
            .unwrap();

        match signer.verify_rise_jwt(&token).unwrap() {
            RiseToken::Session(c) => {
                assert_eq!(c.sub, "user123");
                assert_eq!(c.aud, "https://rise.test");
            }
            other => panic!("expected Session, got {other:?}"),
        }
    }

    #[test]
    fn test_verify_rise_jwt_rejects_workload_token() {
        // A workload identity token is RS256 and signed with the same key and
        // issuer as an ingress token; the ONLY thing preventing it from verifying
        // as `RiseToken::Ingress` is that `WorkloadClaims` carries no `email`
        // while `RiseClaims` requires one. Lock that separation so it cannot
        // silently break (e.g. if `email` is ever made `Option`/`#[serde(default)]`).
        let signer = create_test_signer();
        let token = signer
            .sign_workload_jwt(
                &crate::WorkloadSubjectInfo {
                    sub: "rise:proj:myapp:env:prod",
                    project: "myapp",
                    environment: "prod",
                    deployment_group: "default",
                    deployment_id: "20260101-000000",
                },
                "sts.amazonaws.com",
                900,
            )
            .unwrap();

        assert!(
            signer.verify_rise_jwt(&token).is_err(),
            "workload token must not verify as a Rise session/ingress token"
        );
    }

    #[test]
    fn test_verify_rs256_jwt_via_skip_aud_adapter() {
        let signer = create_test_signer();
        let token = signer
            .sign_ingress_jwt(
                &serde_json::json!({"sub": "user456", "email": "user2@example.com"}),
                None,
                "https://myapp.apps.rise.dev",
                None,
            )
            .unwrap();

        let verified_claims = signer.verify_jwt_skip_aud(&token).unwrap();
        assert_eq!(verified_claims.sub, "user456");
        assert_eq!(verified_claims.email, "user2@example.com");
        assert_eq!(verified_claims.aud, "https://myapp.apps.rise.dev");
    }

    #[test]
    fn test_verify_user_jwt_accepts_valid_hs256() {
        let signer = create_test_signer();
        let token = signer
            .sign_user_jwt(
                &serde_json::json!({"sub": "user123", "email": "user@example.com"}),
                None,
                "https://rise.test",
                None,
            )
            .unwrap();

        let verified = signer.verify_user_jwt(&token, "https://rise.test").unwrap();
        assert_eq!(verified.sub, "user123");
        assert_eq!(verified.email, "user@example.com");
        assert_eq!(verified.aud, "https://rise.test");
    }

    #[test]
    fn test_verify_user_jwt_rejects_rs256() {
        let signer = create_test_signer();
        let claims = RiseClaims {
            sub: "user456".to_string(),
            email: "user2@example.com".to_string(),
            name: None,
            groups: None,
            iat: now(),
            exp: now() + 3600,
            iss: "https://rise.test".to_string(),
            aud: "https://rise.test".to_string(),
        };
        let token = signer
            .sign_ingress_jwt(
                &serde_json::json!({"sub": claims.sub, "email": claims.email}),
                None,
                &claims.aud,
                Some(claims.exp),
            )
            .unwrap();

        let result = signer.verify_user_jwt(&token, "https://rise.test");
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_user_jwt_rejects_wrong_audience() {
        let signer = create_test_signer();
        let token = signer
            .sign_user_jwt(
                &serde_json::json!({"sub": "user789", "email": "user3@example.com"}),
                None,
                "https://myapp.apps.rise.dev",
                None,
            )
            .unwrap();

        let result = signer.verify_user_jwt(&token, "https://rise.test");
        assert!(matches!(
            result,
            Err(crate::error::JwtSignerError::AudienceMismatch)
        ));
    }

    /// The test signer is built from a known HS256 secret (`[0u8; 32]`), so a key
    /// reconstructed from the same bytes signs tokens that verify against it. This
    /// lets the tests hand-craft HS256 tokens with arbitrary `typ` / payloads.
    fn hs256_key() -> jsonwebtoken::EncodingKey {
        jsonwebtoken::EncodingKey::from_secret(&[0u8; 32])
    }

    fn session_claims() -> RiseClaims {
        RiseClaims {
            sub: "u".to_string(),
            email: "u@example.com".to_string(),
            name: None,
            groups: None,
            iat: now(),
            exp: now() + 3600,
            iss: "https://rise.test".to_string(),
            aud: "https://rise.test".to_string(),
        }
    }

    fn controller_principal() -> crate::PrincipalClaims {
        crate::PrincipalClaims::Controller {
            name: "controller-example".to_string(),
            uid: uuid::Uuid::nil(),
        }
    }

    #[test]
    fn test_verify_rise_jwt_access_round_trip() {
        let signer = create_test_signer();
        let (token, minted) = signer
            .sign_access_jwt(
                "controller:controller-example",
                controller_principal(),
                "https://rise.test",
                600,
            )
            .unwrap();

        // Header carries the access typ.
        let header = jsonwebtoken::decode_header(&token).unwrap();
        assert_eq!(header.alg, Algorithm::HS256);
        assert_eq!(header.typ.as_deref(), Some(crate::RISE_ACCESS_TYP));

        match signer.verify_rise_jwt(&token).unwrap() {
            RiseToken::Access(c) => {
                assert_eq!(c.sub, "controller:controller-example");
                assert_eq!(c.aud, "https://rise.test");
                assert_eq!(c.iss, "https://rise.test");
                assert_eq!(c.jti, minted.jti);
                assert!(matches!(
                    c.principal,
                    crate::PrincipalClaims::Controller { .. }
                ));
            }
            other => panic!("expected Access, got {other:?}"),
        }
    }

    #[test]
    fn test_access_token_rejected_by_legacy_adapters() {
        let signer = create_test_signer();
        let (token, _) = signer
            .sign_access_jwt(
                "controller:x",
                controller_principal(),
                "https://rise.test",
                600,
            )
            .unwrap();

        assert!(
            signer.verify_user_jwt(&token, "https://rise.test").is_err(),
            "access token must not verify as a user/session token"
        );
        assert!(
            signer.verify_jwt_skip_aud(&token).is_err(),
            "access token must not verify on the ingress path"
        );
    }

    #[test]
    fn test_verify_jwt_skip_aud_rejects_principal_claim() {
        // A token WITHOUT the access typ (default "JWT") but carrying a `principal`
        // claim deserializes as a session token (extra field ignored), so the typ
        // discriminator alone would not catch it. The ingress path must still
        // reject it (§4.1 hardening).
        let claims = serde_json::json!({
            "sub": "u",
            "email": "u@example.com",
            "iss": "https://rise.test",
            "aud": "https://rise.test",
            "iat": now(),
            "exp": now() + 3600,
            "principal": { "kind": "controller", "name": "x", "uid": "00000000-0000-0000-0000-000000000000" },
        });
        let header = Header::new(Algorithm::HS256); // default typ "JWT"
        let token = encode(&header, &claims, &hs256_key()).unwrap();

        let signer = create_test_signer();
        // It classifies as a Session (typ is "JWT", principal ignored)...
        assert!(matches!(
            signer.verify_rise_jwt(&token).unwrap(),
            RiseToken::Session(_)
        ));
        // ...but the ingress adapter rejects the principal-shaped payload.
        assert!(
            signer.verify_jwt_skip_aud(&token).is_err(),
            "a principal-carrying token must be rejected on the ingress path"
        );
    }

    /// Anti-drift: assert the `(alg, header-typ) → RiseToken variant / rejection`
    /// matrix against the real `verify_rise_jwt`, keeping the crate README table
    /// and the code from silently diverging.
    #[test]
    fn rise_token_disambiguation_matrix() {
        let signer = create_test_signer();
        let key = hs256_key();

        // Helper: encode session-shaped claims under HS256 with a chosen `typ`.
        let encode_hs256 = |typ: Option<&str>| {
            let mut header = Header::new(Algorithm::HS256);
            header.typ = typ.map(|t| t.to_string());
            encode(&header, &session_claims(), &key).unwrap()
        };

        // HS256 + access typ → Access (use the real signer so the payload is
        // AccessClaims-shaped).
        let (access_token, _) = signer
            .sign_access_jwt(
                "controller:x",
                controller_principal(),
                "https://rise.test",
                600,
            )
            .unwrap();
        assert!(matches!(
            signer.verify_rise_jwt(&access_token).unwrap(),
            RiseToken::Access(_)
        ));

        // HS256 + default "JWT" / missing / unknown typ → Session.
        for typ in [Some("JWT"), None, Some("unknown+jwt")] {
            let token = encode_hs256(typ);
            assert!(
                matches!(
                    signer.verify_rise_jwt(&token).unwrap(),
                    RiseToken::Session(_)
                ),
                "HS256 typ={typ:?} should classify as Session"
            );
        }

        // RS256 (any typ) → Ingress.
        let ingress = signer
            .sign_ingress_jwt(
                &serde_json::json!({"sub": "u", "email": "u@example.com"}),
                None,
                "https://myapp.apps.rise.dev",
                None,
            )
            .unwrap();
        assert!(matches!(
            signer.verify_rise_jwt(&ingress).unwrap(),
            RiseToken::Ingress(_)
        ));

        // HS384 → rejected (unsupported alg).
        let hs384 = encode(
            &Header::new(Algorithm::HS384),
            &session_claims(),
            &jsonwebtoken::EncodingKey::from_secret(&[0u8; 32]),
        )
        .unwrap();
        assert!(signer.verify_rise_jwt(&hs384).is_err());
    }

    #[test]
    fn test_verify_rise_jwt_rejects_other_algorithm() {
        let signer = create_test_signer();
        // Build a token with an unsupported alg in the header by hand-encoding
        // with ES256-style header is hard without a key; instead assert HS384 is
        // rejected via the signer's HS256 secret (alg mismatch yields error).
        let claims = RiseClaims {
            sub: "x".to_string(),
            email: "x@example.com".to_string(),
            name: None,
            groups: None,
            iat: now(),
            exp: now() + 3600,
            iss: "https://rise.test".to_string(),
            aud: "https://rise.test".to_string(),
        };
        let header = Header::new(Algorithm::HS384);
        // Sign with the wrong alg using a throwaway secret; verify must fail.
        let key = jsonwebtoken::EncodingKey::from_secret(&[0u8; 32]);
        let token = encode(&header, &claims, &key).unwrap();
        assert!(signer.verify_rise_jwt(&token).is_err());
    }
}
