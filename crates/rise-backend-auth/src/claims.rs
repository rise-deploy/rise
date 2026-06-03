//! Claim types for Rise-issued and external JWTs.

use serde::{Deserialize, Serialize};

/// Claims for Rise-issued JWTs (both UI and ingress authentication)
///
/// The `aud` claim determines the scope:
/// - For UI login: aud = Rise public URL (e.g., "https://rise.example.com")
/// - For project ingress: aud = project URL (e.g., "https://myapp.apps.rise.dev")
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RiseClaims {
    /// User ID from IdP
    pub sub: String,
    /// User email
    pub email: String,
    /// User name (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Rise team names the user is a member of (ALL teams, not just IdP-managed)
    /// Used for authorization and audit logging
    #[serde(skip_serializing_if = "Option::is_none")]
    pub groups: Option<Vec<String>>,
    /// Issued at timestamp
    pub iat: u64,
    /// Expiration timestamp
    pub exp: u64,
    /// Issuer (Rise backend URL)
    pub iss: String,
    /// Audience (Rise UI URL or project URL)
    pub aud: String,
}

/// Subject info for workload identity JWT claims.
pub struct WorkloadSubjectInfo<'a> {
    pub sub: &'a str,
    pub project: &'a str,
    pub environment: &'a str,
    pub deployment_group: &'a str,
    pub deployment_id: &'a str,
}

/// Claims for Rise-issued workload identity JWTs (RS256).
///
/// Issued to deployed apps so they can federate identity to external systems
/// (AWS STS, GCP WIF, Vault, ...). The subject describes the *Rise* identity —
/// `rise:proj:<project>:env:<environment>` — independent of the runtime.
/// These are distinct from [`RiseClaims`], which is user-shaped and requires
/// an `email` claim; workload tokens must never be accepted by Rise's own auth.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WorkloadClaims {
    /// Issuer (Rise backend URL)
    pub iss: String,
    /// Subject: `rise:proj:<project>:env:<environment>`
    pub sub: String,
    /// Audience supplied by the caller
    pub aud: String,
    /// Issued at timestamp
    pub iat: u64,
    /// Not before timestamp
    pub nbf: u64,
    /// Expiration timestamp
    pub exp: u64,
    /// Unique token ID
    pub jti: String,
    /// Rise project name (informational)
    pub project: String,
    /// Rise environment name (informational; `<null>` if the deployment has none)
    pub environment: String,
    /// Deployment group (informational)
    pub deployment_group: String,
    /// Rise deployment ID (informational)
    pub deployment_id: String,
}

/// An arbitrary external JWT that has been signature- and expiry-validated via
/// JWKS. Opaque proof that verification happened: the only constructor is
/// [`verify_external_jwt`](crate::verify_external_jwt), so a caller cannot
/// fabricate a "verified" value or hand-roll a second validation path.
#[derive(Debug, Clone)]
pub struct ExternalClaims {
    issuer: String,
    claims: serde_json::Value,
}

impl ExternalClaims {
    /// Construct verified external claims. Only callable from within the crate,
    /// by `verify_external_jwt`, after signature + expiry validation.
    pub(crate) fn new(issuer: String, claims: serde_json::Value) -> Self {
        Self { issuer, claims }
    }

    /// The issuer URL the token was validated against.
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// The full validated claim set.
    pub fn claims(&self) -> &serde_json::Value {
        &self.claims
    }
}
