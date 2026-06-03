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
    /// The token's `exp` claim is in the past.
    #[error("Token has expired")]
    Expired,
    /// JWKS could not be fetched / resolved for the issuer.
    #[error("Failed to obtain JWKS for issuer {issuer}: {detail}")]
    Jwks { issuer: String, detail: String },
    /// A custom claim constraint was not satisfied.
    #[error("{0}")]
    ClaimMismatch(String),
    /// A configuration value (e.g. controller id) was invalid.
    #[error("{0}")]
    InvalidConfig(String),
    /// System time could not be read.
    #[error("System time error: {0}")]
    SystemTime(#[from] std::time::SystemTimeError),
}

/// Errors from JWT signing and key setup. Moved verbatim from `jwt_signer.rs`.
#[derive(Debug, thiserror::Error)]
pub enum JwtSignerError {
    #[error("Invalid base64 secret: {0}")]
    InvalidBase64(#[from] base64::DecodeError),
    #[error("JWT signing failed: {0}")]
    SigningFailed(#[from] jsonwebtoken::errors::Error),
    #[error("System time error: {0}")]
    SystemTimeError(#[from] std::time::SystemTimeError),
    #[error("Missing required claim: {0}")]
    MissingClaim(String),
    #[error("RSA key generation failed: {0}")]
    RsaKeyError(String),
    #[error("PEM encoding failed: {0}")]
    PemError(String),
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
            AuthError::SystemTime(e) => JwtSignerError::SystemTimeError(e),
            _ => JwtSignerError::SigningFailed(
                jsonwebtoken::errors::ErrorKind::InvalidAlgorithm.into(),
            ),
        }
    }
}
