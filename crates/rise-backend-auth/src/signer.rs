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
use uuid::Uuid;

use crate::claims::{
    AccessClaims, ActorClaim, IdentityClaims, PrincipalClaims, RiseClaims, WorkloadClaims,
    WorkloadSubjectInfo,
};
use crate::error::JwtSignerError;
use crate::verify::RiseToken;

/// JWT header `typ` for Rise access tokens.
///
/// This is the **primary discriminator** between a session token and an access
/// token (both HS256): `verify_rise_jwt` routes a token carrying this `typ` to
/// [`RiseToken::Access`], and the legacy `verify_user_jwt` / `verify_jwt_skip_aud`
/// adapters reject it. Session tokens carry the default `"JWT"` `typ`.
pub const RISE_ACCESS_TYP: &str = "rise-access+jwt";

/// JWT header `typ` for Rise identity tokens (ADR-0001 §7).
///
/// The discriminator that routes an HS256 token to [`RiseToken::Identity`].
/// Matched exclusively, like [`RISE_ACCESS_TYP`].
pub const RISE_IDENTITY_TYP: &str = "rise-identity+jwt";

/// JWT header `typ` for Rise User session tokens (ADR-0001 §7).
///
/// A session carrying it names a `User` resource: `sub` is its canonical
/// `user:<name>` and `rise_uid` its UID, and both must still identify one live,
/// active User on every request. Matched exclusively, like
/// [`RISE_ACCESS_TYP`]; an HS256 token without it is a legacy session, which
/// carries the IdP's `sub` and no `rise_uid`.
pub const RISE_SESSION_TYP: &str = "rise-session+jwt";

/// The Rise `User` resource a session token is issued for.
#[derive(Debug, Clone)]
pub struct SessionUser {
    /// Canonical `user:<name>` subject.
    pub subject: String,
    /// The `User` resource's UID.
    pub rise_uid: uuid::Uuid,
}

/// What an identity token is minted for.
///
/// The signer stamps `iss`, `iat`/`exp`, and a random `jti`; everything
/// identity-specific arrives here, already validated by the `/token` handler.
pub struct IdentityTokenSpec<'a> {
    /// Canonical subject of the target identity.
    pub subject: &'a str,
    /// The target resource's UID.
    pub rise_uid: Uuid,
    /// The `aud` claim: the Rise public URL for a token Rise's own API accepts,
    /// or an external verifier's audience.
    pub audience: &'a str,
    /// Token lifetime in seconds (the caller clamps to the platform maximum).
    pub ttl_secs: u64,
    /// Canonical `rise.dev/rbac` authorization details, or `None` for the
    /// target's full live policy.
    pub authorization_details: Option<Vec<serde_json::Value>>,
    /// The delegation chain for delegated issuance; `None` for a workload
    /// exchange.
    pub act: Option<ActorClaim>,
}

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
            rise_uid: None,
        })
    }

    /// Sign a Rise session token for a resolved `User` (HS256).
    ///
    /// This JWT authenticates users to Rise (UI and CLI). It carries the
    /// header `typ` [`RISE_SESSION_TYP`], `sub` = the User's canonical subject,
    /// `rise_uid` = its UID, and `aud` = the Rise public URL. `email` and
    /// `name` still come from the IdP claims: the typed APIs key on the email.
    ///
    /// # Arguments
    /// * `idp_claims` - Claims from the IdP JWT (must contain at least "sub" and "email")
    /// * `user` - The `User` resource the login resolved to
    /// * `groups` - The user's Rise team names, placed directly into the `groups` claim.
    ///   Callers resolve these (e.g. via the DB) and pass them in; the signer never
    ///   touches the database.
    /// * `rise_public_url` - The Rise public URL (used as aud claim)
    /// * `expiry_override` - Optional expiry timestamp (if None, uses default_expiry_seconds)
    pub fn sign_user_jwt(
        &self,
        idp_claims: &serde_json::Value,
        user: &SessionUser,
        groups: Option<Vec<String>>,
        rise_public_url: &str,
        expiry_override: Option<u64>,
    ) -> Result<String, JwtSignerError> {
        let mut claims =
            self.build_rise_claims(idp_claims, groups, rise_public_url, expiry_override)?;
        claims.sub = user.subject.clone();
        claims.rise_uid = Some(user.rise_uid);

        let mut header = Header::new(Algorithm::HS256);
        header.typ = Some(RISE_SESSION_TYP.to_string());
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

    /// Sign a Rise access token (HS256) for an exchanged principal.
    ///
    /// Minted by the token-exchange endpoint. The signer stamps `iss` (this
    /// signer's issuer), `aud`, `iat`/`exp`, and a random `jti`, and sets the
    /// header `typ` to [`RISE_ACCESS_TYP`] so `verify_rise_jwt` classifies it as
    /// [`RiseToken::Access`] (and the session/ingress adapters reject it).
    ///
    /// # Arguments
    /// * `sub` - stable principal id (`rise:sa:<id>`, `controller:<name>`, ...)
    /// * `principal` - the fully-resolved principal to embed
    /// * `aud` - the `aud` claim (the Rise public URL)
    /// * `ttl_secs` - token lifetime in seconds (caller clamps to the max TTL)
    pub fn sign_access_jwt(
        &self,
        sub: &str,
        principal: PrincipalClaims,
        aud: &str,
        ttl_secs: u64,
    ) -> Result<(String, AccessClaims), JwtSignerError> {
        use rand::Rng;

        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

        let mut jti_bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut jti_bytes);
        let jti = BASE64URL.encode(jti_bytes);

        let claims = AccessClaims {
            iss: self.issuer.clone(),
            aud: aud.to_string(),
            sub: sub.to_string(),
            iat: now,
            exp: now + ttl_secs,
            jti,
            principal,
        };

        let mut header = Header::new(Algorithm::HS256);
        header.typ = Some(RISE_ACCESS_TYP.to_string());
        let token = encode(&header, &claims, &self.hs256_encoding_key)?;

        Ok((token, claims))
    }

    /// Sign a Rise identity token (RS256) for a ServiceAccount or Controller
    /// resource principal (ADR-0001 §7).
    ///
    /// Always asymmetric, whatever the audience: a token for Rise's own API and
    /// one for an external verifier are the same kind, signed with the RS256
    /// key the JWKS publishes, so any audience can verify it and Rise has one
    /// verification path rather than two. The header carries the `kid` and the
    /// [`RISE_IDENTITY_TYP`] `typ` that routes `verify_rise_jwt` to
    /// [`RiseToken::Identity`]; every other adapter rejects it. A delegation
    /// chain longer than [`crate::MAX_DELEGATION_DEPTH`] is refused here as well
    /// as at the handler, so no signing path can produce a token the platform
    /// limit forbids.
    pub fn sign_identity_jwt(
        &self,
        spec: IdentityTokenSpec<'_>,
    ) -> Result<(String, IdentityClaims), JwtSignerError> {
        use rand::Rng;

        if spec.act.as_ref().map_or(0, ActorClaim::depth) > crate::MAX_DELEGATION_DEPTH {
            return Err(JwtSignerError::DelegationChainTooLong);
        }

        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

        let mut jti_bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut jti_bytes);
        let jti = BASE64URL.encode(jti_bytes);

        let claims = IdentityClaims {
            iss: self.issuer.clone(),
            aud: spec.audience.to_string(),
            sub: spec.subject.to_string(),
            rise_uid: spec.rise_uid,
            iat: now,
            exp: now + spec.ttl_secs,
            jti,
            authorization_details: spec.authorization_details,
            act: spec.act,
        };

        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(self.rs256_key_id.clone());
        header.typ = Some(RISE_IDENTITY_TYP.to_string());
        let token = encode(&header, &claims, &self.rs256_encoding_key)?;

        Ok((token, claims))
    }

    /// Verify and decode a Rise user JWT (HS256 only) with audience validation.
    ///
    /// Adapter over [`Self::verify_rise_jwt`] for the API authentication path:
    /// accepts only an HS256 `Session` token whose `aud` matches the Rise public
    /// URL, rejecting RS256 ingress tokens and wrong-audience tokens.
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
            // An RS256 ingress token, an exchanged access token, or a resource
            // identity token — genuinely the wrong token kind on the user-login
            // path, so reject as an alg error.
            RiseToken::Ingress(_) | RiseToken::Access(_) | RiseToken::Identity(_) => {
                Err(JwtSignerError::SigningFailed(
                    jsonwebtoken::errors::ErrorKind::InvalidAlgorithm.into(),
                ))
            }
        }
    }

    /// Verify and decode a Rise JWT without audience validation.
    ///
    /// Adapter over [`Self::verify_rise_jwt`] for the `ingress_auth` handler, which
    /// accepts both HS256 (user session) and RS256 (app-scoped ingress) tokens and
    /// does not check `aud`. Project access is validated separately via database
    /// checks.
    pub fn verify_jwt_skip_aud(&self, token: &str) -> Result<RiseClaims, JwtSignerError> {
        let claims = match self.verify_rise_jwt(token)? {
            RiseToken::Session(claims) | RiseToken::Ingress(claims) => claims,
            // An exchanged access token or a resource identity token must never
            // be honored on the ingress path.
            RiseToken::Access(_) | RiseToken::Identity(_) => {
                return Err(JwtSignerError::SigningFailed(
                    jsonwebtoken::errors::ErrorKind::InvalidAlgorithm.into(),
                ))
            }
        };

        // Defense-in-depth (§4.1 ingress hardening): the `typ` discriminator above
        // already routes a Rise access or identity token to its own variant, but
        // because `RiseClaims` intentionally does NOT use `deny_unknown_fields`,
        // a token *without* that `typ` that nonetheless carries a `principal`
        // claim would deserialize cleanly as a session/ingress token (extra
        // field ignored). Reject any such token outright so a principal-shaped
        // payload can never be accepted on the ingress path. `rise_uid` needs
        // no peek: `RiseClaims` reads it, and the verifier admits it only on a
        // session token wearing `RISE_SESSION_TYP`. The payload was just
        // signature-verified, so this peek reads authenticated bytes.
        if rise_jwt_payload_has_principal(token) {
            return Err(JwtSignerError::SigningFailed(
                jsonwebtoken::errors::ErrorKind::InvalidAlgorithm.into(),
            ));
        }

        Ok(claims)
    }
}

/// Whether a (already signature-verified) JWT's payload carries a top-level
/// `principal` claim — the field that marks an access token. Used to
/// fail-close the ingress path against principal-shaped tokens (§4.1). Returns `false` if the payload cannot be
/// parsed (the caller has already verified the signature, so this only guards
/// the claim shape).
fn rise_jwt_payload_has_principal(token: &str) -> bool {
    let Some(payload_b64) = token.split('.').nth(1) else {
        return false;
    };
    let Ok(payload) = BASE64URL.decode(payload_b64) else {
        return false;
    };
    serde_json::from_slice::<serde_json::Value>(&payload)
        .ok()
        .and_then(|v| v.as_object().map(|obj| obj.contains_key("principal")))
        .unwrap_or(false)
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

    /// A chain built to an exact depth, outermost delegator first.
    fn chain(depth: usize) -> ActorClaim {
        let mut act: Option<Box<ActorClaim>> = None;
        for level in 0..depth {
            act = Some(Box::new(ActorClaim {
                sub: format!("controller:c{level}"),
                rise_uid: Uuid::new_v4(),
                act,
            }));
        }
        *act.expect("depth is at least 1")
    }

    /// ADR-0001 scenario 48
    #[test]
    fn sign_identity_jwt_refuses_a_chain_past_the_depth_limit() {
        let signer = create_test_signer();

        let at_limit = chain(crate::MAX_DELEGATION_DEPTH);
        assert_eq!(at_limit.depth(), crate::MAX_DELEGATION_DEPTH);
        let (token, _) = signer
            .sign_identity_jwt(IdentityTokenSpec {
                subject: "controller:k8s",
                rise_uid: Uuid::new_v4(),
                audience: "https://rise.test",
                ttl_secs: 600,
                authorization_details: None,
                act: Some(at_limit),
            })
            .expect("a chain at the platform limit is accepted");
        match signer.verify_rise_jwt(&token).unwrap() {
            RiseToken::Identity(claims) => assert_eq!(
                claims.delegation_depth(),
                crate::MAX_DELEGATION_DEPTH,
                "the verified token reports the same depth it was signed with"
            ),
            other => panic!("expected Identity, got {other:?}"),
        }

        let too_long = chain(crate::MAX_DELEGATION_DEPTH + 1);
        let err = signer
            .sign_identity_jwt(IdentityTokenSpec {
                subject: "controller:k8s",
                rise_uid: Uuid::new_v4(),
                audience: "https://rise.test",
                ttl_secs: 600,
                authorization_details: None,
                act: Some(too_long),
            })
            .unwrap_err();
        assert!(matches!(err, JwtSignerError::DelegationChainTooLong));
    }
}
