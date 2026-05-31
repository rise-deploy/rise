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
use std::time::{Duration, Instant};
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

/// Default TTL for opaque (non-JWT) tokens produced by `RISE_TOKEN_COMMAND`.
/// Overridable via `RISE_TOKEN_COMMAND_TTL` (seconds).
const COMMAND_TOKEN_TTL_DEFAULT_SECS: u64 = 10 * 60;

/// Runs a user-supplied shell command and uses its trimmed stdout as the
/// bearer token. Generic escape hatch for any CI / identity system.
///
/// If the command outputs a JWT, its `exp` claim governs freshness. For opaque
/// tokens (no decodable `exp`), the command is re-run after `ttl` elapses
/// (default 10 minutes, overridable via `RISE_TOKEN_COMMAND_TTL`).
pub struct CommandToken {
    command: String,
    ttl: Duration,
    cache: Mutex<Option<(CachedToken, Instant)>>,
}

impl CommandToken {
    pub fn new(command: String, ttl: Duration) -> Self {
        Self {
            command,
            ttl,
            cache: Mutex::new(None),
        }
    }

    fn is_cached_fresh(&self, cached: &CachedToken, minted_at: Instant, now_ts: i64) -> bool {
        // JWT output: trust the embedded exp (more precise than wall-clock TTL).
        if cached.exp.is_some() {
            return cached.is_fresh(now_ts);
        }
        // Opaque output: use wall-clock TTL.
        minted_at.elapsed() < self.ttl
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
        let now_ts = Utc::now().timestamp();
        if let Some((cached, minted_at)) = guard.as_ref() {
            if self.is_cached_fresh(cached, *minted_at, now_ts) {
                return Ok(cached.value.clone());
            }
        }
        let fresh = self.run().await?;
        let value = fresh.value.clone();
        *guard = Some((fresh, Instant::now()));
        Ok(value)
    }

    async fn refresh(&self) -> Result<String> {
        let mut guard = self.cache.lock().await;
        let fresh = self.run().await?;
        let value = fresh.value.clone();
        *guard = Some((fresh, Instant::now()));
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
    /// How long to cache an opaque (non-JWT) token from `RISE_TOKEN_COMMAND`
    /// before re-running the command. JWT tokens use their `exp` claim instead.
    pub rise_token_command_ttl: Duration,
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
        return Ok(Arc::new(CommandToken::new(
            cmd,
            inputs.rise_token_command_ttl,
        )));
    }
    // 3. GitHub Actions OIDC auto-detection.
    if let (Some(url), Some(req_token)) = (
        inputs.gha_request_url.filter(|s| !s.is_empty()),
        inputs.gha_request_token.filter(|s| !s.is_empty()),
    ) {
        let audience = inputs.audience.filter(|a| !a.is_empty()).ok_or_else(|| {
            anyhow::anyhow!(
                "GitHub Actions OIDC detected but no audience configured. \
                 Set RISE_GHA_AUDIENCE (recommended: the Rise server URL, e.g. {}).",
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

/// Read real env + config and build the token provider.
pub fn resolve_token_provider(http: &reqwest::Client, config: &Config) -> Result<TokenProvider> {
    let command_ttl_secs = std::env::var("RISE_TOKEN_COMMAND_TTL")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(COMMAND_TOKEN_TTL_DEFAULT_SECS);
    let inputs = ProviderInputs {
        rise_token: std::env::var("RISE_TOKEN").ok(),
        rise_token_command: std::env::var("RISE_TOKEN_COMMAND").ok(),
        rise_token_command_ttl: Duration::from_secs(command_ttl_secs),
        gha_request_url: std::env::var("ACTIONS_ID_TOKEN_REQUEST_URL").ok(),
        gha_request_token: std::env::var("ACTIONS_ID_TOKEN_REQUEST_TOKEN").ok(),
        audience: std::env::var("RISE_GHA_AUDIENCE").ok(),
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
            rise_token_command_ttl: Duration::from_secs(COMMAND_TOKEN_TTL_DEFAULT_SECS),
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
        let err = match select_token_provider(&client(), i) {
            Ok(_) => panic!("expected an error when no audience is configured"),
            Err(e) => e.to_string(),
        };
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
        let err = match select_token_provider(&client(), base_inputs()) {
            Ok(_) => panic!("expected an error when no token source is available"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("Not authenticated"));
    }

    mod command_token_ttl {
        use super::*;

        fn opaque_command_token(ttl: Duration) -> CommandToken {
            CommandToken::new("echo opaque-key".to_string(), ttl)
        }

        #[test]
        fn opaque_token_is_fresh_within_ttl() {
            let ct = opaque_command_token(Duration::from_secs(600));
            let cached = CachedToken::new("opaque-key".to_string());
            assert!(cached.exp.is_none(), "plain string should have no exp");
            // Fresh immediately after minting.
            assert!(ct.is_cached_fresh(&cached, Instant::now(), Utc::now().timestamp()));
        }

        #[test]
        fn opaque_token_is_stale_after_ttl() {
            let ct = opaque_command_token(Duration::from_secs(1));
            let cached = CachedToken::new("opaque-key".to_string());
            // Simulate minted 2 seconds ago.
            let minted_at = Instant::now() - Duration::from_secs(2);
            assert!(!ct.is_cached_fresh(&cached, minted_at, Utc::now().timestamp()));
        }

        #[test]
        fn jwt_token_uses_exp_not_ttl() {
            // Very short TTL but the JWT exp is far in the future — should be fresh.
            let ct = opaque_command_token(Duration::from_millis(1));
            let exp = Utc::now().timestamp() + 3600;
            let cached = CachedToken::new(jwt_with_exp(exp));
            assert!(cached.exp.is_some());
            // Even though TTL is 1ms (already elapsed), exp governs.
            let minted_at = Instant::now() - Duration::from_secs(60);
            assert!(ct.is_cached_fresh(&cached, minted_at, Utc::now().timestamp()));
        }

        #[test]
        fn jwt_token_stale_when_exp_near() {
            // Long TTL but JWT is near expiry — should be stale.
            let ct = opaque_command_token(Duration::from_secs(3600));
            let exp = Utc::now().timestamp() + EXPIRY_SKEW_SECONDS - 10; // within skew window
            let cached = CachedToken::new(jwt_with_exp(exp));
            assert!(!ct.is_cached_fresh(&cached, Instant::now(), Utc::now().timestamp()));
        }
    }
}
