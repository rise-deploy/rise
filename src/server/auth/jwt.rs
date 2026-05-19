use anyhow::{anyhow, Context, Result};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
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

/// Unified JWT validator supporting multiple OIDC issuers with caching
///
/// Uses OIDC discovery to find JWKS endpoints, caches keys per issuer,
/// and validates tokens with custom claim requirements.
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

        Self::validate_oidc_issuer(issuer_url, &discovery)?;

        Ok(discovery.jwks_uri)
    }

    /// Validate that the OIDC discovery document's issuer matches the expected issuer URL.
    /// Per RFC 8414 Section 3.1, a discovery document must not claim a different issuer.
    fn validate_oidc_issuer(expected_issuer: &str, discovery: &OidcDiscovery) -> Result<()> {
        let expected = expected_issuer.trim_end_matches('/');
        let actual = discovery.issuer.trim_end_matches('/');
        if expected != actual {
            return Err(anyhow!(
                "OIDC issuer mismatch: expected '{}', discovery returned '{}'",
                expected_issuer,
                discovery.issuer
            ));
        }
        Ok(())
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

    /// Validate custom claims (supports exact matching and wildcard patterns)
    ///
    /// Claims can use wildcard patterns with `*`:
    /// - `app*` matches `app`, `app-mr/6`, `app-staging`, etc.
    /// - `*-prod` matches `api-prod`, `web-prod`, etc.
    /// - `app-*-prod` matches `app-staging-prod`, `app-test-prod`, etc.
    ///
    /// If no wildcard is present, exact matching is performed (backward compatible).
    pub fn validate_custom_claims(
        jwt_claims: &serde_json::Value,
        expected_claims: &HashMap<String, String>,
    ) -> Result<()> {
        let claims_obj = jwt_claims
            .as_object()
            .ok_or_else(|| anyhow!("JWT claims is not an object"))?;

        for (key, expected_value) in expected_claims {
            let actual_value = claims_obj
                .get(key)
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("Claim '{}' not found or not a string", key))?;

            // Check if expected value contains wildcard
            if expected_value.contains('*') {
                // Use glob-style pattern matching
                if !Self::matches_wildcard_pattern(expected_value, actual_value) {
                    return Err(anyhow!(
                        "Claim mismatch: '{}' pattern '{}' does not match '{}'",
                        key,
                        expected_value,
                        actual_value
                    ));
                }
            } else {
                // Exact matching (backward compatible)
                if actual_value != expected_value {
                    return Err(anyhow!(
                        "Claim mismatch: '{}' expected '{}', got '{}'",
                        key,
                        expected_value,
                        actual_value
                    ));
                }
            }
        }

        Ok(())
    }

    /// Match a string against a glob-style pattern with `*` wildcards
    ///
    /// This implements simple glob-style pattern matching where `*` matches any
    /// sequence of characters (including empty string). Unlike filesystem globs,
    /// this matches across any characters, including path separators.
    ///
    /// Examples:
    /// - `matches_wildcard_pattern("app*", "app-mr/6")` → true
    /// - `matches_wildcard_pattern("app*", "webapp")` → false (doesn't start with "app")
    /// - `matches_wildcard_pattern("*-prod", "api-prod")` → true
    /// - `matches_wildcard_pattern("app-*-prod", "app-staging-prod")` → true
    ///
    /// Note: Consecutive wildcards (e.g., `app**prod`) are treated as a single wildcard
    /// due to split() creating empty parts, which always match.
    pub(crate) fn matches_wildcard_pattern(pattern: &str, text: &str) -> bool {
        // Split pattern by '*' to get literal parts
        let parts: Vec<&str> = pattern.split('*').collect();

        // If no wildcards (shouldn't happen, but handle it)
        if parts.len() == 1 {
            return pattern == text;
        }

        let mut pos = 0;

        for (i, part) in parts.iter().enumerate() {
            if i == 0 {
                // First part must match the beginning
                if !text.starts_with(part) {
                    return false;
                }
                pos = part.len();
            } else if i == parts.len() - 1 {
                // Last part must match the end (empty part matches any suffix when pattern ends with *)
                if !text[pos..].ends_with(part) {
                    return false;
                }
            } else {
                // Middle parts must appear in order
                if let Some(found_pos) = text[pos..].find(part) {
                    pos += found_pos + part.len();
                } else {
                    return false;
                }
            }
        }

        true
    }

    /// Validate a JWT token's signature and expiry against an issuer (JWKS only).
    ///
    /// This performs steps 1-4 of token validation:
    /// 1. Decode the JWT header to get the key ID
    /// 2. Fetch JWKS for the issuer (with caching)
    /// 3. Verify the signature using the matching key
    /// 4. Check token expiry
    ///
    /// Custom claim validation is NOT performed. Use `validate_custom_claims`
    /// separately when additional claim matching is needed.
    ///
    /// # Arguments
    /// * `token` - The JWT token string
    /// * `issuer_url` - The OIDC issuer URL (used for JWKS discovery and iss validation)
    ///
    /// # Returns
    /// The full JWT claims as a `serde_json::Value` on success
    pub async fn validate_token(&self, token: &str, issuer_url: &str) -> Result<serde_json::Value> {
        // Decode header to get key ID
        let header = decode_header(token).context("Failed to decode JWT header")?;
        let kid = header
            .kid
            .ok_or_else(|| anyhow!("JWT header missing kid"))?;

        // Get JWKS for this issuer
        let keys = self.get_jwks(issuer_url).await?;

        // Get the decoding key
        let key = keys
            .get(&kid)
            .ok_or_else(|| anyhow!("Key {} not found in JWKS for issuer {}", kid, issuer_url))?;

        // Set up validation parameters
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[issuer_url]);
        // Disable built-in audience validation - we'll validate it ourselves in expected_claims
        validation.validate_aud = false;

        // Validate and decode the token
        let token_data = decode::<serde_json::Value>(token, key, &validation)
            .context("Failed to validate JWT token")?;

        // Validate exp claim manually (should be handled by jsonwebtoken, but double-check)
        if let Some(exp) = token_data.claims.get("exp").and_then(|v| v.as_u64()) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            if now > exp {
                return Err(anyhow!("Token has expired"));
            }
        }

        Ok(token_data.claims)
    }

    /// Validate a JWT token against an issuer with expected claims
    ///
    /// This calls `validate_token` for signature/expiry validation, then
    /// `validate_custom_claims` for claim matching.
    ///
    /// # Arguments
    /// * `token` - The JWT token string
    /// * `issuer_url` - The OIDC issuer URL (used for JWKS discovery and iss validation)
    /// * `expected_claims` - Claims that must match exactly (including "aud" if required)
    ///
    /// # Returns
    /// The full JWT claims as a `serde_json::Value` on success
    pub async fn validate(
        &self,
        token: &str,
        issuer_url: &str,
        expected_claims: &HashMap<String, String>,
    ) -> Result<serde_json::Value> {
        let claims = self.validate_token(token, issuer_url).await?;
        Self::validate_custom_claims(&claims, expected_claims)?;
        Ok(claims)
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
    fn test_validate_custom_claims_success() {
        let jwt_claims = serde_json::json!({
            "aud": "my-audience",
            "project_path": "myorg/myrepo",
            "extra": "value"
        });

        let mut expected = HashMap::new();
        expected.insert("aud".to_string(), "my-audience".to_string());
        expected.insert("project_path".to_string(), "myorg/myrepo".to_string());

        let result = JwtValidator::validate_custom_claims(&jwt_claims, &expected);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_custom_claims_missing() {
        let jwt_claims = serde_json::json!({
            "aud": "my-audience"
        });

        let mut expected = HashMap::new();
        expected.insert("aud".to_string(), "my-audience".to_string());
        expected.insert("project_path".to_string(), "myorg/myrepo".to_string());

        let result = JwtValidator::validate_custom_claims(&jwt_claims, &expected);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("project_path"));
    }

    #[test]
    fn test_validate_custom_claims_mismatch() {
        let jwt_claims = serde_json::json!({
            "aud": "wrong-audience",
            "project_path": "myorg/myrepo"
        });

        let mut expected = HashMap::new();
        expected.insert("aud".to_string(), "my-audience".to_string());

        let result = JwtValidator::validate_custom_claims(&jwt_claims, &expected);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("mismatch"));
    }

    #[test]
    fn test_wildcard_pattern_prefix() {
        // Pattern: app*
        assert!(JwtValidator::matches_wildcard_pattern("app*", "app"));
        assert!(JwtValidator::matches_wildcard_pattern("app*", "app-mr/6"));
        assert!(JwtValidator::matches_wildcard_pattern(
            "app*",
            "app-staging"
        ));
        assert!(JwtValidator::matches_wildcard_pattern(
            "app*",
            "application"
        ));

        // Should not match
        assert!(!JwtValidator::matches_wildcard_pattern("app*", "myapp"));
        assert!(!JwtValidator::matches_wildcard_pattern("app*", "webapp"));
    }

    #[test]
    fn test_wildcard_pattern_suffix() {
        // Pattern: *-prod
        assert!(JwtValidator::matches_wildcard_pattern("*-prod", "api-prod"));
        assert!(JwtValidator::matches_wildcard_pattern("*-prod", "web-prod"));
        assert!(JwtValidator::matches_wildcard_pattern(
            "*-prod",
            "my-service-prod"
        ));

        // Should not match
        assert!(!JwtValidator::matches_wildcard_pattern(
            "*-prod",
            "production"
        ));
        assert!(!JwtValidator::matches_wildcard_pattern("*-prod", "prod"));
        assert!(!JwtValidator::matches_wildcard_pattern(
            "*-prod",
            "api-prod-backup"
        ));
    }

    #[test]
    fn test_wildcard_pattern_middle() {
        // Pattern: app-*-prod
        assert!(JwtValidator::matches_wildcard_pattern(
            "app-*-prod",
            "app-staging-prod"
        ));
        assert!(JwtValidator::matches_wildcard_pattern(
            "app-*-prod",
            "app-test-prod"
        ));
        assert!(JwtValidator::matches_wildcard_pattern(
            "app-*-prod",
            "app-mr/6-prod"
        ));

        // Should not match
        assert!(!JwtValidator::matches_wildcard_pattern(
            "app-*-prod",
            "app-prod"
        ));
        assert!(!JwtValidator::matches_wildcard_pattern(
            "app-*-prod",
            "app-staging"
        ));
        assert!(!JwtValidator::matches_wildcard_pattern(
            "app-*-prod",
            "web-staging-prod"
        ));
    }

    #[test]
    fn test_wildcard_pattern_multiple() {
        // Pattern with multiple wildcards: *-app-*
        assert!(JwtValidator::matches_wildcard_pattern(
            "*-app-*",
            "my-app-staging"
        ));
        assert!(JwtValidator::matches_wildcard_pattern(
            "*-app-*",
            "test-app-mr/6"
        ));
        assert!(JwtValidator::matches_wildcard_pattern(
            "*-app-*",
            "web-app-prod"
        ));

        // Should not match
        assert!(!JwtValidator::matches_wildcard_pattern(
            "*-app-*",
            "my-application"
        ));
        assert!(!JwtValidator::matches_wildcard_pattern("*-app-*", "app"));
    }

    #[test]
    fn test_wildcard_pattern_edge_cases() {
        // Single wildcard matches everything
        assert!(JwtValidator::matches_wildcard_pattern("*", "anything"));
        assert!(JwtValidator::matches_wildcard_pattern("*", ""));

        // Pattern ending with * (wildcard can match empty string)
        assert!(JwtValidator::matches_wildcard_pattern(
            "app*",
            "application"
        ));
        assert!(JwtValidator::matches_wildcard_pattern("app*", "app")); // * matches empty

        // Pattern starting with * (wildcard can match empty string)
        assert!(JwtValidator::matches_wildcard_pattern("*app", "myapp"));
        assert!(JwtValidator::matches_wildcard_pattern("*app", "app")); // * matches empty

        // Pattern with middle wildcard and required suffix
        assert!(JwtValidator::matches_wildcard_pattern(
            "app-*",
            "app-staging"
        ));
        assert!(JwtValidator::matches_wildcard_pattern("app-*", "app-")); // * matches empty
        assert!(!JwtValidator::matches_wildcard_pattern("app-*", "app")); // missing '-'

        // Multiple consecutive wildcards are implicitly treated as a single wildcard
        // because split('*') creates empty string parts that always match
        assert!(JwtValidator::matches_wildcard_pattern(
            "app**prod",
            "appprod"
        ));
        assert!(JwtValidator::matches_wildcard_pattern(
            "app**prod",
            "app-staging-prod"
        ));
        // This is equivalent to "app*prod" due to how split works
        assert!(JwtValidator::matches_wildcard_pattern(
            "app***prod",
            "app-test-prod"
        ));

        // No match cases
        assert!(!JwtValidator::matches_wildcard_pattern("app*", ""));
        assert!(!JwtValidator::matches_wildcard_pattern("*app", "ap"));
    }

    #[test]
    fn test_validate_custom_claims_with_wildcard() {
        // Test wildcard in environment claim
        let jwt_claims = serde_json::json!({
            "aud": "my-audience",
            "environment": "app-mr/6"
        });

        let mut expected = HashMap::new();
        expected.insert("aud".to_string(), "my-audience".to_string());
        expected.insert("environment".to_string(), "app*".to_string());

        let result = JwtValidator::validate_custom_claims(&jwt_claims, &expected);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_custom_claims_with_wildcard_no_match() {
        // Test wildcard that doesn't match
        let jwt_claims = serde_json::json!({
            "aud": "my-audience",
            "environment": "webapp-staging"
        });

        let mut expected = HashMap::new();
        expected.insert("aud".to_string(), "my-audience".to_string());
        expected.insert("environment".to_string(), "app*".to_string());

        let result = JwtValidator::validate_custom_claims(&jwt_claims, &expected);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("pattern"));
    }

    #[test]
    fn test_validate_custom_claims_mixed_exact_and_wildcard() {
        // Test mix of exact and wildcard matching
        let jwt_claims = serde_json::json!({
            "aud": "my-audience",
            "project_path": "myorg/myrepo",
            "environment": "app-mr/12"
        });

        let mut expected = HashMap::new();
        expected.insert("aud".to_string(), "my-audience".to_string());
        expected.insert("project_path".to_string(), "myorg/myrepo".to_string());
        expected.insert("environment".to_string(), "app*".to_string());

        let result = JwtValidator::validate_custom_claims(&jwt_claims, &expected);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_custom_claims_wildcard_backward_compat() {
        // Ensure exact matching still works (backward compatibility)
        let jwt_claims = serde_json::json!({
            "aud": "my-audience",
            "environment": "production"
        });

        let mut expected = HashMap::new();
        expected.insert("aud".to_string(), "my-audience".to_string());
        expected.insert("environment".to_string(), "production".to_string());

        let result = JwtValidator::validate_custom_claims(&jwt_claims, &expected);
        assert!(result.is_ok());

        // Should fail with different value
        let mut expected_wrong = HashMap::new();
        expected_wrong.insert("aud".to_string(), "my-audience".to_string());
        expected_wrong.insert("environment".to_string(), "staging".to_string());

        let result_wrong = JwtValidator::validate_custom_claims(&jwt_claims, &expected_wrong);
        assert!(result_wrong.is_err());
    }

    #[test]
    fn test_oidc_discovery_issuer_field_required() {
        // Discovery document without issuer field should fail to deserialize
        let json = r#"{"jwks_uri": "https://example.com/jwks"}"#;
        let result: Result<OidcDiscovery, _> = serde_json::from_str(json);
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

    #[test]
    fn test_validate_oidc_issuer_match() {
        let discovery = OidcDiscovery {
            issuer: "https://accounts.example.com".to_string(),
            jwks_uri: "https://accounts.example.com/jwks".to_string(),
        };
        assert!(
            JwtValidator::validate_oidc_issuer("https://accounts.example.com", &discovery).is_ok()
        );
    }

    #[test]
    fn test_validate_oidc_issuer_mismatch() {
        let discovery = OidcDiscovery {
            issuer: "https://evil.example.com".to_string(),
            jwks_uri: "https://evil.example.com/jwks".to_string(),
        };
        let err = JwtValidator::validate_oidc_issuer("https://accounts.example.com", &discovery)
            .unwrap_err();
        assert!(
            err.to_string().contains("OIDC issuer mismatch"),
            "Expected issuer mismatch error, got: {}",
            err
        );
    }

    #[test]
    fn test_validate_oidc_issuer_trailing_slash_normalization() {
        // Discovery has trailing slash, expected does not
        let discovery = OidcDiscovery {
            issuer: "https://accounts.example.com/".to_string(),
            jwks_uri: "https://accounts.example.com/jwks".to_string(),
        };
        assert!(
            JwtValidator::validate_oidc_issuer("https://accounts.example.com", &discovery).is_ok()
        );

        // Expected has trailing slash, discovery does not
        assert!(
            JwtValidator::validate_oidc_issuer("https://accounts.example.com/", &discovery).is_ok()
        );
    }
}
