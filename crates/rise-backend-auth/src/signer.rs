//! Rise-issued token signing and verification.
//!
//! [`RiseTokenSigner`] holds an HS256 symmetric key and an RS256 keypair (the
//! RS256 public key is exposed via JWKS for third parties to verify). It is the
//! single home for minting and verifying Rise-issued JWTs.

use base64::engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD as BASE64URL};
use base64::Engine;
use jsonwebtoken::{encode, Algorithm, DecodingKey, EncodingKey, Header};
use rsa::traits::PublicKeyParts;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::claims::{RiseClaims, WorkloadClaims, WorkloadSubjectInfo};
use crate::error::JwtSignerError;
use crate::verify::RiseToken;

/// JWT signer supporting both HS256 (symmetric) and RS256 (asymmetric) algorithms
///
/// - HS256 is used for user authentication (aud = Rise public URL)
/// - RS256 is used for project ingress authentication (aud = project URL)
///
/// The RS256 keys can be exposed via JWKS for deployed apps to validate tokens.
pub struct RiseTokenSigner {
    // HS256 symmetric key for user authentication
    hs256_encoding_key: EncodingKey,
    hs256_decoding_key: DecodingKey,

    // RS256 asymmetric key pair for ingress authentication
    rs256_encoding_key: Arc<EncodingKey>,
    rs256_decoding_key: Arc<DecodingKey>,
    rs256_public_key_pem: String,
    rs256_key_id: String,

    issuer: String,
    pub default_expiry_seconds: u64,
    claims_to_include: std::collections::HashSet<String>,
}

/// Compute a short key ID from a public key PEM.
///
/// Takes the SHA-256 hash of the PEM bytes and encodes only the first 8 bytes
/// as hex, producing a 16-character key ID.
pub fn compute_key_id(public_key_pem: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let hash = Sha256::digest(public_key_pem);
    let mut key_id = String::with_capacity(16);
    for b in &hash[..8] {
        write!(key_id, "{:02x}", b).unwrap();
    }
    key_id
}

impl RiseTokenSigner {
    /// Create a new JWT signer with both HS256 and RS256 support
    ///
    /// # Arguments
    /// * `hs256_secret_base64` - Base64-encoded signing secret for HS256 (must be at least 32 bytes when decoded)
    /// * `issuer` - Issuer URL (typically the Rise backend URL)
    /// * `default_expiry_seconds` - Default expiration duration in seconds
    /// * `claims_to_include` - List of claim names to include from IdP token (e.g., ["sub", "email", "name"])
    /// * `rs256_private_key_pem` - Optional pre-configured RS256 private key in PEM format
    /// * `rs256_public_key_pem` - Optional pre-configured RS256 public key in PEM format
    ///
    /// If RS256 keys are not provided, a new key pair will be generated on initialization.
    /// To persist JWTs across restarts, provide pre-configured keys.
    pub fn new(
        hs256_secret_base64: &str,
        issuer: String,
        default_expiry_seconds: u64,
        claims_to_include: Vec<String>,
        rs256_private_key_pem: Option<&str>,
        rs256_public_key_pem: Option<&str>,
    ) -> Result<Self, JwtSignerError> {
        // Set up HS256 symmetric key
        let secret = BASE64.decode(hs256_secret_base64)?;

        if secret.len() < 32 {
            return Err(JwtSignerError::InvalidBase64(
                base64::DecodeError::InvalidLength(secret.len()),
            ));
        }

        let hs256_encoding_key = EncodingKey::from_secret(&secret);
        let hs256_decoding_key = DecodingKey::from_secret(&secret);

        // Set up RS256 key pair - either from config or generate new
        let (rs256_encoding_key, rs256_decoding_key, rs256_public_key_pem, rs256_key_id) = if let (
            Some(private_pem),
            Some(public_pem),
        ) =
            (rs256_private_key_pem, rs256_public_key_pem)
        {
            // Use provided keys
            tracing::info!("Using pre-configured RS256 key pair");

            let encoding_key = EncodingKey::from_rsa_pem(private_pem.as_bytes()).map_err(|e| {
                JwtSignerError::RsaKeyError(format!("Invalid RS256 private key: {}", e))
            })?;

            let decoding_key = DecodingKey::from_rsa_pem(public_pem.as_bytes()).map_err(|e| {
                JwtSignerError::RsaKeyError(format!("Invalid RS256 public key: {}", e))
            })?;

            let key_id = compute_key_id(public_pem.as_bytes());

            (encoding_key, decoding_key, public_pem.to_string(), key_id)
        } else if let Some(private_pem) = rs256_private_key_pem {
            // Derive public key from private key
            tracing::info!("Using pre-configured RS256 private key, deriving public key");

            use rsa::pkcs8::{DecodePrivateKey, EncodePublicKey};
            use rsa::RsaPrivateKey;

            let private_key = RsaPrivateKey::from_pkcs8_pem(private_pem).map_err(|e| {
                JwtSignerError::RsaKeyError(format!("Invalid RS256 private key PEM: {}", e))
            })?;

            let public_key = rsa::RsaPublicKey::from(&private_key);
            let public_key_pem = public_key
                .to_public_key_pem(rsa::pkcs8::LineEnding::LF)
                .map_err(|e| JwtSignerError::PemError(e.to_string()))?;

            let encoding_key = EncodingKey::from_rsa_pem(private_pem.as_bytes())
                .map_err(|e| JwtSignerError::RsaKeyError(e.to_string()))?;

            let decoding_key = DecodingKey::from_rsa_pem(public_key_pem.as_bytes())
                .map_err(|e| JwtSignerError::RsaKeyError(e.to_string()))?;

            let key_id = compute_key_id(public_key_pem.as_bytes());

            (encoding_key, decoding_key, public_key_pem, key_id)
        } else {
            // Generate new RS256 key pair (2048-bit RSA key)
            tracing::warn!("No RS256 keys configured - generating new key pair. JWTs will be invalidated on restart. Configure rs256_private_key_pem to persist keys.");

            use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey};
            use rsa::rand_core::OsRng;
            use rsa::{RsaPrivateKey, RsaPublicKey};

            let mut rng = OsRng;
            let bits = 2048;
            let private_key = RsaPrivateKey::new(&mut rng, bits)
                .map_err(|e| JwtSignerError::RsaKeyError(e.to_string()))?;
            let public_key = RsaPublicKey::from(&private_key);

            // Encode keys to PEM format
            let private_key_pem = private_key
                .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
                .map_err(|e| JwtSignerError::PemError(e.to_string()))?
                .to_string();

            let public_key_pem = public_key
                .to_public_key_pem(rsa::pkcs8::LineEnding::LF)
                .map_err(|e| JwtSignerError::PemError(e.to_string()))?;

            // Create encoding and decoding keys
            let encoding_key = EncodingKey::from_rsa_pem(private_key_pem.as_bytes())
                .map_err(|e| JwtSignerError::RsaKeyError(e.to_string()))?;

            let decoding_key = DecodingKey::from_rsa_pem(public_key_pem.as_bytes())
                .map_err(|e| JwtSignerError::RsaKeyError(e.to_string()))?;

            let key_id = compute_key_id(public_key_pem.as_bytes());

            (encoding_key, decoding_key, public_key_pem, key_id)
        };

        Ok(Self {
            hs256_encoding_key,
            hs256_decoding_key,
            rs256_encoding_key: Arc::new(rs256_encoding_key),
            rs256_decoding_key: Arc::new(rs256_decoding_key),
            rs256_public_key_pem,
            rs256_key_id,
            issuer,
            default_expiry_seconds,
            claims_to_include: claims_to_include.into_iter().collect(),
        })
    }

    /// The issuer URL this signer mints tokens for.
    pub(crate) fn issuer(&self) -> &str {
        &self.issuer
    }

    /// HS256 decoding key, used by the central verifier.
    pub(crate) fn hs256_decoding_key(&self) -> &DecodingKey {
        &self.hs256_decoding_key
    }

    /// RS256 decoding key, used by the central verifier.
    pub(crate) fn rs256_decoding_key(&self) -> &DecodingKey {
        &self.rs256_decoding_key
    }

    /// Generate JWKS (JSON Web Key Set) for RS256 public key
    ///
    /// This can be used by deployed applications to validate Rise-issued RS256 JWTs.
    pub fn generate_jwks(&self) -> Result<serde_json::Value, JwtSignerError> {
        // Parse the public key to extract n and e components
        use rsa::pkcs8::DecodePublicKey;
        use rsa::RsaPublicKey;

        let public_key = RsaPublicKey::from_public_key_pem(&self.rs256_public_key_pem)
            .map_err(|e| JwtSignerError::PemError(e.to_string()))?;

        // Extract n (modulus) and e (exponent) as base64url (RFC 7515 standard for JWK)
        let n = BASE64URL.encode(public_key.n().to_bytes_be());
        let e = BASE64URL.encode(public_key.e().to_bytes_be());

        Ok(serde_json::json!({
            "keys": [{
                "kty": "RSA",
                "use": "sig",
                "alg": "RS256",
                "kid": self.rs256_key_id,
                "n": n,
                "e": e,
            }]
        }))
    }

    /// Build the shared [`RiseClaims`] body for user (HS256) and ingress (RS256)
    /// tokens. Extracts the required `sub`/`email`, the optional `name` (only
    /// when configured in `claims_to_include`), computes `iat`/`exp`, and stamps
    /// the issuer and the supplied audience. The caller chooses the algorithm /
    /// header when encoding.
    fn build_rise_claims(
        &self,
        idp_claims: &serde_json::Value,
        groups: Option<Vec<String>>,
        aud: &str,
        expiry_override: Option<u64>,
    ) -> Result<RiseClaims, JwtSignerError> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let exp = expiry_override.unwrap_or_else(|| now + self.default_expiry_seconds);

        // Extract required claims
        let sub = idp_claims
            .get("sub")
            .and_then(|v| v.as_str())
            .ok_or_else(|| JwtSignerError::MissingClaim("sub".to_string()))?
            .to_string();

        let email = idp_claims
            .get("email")
            .and_then(|v| v.as_str())
            .ok_or_else(|| JwtSignerError::MissingClaim("email".to_string()))?
            .to_string();

        // Extract optional name claim if requested
        let name = if self.claims_to_include.contains("name") {
            idp_claims
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        } else {
            None
        };

        Ok(RiseClaims {
            sub,
            email,
            name,
            groups,
            iat: now,
            exp,
            iss: self.issuer.clone(),
            aud: aud.to_string(),
        })
    }

    /// Sign a new Rise JWT for user authentication (HS256)
    ///
    /// This JWT is used for authenticating users to Rise (both UI and CLI).
    /// Uses HS256 symmetric encryption and sets aud to the Rise public URL.
    ///
    /// # Arguments
    /// * `idp_claims` - Claims from the IdP JWT (must contain at least "sub" and "email")
    /// * `groups` - The user's Rise team names, placed directly into the `groups` claim.
    ///   Callers resolve these (e.g. via the DB) and pass them in; the signer never
    ///   touches the database.
    /// * `rise_public_url` - The Rise public URL (used as aud claim)
    /// * `expiry_override` - Optional expiry timestamp (if None, uses default_expiry_seconds)
    pub fn sign_user_jwt(
        &self,
        idp_claims: &serde_json::Value,
        groups: Option<Vec<String>>,
        rise_public_url: &str,
        expiry_override: Option<u64>,
    ) -> Result<String, JwtSignerError> {
        let claims =
            self.build_rise_claims(idp_claims, groups, rise_public_url, expiry_override)?;

        let header = Header::new(Algorithm::HS256);
        let token = encode(&header, &claims, &self.hs256_encoding_key)?;

        Ok(token)
    }

    /// Sign a new Rise JWT for project ingress authentication (RS256)
    ///
    /// This JWT is used for authenticating to deployed Rise projects.
    /// Uses RS256 asymmetric encryption and sets aud to the project URL.
    ///
    /// # Arguments
    /// * `idp_claims` - Claims from the IdP JWT (must contain at least "sub" and "email")
    /// * `groups` - The user's Rise team names, placed directly into the `groups` claim.
    ///   Callers resolve these (e.g. via the DB) and pass them in; the signer never
    ///   touches the database.
    /// * `project_url` - The project URL (used as aud claim, e.g., "https://myapp.apps.rise.dev")
    /// * `expiry_override` - Optional expiry timestamp (if None, uses default_expiry_seconds)
    pub fn sign_ingress_jwt(
        &self,
        idp_claims: &serde_json::Value,
        groups: Option<Vec<String>>,
        project_url: &str,
        expiry_override: Option<u64>,
    ) -> Result<String, JwtSignerError> {
        let claims = self.build_rise_claims(idp_claims, groups, project_url, expiry_override)?;

        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(self.rs256_key_id.clone());
        let token = encode(&header, &claims, &self.rs256_encoding_key)?;

        Ok(token)
    }

    /// Sign a Rise workload identity JWT (RS256)
    ///
    /// Issued to a deployed app for federating identity to external systems.
    /// Uses the same RS256 key as ingress tokens, so the existing JWKS/discovery
    /// endpoints already cover verification.
    ///
    /// # Arguments
    /// * `info` - Subject and informational claims for the workload identity
    /// * `audience` - the `aud` claim, supplied per request
    /// * `ttl_secs` - token lifetime in seconds
    pub fn sign_workload_jwt(
        &self,
        info: &WorkloadSubjectInfo<'_>,
        audience: &str,
        ttl_secs: u64,
    ) -> Result<String, JwtSignerError> {
        use rand::Rng;

        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

        let mut jti_bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut jti_bytes);
        let jti = BASE64URL.encode(jti_bytes);

        let claims = WorkloadClaims {
            iss: self.issuer.clone(),
            sub: info.sub.to_string(),
            aud: audience.to_string(),
            iat: now,
            nbf: now,
            exp: now + ttl_secs,
            jti,
            project: info.project.to_string(),
            environment: info.environment.to_string(),
            deployment_group: info.deployment_group.to_string(),
            deployment_id: info.deployment_id.to_string(),
        };

        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(self.rs256_key_id.clone());
        let token = encode(&header, &claims, &self.rs256_encoding_key)?;

        Ok(token)
    }

    /// Verify and decode a Rise user JWT (HS256 only) with audience validation
    ///
    /// This is the legacy adapter preserving today's behavior: only accepts
    /// HS256 tokens (user JWTs) and validates that the audience matches the Rise
    /// public URL, rejecting RS256 ingress tokens and wrong-audience tokens.
    ///
    /// Delegates to [`Self::verify_rise_jwt`] (which disambiguates by algorithm)
    /// and then enforces the `Session` + `aud == expected_aud` posture.
    ///
    /// # Arguments
    /// * `token` - The JWT token string
    /// * `expected_aud` - Expected audience (Rise public URL)
    pub fn verify_user_jwt(
        &self,
        token: &str,
        expected_aud: &str,
    ) -> Result<RiseClaims, JwtSignerError> {
        match self.verify_rise_jwt(token)? {
            RiseToken::Session(claims) if claims.aud == expected_aud => Ok(claims),
            // A correctly-signed HS256 session token with the wrong audience:
            // labeled distinctly as an audience mismatch (still rejected).
            RiseToken::Session(_) => Err(JwtSignerError::AudienceMismatch),
            // An RS256 ingress token — genuinely the wrong token kind on the API
            // path, so reject it as an algorithm error, exactly as before.
            RiseToken::Ingress(_) => Err(JwtSignerError::SigningFailed(
                jsonwebtoken::errors::ErrorKind::InvalidAlgorithm.into(),
            )),
        }
    }

    /// Verify and decode a Rise JWT without audience validation
    ///
    /// Legacy adapter used by the `ingress_auth` handler where both HS256 (user
    /// session) and RS256 (app-scoped) tokens must be accepted. Project access is
    /// validated separately via database checks.
    ///
    /// Delegates to [`Self::verify_rise_jwt`] and accepts either variant.
    pub fn verify_jwt_skip_aud(&self, token: &str) -> Result<RiseClaims, JwtSignerError> {
        match self.verify_rise_jwt(token)? {
            RiseToken::Session(claims) | RiseToken::Ingress(claims) => Ok(claims),
        }
    }
}

// Tests live alongside the verifier in `verify.rs` and exercise both the
// adapters and the central `verify_rise_jwt` entry point.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use jsonwebtoken::{decode, Validation};

    pub(crate) fn create_test_signer() -> RiseTokenSigner {
        // Exactly 32 bytes encoded as base64
        let secret = BASE64.encode([0u8; 32]);
        RiseTokenSigner::new(
            &secret,
            "https://rise.test".to_string(),
            3600,
            vec!["sub".to_string(), "email".to_string(), "name".to_string()],
            None, // No pre-configured RS256 keys for tests
            None,
        )
        .unwrap()
    }

    #[test]
    fn test_create_signer() {
        let signer = create_test_signer();

        // Verify RS256 keys were generated
        assert!(!signer.rs256_public_key_pem.is_empty());
        assert!(!signer.rs256_key_id.is_empty());
        assert_eq!(signer.rs256_key_id.len(), 16); // Should be 16 hex chars
    }

    #[test]
    fn test_generate_jwks() {
        let signer = create_test_signer();

        let jwks = signer.generate_jwks().unwrap();

        // Verify JWKS structure
        assert!(jwks.get("keys").is_some());
        let keys = jwks.get("keys").unwrap().as_array().unwrap();
        assert_eq!(keys.len(), 1);

        let key = &keys[0];
        assert_eq!(key.get("kty").unwrap().as_str().unwrap(), "RSA");
        assert_eq!(key.get("use").unwrap().as_str().unwrap(), "sig");
        assert_eq!(key.get("alg").unwrap().as_str().unwrap(), "RS256");
        assert_eq!(
            key.get("kid").unwrap().as_str().unwrap(),
            &signer.rs256_key_id
        );
        assert!(key.get("n").is_some());
        assert!(key.get("e").is_some());
    }

    #[test]
    fn test_sign_workload_jwt() {
        let signer = create_test_signer();

        let token = signer
            .sign_workload_jwt(
                &WorkloadSubjectInfo {
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

        let header = jsonwebtoken::decode_header(&token).unwrap();
        assert_eq!(header.alg, Algorithm::RS256);
        assert_eq!(header.kid.as_deref(), Some(signer.rs256_key_id.as_str()));

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&["https://rise.test"]);
        validation.set_audience(&["sts.amazonaws.com"]);
        let data =
            decode::<WorkloadClaims>(&token, &signer.rs256_decoding_key, &validation).unwrap();

        assert_eq!(data.claims.iss, "https://rise.test");
        assert_eq!(data.claims.sub, "rise:proj:myapp:env:prod");
        assert_eq!(data.claims.aud, "sts.amazonaws.com");
        assert_eq!(data.claims.project, "myapp");
        assert_eq!(data.claims.environment, "prod");
        assert_eq!(data.claims.deployment_group, "default");
        assert_eq!(data.claims.deployment_id, "20260101-000000");
        assert_eq!(data.claims.exp, data.claims.iat + 900);
        assert!(!data.claims.jti.is_empty());
    }

    #[test]
    fn test_invalid_secret_length() {
        let short_secret = BASE64.encode(b"short"); // Less than 32 bytes

        let result = RiseTokenSigner::new(
            &short_secret,
            "https://rise.test".to_string(),
            3600,
            vec!["sub".to_string(), "email".to_string()],
            None,
            None,
        );

        assert!(result.is_err());
    }
}
