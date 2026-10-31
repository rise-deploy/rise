use crate::server::encryption::{EncryptionContext, EncryptionProvider};
use crate::server::extensions::providers::oauth::models::{
    ClientType, OAuthExtensionSpec, OAuthExtensionStatus, RefreshTokenMode,
    TokenEndpointAuthMethod, TokenResponse,
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
    pub ssrf_config: crate::server::ssrf::SsrfConfig,
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

pub fn oauth_client_secret_context(
    project_id: Uuid,
    extension_name: &str,
    spec: &OAuthExtensionSpec,
    token_endpoint: &str,
) -> Result<EncryptionContext> {
    let spec = serde_json::to_value(spec)?;
    Ok(EncryptionContext::extension(
        project_id,
        "oauth",
        extension_name,
        "/spec/client_secret_encrypted",
    )
    .with_binding(
        rise_deployment_spec::credential_binding::oauth_credential_binding(&spec, token_endpoint),
    ))
}

/// Build a POST to the upstream token endpoint with `params` as the form body,
/// authenticating as the extension's client per `spec.token_endpoint_auth_method`.
pub(super) fn upstream_token_request(
    http_client: &reqwest::Client,
    token_endpoint: &str,
    spec: &OAuthExtensionSpec,
    client_secret: &str,
    params: &[(&str, &str)],
) -> reqwest::RequestBuilder {
    let request = http_client
        .post(token_endpoint)
        .header(reqwest::header::ACCEPT, "application/json");
    match spec.token_endpoint_auth_method {
        TokenEndpointAuthMethod::ClientSecretPost => {
            let mut form = params.to_vec();
            form.push(("client_id", &spec.client_id));
            form.push(("client_secret", client_secret));
            request.form(&form)
        }
        TokenEndpointAuthMethod::ClientSecretBasic => request
            .header(
                reqwest::header::AUTHORIZATION,
                basic_client_credentials(&spec.client_id, client_secret),
            )
            .form(params),
    }
}

/// `Basic` credentials per RFC 6749 §2.3.1: both halves are form-urlencoded
/// before joining, so a `:` in the client ID cannot shift the split.
fn basic_client_credentials(client_id: &str, client_secret: &str) -> String {
    use base64::Engine;

    let encode =
        |value: &str| url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>();
    let credentials = format!("{}:{}", encode(client_id), encode(client_secret));
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(credentials)
    )
}

/// Rejects anything but an absolute `http(s)` URL with a host.
fn validate_http_url(field: &str, value: &str) -> Result<()> {
    let url = Url::parse(value).with_context(|| format!("Invalid {field} URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(anyhow!("{field} must be an absolute http(s) URL"));
    }
    Ok(())
}

/// Generate a secure random token for Rise client secret (32 bytes, base64url encoded)
// Only reached from `main` (through the extension registry), which is not a
// dead-code root when the binary is built for tests.
#[cfg_attr(test, allow(dead_code))]
fn generate_rise_client_secret() -> String {
    use base64::Engine;
    use rand::RngExt;

    let mut rng = rand::rng();
    let bytes: Vec<u8> = (0..32).map(|_| rng.random()).collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Gives the extension the Rise client credentials its client type needs: a
/// client ID (`{project}-{extension}`) always, and a client secret only for a
/// confidential client. Switching to `public` removes the secret, and switching
/// back generates a new one.
fn apply_client_credentials(
    status: &mut OAuthExtensionStatus,
    client_type: ClientType,
    project_name: &str,
    extension_name: &str,
    generate_secret: impl FnOnce() -> String,
) {
    if status.rise_client_id.is_none() {
        status.rise_client_id = Some(format!("{project_name}-{extension_name}"));
    }
    match client_type {
        ClientType::Confidential if status.rise_client_secret.is_none() => {
            info!(
                "Generating a Rise client secret for OAuth extension {}/{}",
                project_name, extension_name
            );
            status.rise_client_secret = Some(generate_secret());
        }
        ClientType::Confidential => {}
        ClientType::Public => status.rise_client_secret = None,
    }
}

/// The env vars an app gets for one OAuth extension: `{EXT}_CLIENT_ID`,
/// `{EXT}_ISSUER` and, for a confidential client, `{EXT}_CLIENT_SECRET`
/// (`{EXT}` is the extension name, uppercased with `-` as `_`). `None` while
/// the Rise client credentials the client type needs are not generated yet.
fn oauth_env_vars(
    api_domain: &str,
    project_name: &str,
    ext: &crate::db::models::ProjectExtension,
) -> Option<Vec<InjectedEnvVar>> {
    let status: OAuthExtensionStatus =
        serde_json::from_value(ext.status.clone()).unwrap_or_default();
    let client_type = serde_json::from_value::<OAuthExtensionSpec>(ext.spec.clone())
        .map(|spec| spec.client_type)
        .unwrap_or_default();
    let rise_client_id = status.rise_client_id?;
    let rise_client_secret = match client_type {
        ClientType::Confidential => Some(status.rise_client_secret?),
        ClientType::Public => None,
    };

    let normalized_name = ext.extension.to_uppercase().replace('-', "_");
    let mut vars = vec![InjectedEnvVar {
        key: format!("{normalized_name}_CLIENT_ID"),
        value: InjectedEnvVarValue::Plain(rise_client_id),
    }];
    // CLIENT_SECRET is retrievable; the deployment boundary encrypts its snapshot.
    if let Some(decrypted) = rise_client_secret {
        vars.push(InjectedEnvVar {
            key: format!("{normalized_name}_CLIENT_SECRET"),
            value: InjectedEnvVarValue::Secret { decrypted },
        });
    }
    // The relay's issuer URL for this extension.
    vars.push(InjectedEnvVar {
        key: format!("{normalized_name}_ISSUER"),
        value: InjectedEnvVarValue::Plain(format!(
            "{}/oidc/{}/{}",
            api_domain.trim_end_matches('/'),
            project_name,
            ext.extension
        )),
    });
    Some(vars)
}

impl OAuthProvider {
    pub fn new(config: OAuthProviderConfig) -> Self {
        Self {
            db_pool: config.db_pool,
            encryption_provider: config.encryption_provider,
            http_client: crate::server::ssrf::token_client(&config.ssrf_config),
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

    /// Decrypt only for the independently resolved extension and upstream destination.
    pub async fn resolve_oauth_client_secret(
        &self,
        project_id: Uuid,
        extension_name: &str,
        spec: &OAuthExtensionSpec,
        token_endpoint: &str,
    ) -> Result<String> {
        let encrypted = spec
            .client_secret_encrypted
            .as_deref()
            .context("client_secret_encrypted is required")?;
        self.encryption_provider
            .decrypt(
                encrypted,
                &oauth_client_secret_context(project_id, extension_name, spec, token_endpoint)?,
            )
            .await
            .context("OAuth credential does not match its resource, slot, or destination")
    }

    /// Send credentials only to the already validated and bound token endpoint.
    pub(super) async fn request_tokens(
        &self,
        spec: &OAuthExtensionSpec,
        client_secret: &str,
        token_endpoint: &str,
        params: &[(&str, &str)],
    ) -> Result<reqwest::Response> {
        let response = upstream_token_request(
            &self.http_client,
            token_endpoint,
            spec,
            client_secret,
            params,
        )
        .send()
        .await
        .context("Failed to send OAuth token request")?;
        anyhow::ensure!(!response.status().is_redirection(),
            "OAuth token endpoint redirects are not permitted; configure the canonical endpoint and reseal the credential");
        Ok(response)
    }

    /// Refresh an expired token using refresh_token
    #[allow(dead_code)]
    pub async fn refresh_token(
        &self,
        spec: &OAuthExtensionSpec,
        client_secret: &str,
        refresh_token: &str,
        token_endpoint: &str,
    ) -> Result<TokenResponse> {
        debug!("Refreshing token with endpoint: {}", token_endpoint);

        let response = self
            .request_tokens(
                spec,
                client_secret,
                token_endpoint,
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh_token),
                ],
            )
            .await?;

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

    /// Handle deletion of an OAuth extension
    // Only reached from `main` (through the extension registry), which is not a
    // dead-code root when the binary is built for tests.
    #[cfg_attr(test, allow(dead_code))]
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

        // The relay accepts only absolute http(s) URLs with a host, so a stored
        // spec never fails at request time.
        validate_http_url("issuer_url", &spec.issuer_url)?;
        // An empty endpoint counts as unset.
        if let Some(auth_endpoint) = spec.explicit_authorization_endpoint() {
            validate_http_url("authorization_endpoint", auth_endpoint)?;
        }
        if let Some(token_endpoint) = spec.explicit_token_endpoint() {
            validate_http_url("token_endpoint", token_endpoint)?;
        }

        // An entry may hold several space-separated scopes; repeats are
        // dropped when the relay is built.
        for scope in spec.scope_tokens() {
            if !super::relay::resolver::valid_scope_token(scope) {
                return Err(anyhow!(
                    "Invalid scope {scope:?}: OAuth scopes are printable ASCII without quotes or backslashes"
                ));
            }
        }

        if spec.client_type == ClientType::Public
            && spec.refresh_token_mode == Some(RefreshTokenMode::Sealed)
        {
            return Err(anyhow!(
                "refresh_token_mode: sealed is not available for public clients; public clients always use rotating refresh tokens"
            ));
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

        let client_type = serde_json::from_value::<OAuthExtensionSpec>(new_spec.clone())
            .context("Invalid OAuth extension spec format")?
            .client_type;
        apply_client_credentials(
            &mut status,
            client_type,
            &project.name,
            extension_name,
            generate_rise_client_secret,
        );

        // Always ensure redirect_uri is set/updated
        let redirect_uri = self.compute_redirect_uri(&project.name, extension_name);
        status.redirect_uri = Some(redirect_uri);

        // Initialize configured_at if not already set
        if status.configured_at.is_none() {
            status.configured_at = Some(Utc::now());
        }

        // Check if auth-sensitive fields changed
        let auth_sensitive_fields = [
            "client_type",
            "client_id",
            "client_secret_encrypted",
            "authorization_endpoint",
            "token_endpoint",
            "token_endpoint_auth_method",
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

    fn start(&self, shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
        let provider = self.clone();

        tokio::spawn(async move {
            info!("Starting OAuth provider reconciliation loop");

            let pool = provider.db_pool.clone();
            let result = with_leader_election(
                pool,
                "rise-ext-oauth",
                Uuid::new_v4(),
                std::time::Duration::from_secs(60),
                shutdown.clone(),
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
        })
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
            match oauth_env_vars(&self.api_domain, &project.name, &ext) {
                Some(vars) => {
                    info!(
                        "Prepared OAuth env vars for extension {} ({})",
                        ext.extension,
                        vars.iter()
                            .map(|var| var.key.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    result.extend(vars);
                }
                None => warn!(
                    "OAuth extension {} for project {} missing credentials, skipping env var injection",
                    ext.extension, project.name
                ),
            }
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
   - `issuer_url`: OIDC issuer; endpoints are discovered from its
     `.well-known/openid-configuration`
   - `authorization_endpoint` / `token_endpoint`: set both for providers
     without OIDC discovery (GitHub, Snowflake, Notion)

3. **Scopes**: OAuth scopes to request (provider-specific; empty for providers
   without scopes)

4. **Client authentication** (`token_endpoint_auth_method`, optional):
   `client_secret_post` (default) sends the client credentials in the token
   request body; `client_secret_basic` sends them as an HTTP Basic
   `Authorization` header. Use `client_secret_basic` for providers that reject
   body credentials with `invalid_client`, such as Notion.

## Setup

Create the project first. The extension can be sealed before it exists. Put the
upstream configuration in `oauth.json`:

```json
{
  "provider_name": "Google",
  "client_id": "123456789.apps.googleusercontent.com",
  "issuer_url": "https://accounts.google.com",
  "scopes": ["openid", "email", "profile"]
}
```

Seal the upstream secret for this project, extension name, slot, and OAuth
configuration, then create the extension:

```bash
ENCRYPTED=$(printf '%s' "your_client_secret" | rise encrypt \
  --project my-app --extension-type oauth --extension oauth-google \
  --slot /spec/client_secret_encrypted --oauth-spec oauth.json)

rise extension create oauth-google -p my-app --type oauth \
  --spec "$(jq --arg secret "$ENCRYPTED" '.client_secret_encrypted = $secret' oauth.json)"
```

`--oauth-spec` resolves the effective token endpoint through Rise and binds the
secret to the upstream client ID, issuer, explicit endpoints, authentication
method, and resolved token destination. Token redirects are refused; use the
provider's canonical final token URL. Re-enter and seal the secret when any
of those change. A ciphertext sealed for another project, extension, or slot
cannot be used here. Restoring the same extension name within the same project
permits reuse when its OAuth configuration matches; recreating the project does
not. The generic resource API carries ciphertext as an opaque value.

`rise encrypt` reads plaintext from stdin when its optional plaintext argument
is omitted. It permits 100 encryptions per hour per user and requires an
unrestricted login plus access to the enclosing project.

## Provider endpoints

Put each provider's client ID and endpoint configuration in `oauth.json` before sealing.

| Provider | Issuer | Explicit endpoints |
| --- | --- | --- |
| Google | `https://accounts.google.com` | Discovered |
| Snowflake | `https://ACCOUNT.snowflakecomputing.com` | `/oauth/authorize`, `/oauth/token-request` |
| GitHub | `https://github.com` | `/login/oauth/authorize`, `/login/oauth/access_token` |
| Notion | `https://api.notion.com` | `/v1/oauth/authorize?owner=user`, `/v1/oauth/token` |

Notion uses `token_endpoint_auth_method: "client_secret_basic"` and `scopes: []`.
For providers with explicit endpoints, use full URLs in `authorization_endpoint`
and `token_endpoint`.

## Client types

- `client_type: confidential` (default): server-side apps. Rise injects
  `{EXT}_CLIENT_ID`, `{EXT}_CLIENT_SECRET` and `{EXT}_ISSUER`. The app sends the
  client secret on every token request (code and refresh). PKCE is optional.
- `client_type: public`: browser and native apps that cannot keep a secret.
  Rise injects `{EXT}_CLIENT_ID` and `{EXT}_ISSUER` only. PKCE (S256) is
  required, and presenting a client secret is rejected with `invalid_client`.

Refresh tokens are Rise handles, never the provider's tokens:
`refresh_token_mode: sealed` (default for confidential clients) returns a
reusable sealed handle; `rotating` (always for public clients) returns a
single-use handle and a successor on every refresh.

## OAuth Flows

Every flow uses the endpoints below
`https://rise.example.com/oidc/my-app/oauth-provider`: `/authorize`, `/token`,
`/jwks` and `/.well-known/openid-configuration`. `redirect_uri` is required at
`/authorize` and must be sent again, unchanged, to `/token`. It must be on an
origin of the project's active deployments, or an `http` loopback URL
(`127.0.0.1`, `[::1]`, `localhost`, `*.localhost`, any port) for local
development.

### Public client (SPA) with PKCE

```javascript
import * as oauth from 'oauth4webapi';

const issuer = 'https://rise.example.com/oidc/my-app/oauth-provider';
const redirectUri = `${window.location.origin}/callback`;

async function login() {
  const codeVerifier = oauth.generateRandomCodeVerifier();
  sessionStorage.setItem('pkce_verifier', codeVerifier);
  const authUrl = new URL(`${issuer}/authorize`);
  authUrl.searchParams.set('redirect_uri', redirectUri);
  authUrl.searchParams.set('code_challenge', await oauth.calculatePKCECodeChallenge(codeVerifier));
  authUrl.searchParams.set('code_challenge_method', 'S256');
  window.location.href = authUrl;
}

async function handleCallback() {
  const code = new URLSearchParams(window.location.search).get('code');
  return fetch(`${issuer}/token`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body: new URLSearchParams({
      grant_type: 'authorization_code',
      code,
      redirect_uri: redirectUri,
      client_id: CONFIG.riseClientId,
      code_verifier: sessionStorage.getItem('pkce_verifier'),
    }),
  }).then(r => r.json());
}
```

### Confidential client (backend)

```typescript
const issuer = process.env.OAUTH_PROVIDER_ISSUER!;
const redirectUri = 'https://my-app.rise.example.com/oauth/callback';

app.get('/login', (req, res) => {
  const authUrl = new URL(`${issuer}/authorize`);
  authUrl.searchParams.set('redirect_uri', redirectUri);
  res.redirect(authUrl.toString());
});

app.get('/oauth/callback', async (req, res) => {
  const tokens = await fetch(`${issuer}/token`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body: new URLSearchParams({
      grant_type: 'authorization_code',
      code: req.query.code as string,
      redirect_uri: redirectUri,
      client_id: process.env.OAUTH_PROVIDER_CLIENT_ID!,
      client_secret: process.env.OAUTH_PROVIDER_CLIENT_SECRET!,
    }),
  }).then(r => r.json());
  req.session.tokens = tokens;
  res.redirect('/');
});
```

Refresh with `grant_type=refresh_token`, the latest `refresh_token`, and (for
confidential clients) `client_id` and `client_secret` again.

## Security Features

- Upstream client secrets stored encrypted and bound to their extension and token endpoint
- Authorization codes are sealed, single-use, and expire after 5 minutes
- PKCE (RFC 7636): required for public clients, enforced whenever used
- Redirect URIs limited to the project's deployment origins and loopback hosts
- Refresh tokens are Rise handles: sealed, or rotating with reuse detection
- Constant-time comparison of client credentials and PKCE verifiers
"#
    }

    fn spec_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": [
                "provider_name",
                "client_id",
                "issuer_url"
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
                    "description": "Context-bound client secret; seal with rise encrypt --project ... --extension-type oauth --extension ... --slot /spec/client_secret_encrypted --oauth-spec oauth.json",
                    "example": "rise:enc:v2:..."
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
                "token_endpoint_auth_method": {
                    "type": "string",
                    "enum": ["client_secret_post", "client_secret_basic"],
                    "default": "client_secret_post",
                    "description": "How Rise authenticates to the token endpoint: client_secret_post sends client_id and client_secret in the form body; client_secret_basic sends them as an HTTP Basic Authorization header (required by e.g. Notion).",
                    "example": "client_secret_basic"
                },
                "scopes": {
                    "type": "array",
                    "items": {
                        "type": "string"
                    },
                    "description": "OAuth scopes to request, one scope per entry. Apps may request a subset with the scope parameter, never more. Leave empty for providers without scopes (e.g. Notion).",
                    "example": ["openid", "email", "profile"]
                },
                "client_type": {
                    "type": "string",
                    "enum": ["confidential", "public"],
                    "default": "confidential",
                    "description": "confidential: the app holds the Rise client secret and sends it on every token request. public: browser or native apps without a secret; PKCE is required and refresh tokens always rotate.",
                    "example": "public"
                },
                "refresh_token_mode": {
                    "type": "string",
                    "enum": ["sealed", "rotating"],
                    "description": "How refresh tokens are handed to the app. sealed (default for confidential clients): a reusable sealed handle. rotating (always for public clients): a single-use handle, replaced on every refresh.",
                    "example": "rotating"
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(method: TokenEndpointAuthMethod) -> OAuthExtensionSpec {
        serde_json::from_value(json!({
            "provider_name": "Test",
            "client_id": "id:with space",
            "issuer_url": "https://idp.example.com",
            "token_endpoint_auth_method": method,
        }))
        .unwrap()
    }

    fn build(method: TokenEndpointAuthMethod) -> reqwest::Request {
        upstream_token_request(
            &reqwest::Client::new(),
            "https://idp.example.com/token",
            &spec(method),
            "s3cr&t",
            &[("grant_type", "refresh_token"), ("refresh_token", "rt")],
        )
        .build()
        .unwrap()
    }

    fn body(request: &reqwest::Request) -> String {
        String::from_utf8(request.body().unwrap().as_bytes().unwrap().to_vec()).unwrap()
    }

    #[test]
    fn client_secret_post_sends_credentials_in_the_form_body() {
        let request = build(TokenEndpointAuthMethod::ClientSecretPost);

        assert!(request
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .is_none());
        assert_eq!(
            body(&request),
            "grant_type=refresh_token&refresh_token=rt&client_id=id%3Awith+space&client_secret=s3cr%26t"
        );
    }

    #[test]
    fn client_secret_basic_sends_urlencoded_credentials_in_the_header_only() {
        use base64::Engine;

        let request = build(TokenEndpointAuthMethod::ClientSecretBasic);

        let header = request.headers()[reqwest::header::AUTHORIZATION]
            .to_str()
            .unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(header.strip_prefix("Basic ").unwrap())
            .unwrap();
        assert_eq!(decoded, b"id%3Awith+space:s3cr%26t");
        assert_eq!(body(&request), "grant_type=refresh_token&refresh_token=rt");
    }

    #[test]
    fn auth_method_defaults_to_client_secret_post_and_is_omitted_when_default() {
        let spec: OAuthExtensionSpec = serde_json::from_value(json!({
            "provider_name": "Test",
            "client_id": "id",
            "issuer_url": "https://idp.example.com",
        }))
        .unwrap();

        assert_eq!(
            spec.token_endpoint_auth_method,
            TokenEndpointAuthMethod::ClientSecretPost
        );
        assert!(spec.scopes.is_empty());
        assert!(serde_json::to_value(&spec)
            .unwrap()
            .get("token_endpoint_auth_method")
            .is_none());
    }
}

#[cfg(test)]
mod client_type_tests {
    use super::*;

    fn provider() -> OAuthProvider {
        use crate::server::encryption::providers::local::LocalEncryptionProvider;
        OAuthProvider::new(OAuthProviderConfig {
            db_pool: sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://test:test@localhost/test")
                .unwrap(),
            encryption_provider: Arc::new(
                LocalEncryptionProvider::new("AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=")
                    .unwrap(),
            ),
            ssrf_config: crate::server::ssrf::SsrfConfig::default(),
            api_domain: "https://rise.example.com".into(),
        })
    }

    fn valid_spec() -> Value {
        json!({
            "provider_name": "Test",
            "client_id": "client",
            "client_secret_encrypted": "rise:enc:v2:x",
            "issuer_url": "https://idp.example.com",
            "scopes": ["openid", "email"],
        })
    }

    #[test]
    fn specs_without_the_new_fields_are_confidential_and_sealed_and_reserialize_unchanged() {
        let stored = valid_spec();
        let spec: OAuthExtensionSpec = serde_json::from_value(stored.clone()).unwrap();
        assert_eq!(spec.client_type, ClientType::Confidential);
        assert_eq!(spec.refresh_token_mode, None);
        assert_eq!(
            spec.effective_refresh_token_mode(),
            RefreshTokenMode::Sealed
        );
        let reserialized = serde_json::to_value(&spec).unwrap();
        assert!(reserialized.get("client_type").is_none(), "{reserialized}");
        assert!(
            reserialized.get("refresh_token_mode").is_none(),
            "{reserialized}"
        );
        assert_eq!(
            rise_deployment_spec::credential_binding::oauth_credential_binding(
                &reserialized,
                "https://idp.example.com/token"
            ),
            rise_deployment_spec::credential_binding::oauth_credential_binding(
                &stored,
                "https://idp.example.com/token"
            )
        );

        let mut public = stored.clone();
        public["client_type"] = json!("public");
        let spec: OAuthExtensionSpec = serde_json::from_value(public).unwrap();
        assert_eq!(
            spec.effective_refresh_token_mode(),
            RefreshTokenMode::Rotating
        );
        let mut rotating = stored;
        rotating["refresh_token_mode"] = json!("rotating");
        let spec: OAuthExtensionSpec = serde_json::from_value(rotating).unwrap();
        assert_eq!(spec.client_type, ClientType::Confidential);
        assert_eq!(
            spec.effective_refresh_token_mode(),
            RefreshTokenMode::Rotating
        );
    }

    #[tokio::test]
    async fn validate_spec_rejects_what_the_relay_cannot_serve() {
        let provider = provider();
        provider.validate_spec(&valid_spec()).await.unwrap();
        let mut public_rotating = valid_spec();
        public_rotating["client_type"] = json!("public");
        public_rotating["refresh_token_mode"] = json!("rotating");
        provider.validate_spec(&public_rotating).await.unwrap();
        // Legacy specs stay valid: empty endpoints count as unset, and entries
        // may hold several scopes or repeat one.
        let mut legacy = valid_spec();
        legacy["authorization_endpoint"] = json!("");
        legacy["token_endpoint"] = json!("");
        legacy["scopes"] = json!(["openid email", "openid", "", "session:role:ANALYST"]);
        provider.validate_spec(&legacy).await.unwrap();

        let invalid = [
            (
                "client_type",
                json!("public"),
                Some(("refresh_token_mode", json!("sealed"))),
            ),
            ("issuer_url", json!("not a url"), None),
            ("issuer_url", json!("ftp://idp.example.com"), None),
            ("issuer_url", json!("mailto:a@example.com"), None),
            ("token_endpoint", json!("idp.example.com/token"), None),
            ("authorization_endpoint", json!("javascript:alert(1)"), None),
            ("scopes", json!(["a\"b"]), None),
            ("scopes", json!(["openid caf\u{e9}"]), None),
            ("client_type", json!("unknown"), None),
        ];
        for (field, value, also) in invalid {
            let mut spec = valid_spec();
            spec[field] = value.clone();
            if let Some((field, value)) = also {
                spec[field] = value;
            }
            assert!(
                provider.validate_spec(&spec).await.is_err(),
                "{field}: {value}"
            );
        }
    }

    #[test]
    fn credentials_follow_the_client_type() {
        let mut status = OAuthExtensionStatus::default();
        apply_client_credentials(&mut status, ClientType::Confidential, "demo", "ext", || {
            "first".into()
        });
        assert_eq!(status.rise_client_id.as_deref(), Some("demo-ext"));
        assert_eq!(status.rise_client_secret.as_deref(), Some("first"));

        // Existing credentials are kept.
        apply_client_credentials(&mut status, ClientType::Confidential, "demo", "ext", || {
            "second".into()
        });
        assert_eq!(status.rise_client_secret.as_deref(), Some("first"));

        // Public clients have no secret.
        apply_client_credentials(
            &mut status,
            ClientType::Public,
            "demo",
            "ext",
            || unreachable!(),
        );
        assert_eq!(status.rise_client_id.as_deref(), Some("demo-ext"));
        assert_eq!(status.rise_client_secret, None);

        // Switching back generates a new one.
        apply_client_credentials(&mut status, ClientType::Confidential, "demo", "ext", || {
            "third".into()
        });
        assert_eq!(status.rise_client_secret.as_deref(), Some("third"));
    }

    fn extension(spec: Value, status: Value) -> crate::db::models::ProjectExtension {
        crate::db::models::ProjectExtension {
            project_id: Uuid::new_v4(),
            extension: "oauth-google".into(),
            extension_type: "oauth".into(),
            spec,
            status,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            deleted_at: None,
        }
    }

    fn keys(vars: &[InjectedEnvVar]) -> Vec<&str> {
        vars.iter().map(|var| var.key.as_str()).collect()
    }

    #[test]
    fn env_vars_carry_the_client_secret_only_for_confidential_clients() {
        let creds = json!({"rise_client_id": "demo-oauth-google", "rise_client_secret": "s"});
        let vars = oauth_env_vars(
            "https://rise.example.com/",
            "demo",
            &extension(valid_spec(), creds.clone()),
        )
        .unwrap();
        assert_eq!(
            keys(&vars),
            [
                "OAUTH_GOOGLE_CLIENT_ID",
                "OAUTH_GOOGLE_CLIENT_SECRET",
                "OAUTH_GOOGLE_ISSUER"
            ]
        );
        assert!(matches!(
            &vars[2].value,
            InjectedEnvVarValue::Plain(issuer) if issuer == "https://rise.example.com/oidc/demo/oauth-google"
        ));

        let mut public = valid_spec();
        public["client_type"] = json!("public");
        let vars = oauth_env_vars(
            "https://rise.example.com",
            "demo",
            &extension(
                public.clone(),
                json!({"rise_client_id": "demo-oauth-google"}),
            ),
        )
        .unwrap();
        assert_eq!(
            keys(&vars),
            ["OAUTH_GOOGLE_CLIENT_ID", "OAUTH_GOOGLE_ISSUER"]
        );
        // A secret left in the status is not injected for a public client.
        let vars = oauth_env_vars(
            "https://rise.example.com",
            "demo",
            &extension(public, creds),
        )
        .unwrap();
        assert_eq!(
            keys(&vars),
            ["OAUTH_GOOGLE_CLIENT_ID", "OAUTH_GOOGLE_ISSUER"]
        );

        // A confidential client without its secret is skipped.
        assert!(oauth_env_vars(
            "https://rise.example.com",
            "demo",
            &extension(valid_spec(), json!({"rise_client_id": "demo-oauth-google"})),
        )
        .is_none());
    }
}

#[cfg(test)]
mod credential_scope_tests {
    use super::*;
    use crate::server::encryption::providers::local::LocalEncryptionProvider;

    #[tokio::test]
    async fn oauth_credentials_reject_resource_client_and_effective_destination_substitution() {
        let encryption = Arc::new(
            LocalEncryptionProvider::new("AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=").unwrap(),
        );
        let provider = OAuthProvider::new(OAuthProviderConfig {
            db_pool: sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://test:test@localhost/test")
                .unwrap(),
            encryption_provider: encryption.clone(),
            ssrf_config: crate::server::ssrf::SsrfConfig::default(),
            api_domain: "https://rise.example.com".into(),
        });
        let project = Uuid::new_v4();
        let mut spec: OAuthExtensionSpec = serde_json::from_value(json!({
            "provider_name":"Example", "client_id":"client", "issuer_url":"https://idp.example.com",
            "authorization_endpoint":"https://idp.example.com/auth", "token_endpoint":"https://idp.example.com/token"
        })).unwrap();
        let endpoint = spec.token_endpoint.clone().unwrap();
        spec.client_secret_encrypted = Some(
            encryption
                .encrypt(
                    "secret",
                    &oauth_client_secret_context(project, "login", &spec, &endpoint).unwrap(),
                )
                .await
                .unwrap(),
        );
        // Callback and refresh both resolve against the same effective endpoint.
        assert_eq!(
            provider
                .resolve_oauth_client_secret(project, "login", &spec, &endpoint)
                .await
                .unwrap(),
            "secret"
        );
        assert!(provider
            .resolve_oauth_client_secret(Uuid::new_v4(), "login", &spec, &endpoint)
            .await
            .is_err());
        assert!(provider
            .resolve_oauth_client_secret(project, "other", &spec, &endpoint)
            .await
            .is_err());
        assert!(provider
            .resolve_oauth_client_secret(project, "login", &spec, "https://other.example.com/token")
            .await
            .is_err());
        for field in [
            "client_id",
            "issuer_url",
            "authorization_endpoint",
            "token_endpoint",
            "token_endpoint_auth_method",
        ] {
            let mut changed = serde_json::to_value(&spec).unwrap();
            changed[field] = json!(if field == "token_endpoint_auth_method" {
                "client_secret_basic"
            } else {
                "https://attacker.example.com/"
            });
            let changed: OAuthExtensionSpec = serde_json::from_value(changed).unwrap();
            assert!(
                provider
                    .resolve_oauth_client_secret(project, "login", &changed, &endpoint)
                    .await
                    .is_err(),
                "{field}"
            );
        }
        let mut ref_only = serde_json::to_value(spec).unwrap();
        ref_only
            .as_object_mut()
            .unwrap()
            .remove("client_secret_encrypted");
        ref_only["client_secret_ref"] = json!("PROTECTED_OTHER_SECRET");
        assert!(provider.validate_spec(&ref_only).await.is_err());
    }
    struct HttpsTokenServer {
        url: String,
        requests: Arc<std::sync::Mutex<Vec<String>>>,
        stop: Arc<std::sync::atomic::AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for HttpsTokenServer {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn test_tls_acceptor() -> (openssl::ssl::SslAcceptor, reqwest::Certificate) {
        use openssl::{
            asn1::Asn1Time,
            bn::BigNum,
            hash::MessageDigest,
            pkey::PKey,
            rsa::Rsa,
            ssl::{SslAcceptor, SslMethod},
            x509::{
                extension::{BasicConstraints, ExtendedKeyUsage, KeyUsage, SubjectAlternativeName},
                X509NameBuilder, X509,
            },
        };
        let ca_key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let mut ca_name = X509NameBuilder::new().unwrap();
        ca_name
            .append_entry_by_text("CN", "Rise OAuth test CA")
            .unwrap();
        let ca_name = ca_name.build();
        let mut ca = X509::builder().unwrap();
        ca.set_version(2).unwrap();
        ca.set_serial_number(&BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap())
            .unwrap();
        ca.set_subject_name(&ca_name).unwrap();
        ca.set_issuer_name(&ca_name).unwrap();
        ca.set_pubkey(&ca_key).unwrap();
        let before = Asn1Time::days_from_now(0).unwrap();
        let after = Asn1Time::days_from_now(1).unwrap();
        ca.set_not_before(&before).unwrap();
        ca.set_not_after(&after).unwrap();
        ca.append_extension(BasicConstraints::new().critical().ca().build().unwrap())
            .unwrap();
        ca.append_extension(
            KeyUsage::new()
                .critical()
                .key_cert_sign()
                .crl_sign()
                .build()
                .unwrap(),
        )
        .unwrap();
        ca.sign(&ca_key, MessageDigest::sha256()).unwrap();
        let ca = ca.build();
        let root = reqwest::Certificate::from_der(&ca.to_der().unwrap()).unwrap();
        let leaf_key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let mut leaf_name = X509NameBuilder::new().unwrap();
        leaf_name.append_entry_by_text("CN", "localhost").unwrap();
        let leaf_name = leaf_name.build();
        let mut leaf = X509::builder().unwrap();
        leaf.set_version(2).unwrap();
        leaf.set_serial_number(&BigNum::from_u32(2).unwrap().to_asn1_integer().unwrap())
            .unwrap();
        leaf.set_subject_name(&leaf_name).unwrap();
        leaf.set_issuer_name(&ca_name).unwrap();
        leaf.set_pubkey(&leaf_key).unwrap();
        leaf.set_not_before(&before).unwrap();
        leaf.set_not_after(&after).unwrap();
        leaf.append_extension(BasicConstraints::new().critical().build().unwrap())
            .unwrap();
        leaf.append_extension(
            KeyUsage::new()
                .digital_signature()
                .key_encipherment()
                .build()
                .unwrap(),
        )
        .unwrap();
        leaf.append_extension(ExtendedKeyUsage::new().server_auth().build().unwrap())
            .unwrap();
        let san = SubjectAlternativeName::new()
            .dns("localhost")
            .ip("127.0.0.1")
            .build(&leaf.x509v3_context(Some(&ca), None))
            .unwrap();
        leaf.append_extension(san).unwrap();
        leaf.sign(&ca_key, MessageDigest::sha256()).unwrap();
        let mut acceptor = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
        acceptor.set_certificate(&leaf.build()).unwrap();
        acceptor.set_private_key(&leaf_key).unwrap();
        acceptor.add_extra_chain_cert(ca).unwrap();
        acceptor.check_private_key().unwrap();
        (acceptor.build(), root)
    }

    fn https_token_server(
        acceptor: Arc<openssl::ssl::SslAcceptor>,
        redirect: Option<String>,
    ) -> HttpsTokenServer {
        use std::{
            io::{Read, Write},
            sync::atomic::{AtomicBool, Ordering},
            time::{Duration, Instant},
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!(
            "https://localhost:{}",
            listener.local_addr().unwrap().port()
        );
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = std::thread::spawn({
            let stop = stop.clone();
            let requests = requests.clone();
            move || {
                let deadline = Instant::now() + Duration::from_secs(30);
                while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
                    let stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(error) => panic!("HTTPS fixture accept: {error}"),
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let mut stream = acceptor.accept(stream).unwrap();
                    let mut request = Vec::new();
                    let mut buffer = [0; 4096];
                    loop {
                        let read = stream.read(&mut buffer).unwrap();
                        if read == 0 {
                            break;
                        }
                        request.extend_from_slice(&buffer[..read]);
                        if let Some(header_end) =
                            request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                        {
                            let headers = String::from_utf8_lossy(&request[..header_end]);
                            let length = headers
                                .lines()
                                .find_map(|line| {
                                    line.split_once(':').and_then(|(key, value)| {
                                        key.eq_ignore_ascii_case("content-length")
                                            .then(|| value.trim().parse::<usize>().unwrap())
                                    })
                                })
                                .unwrap_or(0);
                            if request.len() >= header_end + 4 + length {
                                break;
                            }
                        }
                    }
                    let request = String::from_utf8(request).unwrap();
                    let path = request
                        .lines()
                        .next()
                        .unwrap()
                        .split_whitespace()
                        .nth(1)
                        .unwrap();
                    let response = if let Some(target) = redirect
                        .as_ref()
                        .filter(|_| matches!(path, "/307" | "/308"))
                    {
                        let status = &path[1..];
                        format!("HTTP/1.1 {status} Redirect\r\nLocation: {target}/direct\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    } else {
                        let body = r#"{"access_token":"access","token_type":"Bearer","refresh_token":"refresh"}"#;
                        format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                    };
                    requests.lock().unwrap().push(request);
                    stream.write_all(response.as_bytes()).unwrap();
                }
            }
        });
        HttpsTokenServer {
            url,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    #[tokio::test]
    async fn code_exchange_and_refresh_never_send_bound_credentials_through_https_redirects() {
        let (acceptor, root) = test_tls_acceptor();
        let acceptor = Arc::new(acceptor);
        let sink = https_token_server(acceptor.clone(), None);
        let origin = https_token_server(acceptor, Some(sink.url.clone()));
        let ssrf = crate::server::ssrf::SsrfConfig {
            trusted_hosts: vec!["localhost".into()],
            ..Default::default()
        };
        let encryption = Arc::new(
            LocalEncryptionProvider::new("AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=").unwrap(),
        );
        let mut provider = OAuthProvider::new(OAuthProviderConfig {
            db_pool: sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://test:test@localhost/test")
                .unwrap(),
            encryption_provider: encryption.clone(),
            ssrf_config: ssrf.clone(),
            api_domain: "https://rise.example.com".into(),
        });
        // The fixture uses the production redirect policy with its own CA and no local proxy.
        provider.http_client = crate::server::ssrf::token_client_builder(&ssrf)
            .no_proxy()
            .tls_certs_only([root.clone()])
            .build()
            .unwrap();
        let project = Uuid::new_v4();
        let mut spec: OAuthExtensionSpec = serde_json::from_value(json!({"provider_name":"Fixture", "client_id":"client", "issuer_url":origin.url, "authorization_endpoint":format!("{}/authorize", origin.url), "token_endpoint":format!("{}/307", origin.url)})).unwrap();
        // HTTPS-to-HTTPS 307 is followed by the discovery client's policy. HTTP would be refused.
        let discovery_client = crate::server::ssrf::safe_client_builder(&ssrf)
            .no_proxy()
            .tls_certs_only([root])
            .build()
            .unwrap();
        let response = upstream_token_request(
            &discovery_client,
            spec.token_endpoint.as_deref().unwrap(),
            &spec,
            "bound-secret",
            &[("grant_type", "authorization_code")],
        )
        .send()
        .await
        .unwrap();
        assert!(response.status().is_success());
        assert!(sink.requests.lock().unwrap()[0].contains("client_secret=bound-secret"));
        sink.requests.lock().unwrap().clear();

        for path in ["/307", "/308", "/direct"] {
            let received_before = origin.requests.lock().unwrap().len();
            let endpoint = format!("{}{path}", origin.url);
            spec.token_endpoint = Some(endpoint.clone());
            crate::server::ssrf::validate_url(&endpoint, &ssrf)
                .await
                .unwrap();
            spec.client_secret_encrypted = Some(
                encryption
                    .encrypt(
                        "bound-secret",
                        &oauth_client_secret_context(project, "login", &spec, &endpoint).unwrap(),
                    )
                    .await
                    .unwrap(),
            );
            let secret = provider
                .resolve_oauth_client_secret(project, "login", &spec, &endpoint)
                .await
                .unwrap();
            let exchange = provider
                .request_tokens(
                    &spec,
                    &secret,
                    &endpoint,
                    &[
                        ("grant_type", "authorization_code"),
                        ("code", "code"),
                        ("code_verifier", "verifier"),
                    ],
                )
                .await;
            let refresh = provider
                .refresh_token(&spec, &secret, "refresh-token", &endpoint)
                .await;
            if path == "/direct" {
                assert!(exchange.unwrap().status().is_success());
                assert_eq!(refresh.unwrap().access_token, "access");
            } else {
                assert!(exchange
                    .unwrap_err()
                    .to_string()
                    .contains("redirects are not permitted"));
                assert!(refresh
                    .unwrap_err()
                    .to_string()
                    .contains("redirects are not permitted"));
            }
            let received = origin.requests.lock().unwrap();
            assert_eq!(received.len(), received_before + 2);
            assert!(received[received_before..]
                .iter()
                .all(|request| request.contains("client_secret=bound-secret")));
            drop(received);
            assert!(
                sink.requests.lock().unwrap().is_empty(),
                "token credentials must not reach a second HTTPS origin"
            );
        }
    }
}
