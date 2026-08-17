//! Error types for the auth core.
//!
//! `AuthError` covers the verify / match / validate paths; `JwtSignerError`
//! (moved verbatim from `jwt_signer.rs`) covers signing and key setup.

/// Errors from the verify / match / validate paths.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The JWT could not be decoded or failed signature/issuer/expiry validation.
    #[error("JWT validation failed: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    /// The JWT header was missing the `kid` field.
    #[error("JWT header missing kid")]
    MissingKid,
    /// The `kid` in the JWT header was not present in the issuer's JWKS.
    #[error("Key {kid} not found in JWKS for issuer {issuer}")]
    KeyNotFound { kid: String, issuer: String },
    /// JWKS could not be fetched / resolved for the issuer.
    #[error("Failed to obtain JWKS for issuer {issuer}: {detail}")]
    Jwks { issuer: String, detail: String },
    /// A custom claim constraint was not satisfied.
    #[error("{0}")]
    ClaimMismatch(String),
    /// A configuration value (e.g. controller id) was invalid.
    #[error("{0}")]
    InvalidConfig(String),
}

/// Errors from JWT signing and key setup. Moved verbatim from `jwt_signer.rs`.
#[derive(Debug, thiserror::Error)]
pub enum JwtSignerError {
    #[error("Invalid base64 secret: {0}")]
    InvalidBase64(#[from] base64::DecodeError),
    #[error("JWT signing failed: {0}")]
    SigningFailed(#[from] jsonwebtoken::errors::Error),
    #[error("JWT audience does not match expected audience")]
    AudienceMismatch,
    #[error("System time error: {0}")]
    SystemTimeError(#[from] std::time::SystemTimeError),
    #[error("Missing required claim: {0}")]
    MissingClaim(String),
    #[error("RSA key generation failed: {0}")]
    RsaKeyError(String),
    #[error("PEM encoding failed: {0}")]
    PemError(String),
    /// Signing a workload token for a specific audience failed. Wraps the
    /// underlying error so a partial minting failure (one of several
    /// `[identity].audiences`) names the audience that failed.
    #[error("failed to sign workload token for audience {audience}: {source}")]
    WorkloadAudience {
        audience: String,
        #[source]
        source: Box<JwtSignerError>,
    },
}

/// Bridge for the legacy verify adapters (`verify_user_jwt` /
/// `verify_jwt_skip_aud`) which delegate to `verify_rise_jwt` (yielding
/// [`AuthError`]) but historically return [`JwtSignerError`]. Behavior is
/// preserved: a JWT decode failure maps back to `SigningFailed`, and the other
/// verify-path variants collapse to an `InvalidAlgorithm` signing error (the
/// same error the legacy code returned when rejecting a token).
impl From<AuthError> for JwtSignerError {
    fn from(err: AuthError) -> Self {
        match err {
            AuthError::Jwt(e) => JwtSignerError::SigningFailed(e),
            // SAFETY: this catch-all is only correct because the verify adapters
            // (`verify_user_jwt` / `verify_jwt_skip_aud`) only ever propagate the
            // error of `verify_rise_jwt`, whose return set is `{AuthError::Jwt}`.
            // The remaining variants (MissingKid / KeyNotFound / Jwks /
            // ClaimMismatch / InvalidConfig) belong to the external-JWT and
            // controller-matching paths, which do NOT convert into
            // `JwtSignerError`, so they are unreachable here today. If a future
            // change lets any of them reach an adapter, add an explicit arm
            // mapping it to a precise `JwtSignerError` variant rather than
            // silently collapsing it into `InvalidAlgorithm`.
            _ => JwtSignerError::SigningFailed(
                jsonwebtoken::errors::ErrorKind::InvalidAlgorithm.into(),
            ),
        }
    }
}
