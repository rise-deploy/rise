//! The two unified verification entry points.
//!
//! - [`RiseTokenSigner::verify_rise_jwt`] — the single path that turns a
//!   Rise-issued JWT into a typed [`RiseToken`].
//! - [`verify_external_jwt`] — the single path that turns an arbitrary external
//!   JWT into validated [`ExternalClaims`].

use std::collections::HashMap;

use async_trait::async_trait;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};

use crate::claims::{ExternalClaims, RiseClaims};
use crate::error::AuthError;
use crate::signer::RiseTokenSigner;

/// A verified Rise-issued token. The output type models only what the verifier
/// can ever return: `Session` (HS256, UI/CLI login) and `Ingress` (RS256,
/// deployed-app ingress auth). Workload tokens are sign-only and never appear
/// here.
///
/// The variants carry [`RiseClaims`]; there is no public constructor other than
/// [`RiseTokenSigner::verify_rise_jwt`], so a caller cannot fabricate a verified
/// token.
#[derive(Debug, Clone)]
pub enum RiseToken {
    /// HS256, aud = public_url — UI / CLI user login.
    Session(RiseClaims),
    /// RS256, aud = project_url — deployed-app ingress auth (aud not checked here).
    Ingress(RiseClaims),
}

impl RiseTokenSigner {
    /// Verify a Rise-issued JWT and classify it into a typed [`RiseToken`].
    ///
    /// Verifies the signature and `iss`, then disambiguates by **algorithm**:
    /// HS256 → [`RiseToken::Session`], RS256 → [`RiseToken::Ingress`]. No `aud`
    /// check is performed here — callers enforce audience per context (the API
    /// middleware requires `Session` with `aud == public_url`; ingress accepts
    /// either variant and ignores `aud`, exactly as today).
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

                let token_data =
                    decode::<RiseClaims>(token, self.hs256_decoding_key(), &validation)?;
                Ok(RiseToken::Session(token_data.claims))
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
///    audience validation disabled.
/// 4. Double-check token expiry.
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

    // Validate and decode the token
    let token_data = decode::<serde_json::Value>(token, key, &validation)?;

    // Validate exp claim manually (should be handled by jsonwebtoken, but double-check)
    if let Some(exp) = token_data.claims.get("exp").and_then(|v| v.as_u64()) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        if now > exp {
            return Err(AuthError::Expired);
        }
    }

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
        assert!(result.is_err());
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
