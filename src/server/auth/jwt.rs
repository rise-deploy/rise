use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use jsonwebtoken::DecodingKey;
use rise_backend_auth::{AuthError, JwksKeySource};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// JWT claims from OIDC provider ID token
/// Note: Unknown fields (like email_verified) are ignored by default
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Claims {
    pub sub: String,   // Subject (user ID from OIDC provider)
    pub email: String, // User email
    pub iss: String,   // Issuer (OIDC provider URL)
    pub aud: String,   // Audience (client ID) - validated to match configured client_id
    pub exp: usize,    // Expiration time
    pub iat: usize,    // Issued at
    #[serde(default)]
    pub name: Option<String>, // User's full name
    /// Groups claim from IdP (optional array of group names)
    /// Used for automatic team synchronization
    #[serde(default)]
    pub groups: Option<Vec<String>>,
}

impl Claims {
    /// Deserialize validated ID-token claims while mapping the configured IdP
    /// group claim onto Rise's internal `groups` field.
    pub fn from_value_with_group_claim(
        mut value: serde_json::Value,
        group_claim: &str,
    ) -> serde_json::Result<Self> {
        if group_claim != "groups" {
            if let Some(claims) = value.as_object_mut() {
                let configured_groups = claims.remove(group_claim);
                // Selecting a custom claim must not silently fall back to the
                // provider's conventional `groups` claim.
                claims.remove("groups");
                if let Some(groups) = configured_groups {
                    claims.insert("groups".to_string(), groups);
                }
            }
        }

        serde_json::from_value(value)
    }
}

/// JWKS (JSON Web Key Set) response from OIDC provider
#[derive(Debug, Deserialize)]
struct JwksResponse {
    keys: Vec<Jwk>,
}

/// Individual JSON Web Key
#[derive(Debug, Deserialize, Clone)]
struct Jwk {
    #[serde(rename = "use")]
    key_use: Option<String>, // Optional: some providers (like Entra ID) don't include this
    kty: String,
    kid: String,
    #[allow(dead_code)]
    alg: Option<String>, // Optional in some JWKS responses
    n: String,
    e: String,
}

/// OIDC Discovery document
#[derive(Debug, Deserialize)]
struct OidcDiscovery {
    issuer: String,
    jwks_uri: String,
}

/// JWKS cache entry with TTL
#[derive(Clone)]
struct JwksCache {
    keys: HashMap<String, DecodingKey>,
    fetched_at: Instant,
    ttl: Duration,
}

impl JwksCache {
    fn new(keys: HashMap<String, DecodingKey>) -> Self {
        Self {
            keys,
            fetched_at: Instant::now(),
            ttl: Duration::from_secs(300), // 5 minute TTL — ensures key rotation propagates quickly
        }
    }

    fn is_expired(&self) -> bool {
        self.fetched_at.elapsed() > self.ttl
    }
}

/// Unified JWT validator supporting multiple OIDC issuers with caching.
///
/// This is rise-deploy's [`JwksKeySource`] implementation: it owns the reqwest
/// client, SSRF policy, and JWKS cache. The actual signature/expiry verification
/// of external tokens lives in `rise_backend_auth::verify_external_jwt`, which
/// this type backs.
pub struct JwtValidator {
    jwks_cache: Arc<RwLock<HashMap<String, JwksCache>>>,
    http_client: reqwest::Client,
    ssrf_config: crate::server::ssrf::SsrfConfig,
}

impl JwtValidator {
    /// Create a new JWT validator
    pub fn new(ssrf_config: crate::server::ssrf::SsrfConfig) -> Self {
        let http_client = crate::server::ssrf::safe_client(&ssrf_config);
        Self {
            jwks_cache: Arc::new(RwLock::new(HashMap::new())),
            http_client,
            ssrf_config,
        }
    }

    /// Discover JWKS URI from OIDC issuer via .well-known/openid-configuration
    async fn discover_jwks_uri(&self, issuer_url: &str) -> Result<String> {
        let discovery_url = format!(
            "{}/.well-known/openid-configuration",
            issuer_url.trim_end_matches('/')
        );

        // SSRF-validate the discovery URL before fetching
        crate::server::ssrf::validate_url(&discovery_url, &self.ssrf_config)
            .await
            .map_err(|e| anyhow!("OIDC discovery URL failed SSRF validation: {}", e))?;

        tracing::debug!("Discovering OIDC configuration from {}", discovery_url);

        let response = self
            .http_client
            .get(&discovery_url)
            .send()
            .await
            .context("Failed to fetch OIDC discovery document")?;

        let discovery: OidcDiscovery = response
            .json()
            .await
            .context("Failed to parse OIDC discovery document")?;

        // SSRF-validate the JWKS URI before returning it.
        // An attacker-controlled OIDC provider could return a jwks_uri pointing
        // to an internal IP (e.g., metadata endpoint, internal service).
        crate::server::ssrf::validate_url(&discovery.jwks_uri, &self.ssrf_config)
            .await
            .map_err(|e| anyhow!("JWKS URI failed SSRF validation: {}", e))?;

        // Per RFC 8414 §3.1, a discovery document must not claim a different issuer.
        rise_backend_auth::validate_oidc_issuer(issuer_url, &discovery.issuer)
            .map_err(|e| anyhow!("{}", e))?;

        Ok(discovery.jwks_uri)
    }

    /// Fetch JWKS from a JWKS URI
    async fn fetch_jwks(&self, jwks_uri: &str) -> Result<HashMap<String, DecodingKey>> {
        tracing::debug!("Fetching JWKS from {}", jwks_uri);

        let response = self
            .http_client
            .get(jwks_uri)
            .send()
            .await
            .context("Failed to fetch JWKS")?;

        // Get response text for better error logging
        let response_text = response
            .text()
            .await
            .context("Failed to read JWKS response body")?;

        tracing::debug!("JWKS response: {}", response_text);

        let jwks: JwksResponse = serde_json::from_str(&response_text)
            .map_err(|e| anyhow!("Failed to parse JWKS response: {}", e))?;

        let mut keys = HashMap::new();

        for jwk in jwks.keys {
            // Accept RSA keys that either don't have a use field or have use="sig"
            if jwk.kty == "RSA" && (jwk.key_use.is_none() || jwk.key_use.as_deref() == Some("sig"))
            {
                let decoding_key = DecodingKey::from_rsa_components(&jwk.n, &jwk.e)
                    .context("Failed to create decoding key from JWK")?;
                keys.insert(jwk.kid.clone(), decoding_key);
                tracing::debug!(
                    "Loaded JWK with kid: {}, use: {:?}, alg: {:?}",
                    jwk.kid,
                    jwk.key_use,
                    jwk.alg
                );
            }
        }

        tracing::info!("Loaded {} signing keys from JWKS", keys.len());
        Ok(keys)
    }

    /// Get JWKS for an issuer (with caching)
    async fn get_jwks(&self, issuer_url: &str) -> Result<HashMap<String, DecodingKey>> {
        // Check if cache exists and is still valid
        {
            let cache = self.jwks_cache.read().await;
            if let Some(cached) = cache.get(issuer_url) {
                if !cached.is_expired() {
                    tracing::debug!("Using cached JWKS for {}", issuer_url);
                    return Ok(cached.keys.clone());
                } else {
                    tracing::debug!("JWKS cache expired for {}", issuer_url);
                }
            }
        }

        // Cache miss or expired - fetch JWKS
        tracing::info!("Fetching fresh JWKS for {}", issuer_url);

        // Discover JWKS URI
        let jwks_uri = self.discover_jwks_uri(issuer_url).await?;

        // Fetch JWKS
        let keys = self.fetch_jwks(&jwks_uri).await?;

        // Update cache
        {
            let mut cache = self.jwks_cache.write().await;
            cache.insert(issuer_url.to_string(), JwksCache::new(keys.clone()));
        }

        Ok(keys)
    }

    /// Validate a JWT token against an issuer with expected claims.
    ///
    /// Verifies the signature/expiry via [`rise_backend_auth::verify_external_jwt`]
    /// (this type is the [`JwksKeySource`]), then matches claims via
    /// [`rise_backend_auth::validate_custom_claims`].
    ///
    /// # Returns
    /// The full JWT claims as a `serde_json::Value` on success
    pub async fn validate(
        &self,
        token: &str,
        issuer_url: &str,
        expected_claims: &HashMap<String, String>,
    ) -> Result<serde_json::Value> {
        let claims = rise_backend_auth::verify_external_jwt(token, issuer_url, self)
            .await
            .map_err(|e| anyhow!("{}", e))?
            .claims()
            .clone();
        rise_backend_auth::validate_custom_claims(&claims, expected_claims)
            .map_err(|e| anyhow!("{}", e))?;
        Ok(claims)
    }
}

#[async_trait]
impl JwksKeySource for JwtValidator {
    async fn decoding_keys(
        &self,
        issuer: &str,
    ) -> std::result::Result<HashMap<String, DecodingKey>, AuthError> {
        self.get_jwks(issuer).await.map_err(|e| AuthError::Jwks {
            issuer: issuer.to_string(),
            detail: format!("{:#}", e),
        })
    }
}

impl Default for JwtValidator {
    fn default() -> Self {
        Self::new(crate::server::ssrf::SsrfConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jwt_validator_creation() {
        let validator = JwtValidator::default();
        // Validator should be created with empty cache
        assert!(validator.jwks_cache.try_read().is_ok());
    }

    #[test]
    fn test_claims_deserialization() {
        let json = r#"{
            "sub": "user123",
            "email": "test@example.com",
            "iss": "https://issuer.example.com",
            "aud": "my-client-id",
            "exp": 1234567890,
            "iat": 1234567800
        }"#;

        let claims: Claims = serde_json::from_str(json).unwrap();
        assert_eq!(claims.sub, "user123");
        assert_eq!(claims.email, "test@example.com");
        assert_eq!(claims.iss, "https://issuer.example.com");
        assert_eq!(claims.aud, "my-client-id");
    }

    #[test]
    fn test_claims_deserialization_with_unknown_fields() {
        // Test that unknown fields like email_verified are ignored
        let json = r#"{
            "sub": "user123",
            "email": "test@example.com",
            "email_verified": true,
            "iss": "https://issuer.example.com",
            "aud": "my-client-id",
            "exp": 1234567890,
            "iat": 1234567800,
            "unknown_field": "should be ignored"
        }"#;

        let claims: Claims = serde_json::from_str(json).unwrap();
        assert_eq!(claims.sub, "user123");
        assert_eq!(claims.email, "test@example.com");
        assert_eq!(claims.iss, "https://issuer.example.com");
        assert_eq!(claims.aud, "my-client-id");
    }

    #[test]
    fn test_claims_deserialization_with_custom_group_claim() {
        let value = serde_json::json!({
            "sub": "user123",
            "email": "test@example.com",
            "iss": "https://issuer.example.com",
            "aud": "my-client-id",
            "exp": 1234567890,
            "iat": 1234567800,
            "groups": ["wrong-group"],
            "cognito:groups": ["developers", "operators"]
        });

        let claims = Claims::from_value_with_group_claim(value, "cognito:groups").unwrap();

        assert_eq!(
            claims.groups,
            Some(vec!["developers".to_string(), "operators".to_string()])
        );
    }

    #[test]
    fn test_claims_deserialization_with_default_group_claim() {
        let value = serde_json::json!({
            "sub": "user123",
            "email": "test@example.com",
            "iss": "https://issuer.example.com",
            "aud": "my-client-id",
            "exp": 1234567890,
            "iat": 1234567800,
            "groups": ["developers"]
        });

        let claims = Claims::from_value_with_group_claim(value, "groups").unwrap();

        assert_eq!(claims.groups, Some(vec!["developers".to_string()]));
    }

    #[test]
    fn test_custom_group_claim_does_not_fall_back_to_groups() {
        let value = serde_json::json!({
            "sub": "user123",
            "email": "test@example.com",
            "iss": "https://issuer.example.com",
            "aud": "my-client-id",
            "exp": 1234567890,
            "iat": 1234567800,
            "groups": ["wrong-group"]
        });

        let claims = Claims::from_value_with_group_claim(value, "cognito:groups").unwrap();

        assert_eq!(claims.groups, None);
    }

    #[test]
    fn test_oidc_discovery_issuer_field_required() {
        // Discovery document without issuer field should fail to deserialize
        let json = r#"{"jwks_uri": "https://example.com/jwks"}"#;
        let result: std::result::Result<OidcDiscovery, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "Expected deserialization to fail without issuer field"
        );
    }

    #[test]
    fn test_oidc_discovery_deserialization() {
        let json = r#"{"issuer": "https://example.com", "jwks_uri": "https://example.com/jwks"}"#;
        let discovery: OidcDiscovery = serde_json::from_str(json).unwrap();
        assert_eq!(discovery.issuer, "https://example.com");
        assert_eq!(discovery.jwks_uri, "https://example.com/jwks");
    }
}
