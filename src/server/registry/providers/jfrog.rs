use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use crate::server::registry::{
    models::{JfrogConfig, JfrogTokenProvider, RegistryAuthMethod, RegistryCredentials},
    ImageTagType, RegistryProvider,
};

/// Cached pull credentials for a specific scope
struct CachedPullCredentials {
    username: String,
    access_token: String,
    created_at: Instant,
    refresh_after: Duration,
}

/// JFrog Artifactory container registry provider
///
/// Mints short-lived scoped access tokens for each push/pull operation.
/// Supports two token-issuing backends:
/// - Vault (via vault-plugin-secrets-artifactory)
/// - Direct (via JFrog's access token API)
pub struct JfrogProvider {
    config: JfrogConfig,
    http_client: reqwest::Client,
    registry_host: String,
    registry_url: String,
    client_registry_url: String,
    /// Per-scope cache for pull credentials (keyed by scope string)
    pull_cache: RwLock<HashMap<String, CachedPullCredentials>>,
}

#[derive(Deserialize)]
struct VaultTokenResponse {
    data: VaultTokenData,
}

#[derive(Deserialize)]
struct VaultTokenData {
    access_token: String,
    username: String,
}

#[derive(Deserialize)]
struct DirectTokenResponse {
    access_token: String,
    /// Username associated with the token (provided by JFrog)
    #[serde(default)]
    username: Option<String>,
}

impl JfrogProvider {
    pub fn new(config: JfrogConfig) -> Result<Self> {
        let registry_host = config.registry_host.clone();
        let registry_url = format!("{}/{}", config.registry_host, config.docker_repo_key);
        let client_registry_url =
            format!("{}/{}", config.client_registry_host, config.docker_repo_key);

        let http_client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self {
            config,
            http_client,
            registry_host,
            registry_url,
            client_registry_url,
            pull_cache: RwLock::new(HashMap::new()),
        })
    }

    /// Read the current Vault token, supporting file-based rotation.
    fn resolve_vault_token(static_token: &str, token_file: &Option<String>) -> Result<String> {
        if let Some(path) = token_file {
            let token = std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read Vault token from file: {}", path))?;
            let token = token.trim().to_string();
            if token.is_empty() {
                anyhow::bail!("Vault token file '{}' is empty", path);
            }
            Ok(token)
        } else {
            Ok(static_token.to_string())
        }
    }

    /// Request a scoped token from the configured backend.
    ///
    /// `scope` is the desired JFrog artifact scope string. For Vault mode with
    /// `scope_override: false`, the scope parameter is ignored and the Vault
    /// role's configured scope is used instead.
    async fn request_token(&self, scope: &str, ttl: u64) -> Result<(String, String)> {
        match &self.config.token_provider {
            JfrogTokenProvider::Vault {
                vault_addr,
                vault_token,
                vault_token_file,
                vault_mount_path,
                role,
                scope_override,
            } => {
                let token = Self::resolve_vault_token(vault_token, vault_token_file)?;
                let url = format!(
                    "{}/v1/{}/token/{}",
                    vault_addr.trim_end_matches('/'),
                    vault_mount_path,
                    role
                );

                // The vault-plugin-secrets-artifactory uses GET with query
                // parameters (POST returns "unsupported operation").
                let request_url = if *scope_override {
                    tracing::debug!(
                        scope = scope,
                        ttl = ttl,
                        "Requesting JFrog token via Vault (scope override)"
                    );
                    format!("{}?scope={}&ttl={}s", url, urlencoding::encode(scope), ttl)
                } else {
                    tracing::debug!(ttl = ttl, "Requesting JFrog token via Vault (role scope)");
                    format!("{}?ttl={}s", url, ttl)
                };
                let response = self
                    .http_client
                    .get(&request_url)
                    .header("X-Vault-Token", &token)
                    .send()
                    .await
                    .context("Failed to reach Vault token endpoint")?;

                if !response.status().is_success() {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    anyhow::bail!("Vault token request returned {}: {}", status, body);
                }

                let vault_resp: VaultTokenResponse = response
                    .json()
                    .await
                    .context("Failed to parse Vault token response")?;

                Ok((vault_resp.data.username, vault_resp.data.access_token))
            }
            JfrogTokenProvider::Direct {
                jfrog_url,
                admin_token,
            } => {
                let url = format!("{}/access/api/v1/tokens", jfrog_url.trim_end_matches('/'));

                tracing::debug!(
                    scope = scope,
                    ttl = ttl,
                    "Requesting JFrog token via Direct API"
                );

                let response = self
                    .http_client
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", admin_token))
                    .json(&serde_json::json!({
                        "scope": scope,
                        "expires_in": ttl,
                    }))
                    .send()
                    .await
                    .context("Failed to reach JFrog token endpoint")?;

                if !response.status().is_success() {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    anyhow::bail!("JFrog token request returned {}: {}", status, body);
                }

                let token_resp: DirectTokenResponse = response
                    .json()
                    .await
                    .context("Failed to parse JFrog token response")?;

                let username = token_resp.username.unwrap_or_else(|| "admin".to_string());
                Ok((username, token_resp.access_token))
            }
        }
    }

    /// Build an artifact scope string with a recursive wildcard.
    /// JFrog uses `**` for recursive path matching; `*` only matches one level.
    fn artifact_scope(&self, repository: &str, permissions: &str) -> String {
        format!(
            "artifact:{}/**:{}",
            self.scope_path(repository),
            permissions
        )
    }

    /// Build the scope path: `<docker_repo_key>/<repository>`
    fn scope_path(&self, repository: &str) -> String {
        format!("{}/{}", self.config.docker_repo_key, repository)
    }
}

#[async_trait]
impl RegistryProvider for JfrogProvider {
    async fn get_credentials(
        &self,
        repository: &str,
        tags: &[&str],
    ) -> Result<RegistryCredentials> {
        if tags.is_empty() {
            anyhow::bail!("JFrog get_credentials called with no tags");
        }
        let scope = build_push_scope(
            &self.scope_path(repository),
            tags,
            &self.config.push_permissions,
        )?;

        tracing::info!(
            repository = repository,
            tags = ?tags,
            scope = %scope,
            "Fetching scoped JFrog push token"
        );

        let (username, access_token) = self
            .request_token(&scope, self.config.push_token_ttl)
            .await?;

        Ok(RegistryCredentials {
            registry_url: format!("{}/{}", self.client_registry_url, repository),
            username,
            password: access_token,
            expires_in: Some(self.config.push_token_ttl),
            auth_method: RegistryAuthMethod::LoginCredentials,
        })
    }

    async fn get_pull_credentials(&self) -> Result<(String, String)> {
        // Return empty credentials. Pull credentials are always project-scoped
        // via get_k8s_pull_credentials.
        Ok((String::new(), String::new()))
    }

    async fn get_k8s_pull_credentials(&self, repository: &str) -> Result<RegistryCredentials> {
        let scope = self.artifact_scope(repository, &self.config.pull_permissions);

        // Check cache under read lock
        {
            let cache = self.pull_cache.read().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = cache.get(&scope) {
                if entry.created_at.elapsed() < entry.refresh_after {
                    tracing::debug!(
                        repository = repository,
                        scope = %scope,
                        "Using cached JFrog pull credentials"
                    );
                    return Ok(RegistryCredentials {
                        registry_url: self.registry_host.clone(),
                        username: entry.username.clone(),
                        password: entry.access_token.clone(),
                        expires_in: Some(self.config.pull_token_ttl),
                        auth_method: RegistryAuthMethod::LoginCredentials,
                    });
                }
            }
        }

        // Cache miss or expired — mint a new token
        tracing::info!(
            repository = repository,
            scope = %scope,
            "Fetching scoped JFrog pull token"
        );

        let (username, access_token) = self
            .request_token(&scope, self.config.pull_token_ttl)
            .await?;

        // Update cache under write lock
        {
            let mut cache = self.pull_cache.write().unwrap_or_else(|e| e.into_inner());
            cache.insert(
                scope,
                CachedPullCredentials {
                    username: username.clone(),
                    access_token: access_token.clone(),
                    created_at: Instant::now(),
                    refresh_after: Duration::from_secs(self.config.pull_token_ttl * 2 / 3),
                },
            );
        }

        Ok(RegistryCredentials {
            registry_url: self.registry_host.clone(),
            username,
            password: access_token,
            expires_in: Some(self.config.pull_token_ttl),
            auth_method: RegistryAuthMethod::LoginCredentials,
        })
    }

    fn registry_host(&self) -> &str {
        &self.registry_host
    }

    fn registry_url(&self) -> &str {
        &self.registry_url
    }

    fn get_image_tag(&self, repository: &str, tag: &str, tag_type: ImageTagType) -> String {
        let base = match tag_type {
            ImageTagType::ClientFacing => &self.client_registry_url,
            ImageTagType::Internal => &self.registry_url,
        };
        format!("{}/{}:{}", base, repository, tag)
    }

    fn requires_pull_secret(&self) -> bool {
        self.config.mint_pull_secrets
    }
}

/// Build the JFrog `scope` string for a push token covering one or more tags.
///
/// Docker/OCI push writes to three sub-path groups, per tag:
///   1. `{tag}/**` — manifest (manifest.json or list.manifest.json)
///   2. `_uploads/**` — blob staging during upload (SHARED across tags
///      pushed against the same repository)
///   3. `sha256*/*` — content-addressed manifests written by BuildKit,
///      stored as siblings of the tag (SHARED across tags as well)
///
/// The sha256 scope uses `sha256*/*` (not `sha256:*`) because JFrog stores
/// these as `sha256:{digest}/` directories and the colon is matched by the
/// glob wildcard.
///
/// For multi-container deployments the function must extend the scope to
/// cover every tag's manifest path; the shared blob and content-addressed
/// paths only need to appear once.
///
/// Defense-in-depth: each tag is validated against the OCI tag charset
/// (`[a-zA-Z0-9._-]+`) before interpolation. A tag containing a space,
/// `/`, `:`, `*`, or other unexpected character would either malform the
/// space-separated scope string or — worse — be parsed by Artifactory as
/// additional scope entries, potentially escalating privileges. Callers
/// validate inputs upstream, but we reject malformed tags here as well.
fn build_push_scope(base: &str, tags: &[&str], perms: &str) -> Result<String> {
    for tag in tags {
        if !is_valid_oci_tag(tag) {
            anyhow::bail!(
                "JFrog push scope rejected: tag '{}' contains characters outside the OCI tag charset [a-zA-Z0-9._-]",
                tag
            );
        }
    }
    let mut scope_parts: Vec<String> = tags
        .iter()
        .map(|t| format!("artifact:{base}/{t}/**:{perms}"))
        .collect();
    scope_parts.push(format!("artifact:{base}/_uploads/**:{perms}"));
    scope_parts.push(format!("artifact:{base}/sha256*/*:{perms}"));
    Ok(scope_parts.join(" "))
}

/// Validate that a string conforms to the OCI tag charset.
///
/// Per the OCI distribution spec, tags match `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`.
/// We additionally require non-empty; this is sufficient to keep the JFrog
/// scope string well-formed (no spaces, no `/`, no `:`, no `*`, no control chars).
fn is_valid_oci_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::build_push_scope;

    #[test]
    fn single_tag_scope_matches_legacy_format() {
        // Single-container deployments mint a token covering one manifest
        // path plus the shared blob and content-addressed scopes. This must
        // stay byte-compatible with what JFrog has been seeing in prod.
        let scope = build_push_scope("docker/my-app", &["20251215-204525"], "write").unwrap();
        assert_eq!(
            scope,
            "artifact:docker/my-app/20251215-204525/**:write \
             artifact:docker/my-app/_uploads/**:write \
             artifact:docker/my-app/sha256*/*:write"
        );
    }

    #[test]
    fn multi_tag_scope_covers_every_container() {
        // Multi-container: every container's tag manifest path needs its
        // own artifact: scope. Without this the second push (e.g. the
        // backend image after the frontend) would be rejected by JFrog
        // with a 401, since the token's scope only covers the first tag.
        let tags = ["20251215-204525-frontend", "20251215-204525-backend"];
        let scope = build_push_scope("docker/my-app", &tags, "write").unwrap();
        assert!(scope.contains("artifact:docker/my-app/20251215-204525-frontend/**:write"));
        assert!(scope.contains("artifact:docker/my-app/20251215-204525-backend/**:write"));
        // Shared scopes still appear exactly once.
        assert_eq!(scope.matches("/_uploads/**").count(), 1);
        assert_eq!(scope.matches("/sha256*/*").count(), 1);
    }

    #[test]
    fn scope_preserves_caller_tag_order() {
        // Deterministic order makes the scope string stable across
        // reconciles — useful when something logs or compares the string.
        let tags = ["c", "a", "b"];
        let scope = build_push_scope("docker/x", &tags, "write").unwrap();
        let c_pos = scope.find("/c/**").expect("c");
        let a_pos = scope.find("/a/**").expect("a");
        let b_pos = scope.find("/b/**").expect("b");
        assert!(c_pos < a_pos);
        assert!(a_pos < b_pos);
    }

    #[test]
    fn scope_rejects_tags_with_unsafe_characters() {
        // Defense-in-depth: any character outside the OCI tag charset
        // could either malform the space-separated scope string or be
        // re-parsed by Artifactory as additional scope entries. Each of
        // these must be rejected with a clear error naming the tag.
        for bad in [
            "20260528-120000 api", // space
            "20260528/api",        // path separator
            "20260528:api",        // scope separator
            "20260528-*",          // wildcard
        ] {
            let err = build_push_scope("docker/my-app", &[bad], "write")
                .expect_err(&format!("expected rejection for tag '{}'", bad));
            let msg = format!("{:#}", err);
            assert!(
                msg.contains(bad),
                "error message should name the offending tag '{}', got: {}",
                bad,
                msg
            );
        }
    }

    #[test]
    fn scope_accepts_valid_oci_tags() {
        // Valid tags from the OCI charset: digits, letters, `.`, `_`, `-`.
        // Covers the single-container, multi-container, and library-style
        // forms we actually produce.
        for good in ["20260528-120000", "20260528-120000-api", "foo.bar_baz-0"] {
            build_push_scope("docker/my-app", &[good], "write").unwrap_or_else(|e| {
                panic!("tag '{}' should be accepted, got error: {:#}", good, e)
            });
        }
    }
}
