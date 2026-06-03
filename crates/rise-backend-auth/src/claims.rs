//! Claim types for Rise-issued and external JWTs.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Claims for Rise-issued JWTs (both UI and ingress authentication)
///
/// The `aud` claim determines the scope:
/// - For UI login: aud = Rise public URL (e.g., "https://rise.example.com")
/// - For project ingress: aud = project URL (e.g., "https://myapp.apps.rise.dev")
///
/// `Debug` is implemented manually to redact the `email` field (PII) so it never
/// leaks via `{:?}` (e.g. in logs or panic messages).
#[derive(Serialize, Deserialize, Clone)]
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

impl std::fmt::Debug for RiseClaims {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RiseClaims")
            .field("sub", &self.sub)
            .field("email", &"<redacted>")
            .field("name", &self.name)
            .field("groups", &self.groups)
            .field("iat", &self.iat)
            .field("exp", &self.exp)
            .field("iss", &self.iss)
            .field("aud", &self.aud)
            .finish()
    }
}

/// Coarse capability scopes embedded in a Rise access token.
///
/// Scopes let handlers gate on `has_scope(..)` with zero DB work; the source of
/// truth at exchange time remains the service-account row. The set is
/// intentionally coarse — per-SA configurable scopes are deferred (they need a
/// DB column + migration). Service-account exchanges currently receive the full
/// set (matching what an SA can do today).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Create / manage deployments for the bound project.
    Deploy,
    /// Obtain temporary registry-push credentials for the bound project.
    RegistryPush,
    /// Read the bound project's deployments / metadata.
    ReadProject,
}

/// The resolved principal embedded in a Rise [`AccessClaims`] token.
///
/// Internally tagged on `kind` (`user` / `service_account` / `controller`). The
/// `User` variant is **reserved**: the Phase-1 exchange pipeline mints only
/// `ServiceAccount` / `Controller`. It exists for a future unification of the
/// user OIDC login flow onto access tokens.
#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PrincipalClaims {
    /// A Rise user principal (reserved — not minted by the Phase-1 exchange).
    User {
        email: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        groups: Option<Vec<String>>,
    },
    /// A project-scoped service account, resolved at exchange time.
    ServiceAccount {
        /// The matched service account's id.
        service_account_id: Uuid,
        /// The SA's synthetic user id (used as `created_by` on deployments).
        synthetic_user_id: Uuid,
        /// The bound project's id — the token may act only within this project.
        project_id: Uuid,
        /// The bound project's name (informational / audit).
        project_name: String,
        /// Environment restriction snapshot. `None` = any environment.
        #[serde(skip_serializing_if = "Option::is_none")]
        allowed_environment_ids: Option<Vec<Uuid>>,
        /// Capabilities granted to this token.
        scopes: Vec<Scope>,
    },
    /// A trusted controller identity.
    Controller {
        /// The matched controller's stable identity id.
        identity_id: String,
    },
}

impl std::fmt::Debug for PrincipalClaims {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Redact `email` (PII), mirroring `RiseClaims`.
            PrincipalClaims::User { groups, .. } => f
                .debug_struct("User")
                .field("email", &"<redacted>")
                .field("groups", groups)
                .finish(),
            PrincipalClaims::ServiceAccount {
                service_account_id,
                synthetic_user_id,
                project_id,
                project_name,
                allowed_environment_ids,
                scopes,
            } => f
                .debug_struct("ServiceAccount")
                .field("service_account_id", service_account_id)
                .field("synthetic_user_id", synthetic_user_id)
                .field("project_id", project_id)
                .field("project_name", project_name)
                .field("allowed_environment_ids", allowed_environment_ids)
                .field("scopes", scopes)
                .finish(),
            PrincipalClaims::Controller { identity_id } => f
                .debug_struct("Controller")
                .field("identity_id", identity_id)
                .finish(),
        }
    }
}

/// Claims for a Rise-issued **access token** (HS256).
///
/// Minted by the token-exchange endpoint after resolving an external OIDC token
/// to a Rise principal. It is consumed **only** by Rise's own middleware (which
/// holds the HS256 secret), so it is never RS256 / JWKS-verifiable by third
/// parties. Kept structurally separate from [`RiseClaims`] so an SA-shaped token
/// can never be honored on the user/ingress paths, and vice-versa.
///
/// The carried `principal` fully encodes the resolved identity, letting handlers
/// make snap authorization decisions with no DB round-trips.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct AccessClaims {
    /// Issuer (Rise public URL) — satisfies the middleware's `is_rise_issued_jwt` branch.
    pub iss: String,
    /// Audience (Rise public URL) — verified by the middleware like a session token.
    pub aud: String,
    /// Stable principal id: `rise:sa:<sa_id>`, `rise:ctrl:<id>`, or a user uuid.
    pub sub: String,
    /// Issued at timestamp.
    pub iat: u64,
    /// Expiration timestamp.
    pub exp: u64,
    /// Audit id; room for a future revocation deny-list.
    pub jti: String,
    /// The resolved principal.
    pub principal: PrincipalClaims,
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
