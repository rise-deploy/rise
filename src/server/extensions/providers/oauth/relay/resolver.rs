//! Resolves an OAuth extension's spec and status into oauthrelay upstream and relay configuration.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, SecondsFormat, Utc};
use oauthrelay_core::{
    ClientAuth, RedirectMatcher, RedirectOptions, RefreshTokenMode as RelayRefreshTokenMode,
    RefreshTokens, Relay, ResolveError, ResourceKey, ResourceResolver, SecretString,
    TokenEndpointAuthMethod as RelayAuthMethod, Upstream,
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tracing::{debug, error, warn};
use url::Url;

use super::layout::split_key;
use crate::db::{extensions as db_extensions, models::Project, projects as db_projects};
use crate::server::deployment::controller::DeploymentBackend;
use crate::server::encryption::EncryptionProvider;
use crate::server::extensions::providers::oauth::endpoints::{CachedDiscovery, DiscoveryFailure};
use crate::server::extensions::providers::oauth::models::{
    ClientType, OAuthExtensionSpec, OAuthExtensionStatus, RefreshTokenMode, TokenEndpointAuthMethod,
};
use crate::server::extensions::providers::oauth::oauth_client_secret_context;
use crate::server::ssrf::{self, SsrfConfig};

/// How long a decrypted upstream client secret is reused. The relay resolves the
/// upstream on every endpoint, including unauthenticated discovery and JWKS, so
/// without the cache each request would be a decryption (a KMS call).
const SECRET_TTL: Duration = Duration::from_secs(5 * 60);

/// The path, below Rise's public URL, of the Rise UI's OAuth test callback.
pub fn test_callback_path(project: &str, extension: &str) -> String {
    format!("/api/v1/projects/{project}/extensions/{extension}/oauth/test/callback")
}

/// The absolute URL of the Rise UI's OAuth test callback.
pub fn test_callback_url(public_url: &Url, project: &str, extension: &str) -> String {
    format!(
        "{}{}",
        public_url.as_str().trim_end_matches('/'),
        test_callback_path(project, extension)
    )
}

/// The active OAuth extension behind a relay key.
pub struct ActiveExtension {
    pub project: Project,
    pub extension: crate::db::models::ProjectExtension,
    pub spec: OAuthExtensionSpec,
    pub status: OAuthExtensionStatus,
}

impl ActiveExtension {
    /// The extension's incarnation, bound into every refresh-token handle so a
    /// deleted and recreated extension does not accept its predecessor's handles.
    pub fn generation(&self) -> String {
        generation(self.extension.created_at)
    }
}

/// The relay `generation` of an extension created at `created_at`.
pub fn generation(created_at: DateTime<Utc>) -> String {
    created_at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Loads the active (not soft-deleted) `oauth` extension `extension` of project
/// `project`; `None` when there is none.
pub async fn load_active_extension(
    pool: &PgPool,
    project: &str,
    extension: &str,
) -> anyhow::Result<Option<ActiveExtension>> {
    let Some(project) = db_projects::find_by_name(pool, project).await? else {
        return Ok(None);
    };
    let Some(ext) = db_extensions::find_by_project_and_name(pool, project.id, extension).await?
    else {
        return Ok(None);
    };
    if ext.deleted_at.is_some() || ext.extension_type != "oauth" {
        return Ok(None);
    }
    let spec: OAuthExtensionSpec = serde_json::from_value(ext.spec.clone())
        .map_err(|e| anyhow::anyhow!("invalid OAuth extension spec: {e}"))?;
    let status: OAuthExtensionStatus =
        serde_json::from_value(ext.status.clone()).unwrap_or_default();
    Ok(Some(ActiveExtension {
        project,
        extension: ext,
        spec,
        status,
    }))
}

/// Serves oauthrelay's upstreams and relays from Rise's OAuth extensions.
///
/// Every relay key is `{project}/{extension}` and names both the relay and its
/// upstream. Only active `oauth` extensions resolve; anything else is `None`,
/// which the relay answers with `404`.
pub struct ExtensionResolver {
    pool: PgPool,
    deployment_backend: Arc<dyn DeploymentBackend>,
    encryption_provider: Arc<dyn EncryptionProvider>,
    public_url: Url,
    redirect: RedirectOptions,
    ssrf: SsrfConfig,
    discovery: CachedDiscovery,
    secrets: moka::future::Cache<String, SecretString>,
    resolution_ttl: Duration,
    relays: moka::future::Cache<ResourceKey, Option<Arc<Relay>>>,
    upstreams: moka::future::Cache<ResourceKey, Option<Arc<Upstream>>>,
}

impl ExtensionResolver {
    pub fn new(
        pool: PgPool,
        deployment_backend: Arc<dyn DeploymentBackend>,
        encryption_provider: Arc<dyn EncryptionProvider>,
        public_url: Url,
        redirect: RedirectOptions,
        ssrf: SsrfConfig,
        resolution_ttl: Duration,
    ) -> Self {
        Self {
            resolution_ttl,
            relays: resolution_cache(resolution_ttl),
            upstreams: resolution_cache(resolution_ttl),
            pool,
            deployment_backend,
            encryption_provider,
            public_url,
            redirect,
            discovery: CachedDiscovery::new(ssrf.clone()),
            ssrf,
            secrets: moka::future::Cache::builder()
                .max_capacity(10_000)
                .time_to_live(SECRET_TTL)
                .build(),
        }
    }

    async fn load(&self, key: &ResourceKey) -> Result<Option<ActiveExtension>, ResolveError> {
        let Some((project, extension)) = split_key(key) else {
            return Ok(None);
        };
        load_active_extension(&self.pool, project, extension)
            .await
            .map_err(|e| failure(key, "failed to load the OAuth extension", e))
    }

    /// The application origins of the project's active deployments.
    async fn app_urls(&self, project: &Project) -> Vec<String> {
        let deployments = match crate::db::deployments::get_active_deployments_for_project(
            &self.pool, project.id,
        )
        .await
        {
            Ok(deployments) => deployments,
            Err(e) => {
                warn!(
                    "Failed to fetch active deployments for project {}: {:?}",
                    project.name, e
                );
                Vec::new()
            }
        };
        let mut urls = Vec::new();
        for deployment in &deployments {
            match self
                .deployment_backend
                .get_deployment_urls(deployment, project)
                .await
            {
                Ok(deployment_urls) => {
                    if !deployment_urls.default_url.is_empty() {
                        urls.push(deployment_urls.default_url);
                    }
                    urls.extend(deployment_urls.custom_domain_urls);
                }
                Err(e) => warn!(
                    "Failed to get deployment URLs for deployment {}: {:?}",
                    deployment.deployment_id, e
                ),
            }
        }
        urls
    }

    /// The decrypted upstream client secret, bound to the extension and the
    /// effective token endpoint. The token endpoint is SSRF-checked before the
    /// first decryption; both results are cached together.
    async fn client_secret(
        &self,
        key: &ResourceKey,
        active: &ActiveExtension,
        token_endpoint: &str,
    ) -> Result<SecretString, ResolveError> {
        let ciphertext = active
            .spec
            .client_secret_encrypted
            .as_deref()
            .ok_or_else(|| failure(key, "client_secret_encrypted is missing", ""))?;
        let spec_value = serde_json::to_value(&active.spec)
            .map_err(|e| failure(key, "failed to serialize the OAuth spec", e))?;
        let binding = rise_deployment_spec::credential_binding::oauth_credential_binding(
            &spec_value,
            token_endpoint,
        );
        let cache_key = URL_SAFE_NO_PAD.encode(Sha256::digest(
            serde_json::json!([
                active.project.id,
                active.extension.extension,
                generation(active.extension.created_at),
                ciphertext,
                binding,
            ])
            .to_string(),
        ));
        self.secrets
            .try_get_with(cache_key, async {
                ssrf::validate_url(token_endpoint, &self.ssrf)
                    .await
                    .map_err(|e| {
                        failure(key, "token endpoint failed SSRF validation", e.to_string())
                    })?;
                let context = oauth_client_secret_context(
                    active.project.id,
                    &active.extension.extension,
                    &active.spec,
                    token_endpoint,
                )
                .map_err(|e| failure(key, "failed to build the credential context", e))?;
                let secret = self
                    .encryption_provider
                    .decrypt(ciphertext, &context)
                    .await
                    .map_err(|e| {
                        failure(
                            key,
                            "OAuth credential does not match its resource, slot, or destination",
                            e,
                        )
                    })?;
                Ok::<_, ResolveError>(SecretString::new(secret))
            })
            .await
            .map_err(|e: Arc<ResolveError>| ResolveError(e.0.clone()))
    }
}

fn resolution_cache<V: Clone + Send + Sync + 'static>(
    ttl: Duration,
) -> moka::future::Cache<ResourceKey, V> {
    moka::future::Cache::builder()
        .max_capacity(10_000)
        .time_to_live(ttl.max(Duration::from_millis(1)))
        .build()
}

/// Logs a resolution failure with its cause and turns it into a `ResolveError`
/// (a `500` / `server_error` at the relay).
fn failure(key: &ResourceKey, what: &str, cause: impl std::fmt::Debug) -> ResolveError {
    error!(%key, "OAuth relay resolution failed: {what}: {cause:?}");
    ResolveError(format!("{key}: {what}"))
}

#[async_trait]
impl ResourceResolver for ExtensionResolver {
    async fn resolve_relay(&self, key: &ResourceKey) -> Result<Option<Arc<Relay>>, ResolveError> {
        if self.resolution_ttl.is_zero() {
            return self.load_relay(key).await;
        }
        self.relays
            .try_get_with_by_ref(key, self.load_relay(key))
            .await
            .map_err(|e: Arc<ResolveError>| ResolveError(e.0.clone()))
    }

    async fn resolve_upstream(
        &self,
        key: &ResourceKey,
    ) -> Result<Option<Arc<Upstream>>, ResolveError> {
        if self.resolution_ttl.is_zero() {
            return self.load_upstream(key).await;
        }
        self.upstreams
            .try_get_with_by_ref(key, self.load_upstream(key))
            .await
            .map_err(|e: Arc<ResolveError>| ResolveError(e.0.clone()))
    }
}

impl ExtensionResolver {
    async fn load_relay(&self, key: &ResourceKey) -> Result<Option<Arc<Relay>>, ResolveError> {
        let Some(active) = self.load(key).await? else {
            return Ok(None);
        };
        let app_urls = self.app_urls(&active.project).await;
        build_relay(&RelayInputs {
            key,
            created_at: active.extension.created_at,
            spec: &active.spec,
            status: &active.status,
            app_urls: &app_urls,
            public_url: &self.public_url,
            options: self.redirect,
        })
        .map(|relay| Some(Arc::new(relay)))
        .map_err(|e| failure(key, "invalid relay configuration", e))
    }

    async fn load_upstream(
        &self,
        key: &ResourceKey,
    ) -> Result<Option<Arc<Upstream>>, ResolveError> {
        let Some(active) = self.load(key).await? else {
            return Ok(None);
        };
        let spec = &active.spec;
        let needs_discovery = spec.explicit_authorization_endpoint().is_none()
            || spec.explicit_token_endpoint().is_none();
        let discovered = match self.discovery.get(&spec.issuer_url).await {
            Ok(doc) => Some(doc),
            // Both endpoints are explicit, so discovery only matters for the
            // JWKS. The relay's outbound policy refuses a blocked issuer too, so
            // its discovery and JWKS answer 502 while the flows keep working.
            Err(DiscoveryFailure::Unavailable(reason)) if !needs_discovery => {
                debug!(%key, "No OIDC discovery for an explicit-endpoint upstream: {reason}");
                None
            }
            Err(DiscoveryFailure::Blocked(reason)) if !needs_discovery => {
                warn!(%key, "OIDC discovery of an explicit-endpoint upstream is blocked: {reason}");
                None
            }
            Err(e) => return Err(failure(key, "OIDC discovery failed", e.to_string())),
        };
        let token_endpoint = match (spec.explicit_token_endpoint(), &discovered) {
            (Some(explicit), _) => explicit.to_owned(),
            (None, Some(doc)) => doc
                .token_endpoint
                .clone()
                .ok_or_else(|| failure(key, "no token_endpoint in OIDC discovery", ""))?,
            (None, None) => return Err(failure(key, "no token endpoint", "")),
        };
        let jwks_uri = discovered.as_ref().and_then(|doc| doc.jwks_uri.clone());
        let client_secret = self.client_secret(key, &active, &token_endpoint).await?;
        build_upstream(
            key,
            spec,
            &token_endpoint,
            jwks_uri.as_deref(),
            client_secret,
        )
        .map(|upstream| Some(Arc::new(upstream)))
        .map_err(|e| failure(key, "invalid upstream configuration", e))
    }
}

fn parse_http_url(field: &str, value: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|e| format!("{field} is not a valid URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(format!("{field} must be an absolute http(s) URL"));
    }
    Ok(url)
}

/// The upstream for an extension. `token_endpoint` is the effective endpoint the
/// client secret is bound to: pinning it means the relay sends the secret only
/// there, whatever discovery says later. The authorization endpoint stays unset
/// when the spec has none, so the relay discovers it and keeps the upstream's
/// metadata in its discovery document.
pub fn build_upstream(
    key: &ResourceKey,
    spec: &OAuthExtensionSpec,
    token_endpoint: &str,
    jwks_uri: Option<&str>,
    client_secret: SecretString,
) -> Result<Upstream, String> {
    let upstream = Upstream {
        key: key.clone(),
        issuer_url: parse_http_url("issuer_url", &spec.issuer_url)?,
        authorization_endpoint: spec
            .explicit_authorization_endpoint()
            .map(|value| parse_http_url("authorization_endpoint", value))
            .transpose()?,
        token_endpoint: Some(parse_http_url("token_endpoint", token_endpoint)?),
        jwks_uri: jwks_uri
            .map(|value| parse_http_url("jwks_uri", value))
            .transpose()?,
        client_id: spec.client_id.clone(),
        client_secret,
        token_endpoint_auth_method: match spec.token_endpoint_auth_method {
            TokenEndpointAuthMethod::ClientSecretPost => RelayAuthMethod::ClientSecretPost,
            TokenEndpointAuthMethod::ClientSecretBasic => RelayAuthMethod::ClientSecretBasic,
        },
    };
    upstream.validate()?;
    Ok(upstream)
}

/// Whether `token` is an RFC 6749 §3.3 `scope-token` (`1*NQCHAR`).
pub fn valid_scope_token(token: &str) -> bool {
    !token.is_empty()
        && token
            .bytes()
            .all(|b| b == 0x21 || (0x23..=0x5B).contains(&b) || (0x5D..=0x7E).contains(&b))
}

/// The spec's scopes as RFC 6749 scope tokens: entries are split on ASCII
/// whitespace, invalid tokens are dropped with a warning, and duplicates are
/// removed keeping the first occurrence.
pub fn normalize_scopes(key: &ResourceKey, spec: &OAuthExtensionSpec) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    for token in spec.scope_tokens() {
        if !valid_scope_token(token) {
            warn!(%key, scope = %token, "Dropping an invalid OAuth scope");
            continue;
        }
        if seen.insert(token) {
            normalized.push(token.to_owned());
        }
    }
    normalized
}

/// Everything [`build_relay`] maps into a relay.
pub struct RelayInputs<'a> {
    pub key: &'a ResourceKey,
    pub created_at: DateTime<Utc>,
    pub spec: &'a OAuthExtensionSpec,
    pub status: &'a OAuthExtensionStatus,
    /// URLs the project's active deployments are served at (default and custom domains).
    pub app_urls: &'a [String],
    pub public_url: &'a Url,
    pub options: RedirectOptions,
}

/// The relay for an extension.
pub fn build_relay(inputs: &RelayInputs<'_>) -> Result<Relay, String> {
    let RelayInputs {
        key,
        created_at,
        spec,
        status,
        app_urls,
        public_url,
        options,
    } = inputs;
    let (project, extension) =
        split_key(key).ok_or_else(|| format!("{key} is not a project/extension key"))?;
    let client_auth = match spec.client_type {
        ClientType::Public => ClientAuth::Public,
        ClientType::Confidential => ClientAuth::ClientSecret {
            client_id: status
                .rise_client_id
                .clone()
                .ok_or("the extension has no rise_client_id")?,
            client_secret: SecretString::new(
                status
                    .rise_client_secret
                    .clone()
                    .ok_or("the extension has no rise_client_secret")?,
            ),
        },
    };
    let mode = match spec.effective_refresh_token_mode() {
        RefreshTokenMode::Sealed => RelayRefreshTokenMode::Sealed,
        RefreshTokenMode::Rotating => RelayRefreshTokenMode::Rotating,
    };
    let scopes = normalize_scopes(key, spec);
    let relay = Relay {
        key: (*key).clone(),
        upstream: (*key).clone(),
        generation: Some(generation(*created_at)),
        client_auth,
        allowed_scopes: Some(scopes.clone()),
        scopes,
        required_id_token_claims: HashMap::new(),
        id_token_policy: None,
        refresh_tokens: RefreshTokens {
            mode,
            ..Default::default()
        },
        redirect_policy: redirect_policy(key, project, extension, app_urls, public_url, *options),
    };
    relay.validate_for(*options)?;
    Ok(relay)
}

/// Where the relay may send users back to:
///
/// 1. every origin of the project's active deployments (`http` ones only when
///    `allow_http_origins` is on, which follows the scheme of Rise's public URL);
/// 2. HTTP loopback hosts on any port, for local development;
/// 3. the Rise UI's test callback for this extension, exactly (or, on an
///    `http` Rise, its origin, since exact matchers require `https`).
fn redirect_policy(
    key: &ResourceKey,
    project: &str,
    extension: &str,
    app_urls: &[String],
    public_url: &Url,
    options: RedirectOptions,
) -> Vec<RedirectMatcher> {
    let mut seen_origins = HashSet::new();
    let mut matchers = Vec::new();
    for raw in app_urls {
        let origin = match Url::parse(raw) {
            Ok(url) => url.origin().ascii_serialization(),
            Err(e) => {
                warn!(%key, url = %raw, error = %e, "Ignoring an invalid deployment URL for OAuth redirects");
                continue;
            }
        };
        if !seen_origins.insert(origin.clone()) {
            continue;
        }
        let matcher = if origin.starts_with("https://") {
            RedirectMatcher::origin(&origin)
        } else if origin.starts_with("http://") {
            if !options.allow_http_origins {
                debug!(%key, %origin, "Skipping an http deployment origin: Rise's public URL is https");
                continue;
            }
            RedirectMatcher::http_origin(&origin)
        } else {
            Err("not an http(s) origin".to_owned())
        };
        match matcher {
            Ok(matcher) => matchers.push(matcher),
            Err(e) => {
                warn!(%key, %origin, error = %e, "Ignoring a deployment origin for OAuth redirects")
            }
        }
    }
    for loopback in ["http://127.0.0.1", "http://[::1]"] {
        matchers.push(RedirectMatcher::loopback_origin(loopback).expect("valid loopback origin"));
    }
    let test_callback = test_callback_url(public_url, project, extension);
    let test_matcher = if public_url.scheme() == "https" {
        RedirectMatcher::uri(test_callback)
    } else {
        let origin = public_url.origin().ascii_serialization();
        if !seen_origins.insert(origin.clone()) {
            return matchers;
        }
        RedirectMatcher::http_origin(&origin)
    };
    match test_matcher {
        Ok(matcher) => matchers.push(matcher),
        Err(e) => {
            warn!(%key, error = %e, "Rise's public URL cannot receive the OAuth test callback")
        }
    }
    matchers
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CREATED_AT: &str = "2026-10-31T12:34:56.123456789Z";

    fn key() -> ResourceKey {
        ResourceKey::new("demo/ext").unwrap()
    }

    fn spec(client_type: ClientType, mode: Option<RefreshTokenMode>) -> OAuthExtensionSpec {
        let mut spec: OAuthExtensionSpec = serde_json::from_value(json!({
            "provider_name": "Test",
            "client_id": "client",
            "client_secret_encrypted": "rise:enc:v2:x",
            "issuer_url": "https://idp.example.com",
            "scopes": ["openid email", "profile", "openid", "bad\"scope", ""],
        }))
        .unwrap();
        spec.client_type = client_type;
        spec.refresh_token_mode = mode;
        spec
    }

    fn status() -> OAuthExtensionStatus {
        OAuthExtensionStatus {
            rise_client_id: Some("demo-ext".into()),
            rise_client_secret: Some("rise-secret".into()),
            ..Default::default()
        }
    }

    fn options(http: bool) -> RedirectOptions {
        RedirectOptions {
            allow_localhost_loopback: true,
            allow_http_origins: http,
        }
    }

    fn relay(
        spec: &OAuthExtensionSpec,
        status: &OAuthExtensionStatus,
        app_urls: &[&str],
        public_url: &str,
    ) -> Result<Relay, String> {
        let public_url = Url::parse(public_url).unwrap();
        let app_urls: Vec<String> = app_urls.iter().map(|u| u.to_string()).collect();
        build_relay(&RelayInputs {
            key: &key(),
            created_at: CREATED_AT.parse().unwrap(),
            spec,
            status,
            app_urls: &app_urls,
            public_url: &public_url,
            options: options(public_url.scheme() == "http"),
        })
    }

    fn origins(relay: &Relay) -> Vec<String> {
        relay
            .redirect_policy
            .iter()
            .filter_map(|matcher| match matcher {
                RedirectMatcher::Origin(origin) => {
                    Some(origin.url().origin().ascii_serialization())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn maps_every_client_type_mode_and_scheme_to_a_valid_relay() {
        let app_urls = [
            "https://demo.apps.example.com",
            "https://demo.apps.example.com/",
            "https://custom.example.com/path",
            "http://demo.local:8080",
            "not a url",
        ];
        for client_type in [ClientType::Confidential, ClientType::Public] {
            for mode in [
                None,
                Some(RefreshTokenMode::Sealed),
                Some(RefreshTokenMode::Rotating),
            ] {
                for public_url in ["https://rise.example.com", "http://rise.localhost:3000"] {
                    let http = public_url.starts_with("http://");
                    let spec = spec(client_type, mode);
                    let relay = relay(&spec, &status(), &app_urls, public_url).unwrap();
                    relay.validate_for(options(http)).unwrap();

                    let expected_mode = match (client_type, mode) {
                        (ClientType::Public, _) | (_, Some(RefreshTokenMode::Rotating)) => {
                            RelayRefreshTokenMode::Rotating
                        }
                        _ => RelayRefreshTokenMode::Sealed,
                    };
                    assert_eq!(relay.refresh_tokens.mode, expected_mode);
                    match client_type {
                        ClientType::Public => {
                            assert!(matches!(relay.client_auth, ClientAuth::Public))
                        }
                        ClientType::Confidential => assert!(matches!(
                            &relay.client_auth,
                            ClientAuth::ClientSecret { client_id, client_secret }
                                if client_id == "demo-ext" && client_secret.expose() == "rise-secret"
                        )),
                    }

                    let mut expected_origins = vec![
                        "https://demo.apps.example.com".to_owned(),
                        "https://custom.example.com".to_owned(),
                    ];
                    if http {
                        expected_origins.push("http://demo.local:8080".to_owned());
                        expected_origins.push("http://rise.localhost:3000".to_owned());
                    }
                    assert_eq!(origins(&relay), expected_origins, "{public_url}");

                    let loopbacks = relay
                        .redirect_policy
                        .iter()
                        .filter(|m| matches!(m, RedirectMatcher::LoopbackOrigin(_)))
                        .count();
                    assert_eq!(loopbacks, 2);

                    let exact: Vec<_> = relay
                        .redirect_policy
                        .iter()
                        .filter_map(|m| match m {
                            RedirectMatcher::Uri(uri) => Some(uri.as_str()),
                            _ => None,
                        })
                        .collect();
                    if http {
                        assert!(exact.is_empty());
                    } else {
                        assert_eq!(
                            exact,
                            ["https://rise.example.com/api/v1/projects/demo/extensions/ext/oauth/test/callback"]
                        );
                    }

                    assert_eq!(relay.scopes, ["openid", "email", "profile"]);
                    assert_eq!(relay.allowed_scopes.as_deref(), Some(&relay.scopes[..]));
                    assert_eq!(
                        relay.generation.as_deref(),
                        Some("2026-10-31T12:34:56.123456Z")
                    );
                    assert_eq!(relay.key, key());
                    assert_eq!(relay.upstream, key());
                }
            }
        }
    }

    #[test]
    fn redirects_follow_the_policy() {
        let https = relay(
            &spec(ClientType::Confidential, None),
            &status(),
            &["https://demo.apps.example.com", "http://demo.local:8080"],
            "https://rise.example.com",
        )
        .unwrap();
        let opts = options(false);
        for allowed in [
            "https://demo.apps.example.com/cb",
            "http://127.0.0.1:5173/cb",
            "http://[::1]:8080/",
            "http://localhost:3000/cb",
            "http://foo.localhost:3000/",
            "https://rise.example.com/api/v1/projects/demo/extensions/ext/oauth/test/callback",
        ] {
            assert!(https.redirect_allowed(allowed, opts), "{allowed}");
        }
        for refused in [
            "http://demo.local:8080/cb",
            "https://rise.example.com/",
            "https://rise.example.com/projects/demo",
            "https://rise.example.com/api/v1/projects/demo/extensions/other/oauth/test/callback",
            "https://evil.example.com/",
        ] {
            assert!(!https.redirect_allowed(refused, opts), "{refused}");
        }
    }

    #[test]
    fn duplicate_origins_are_collapsed() {
        let relay = relay(
            &spec(ClientType::Confidential, None),
            &status(),
            &[
                "http://rise.localhost:3000/",
                "http://rise.localhost:3000/app",
            ],
            "http://rise.localhost:3000",
        )
        .unwrap();
        assert_eq!(origins(&relay), ["http://rise.localhost:3000"]);
    }

    #[test]
    fn a_confidential_relay_needs_rise_client_credentials() {
        for status in [
            OAuthExtensionStatus {
                rise_client_secret: Some("s".into()),
                ..Default::default()
            },
            OAuthExtensionStatus {
                rise_client_id: Some("demo-ext".into()),
                ..Default::default()
            },
        ] {
            assert!(relay(
                &spec(ClientType::Confidential, None),
                &status,
                &[],
                "https://rise.example.com"
            )
            .is_err());
        }
        // A public relay needs none.
        assert!(relay(
            &spec(ClientType::Public, None),
            &OAuthExtensionStatus::default(),
            &[],
            "https://rise.example.com"
        )
        .is_ok());
    }

    #[test]
    fn the_upstream_pins_the_bound_token_endpoint() {
        let mut spec = spec(ClientType::Confidential, None);
        spec.token_endpoint_auth_method = TokenEndpointAuthMethod::ClientSecretBasic;
        let upstream = build_upstream(
            &key(),
            &spec,
            "https://idp.example.com/discovered/token",
            Some("https://idp.example.com/jwks"),
            SecretString::new("upstream-secret"),
        )
        .unwrap();
        assert_eq!(
            upstream.token_endpoint.as_ref().map(Url::as_str),
            Some("https://idp.example.com/discovered/token")
        );
        assert_eq!(upstream.authorization_endpoint, None);
        assert_eq!(
            upstream.jwks_uri.as_ref().map(Url::as_str),
            Some("https://idp.example.com/jwks")
        );
        assert_eq!(upstream.client_id, "client");
        assert_eq!(upstream.client_secret.expose(), "upstream-secret");
        assert_eq!(
            upstream.token_endpoint_auth_method,
            RelayAuthMethod::ClientSecretBasic
        );

        // An empty endpoint counts as unset.
        spec.authorization_endpoint = Some(String::new());
        let upstream = build_upstream(
            &key(),
            &spec,
            "https://idp.example.com/token",
            None,
            SecretString::new("upstream-secret"),
        )
        .unwrap();
        assert_eq!(upstream.authorization_endpoint, None);

        spec.authorization_endpoint = Some("https://idp.example.com/authorize".into());
        let upstream = build_upstream(
            &key(),
            &spec,
            "https://idp.example.com/token",
            None,
            SecretString::new("upstream-secret"),
        )
        .unwrap();
        assert_eq!(
            upstream.authorization_endpoint.as_ref().map(Url::as_str),
            Some("https://idp.example.com/authorize")
        );
        assert_eq!(upstream.jwks_uri, None);

        for bad in ["", "not a url", "ftp://idp.example.com/token"] {
            assert!(build_upstream(
                &key(),
                &spec,
                bad,
                None,
                SecretString::new("upstream-secret")
            )
            .is_err());
        }
    }

    #[test]
    fn scope_tokens_follow_rfc_6749() {
        for valid in ["openid", "a:b", "https://x/y", "!#[]~"] {
            assert!(valid_scope_token(valid), "{valid}");
        }
        for invalid in ["", "a b", "a\"b", "a\\b", "é", "a\tb"] {
            assert!(!valid_scope_token(invalid), "{invalid}");
        }
    }
}
