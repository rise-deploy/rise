//! Upstream endpoint resolution for OAuth extensions: explicit endpoints from the
//! spec, falling back to the issuer's OIDC discovery document.

use super::models::OAuthExtensionSpec;
use crate::server::ssrf::{self, SsrfConfig};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::error;

/// OIDC discovery document (partial).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub(crate) struct OidcDiscoveryDocument {
    pub(crate) authorization_endpoint: Option<String>,
    pub(crate) token_endpoint: Option<String>,
    pub(crate) jwks_uri: Option<String>,
}

/// Resolved OAuth endpoints from spec or OIDC discovery.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedEndpoints {
    pub(crate) authorization_endpoint: String,
    pub(crate) token_endpoint: String,
}

/// The issuer's OIDC discovery URL.
pub(crate) fn discovery_url(issuer_url: &str) -> String {
    format!(
        "{}/.well-known/openid-configuration",
        issuer_url.trim_end_matches('/')
    )
}

/// Fetch the OIDC discovery document from the issuer URL.
pub(crate) async fn fetch_oidc_discovery(
    issuer_url: &str,
    ssrf_config: &SsrfConfig,
) -> Result<OidcDiscoveryDocument, String> {
    let discovery_url = discovery_url(issuer_url);

    // SSRF-validate the discovery URL before fetching
    ssrf::validate_url(&discovery_url, ssrf_config)
        .await
        .map_err(|e| {
            error!("OIDC discovery URL failed SSRF validation: {}", e);
            format!("OIDC discovery URL failed SSRF validation: {}", e)
        })?;

    let http_client = ssrf::safe_client(ssrf_config);
    let response = http_client.get(&discovery_url).send().await.map_err(|e| {
        error!(
            "Failed to fetch OIDC discovery from {}: {:?}",
            discovery_url, e
        );
        format!("Failed to fetch OIDC discovery: {}", e)
    })?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unable to read error response".to_string());
        error!(
            "OIDC discovery failed with status {}: {}",
            status, error_text
        );
        return Err(format!("OIDC discovery failed: {}", status));
    }

    response.json().await.map_err(|e| {
        error!("Failed to parse OIDC discovery response: {:?}", e);
        format!("Failed to parse OIDC discovery: {}", e)
    })
}

/// Resolve OAuth endpoints from the spec, falling back to OIDC discovery.
pub(crate) async fn resolve_oauth_endpoints(
    spec: &OAuthExtensionSpec,
    ssrf_config: &SsrfConfig,
) -> Result<ResolvedEndpoints, String> {
    // If both endpoints are provided in spec, use them directly
    if let (Some(auth), Some(token)) = (&spec.authorization_endpoint, &spec.token_endpoint) {
        return Ok(ResolvedEndpoints {
            authorization_endpoint: auth.clone(),
            token_endpoint: token.clone(),
        });
    }

    let discovery = fetch_oidc_discovery(&spec.issuer_url, ssrf_config).await?;

    // Use spec override if provided, otherwise use discovery
    let authorization_endpoint = spec
        .authorization_endpoint
        .clone()
        .or(discovery.authorization_endpoint)
        .ok_or_else(|| "No authorization_endpoint in spec or OIDC discovery".to_string())?;

    let token_endpoint = spec
        .token_endpoint
        .clone()
        .or(discovery.token_endpoint)
        .ok_or_else(|| "No token_endpoint in spec or OIDC discovery".to_string())?;

    Ok(ResolvedEndpoints {
        authorization_endpoint,
        token_endpoint,
    })
}

/// How long a fetched discovery document is reused; the same as oauthrelay's
/// own discovery cache.
const DISCOVERY_TTL: Duration = Duration::from_secs(60 * 60);
/// How long a failed fetch is remembered, so an upstream without a discovery
/// document (GitHub, Notion) is not queried on every request.
const DISCOVERY_FAILURE_TTL: Duration = Duration::from_secs(30);

/// Why a discovery lookup produced no usable document.
#[derive(Clone, Debug)]
pub(crate) enum DiscoveryFailure {
    /// The discovery URL, or the `jwks_uri` it names, is not a permitted
    /// server-side request target. Nothing at that issuer may be fetched.
    Blocked(String),
    /// The document could not be fetched or parsed.
    Unavailable(String),
}

impl std::fmt::Display for DiscoveryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Blocked(reason) => write!(f, "blocked: {reason}"),
            Self::Unavailable(reason) => write!(f, "unavailable: {reason}"),
        }
    }
}

type DiscoveryResult = Result<Arc<OidcDiscoveryDocument>, DiscoveryFailure>;

struct DiscoveryExpiry;

impl moka::Expiry<String, DiscoveryResult> for DiscoveryExpiry {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &DiscoveryResult,
        _created_at: Instant,
    ) -> Option<Duration> {
        Some(match value {
            Ok(_) => DISCOVERY_TTL,
            Err(_) => DISCOVERY_FAILURE_TTL,
        })
    }
}

/// SSRF-checked OIDC discovery, cached per issuer.
///
/// A cached document has passed SSRF validation of its discovery URL and of its
/// `jwks_uri`, so every URL in it that Rise or the relay fetches server-side
/// was checked.
pub(crate) struct CachedDiscovery {
    ssrf: SsrfConfig,
    cache: moka::future::Cache<String, DiscoveryResult>,
}

impl CachedDiscovery {
    pub(crate) fn new(ssrf: SsrfConfig) -> Self {
        Self {
            ssrf,
            cache: moka::future::Cache::builder()
                .max_capacity(10_000)
                .expire_after(DiscoveryExpiry)
                .build(),
        }
    }

    /// The issuer's discovery document, fetched at most once per TTL.
    pub(crate) async fn get(&self, issuer_url: &str) -> DiscoveryResult {
        let issuer = issuer_url.to_owned();
        self.cache
            .get_with(issuer.clone(), async move {
                let url = discovery_url(&issuer);
                if let Err(e) = ssrf::validate_url(&url, &self.ssrf).await {
                    return Err(DiscoveryFailure::Blocked(format!("{url}: {e}")));
                }
                let doc = fetch_oidc_discovery(&issuer, &self.ssrf)
                    .await
                    .map_err(DiscoveryFailure::Unavailable)?;
                if let Some(jwks_uri) = &doc.jwks_uri {
                    if let Err(e) = ssrf::validate_url(jwks_uri, &self.ssrf).await {
                        return Err(DiscoveryFailure::Blocked(format!(
                            "jwks_uri {jwks_uri}: {e}"
                        )));
                    }
                }
                Ok(Arc::new(doc))
            })
            .await
    }
}
