use crate::db::env_vars as db_env_vars;
use crate::server::encryption::EncryptionProvider;
use crate::server::extensions::providers::oauth::models::{
    OAuthExtensionSpec, OAuthExtensionStatus, TokenResponse,
};
use crate::server::extensions::{Extension, InjectedEnvVar, InjectedEnvVarValue};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use rise_runtime_sync::with_leader_election;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use url::Url;
use uuid::Uuid;

pub struct OAuthProviderConfig {
    pub db_pool: PgPool,
    pub encryption_provider: Arc<dyn EncryptionProvider>,
    pub http_client: reqwest::Client,
    pub api_domain: String,
}

#[allow(dead_code)]
pub struct OAuthProvider {
    db_pool: PgPool,
    encryption_provider: Arc<dyn EncryptionProvider>,
    http_client: reqwest::Client,
    api_domain: String,
}

impl Clone for OAuthProvider {
    fn clone(&self) -> Self {
        Self {
            db_pool: self.db_pool.clone(),
            encryption_provider: self.encryption_provider.clone(),
            http_client: self.http_client.clone(),
            api_domain: self.api_domain.clone(),
        }
    }
}

/// Generate a secure random token for Rise client secret (32 bytes, base64url encoded)
fn generate_rise_client_secret() -> String {
    use base64::Engine;
    use rand::RngExt;

    let mut rng = rand::rng();
    let bytes: Vec<u8> = (0..32).map(|_| rng.random()).collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

impl OAuthProvider {
    pub fn new(config: OAuthProviderConfig) -> Self {
        Self {
            db_pool: config.db_pool,
            encryption_provider: config.encryption_provider,
            http_client: config.http_client,
            api_domain: config.api_domain,
        }
    }

    /// Compute the redirect URI for this OAuth extension
    #[allow(dead_code)]
    pub fn compute_redirect_uri(&self, project_name: &str, extension_name: &str) -> String {
        format!(
            "{}/oidc/{}/{}/callback",
            self.api_domain.trim_end_matches('/'),
            project_name,
            extension_name
        )
    }

    /// Resolve client_secret from project environment variable
    #[allow(dead_code)]
    pub async fn resolve_client_secret(
        &self,
        project_id: Uuid,
        client_secret_ref: &str,
    ) -> Result<String> {
        // Get all environment variables for the project
        let env_vars = db_env_vars::list_project_env_vars(&self.db_pool, project_id, None).await?;

        // Find the environment variable by key
        let env_var = env_vars
            .iter()
            .find(|var| var.key == client_secret_ref)
            .ok_or_else(|| {
                anyhow!(
                    "Environment variable '{}' not found for OAuth client secret",
                    client_secret_ref
                )
            })?;

        // Decrypt the value if it's a secret
        let client_secret = if env_var.is_secret {
            self.encryption_provider
                .decrypt(&env_var.value)
                .await
                .context("Failed to decrypt OAuth client secret")?
        } else {
            env_var.value.clone()
        };

        Ok(client_secret)
    }

    /// Resolve token endpoint from spec or OIDC discovery
    #[allow(dead_code)]
    async fn resolve_token_endpoint(&self, spec: &OAuthExtensionSpec) -> Result<String> {
        // If token_endpoint is provided in spec, use it
        if let Some(ref endpoint) = spec.token_endpoint {
            if !endpoint.is_empty() {
                return Ok(endpoint.clone());
            }
        }

        // Fetch from OIDC discovery
        let discovery_url = format!(
            "{}/.well-known/openid-configuration",
            spec.issuer_url.trim_end_matches('/')
        );

        let response = self
            .http_client
            .get(&discovery_url)
            .send()
            .await
            .context("Failed to fetch OIDC discovery")?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "OIDC discovery failed with status {}",
                response.status()
            ));
        }

        #[derive(serde::Deserialize)]
        struct DiscoveryDoc {
            token_endpoint: Option<String>,
        }

        let discovery: DiscoveryDoc = response
            .json()
            .await
            .context("Failed to parse OIDC discovery")?;

        discovery
            .token_endpoint
            .ok_or_else(|| anyhow!("No token_endpoint in OIDC discovery"))
    }

    /// Resolve OAuth provider's client_secret from spec
    #[allow(dead_code)]
    pub async fn resolve_oauth_client_secret(
        &self,
        _project_id: Uuid,
        spec: &OAuthExtensionSpec,
    ) -> Result<String> {
        if let Some(ref encrypted) = spec.client_secret_encrypted {
            return self
                .encryption_provider
                .decrypt(encrypted)
                .await
                .context("Failed to decrypt OAuth client secret from spec");
        }

        Err(anyhow!(
            "No client_secret_encrypted configured. \
             If this extension uses the deprecated client_secret_ref, \
             it should be auto-migrated by the reconciler on the next cycle."
        ))
    }

    /// Exchange authorization code for tokens
    #[allow(dead_code)]
    pub async fn exchange_code_for_tokens(
        &self,
        spec: &OAuthExtensionSpec,
        client_secret: &str,
        authorization_code: &str,
        redirect_uri: &str,
    ) -> Result<TokenResponse> {
        // Resolve token endpoint from spec or OIDC discovery
        let token_endpoint = self.resolve_token_endpoint(spec).await?;

        debug!(
            "Exchanging authorization code for tokens with endpoint: {}",
            token_endpoint
        );

        let response = self
            .http_client
            .post(&token_endpoint)
            .header("Accept", "application/json")
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", authorization_code),
                ("client_id", &spec.client_id),
                ("client_secret", client_secret),
                ("redirect_uri", redirect_uri),
            ])
            .send()
            .await
            .context("Failed to send token exchange request")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unable to read error response".to_string());
            error!(
                "Token exchange failed with status {}: {}",
                status, error_text
            );
            return Err(anyhow!(
                "Token exchange failed with status {}: {}",
                status,
                error_text
            ));
        }

        let token_response: TokenResponse = response
            .json()
            .await
            .context("Failed to parse token response")?;

        Ok(token_response)
    }

    /// Refresh an expired token using refresh_token
    #[allow(dead_code)]
    pub async fn refresh_token(
        &self,
        spec: &OAuthExtensionSpec,
        client_secret: &str,
        refresh_token: &str,
    ) -> Result<TokenResponse> {
        // Resolve token endpoint from spec or OIDC discovery
        let token_endpoint = self.resolve_token_endpoint(spec).await?;

        debug!("Refreshing token with endpoint: {}", token_endpoint);

        let response = self
            .http_client
            .post(&token_endpoint)
            .header("Accept", "application/json")
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", &spec.client_id),
                ("client_secret", client_secret),
            ])
            .send()
            .await
            .context("Failed to send token refresh request")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unable to read error response".to_string());
            error!(
                "Token refresh failed with status {}: {}",
                status, error_text
            );
            return Err(anyhow!(
                "Token refresh failed with status {}: {}",
                status,
                error_text
            ));
        }

        let token_response: TokenResponse = response
            .json()
            .await
            .context("Failed to parse token refresh response")?;

        Ok(token_response)
    }

    /// Migrate an extension from client_secret_ref to client_secret_encrypted.
    /// If the extension has client_secret_ref set but no client_secret_encrypted,
    /// resolves the env var value, encrypts it, updates the spec, and deletes the env var.
    async fn migrate_client_secret_ref(
        &self,
        ext: &crate::db::models::ProjectExtension,
    ) -> Result<()> {
        let spec: OAuthExtensionSpec = serde_json::from_value(ext.spec.clone())
            .context("Failed to parse OAuth extension spec")?;

        // Only migrate if client_secret_ref is set and client_secret_encrypted is not
        let client_secret_ref = match (&spec.client_secret_ref, &spec.client_secret_encrypted) {
            (Some(ref_name), None) => ref_name.clone(),
            _ => return Ok(()),
        };

        info!(
            "Migrating OAuth extension {}/{} from client_secret_ref ({}) to client_secret_encrypted",
            ext.project_id, ext.extension, client_secret_ref
        );

        // Resolve the secret from the env var
        let client_secret = self
            .resolve_client_secret(ext.project_id, &client_secret_ref)
            .await
            .context("Failed to resolve client secret during migration")?;

        // Encrypt it
        let encrypted = self
            .encryption_provider
            .encrypt(&client_secret)
            .await
            .context("Failed to encrypt client secret during migration")?;

        // Update the spec: set client_secret_encrypted, client_secret_ref will be
        // dropped on serialization because it has skip_serializing
        let mut updated_spec = spec.clone();
        updated_spec.client_secret_encrypted = Some(encrypted);
        updated_spec.client_secret_ref = None;

        crate::db::extensions::update_spec(
            &self.db_pool,
            ext.project_id,
            &ext.extension,
            &serde_json::to_value(&updated_spec)
                .context("Failed to serialize updated OAuth spec")?,
        )
        .await
        .context("Failed to update OAuth extension spec during migration")?;

        // Best-effort cleanup: delete the legacy env var
        if let Err(e) = db_env_vars::delete_project_env_var(
            &self.db_pool,
            ext.project_id,
            &client_secret_ref,
            None,
        )
        .await
        {
            warn!(
                "Failed to delete migrated environment variable {}: {:?}",
                client_secret_ref, e
            );
        } else {
            info!(
                "Deleted migrated environment variable {} for project {}",
                client_secret_ref, ext.project_id
            );
        }

        info!(
            "Successfully migrated OAuth extension {}/{} to client_secret_encrypted",
            ext.project_id, ext.extension
        );

        Ok(())
    }

    /// Handle deletion of an OAuth extension
    async fn reconcile_deletion(&self, ext: crate::db::models::ProjectExtension) -> Result<()> {
        use crate::db::extensions as db_extensions;

        info!(
            "Reconciling deletion for OAuth extension: project_id={}, extension={}",
            ext.project_id, ext.extension
        );

        // Permanently delete the extension
        db_extensions::delete_permanently(&self.db_pool, ext.project_id, &ext.extension)
            .await
            .context("Failed to permanently delete OAuth extension")?;

        info!(
            "Permanently deleted OAuth extension: project_id={}, extension={}",
            ext.project_id, ext.extension
        );

        Ok(())
    }
}

#[async_trait]
impl Extension for OAuthProvider {
    fn extension_type(&self) -> &str {
        "oauth"
    }

    fn display_name(&self) -> &str {
        "Generic OAuth 2.0"
    }

    async fn validate_spec(&self, spec: &Value) -> Result<()> {
        // Parse and validate the OAuth extension spec
        let spec: OAuthExtensionSpec =
            serde_json::from_value(spec.clone()).context("Invalid OAuth extension spec format")?;

        // Validate required fields
        if spec.client_id.is_empty() {
            return Err(anyhow!("client_id is required"));
        }
        if spec.client_secret_encrypted.is_none() {
            return Err(anyhow!("client_secret_encrypted is required"));
        }
        if spec.issuer_url.is_empty() {
            return Err(anyhow!("issuer_url is required"));
        }
        if spec.scopes.is_empty() {
            return Err(anyhow!("at least one scope is required"));
        }

        // Validate issuer_url
        Url::parse(&spec.issuer_url).context("Invalid issuer_url URL")?;

        // Validate optional endpoint URLs if provided
        if let Some(ref auth_endpoint) = spec.authorization_endpoint {
            if !auth_endpoint.is_empty() {
                Url::parse(auth_endpoint).context("Invalid authorization_endpoint URL")?;
            }
        }
        if let Some(ref token_endpoint) = spec.token_endpoint {
            if !token_endpoint.is_empty() {
                Url::parse(token_endpoint).context("Invalid token_endpoint URL")?;
            }
        }

        Ok(())
    }

    async fn on_spec_updated(
        &self,
        old_spec: &Value,
        new_spec: &Value,
        project_id: Uuid,
        extension_name: &str,
        db_pool: &sqlx::PgPool,
    ) -> Result<()> {
        use crate::db::{extensions as db_extensions, projects};
        use chrono::Utc;

        // Get project name for redirect URI
        let project = projects::find_by_id(db_pool, project_id)
            .await?
            .ok_or_else(|| anyhow!("Project not found"))?;

        // Get current extension to check status
        let ext = db_extensions::find_by_project_and_name(db_pool, project_id, extension_name)
            .await?
            .ok_or_else(|| anyhow!("Extension not found"))?;

        // Parse current status
        let mut status: OAuthExtensionStatus =
            serde_json::from_value(ext.status).unwrap_or_default();

        // Generate Rise client credentials if they don't exist in status
        if status.rise_client_id.is_none() || status.rise_client_secret.is_none() {
            info!(
                "Generating Rise client credentials for OAuth extension {}/{}",
                project_id, extension_name
            );

            // Generate credentials with deterministic client ID: {project_name}-{extension_name}
            let rise_client_id = format!("{}-{}", project.name, extension_name);
            let rise_client_secret = generate_rise_client_secret();

            // Store credentials in status (plaintext)
            status.rise_client_id = Some(rise_client_id);
            status.rise_client_secret = Some(rise_client_secret);

            info!(
                "Generated Rise client credentials for OAuth extension {}/{}",
                project_id, extension_name
            );
        }

        // Always ensure redirect_uri is set/updated
        let redirect_uri = self.compute_redirect_uri(&project.name, extension_name);
        status.redirect_uri = Some(redirect_uri);

        // Initialize configured_at if not already set
        if status.configured_at.is_none() {
            status.configured_at = Some(Utc::now());
        }

        // Check if auth-sensitive fields changed
        let auth_sensitive_fields = [
            "client_id",
            "client_secret_encrypted",
            "authorization_endpoint",
            "token_endpoint",
            "scopes",
        ];

        let mut auth_changed = false;
        for field in &auth_sensitive_fields {
            if old_spec.get(field) != new_spec.get(field) {
                auth_changed = true;
                break;
            }
        }

        // Reset auth_verified when critical fields change
        if auth_changed {
            debug!(
                "OAuth spec changed for {}/{}, resetting auth_verified",
                project_id, extension_name
            );
            status.auth_verified = false;
        }

        // Save updated status
        db_extensions::update_status(
            db_pool,
            project_id,
            extension_name,
            &serde_json::to_value(&status)?,
        )
        .await?;

        Ok(())
    }

    fn start(&self, shutdown: CancellationToken) {
        let provider = self.clone();

        tokio::spawn(async move {
            info!("Starting OAuth provider reconciliation loop");

            let pool = provider.db_pool.clone();
            let result = with_leader_election(
                pool,
                "rise-ext-oauth",
                Uuid::new_v4(),
                std::time::Duration::from_secs(60),
                move |election| async move {
            loop {
                if !election.is_leader() {
                    tokio::select! {
                        _ = shutdown.cancelled() => break,
                        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                    }
                    continue;
                }

                match crate::db::extensions::list_by_extension_type(&provider.db_pool, "oauth")
                    .await
                {
                    Ok(extensions) => {
                        for ext in extensions {
                            // Handle deleted extensions
                            if ext.deleted_at.is_some() {
                                if !election.is_leader() {
                                    warn!("Lost leadership before OAuth deletion reconcile");
                                    break;
                                }
                                if let Err(e) = provider.reconcile_deletion(ext).await {
                                    error!("Failed to reconcile OAuth extension deletion: {:?}", e);
                                }
                                continue;
                            }

                            // Migrate client_secret_ref → client_secret_encrypted
                            if !election.is_leader() {
                                warn!("Lost leadership before OAuth migration reconcile");
                                break;
                            }
                            if let Err(e) = provider.migrate_client_secret_ref(&ext).await {
                                error!(
                                    "Failed to migrate client_secret_ref for OAuth extension {}/{}: {:?}",
                                    ext.project_id, ext.extension, e
                                );
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to list OAuth extensions: {:?}", e);
                    }
                }

                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                }
            }
            Ok(())
                },
            )
            .await;
            if let Err(e) = result {
                error!("OAuth extension reconciliation loop error: {:?}", e);
            }
        });
    }

    async fn before_deployment(
        &self,
        project_id: Uuid,
        _deployment_group: &str,
    ) -> Result<Vec<InjectedEnvVar>> {
        use crate::db::{extensions as db_extensions, projects as db_projects};

        // Find all OAuth extensions for this project
        let extensions = db_extensions::list_by_extension_type(&self.db_pool, "oauth")
            .await?
            .into_iter()
            .filter(|e| e.project_id == project_id && e.deleted_at.is_none())
            .collect::<Vec<_>>();

        if extensions.is_empty() {
            debug!(
                "No OAuth extensions found for project {}, skipping before_deployment hook",
                project_id
            );
            return Ok(vec![]);
        }

        // Get project info
        let project = db_projects::find_by_id(&self.db_pool, project_id)
            .await?
            .ok_or_else(|| anyhow!("Project not found"))?;

        let mut result = Vec::new();

        // Build env vars for each OAuth extension
        for ext in extensions {
            // Parse status to get credentials
            let status: OAuthExtensionStatus =
                serde_json::from_value(ext.status.clone()).unwrap_or_default();

            // Skip if credentials are not yet generated
            if status.rise_client_id.is_none() || status.rise_client_secret.is_none() {
                warn!(
                    "OAuth extension {} for project {} missing credentials, skipping env var injection",
                    ext.extension, project.name
                );
                continue;
            }

            let rise_client_id = status.rise_client_id.as_ref().unwrap();
            let rise_client_secret = status.rise_client_secret.as_ref().unwrap();

            // Normalize extension name for env var (uppercase, replace hyphens with underscores)
            // Pattern: {EXT_NAME}_{KEY} (e.g., OAUTH_DEX_CLIENT_ID)
            let normalized_name = ext.extension.to_uppercase().replace('-', "_");
            let client_id_key = format!("{}_CLIENT_ID", normalized_name);
            let client_secret_key = format!("{}_CLIENT_SECRET", normalized_name);
            let issuer_key = format!("{}_ISSUER", normalized_name);

            // Compute Rise issuer URL (OIDC proxy)
            let rise_issuer = format!(
                "{}/oidc/{}/{}",
                self.api_domain.trim_end_matches('/'),
                project.name,
                ext.extension
            );

            // CLIENT_ID (plaintext)
            result.push(InjectedEnvVar {
                key: client_id_key.clone(),
                value: InjectedEnvVarValue::Plain(rise_client_id.clone()),
            });

            // CLIENT_SECRET (encrypted secret, unprotected — safe for local dev)
            let encrypted_client_secret = self
                .encryption_provider
                .encrypt(rise_client_secret)
                .await
                .context(format!(
                    "Failed to encrypt {} for project {}",
                    client_secret_key, project.name
                ))?;

            result.push(InjectedEnvVar {
                key: client_secret_key.clone(),
                value: InjectedEnvVarValue::Secret {
                    decrypted: rise_client_secret.clone(),
                    encrypted: encrypted_client_secret,
                },
            });

            // ISSUER (plaintext)
            result.push(InjectedEnvVar {
                key: issuer_key.clone(),
                value: InjectedEnvVarValue::Plain(rise_issuer),
            });

            info!(
                "Prepared OAuth env vars for extension {} ({}, {}, {})",
                ext.extension, client_id_key, client_secret_key, issuer_key
            );
        }

        Ok(result)
    }

    fn format_status(&self, status: &Value) -> String {
        let status: OAuthExtensionStatus = match serde_json::from_value(status.clone()) {
            Ok(s) => s,
            Err(_) => return "Invalid status".to_string(),
        };

        if let Some(error) = &status.error {
            return format!("Error: {}", error);
        }

        if let Some(configured_at) = status.configured_at {
            if status.auth_verified {
                format!("Configured ({})", configured_at.format("%Y-%m-%d %H:%M:%S"))
            } else {
                "Waiting For Auth".to_string()
            }
        } else {
            "Not configured".to_string()
        }
    }

    fn description(&self) -> &str {
        "Generic OAuth 2.0 provider for user authentication"
    }

    fn documentation(&self) -> &str {
        r#"# Generic OAuth 2.0 Extension

This extension allows your application to authenticate end users via any OAuth 2.0 provider
(Snowflake, Google, GitHub, custom SSO, etc.) without managing OAuth client secrets locally.

## Configuration

The OAuth extension requires:

1. **OAuth Client Credentials**: Register an OAuth application with your provider to get:
   - `client_id`: OAuth client identifier
   - `client_secret`: OAuth client secret (stored encrypted via `rise encrypt`)

2. **Provider Endpoints**:
   - `authorization_endpoint`: OAuth provider's authorization URL
   - `token_endpoint`: OAuth provider's token URL

3. **Scopes**: OAuth scopes to request (provider-specific)

## Setup Steps

### Step 1: Encrypt Client Secret

```bash
ENCRYPTED=$(rise encrypt "your_client_secret")
```

### Step 2: Create OAuth Extension

```bash
rise extension create my-app oauth-provider \
  --type oauth \
  --spec '{
    "provider_name": "My OAuth Provider",
    "description": "OAuth authentication for my app",
    "client_id": "your_client_id",
    "client_secret_encrypted": "'"$ENCRYPTED"'",
    "authorization_endpoint": "https://provider.com/oauth/authorize",
    "token_endpoint": "https://provider.com/oauth/token",
    "scopes": ["openid", "email", "profile"]
  }'
```

## Provider-Specific Examples

### Snowflake OAuth

```bash
ENCRYPTED=$(rise encrypt "your_snowflake_client_secret")
rise extension create my-app oauth-snowflake \
  --type oauth \
  --spec '{
    "provider_name": "Snowflake Production",
    "description": "Snowflake OAuth for analytics",
    "client_id": "ABC123XYZ...",
    "client_secret_encrypted": "'"$ENCRYPTED"'",
    "authorization_endpoint": "https://myorg.snowflakecomputing.com/oauth/authorize",
    "token_endpoint": "https://myorg.snowflakecomputing.com/oauth/token-request",
    "scopes": ["refresh_token"]
  }'
```

### Google OAuth

```bash
ENCRYPTED=$(rise encrypt "your_google_client_secret")
rise extension create my-app oauth-google \
  --type oauth \
  --spec '{
    "provider_name": "Google",
    "description": "Sign in with Google",
    "client_id": "123456789.apps.googleusercontent.com",
    "client_secret_encrypted": "'"$ENCRYPTED"'",
    "authorization_endpoint": "https://accounts.google.com/o/oauth2/v2/auth",
    "token_endpoint": "https://oauth2.googleapis.com/token",
    "scopes": ["openid", "email", "profile"]
  }'
```

### GitHub OAuth

```bash
ENCRYPTED=$(rise encrypt "your_github_client_secret")
rise extension create my-app oauth-github \
  --type oauth \
  --spec '{
    "provider_name": "GitHub",
    "description": "Sign in with GitHub",
    "client_id": "Iv1.abc123...",
    "client_secret_encrypted": "'"$ENCRYPTED"'",
    "authorization_endpoint": "https://github.com/login/oauth/authorize",
    "token_endpoint": "https://github.com/login/oauth/access_token",
    "scopes": ["read:user", "user:email"]
  }'
```

## OAuth Flows

The extension supports RFC 6749-compliant OAuth flows with PKCE support:

### PKCE Flow (Recommended for SPAs)

For single-page applications, use PKCE (RFC 7636) to securely exchange authorization codes
for tokens. This prevents authorization code interception attacks.

**Frontend Integration (using oauth4webapi):**

```bash
npm install oauth4webapi
```

```javascript
import * as oauth from 'oauth4webapi';

// 1. Initiate OAuth login with PKCE
async function login() {
  const codeVerifier = oauth.generateRandomCodeVerifier();
  const codeChallenge = await oauth.calculatePKCECodeChallenge(codeVerifier);
  sessionStorage.setItem('pkce_verifier', codeVerifier);

  const authUrl = new URL(
    `https://api.rise.dev/oidc/my-app/oauth-provider/authorize`
  );
  authUrl.searchParams.set('code_challenge', codeChallenge);
  authUrl.searchParams.set('code_challenge_method', 'S256');

  window.location.href = authUrl;
}

// 2. Handle callback and exchange code for tokens
async function handleCallback() {
  const code = new URLSearchParams(window.location.search).get('code');
  const codeVerifier = sessionStorage.getItem('pkce_verifier');

  const tokens = await fetch(
    'https://api.rise.dev/oidc/my-app/oauth-provider/token',
    {
      method: 'POST',
      headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
      body: new URLSearchParams({
        grant_type: 'authorization_code',
        code: code,
        client_id: CONFIG.riseClientId,  // From build-time config
        code_verifier: codeVerifier
      })
    }
  ).then(r => r.json());

  sessionStorage.setItem('tokens', JSON.stringify(tokens));
  return tokens;
}
```

### Token Endpoint Flow (For Backend Apps)

For server-rendered applications, use the RFC 6749-compliant token endpoint with client credentials.
Rise auto-generates client credentials for each OAuth extension.

**Backend Integration (TypeScript/Express):**

```typescript
app.get('/oauth/callback', async (req, res) => {
  const { code } = req.query;

  const tokens = await fetch(
    'https://api.rise.dev/oidc/my-app/oauth-provider/token',
    {
      method: 'POST',
      headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
      body: new URLSearchParams({
        grant_type: 'authorization_code',
        code: code as string,
        client_id: process.env.OAUTH_PROVIDER_CLIENT_ID!,
        client_secret: process.env.OAUTH_PROVIDER_CLIENT_SECRET!
      })
    }
  ).then(r => r.json());

  req.session.tokens = tokens;
  res.redirect('/');
});
```

## Local Development

For local development, add `redirect_uri` parameter:

```javascript
// PKCE flow
authUrl.searchParams.set('redirect_uri', 'http://localhost:3000/callback');

// Backend flow
const authUrl = 'https://api.rise.dev/oidc/my-app/oauth-provider/authorize?redirect_uri=http://localhost:3000/oauth/callback';
```

## Security Features

- Client secrets stored encrypted in database
- PKCE (RFC 7636): Prevents authorization code interception for public clients
- Rise client credentials: Auto-generated per extension for token endpoint authentication
- Temporary single-use authorization codes with 5-minute TTL
- Redirect URI validation (localhost or project domains only)
- CSRF protection via state tokens
- Constant-time comparison for client secret and PKCE validation
- Stateless OAuth proxy: clients own their tokens after exchange
"#
    }

    fn spec_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": [
                "provider_name",
                "client_id",
                "issuer_url",
                "scopes"
            ],
            "properties": {
                "provider_name": {
                    "type": "string",
                    "description": "Display name for this OAuth provider",
                    "example": "Snowflake Production"
                },
                "description": {
                    "type": "string",
                    "description": "Description of this OAuth configuration",
                    "example": "Snowflake OAuth for analytics access"
                },
                "client_id": {
                    "type": "string",
                    "description": "OAuth client ID",
                    "example": "ABC123XYZ..."
                },
                "client_secret_encrypted": {
                    "type": "string",
                    "description": "Encrypted client secret (use 'rise encrypt' to encrypt)",
                    "example": "encrypted:..."
                },
                "issuer_url": {
                    "type": "string",
                    "format": "uri",
                    "description": "OIDC issuer URL. Endpoints are fetched via OIDC discovery. For non-OIDC providers, also set authorization_endpoint and token_endpoint.",
                    "example": "https://accounts.google.com"
                },
                "authorization_endpoint": {
                    "type": "string",
                    "format": "uri",
                    "description": "OAuth authorization URL (optional). If not provided, fetched from OIDC discovery.",
                    "example": "https://github.com/login/oauth/authorize"
                },
                "token_endpoint": {
                    "type": "string",
                    "format": "uri",
                    "description": "OAuth token URL (optional). If not provided, fetched from OIDC discovery.",
                    "example": "https://github.com/login/oauth/access_token"
                },
                "scopes": {
                    "type": "array",
                    "items": {
                        "type": "string"
                    },
                    "description": "OAuth scopes to request",
                    "example": ["openid", "email", "profile"]
                }
            }
        })
    }
}
