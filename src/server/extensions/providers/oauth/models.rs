use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Extension spec - user-provided OAuth configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthExtensionSpec {
    /// Display name (e.g., "Snowflake Production", "Google OAuth")
    pub provider_name: String,
    /// Description of this OAuth configuration
    #[serde(default)]
    pub description: String,
    /// OAuth client ID (for upstream provider)
    pub client_id: String,
    /// Context-bound upstream client credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_encrypted: Option<String>,
    /// OIDC issuer URL (required) - used for endpoint discovery via .well-known/openid-configuration
    /// For OIDC-compliant providers (Google, Snowflake, Dex), endpoints are fetched automatically
    /// For non-OIDC providers (GitHub), set authorization_endpoint and token_endpoint manually
    pub issuer_url: String,
    /// OAuth provider authorization URL (optional override)
    /// If not provided, fetched from issuer_url's OIDC discovery document
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_endpoint: Option<String>,
    /// OAuth provider token URL (optional override)
    /// If not provided, fetched from issuer_url's OIDC discovery document
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    /// How Rise authenticates to the upstream token endpoint
    #[serde(default, skip_serializing_if = "TokenEndpointAuthMethod::is_default")]
    pub token_endpoint_auth_method: TokenEndpointAuthMethod,
    /// OAuth scopes to request. Empty for providers without scopes (e.g. Notion),
    /// in which case the authorization request carries no `scope` parameter.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// How the refresh tokens handed to the app behave.
    #[serde(default, skip_serializing_if = "RefreshTokenSettings::is_default")]
    pub refresh_token: RefreshTokenSettings,
}

/// Handling of the refresh tokens an app receives from the token endpoint.
///
/// The upstream provider's refresh token never reaches the app in either mode;
/// the app gets a Rise-issued handle for it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshTokenSettings {
    #[serde(default)]
    pub mode: RefreshTokenMode,
    /// `rotating` mode only: how long after a refresh the same handle still
    /// returns the same successor (multiple tabs, retried requests). A handle
    /// presented later is treated as stolen and ends the whole login.
    /// 1 second to 5 minutes, 30 seconds by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace_period_seconds: Option<u64>,
    /// `rotating` mode only: absolute lifetime of a login, counted from the
    /// sign-in, after which the user has to sign in again. From the grace
    /// period to 10 years, 30 days by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_lifetime_seconds: Option<u64>,
}

/// Which kind of refresh-token handle the extension issues.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshTokenMode {
    /// Stateless `rrt1.` handles: the upstream token sealed under the extension's key.
    #[default]
    Sealed,
    /// Stateful, single-use `rrt2.` handles with reuse detection.
    Rotating,
}

impl RefreshTokenSettings {
    pub const DEFAULT_GRACE_PERIOD_SECONDS: u64 = 30;
    pub const MIN_GRACE_PERIOD_SECONDS: u64 = 1;
    pub const MAX_GRACE_PERIOD_SECONDS: u64 = 5 * 60;
    pub const DEFAULT_MAX_LIFETIME_SECONDS: u64 = 30 * 24 * 60 * 60;
    pub const MAX_MAX_LIFETIME_SECONDS: u64 = 10 * 365 * 24 * 60 * 60;

    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn grace_period(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.grace_period_seconds
                .unwrap_or(Self::DEFAULT_GRACE_PERIOD_SECONDS),
        )
    }

    pub fn max_lifetime(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.max_lifetime_seconds
                .unwrap_or(Self::DEFAULT_MAX_LIFETIME_SECONDS),
        )
    }

    /// Check the settings: the numeric ones belong to `rotating` mode and are
    /// bounds-checked there.
    pub fn validate(&self) -> Result<(), String> {
        if self.mode != RefreshTokenMode::Rotating {
            if self.grace_period_seconds.is_some() || self.max_lifetime_seconds.is_some() {
                return Err(
                    "refresh_token.grace_period_seconds and refresh_token.max_lifetime_seconds \
                     require refresh_token.mode \"rotating\""
                        .into(),
                );
            }
            return Ok(());
        }
        let grace = self.grace_period().as_secs();
        if !(Self::MIN_GRACE_PERIOD_SECONDS..=Self::MAX_GRACE_PERIOD_SECONDS).contains(&grace) {
            return Err(format!(
                "refresh_token.grace_period_seconds must be between {} and {}",
                Self::MIN_GRACE_PERIOD_SECONDS,
                Self::MAX_GRACE_PERIOD_SECONDS
            ));
        }
        let lifetime = self.max_lifetime().as_secs();
        if !(grace..=Self::MAX_MAX_LIFETIME_SECONDS).contains(&lifetime) {
            return Err(format!(
                "refresh_token.max_lifetime_seconds must be between the grace period ({grace}) and {}",
                Self::MAX_MAX_LIFETIME_SECONDS
            ));
        }
        Ok(())
    }
}

/// Client authentication at the upstream token endpoint (RFC 6749 §2.3.1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenEndpointAuthMethod {
    /// `client_id` and `client_secret` in the form body.
    #[default]
    ClientSecretPost,
    /// `Authorization: Basic` header built from the form-urlencoded
    /// `client_id` and `client_secret`; neither appears in the body.
    ClientSecretBasic,
}

impl TokenEndpointAuthMethod {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Extension status - system-computed metadata
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OAuthExtensionStatus {
    /// Computed redirect URI: {RISE_PUBLIC_URL}/oidc/{project}/{extension}/callback
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
    /// When the extension was configured
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configured_at: Option<DateTime<Utc>>,
    /// Whether OAuth flow has been successfully tested
    #[serde(default)]
    pub auth_verified: bool,
    /// Configuration or validation errors
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Rise client ID (for apps to authenticate to Rise's /token endpoint)
    /// Format: {project}-{extension}
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rise_client_id: Option<String>,
    /// Rise client secret (plaintext, not highly sensitive)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rise_client_secret: Option<String>,
}

/// OAuth state stored temporarily during authorization flow
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthState {
    /// Final redirect destination after OAuth completes (localhost or project URL)
    pub redirect_uri: Option<String>,
    /// Application's CSRF state parameter (passed through to final redirect)
    pub application_state: Option<String>,
    /// Project name
    pub project_name: String,
    /// Extension name (e.g., "oauth-snowflake")
    pub extension_name: String,
    /// PKCE code verifier (for upstream OAuth provider)
    pub code_verifier: String,
    /// When this state was created
    pub created_at: DateTime<Utc>,
    /// Client's PKCE code challenge (for Rise's token endpoint validation)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_code_challenge: Option<String>,
    /// Client's PKCE code challenge method
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_code_challenge_method: Option<String>,
}

/// Token response from OAuth provider
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
}

/// OAuth callback request from provider
#[derive(Debug, Deserialize)]
pub struct CallbackRequest {
    /// Authorization code from provider
    pub code: String,
    /// State token for CSRF protection
    pub state: String,
}

/// Authorization code state for OAuth 2.0 flow
/// Stores encrypted raw token response from upstream provider for single-use exchange
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCodeState {
    /// Project ID
    pub project_id: Uuid,
    /// Extension name
    pub extension_name: String,
    /// When this authorization code was created
    pub created_at: DateTime<Utc>,
    /// Redirect URI from authorization request (for validation during token exchange per RFC 6749)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
    /// PKCE code challenge from client (if PKCE flow)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_challenge: Option<String>,
    /// PKCE code challenge method (only "S256" is supported)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_challenge_method: Option<String>,
    /// Raw encrypted token response body from upstream provider (cached as-is, no parsing)
    pub token_response_encrypted: String,
    /// Content-Type header from upstream provider (preserved for accurate passthrough)
    pub content_type: String,
    /// HTTP status code from upstream provider (preserved for accurate passthrough)
    pub status_code: u16,
}

/// OAuth authorization request parameters
#[derive(Debug, Deserialize)]
pub struct AuthorizeFlowQuery {
    /// Where to redirect after OAuth completes (optional, for local dev)
    pub redirect_uri: Option<String>,
    /// Application's CSRF state parameter (passed through)
    pub state: Option<String>,
    /// PKCE code challenge (for public clients/SPAs)
    pub code_challenge: Option<String>,
    /// PKCE code challenge method (only "S256" is supported, defaults to "S256")
    pub code_challenge_method: Option<String>,
}

/// Token request (RFC 6749-compliant)
#[derive(Debug, Deserialize)]
pub struct TokenRequest {
    /// Grant type: "authorization_code" or "refresh_token"
    pub grant_type: String,
    /// Authorization code (for authorization_code grant)
    pub code: Option<String>,
    /// Refresh token (for refresh_token grant)
    pub refresh_token: Option<String>,
    /// Redirect URI (must match authorization request per RFC 6749 Section 4.1.3)
    pub redirect_uri: Option<String>,
    /// Rise client ID (required)
    pub client_id: String,
    /// Rise client secret (for confidential clients)
    pub client_secret: Option<String>,
    /// PKCE code verifier (for public clients)
    pub code_verifier: Option<String>,
}

/// OAuth2 token response (RFC 6749-compliant)
#[derive(Debug, Serialize)]
pub struct OAuth2TokenResponse {
    /// Access token
    pub access_token: String,
    /// Token type (always "Bearer")
    pub token_type: String,
    /// Expires in seconds from now (not timestamp)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<i64>,
    /// Refresh token (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Scope (space-delimited from spec.scopes)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// ID token (optional, OIDC)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
}

/// OAuth2 error response (RFC 6749-compliant)
#[derive(Debug, Serialize)]
pub struct OAuth2ErrorResponse {
    /// Error code
    pub error: String,
    /// Error description (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_description: Option<String>,
}
