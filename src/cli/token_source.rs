//! Pluggable bearer-token sources for authenticating the CLI to the backend.
//!
//! In CI the backend token is often a short-lived OIDC token (GitHub Actions
//! ID tokens last ~5 minutes and cannot be extended). A long deploy that
//! builds and pushes several containers can outlast a single pre-minted token,
//! so instead of capturing one fixed token string we thread a [`TokenProvider`]
//! through the deployment path and resolve a fresh token immediately before
//! each request. Implementations cache the minted token and re-mint lazily as
//! it nears expiry (or on an explicit [`TokenSource::refresh`] after a 401).

use anyhow::{bail, Context, Result};
use chrono::Utc;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::login::token_utils::read_token_exp;

/// Re-mint a cached token once it is within this many seconds of expiry, so a
/// request issued right after [`TokenSource::token`] still has comfortable
/// headroom against clock skew and request latency.
const EXPIRY_SKEW_SECONDS: i64 = 60;

/// A source of bearer tokens for backend authentication.
#[async_trait::async_trait]
pub trait TokenSource: Send + Sync {
    /// Return a currently-valid bearer token, re-minting lazily if the cached
    /// one is missing or within the skew window of its `exp`.
    async fn token(&self) -> Result<String>;

    /// Force a re-mint, discarding any cached token. A no-op (returns the same
    /// value) for static sources.
    async fn refresh(&self) -> Result<String>;

    /// Short human label for diagnostics/tests.
    fn describe(&self) -> &'static str;
}

/// Cheaply-cloneable handle threaded through the deployment path.
pub type TokenProvider = Arc<dyn TokenSource>;

/// A token cached alongside its decoded `exp` (seconds since epoch). `None`
/// expiry means the token is opaque (not a decodable JWT) and must be re-minted
/// on every use to stay safe.
#[derive(Clone)]
struct CachedToken {
    value: String,
    exp: Option<i64>,
}

impl CachedToken {
    fn new(value: String) -> Self {
        let exp = read_token_exp(&value);
        Self { value, exp }
    }

    fn is_fresh(&self, now_secs: i64) -> bool {
        match self.exp {
            Some(exp) => exp - EXPIRY_SKEW_SECONDS > now_secs,
            None => false,
        }
    }
}

/// A fixed token (from `RISE_TOKEN` or the stored login config). Never expires
/// from the CLI's perspective; `refresh` returns the same value.
pub struct StaticToken {
    value: String,
}

impl StaticToken {
    pub fn new(value: String) -> Self {
        Self { value }
    }
}

#[async_trait::async_trait]
impl TokenSource for StaticToken {
    async fn token(&self) -> Result<String> {
        Ok(self.value.clone())
    }
    async fn refresh(&self) -> Result<String> {
        Ok(self.value.clone())
    }
    fn describe(&self) -> &'static str {
        "static token"
    }
}

/// Mints GitHub Actions OIDC ID tokens on demand via the runner's token
/// endpoint (`ACTIONS_ID_TOKEN_REQUEST_URL` / `ACTIONS_ID_TOKEN_REQUEST_TOKEN`).
pub struct GithubActionsOidc {
    http: reqwest::Client,
    request_url: String,
    request_token: String,
    audience: String,
    cache: Mutex<Option<CachedToken>>,
}

impl GithubActionsOidc {
    pub fn new(
        http: reqwest::Client,
        request_url: String,
        request_token: String,
        audience: String,
    ) -> Self {
        Self {
            http,
            request_url,
            request_token,
            audience,
            cache: Mutex::new(None),
        }
    }

    async fn mint(&self) -> Result<CachedToken> {
        // The request URL already carries query params; append the audience.
        let sep = if self.request_url.contains('?') {
            '&'
        } else {
            '?'
        };
        let url = format!(
            "{}{}audience={}",
            self.request_url,
            sep,
            urlencoding::encode(&self.audience)
        );
        let response = self
            .http
            .get(&url)
            .bearer_auth(&self.request_token)
            .send()
            .await
            .context("Failed to request GitHub Actions OIDC token")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!(
                "Failed to mint GitHub Actions OIDC token (audience '{}'): {} {}. \
                 Ensure the workflow grants 'id-token: write' permission.",
                self.audience,
                status,
                body
            );
        }
        #[derive(serde::Deserialize)]
        struct OidcResponse {
            value: String,
        }
        let parsed: OidcResponse = response
            .json()
            .await
            .context("Failed to parse GitHub Actions OIDC token response")?;
        if parsed.value.trim().is_empty() {
            bail!("GitHub Actions OIDC endpoint returned an empty token");
        }
        Ok(CachedToken::new(parsed.value))
    }
}

#[async_trait::async_trait]
impl TokenSource for GithubActionsOidc {
    async fn token(&self) -> Result<String> {
        let mut guard = self.cache.lock().await;
        let now = Utc::now().timestamp();
        if let Some(cached) = guard.as_ref() {
            if cached.is_fresh(now) {
                return Ok(cached.value.clone());
            }
        }
        let fresh = self.mint().await?;
        let value = fresh.value.clone();
        *guard = Some(fresh);
        Ok(value)
    }

    async fn refresh(&self) -> Result<String> {
        let mut guard = self.cache.lock().await;
        let fresh = self.mint().await?;
        let value = fresh.value.clone();
        *guard = Some(fresh);
        Ok(value)
    }

    fn describe(&self) -> &'static str {
        "GitHub Actions OIDC"
    }
}

/// Runs a user-supplied shell command and uses its trimmed stdout as the
/// bearer token. Generic escape hatch for any CI / identity system.
pub struct CommandToken {
    command: String,
    cache: Mutex<Option<CachedToken>>,
}

impl CommandToken {
    pub fn new(command: String) -> Self {
        Self {
            command,
            cache: Mutex::new(None),
        }
    }

    async fn run(&self) -> Result<CachedToken> {
        let command = self.command.clone();
        let output = tokio::task::spawn_blocking(move || {
            std::process::Command::new("sh")
                .arg("-c")
                .arg(&command)
                .output()
        })
        .await
        .context("Failed to spawn RISE_TOKEN_COMMAND")?
        .context("Failed to run RISE_TOKEN_COMMAND")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "RISE_TOKEN_COMMAND failed (exit {}): {}",
                output
                    .status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string()),
                stderr.trim()
            );
        }
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if token.is_empty() {
            bail!("RISE_TOKEN_COMMAND produced no token on stdout");
        }
        Ok(CachedToken::new(token))
    }
}

#[async_trait::async_trait]
impl TokenSource for CommandToken {
    async fn token(&self) -> Result<String> {
        let mut guard = self.cache.lock().await;
        let now = Utc::now().timestamp();
        if let Some(cached) = guard.as_ref() {
            if cached.is_fresh(now) {
                return Ok(cached.value.clone());
            }
        }
        let fresh = self.run().await?;
        let value = fresh.value.clone();
        *guard = Some(fresh);
        Ok(value)
    }

    async fn refresh(&self) -> Result<String> {
        let mut guard = self.cache.lock().await;
        let fresh = self.run().await?;
        let value = fresh.value.clone();
        *guard = Some(fresh);
        Ok(value)
    }

    fn describe(&self) -> &'static str {
        "RISE_TOKEN_COMMAND"
    }
}

/// Inputs to the token-source selection, separated from env/IO so the
/// precedence logic is unit-testable without mutating process env.
pub struct ProviderInputs {
    pub rise_token: Option<String>,
    pub rise_token_command: Option<String>,
    pub gha_request_url: Option<String>,
    pub gha_request_token: Option<String>,
    pub audience: Option<String>,
    pub stored_token: Option<String>,
    /// Resolved backend URL, used only to enrich the "no audience" error.
    pub backend_url: String,
}

/// Pure precedence: pick a token source from already-resolved inputs.
pub fn select_token_provider(
    http: &reqwest::Client,
    inputs: ProviderInputs,
) -> Result<TokenProvider> {
    // 1. Explicit RISE_TOKEN wins (backward compatible).
    if let Some(token) = inputs.rise_token.filter(|t| !t.is_empty()) {
        return Ok(Arc::new(StaticToken::new(token)));
    }
    // 2. Generic command escape hatch.
    if let Some(cmd) = inputs.rise_token_command.filter(|c| !c.is_empty()) {
        return Ok(Arc::new(CommandToken::new(cmd)));
    }
    // 3. GitHub Actions OIDC auto-detection.
    if let (Some(url), Some(req_token)) = (
        inputs.gha_request_url.filter(|s| !s.is_empty()),
        inputs.gha_request_token.filter(|s| !s.is_empty()),
    ) {
        let audience = inputs.audience.filter(|a| !a.is_empty()).ok_or_else(|| {
            anyhow::anyhow!(
                "GitHub Actions OIDC detected but no audience configured. \
                 Set RISE_AUDIENCE (recommended: the Rise server URL, e.g. {}) \
                 or add `[auth] audience` to .rise.toml.",
                inputs.backend_url
            )
        })?;
        return Ok(Arc::new(GithubActionsOidc::new(
            http.clone(),
            url,
            req_token,
            audience,
        )));
    }
    // 4. Stored login token.
    if let Some(token) = inputs.stored_token.filter(|t| !t.is_empty()) {
        return Ok(Arc::new(StaticToken::new(token)));
    }
    // 5. Nothing.
    bail!("Not authenticated. Run 'rise login' first.")
}

/// Read real env + config and build the token provider. `audience_override`
/// comes from `.rise.toml [auth] audience` (deployment path only).
pub fn resolve_token_provider(
    http: &reqwest::Client,
    config: &Config,
    audience_override: Option<&str>,
) -> Result<TokenProvider> {
    let inputs = ProviderInputs {
        rise_token: std::env::var("RISE_TOKEN").ok(),
        rise_token_command: std::env::var("RISE_TOKEN_COMMAND").ok(),
        gha_request_url: std::env::var("ACTIONS_ID_TOKEN_REQUEST_URL").ok(),
        gha_request_token: std::env::var("ACTIONS_ID_TOKEN_REQUEST_TOKEN").ok(),
        audience: audience_override
            .map(|s| s.to_string())
            .or_else(|| std::env::var("RISE_AUDIENCE").ok()),
        stored_token: config.stored_token(),
        backend_url: config.get_backend_url(),
    };
    select_token_provider(http, inputs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose, Engine as _};

    fn jwt_with_exp(exp: i64) -> String {
        let header = general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#);
        let claims =
            general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{}}}"#, exp).as_bytes());
        format!("{}.{}.sig", header, claims)
    }

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    fn base_inputs() -> ProviderInputs {
        ProviderInputs {
            rise_token: None,
            rise_token_command: None,
            gha_request_url: None,
            gha_request_token: None,
            audience: None,
            stored_token: None,
            backend_url: "https://rise.example".to_string(),
        }
    }

    #[tokio::test]
    async fn static_token_returns_and_refreshes_same_value() {
        let s = StaticToken::new("abc".to_string());
        assert_eq!(s.token().await.unwrap(), "abc");
        assert_eq!(s.refresh().await.unwrap(), "abc");
    }

    #[test]
    fn cached_token_freshness_respects_skew() {
        let now = 1_000_000;
        let fresh = CachedToken {
            value: "x".into(),
            exp: Some(now + EXPIRY_SKEW_SECONDS + 10),
        };
        assert!(fresh.is_fresh(now));
        let near = CachedToken {
            value: "x".into(),
            exp: Some(now + EXPIRY_SKEW_SECONDS - 10),
        };
        assert!(!near.is_fresh(now));
        let opaque = CachedToken {
            value: "x".into(),
            exp: None,
        };
        assert!(!opaque.is_fresh(now));
    }

    #[test]
    fn cached_token_decodes_exp_from_jwt() {
        let c = CachedToken::new(jwt_with_exp(4_102_444_800));
        assert_eq!(c.exp, Some(4_102_444_800));
    }

    #[test]
    fn precedence_rise_token_wins_over_everything() {
        let mut i = base_inputs();
        i.rise_token = Some("tok".into());
        i.rise_token_command = Some("echo x".into());
        i.gha_request_url = Some("https://gha".into());
        i.gha_request_token = Some("rt".into());
        i.audience = Some("aud".into());
        i.stored_token = Some("stored".into());
        let p = select_token_provider(&client(), i).unwrap();
        assert_eq!(p.describe(), "static token");
    }

    #[test]
    fn precedence_command_over_gha_and_stored() {
        let mut i = base_inputs();
        i.rise_token_command = Some("echo x".into());
        i.gha_request_url = Some("https://gha".into());
        i.gha_request_token = Some("rt".into());
        i.audience = Some("aud".into());
        i.stored_token = Some("stored".into());
        let p = select_token_provider(&client(), i).unwrap();
        assert_eq!(p.describe(), "RISE_TOKEN_COMMAND");
    }

    #[test]
    fn precedence_gha_over_stored() {
        let mut i = base_inputs();
        i.gha_request_url = Some("https://gha".into());
        i.gha_request_token = Some("rt".into());
        i.audience = Some("aud".into());
        i.stored_token = Some("stored".into());
        let p = select_token_provider(&client(), i).unwrap();
        assert_eq!(p.describe(), "GitHub Actions OIDC");
    }

    #[test]
    fn gha_without_audience_errors_with_backend_url() {
        let mut i = base_inputs();
        i.gha_request_url = Some("https://gha".into());
        i.gha_request_token = Some("rt".into());
        let err = select_token_provider(&client(), i).unwrap_err().to_string();
        assert!(err.contains("no audience"));
        assert!(err.contains("https://rise.example"));
    }

    #[test]
    fn stored_token_is_last_resort() {
        let mut i = base_inputs();
        i.stored_token = Some("stored".into());
        let p = select_token_provider(&client(), i).unwrap();
        assert_eq!(p.describe(), "static token");
    }

    #[test]
    fn no_source_errors() {
        let err = select_token_provider(&client(), base_inputs())
            .unwrap_err()
            .to_string();
        assert!(err.contains("Not authenticated"));
    }
}
