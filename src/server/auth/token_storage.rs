use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use base64::Engine;
use rand::Rng;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

/// OAuth2 state data stored temporarily during the PKCE flow
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OAuth2State {
    /// Proof required on the central callback host.
    #[serde(default)]
    pub browser_proof: Option<super::browser_binding::BrowserProof>,
    /// Independent proof required on the app's completion host.
    #[serde(default)]
    pub app_browser_proof: Option<super::browser_binding::BrowserProof>,
    /// Project instance that owns this sign-in transaction.
    #[serde(default)]
    pub project_id: Option<uuid::Uuid>,
    pub code_verifier: String,
    pub redirect_url: Option<String>,
    /// Project name for ingress authentication flow
    pub project_name: Option<String>,
    /// Exact app origin that receives the browser-bound completion.
    pub app_base_url: Option<String>,
}

/// Completed auth session data for custom domain token exchange
/// After IdP callback on main domain, this data is stored temporarily
/// and retrieved by the custom domain to set cookies
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompletedAuthSession {
    #[serde(default)]
    pub browser_proof: Option<super::browser_binding::BrowserProof>,
    #[serde(default)]
    pub app_base_url: Option<String>,
    #[serde(default)]
    pub project_id: Option<uuid::Uuid>,
    /// The signed Rise JWT to set as cookie
    pub rise_jwt: String,
    /// Max age for the cookie in seconds
    pub max_age: u64,
    /// Final redirect URL after setting cookie
    pub redirect_url: String,
    /// Project name for display
    pub project_name: String,
}

/// Trait for storing and retrieving OAuth2 state tokens
#[async_trait]
pub trait TokenStore: Send + Sync {
    /// Save OAuth2 state with the given state token as the key
    async fn save(&self, state: String, data: OAuth2State) -> Result<()>;

    /// Claim OAuth2 state by state token.
    async fn claim(&self, state: &str) -> Result<Option<ClaimedToken<OAuth2State>>>;

    /// Save completed auth session for custom domain token exchange
    async fn save_completed_session(
        &self,
        token: String,
        session: CompletedAuthSession,
    ) -> Result<()>;

    /// Claim completed auth session by token.
    async fn claim_completed_session(
        &self,
        token: &str,
    ) -> Result<Option<ClaimedToken<CompletedAuthSession>>>;
}

pub type ClaimedToken<T> = crate::db::oauth_transient_state::ClaimedState<T>;

/// Database-backed implementation of TokenStore — safe to use in multi-replica deployments
pub struct DbTokenStore {
    pool: PgPool,
    pkce_ttl: Duration,
    session_ttl: Duration,
}

impl DbTokenStore {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            pkce_ttl: Duration::from_secs(600),
            session_ttl: Duration::from_secs(300),
        }
    }
}

#[async_trait]
impl TokenStore for DbTokenStore {
    async fn save(&self, state: String, data: OAuth2State) -> Result<()> {
        crate::db::oauth_transient_state::insert(&self.pool, &state, &data, self.pkce_ttl).await
    }

    async fn claim(&self, state: &str) -> Result<Option<ClaimedToken<OAuth2State>>> {
        crate::db::oauth_transient_state::claim(
            &self.pool,
            state,
            uuid::Uuid::new_v4(),
            crate::db::oauth_transient_state::CLAIM_GRACE_TTL,
        )
        .await
    }

    async fn save_completed_session(
        &self,
        token: String,
        session: CompletedAuthSession,
    ) -> Result<()> {
        crate::db::oauth_transient_state::insert(&self.pool, &token, &session, self.session_ttl)
            .await
    }

    async fn claim_completed_session(
        &self,
        token: &str,
    ) -> Result<Option<ClaimedToken<CompletedAuthSession>>> {
        crate::db::oauth_transient_state::claim(
            &self.pool,
            token,
            uuid::Uuid::new_v4(),
            crate::db::oauth_transient_state::CLAIM_GRACE_TTL,
        )
        .await
    }
}

/// Generate a cryptographically secure PKCE code verifier
///
/// The verifier is a random string of 43-128 characters using unreserved characters
/// defined in RFC 3986: [A-Z] / [a-z] / [0-9] / "-" / "." / "_" / "~"
///
/// This implementation generates a 64-character verifier (48 random bytes base64url encoded)
pub fn generate_code_verifier() -> String {
    let mut random_bytes = [0u8; 48];
    rand::rng().fill_bytes(&mut random_bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&random_bytes[..])
}

/// Generate a PKCE code challenge from a code verifier using S256 method
///
/// code_challenge = BASE64URL(SHA256(ASCII(code_verifier)))
pub fn generate_code_challenge(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let hash = hasher.finalize();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&hash[..])
}

/// Generate a cryptographically secure random state token
///
/// The state token is used to prevent CSRF attacks in the OAuth2 flow
pub fn generate_state_token() -> String {
    let mut random_bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut random_bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&random_bytes[..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_code_verifier_length() {
        let verifier = generate_code_verifier();
        // 48 bytes base64url encoded = 64 characters
        assert_eq!(verifier.len(), 64);
        // Verify it only contains valid base64url characters
        assert!(verifier
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn test_code_challenge_deterministic() {
        let verifier = "test_verifier_123";
        let challenge1 = generate_code_challenge(verifier);
        let challenge2 = generate_code_challenge(verifier);
        assert_eq!(challenge1, challenge2);
    }

    #[test]
    fn test_code_challenge_unique() {
        let verifier1 = "verifier1";
        let verifier2 = "verifier2";
        let challenge1 = generate_code_challenge(verifier1);
        let challenge2 = generate_code_challenge(verifier2);
        assert_ne!(challenge1, challenge2);
    }

    #[test]
    fn test_state_token_length() {
        let state = generate_state_token();
        // 32 bytes base64url encoded = 43 characters
        assert_eq!(state.len(), 43);
    }

    #[test]
    fn test_state_token_randomness() {
        let state1 = generate_state_token();
        let state2 = generate_state_token();
        assert_ne!(state1, state2);
    }
}
