//! Pluggable bearer-token sources for authenticating the CLI to the backend.
//!
//! In CI the backend token is often a short-lived OIDC token (GitHub Actions
//! ID tokens last ~5 minutes and cannot be extended). A long deploy that
//! builds and pushes several containers can outlast a single pre-minted token,
//! so instead of capturing one fixed token string we thread a [`TokenProvider`]
//! through the deployment path and resolve a fresh token immediately before
//! each request. Implementations cache the minted token and re-mint lazily
//! before it nears expiry.

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use std::error::Error;
use std::fmt;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::warn;

use crate::config::Config;
use crate::login::token_utils::read_token_exp;

/// Re-mint cached JWTs before two thirds of their observed lifetime has elapsed
/// and once they are within this many seconds of expiry, so a request issued
/// right after [`TokenSource::token`] still has comfortable headroom against
/// clock skew and request latency.
const EXPIRY_SKEW_SECONDS: i64 = 60;
const REFRESH_FRACTION_NUMERATOR: u32 = 2;
const REFRESH_FRACTION_DENOMINATOR: u32 = 3;

/// A source of bearer tokens for backend authentication.
#[async_trait::async_trait]
pub trait TokenSource: Send + Sync {
    /// Return a currently-valid bearer token, re-minting lazily if the cached
    /// one is missing or within the skew window of its `exp`.
    async fn token(&self) -> Result<String>;

    /// Short human label for diagnostics/tests.
    fn describe(&self) -> &'static str;
}

/// Cheaply-cloneable handle threaded through the deployment path.
pub type TokenProvider = Arc<dyn TokenSource>;

#[derive(Debug)]
pub enum TokenSourceError {
    Retryable(String),
    NonRetryable(String),
    NoSource(String),
}

impl TokenSourceError {
    fn retryable(message: impl Into<String>) -> Self {
        Self::Retryable(message.into())
    }

    fn non_retryable(message: impl Into<String>) -> Self {
        Self::NonRetryable(message.into())
    }

    pub fn is_non_retryable(&self) -> bool {
        matches!(self, Self::NonRetryable(_))
    }

    pub fn is_no_source(&self) -> bool {
        matches!(self, Self::NoSource(_))
    }
}

impl fmt::Display for TokenSourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Retryable(message) | Self::NonRetryable(message) | Self::NoSource(message) => {
                f.write_str(message)
            }
        }
    }
}

impl Error for TokenSourceError {}

pub fn is_non_retryable_token_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<TokenSourceError>())
        .is_some_and(TokenSourceError::is_non_retryable)
}

pub fn is_no_token_source_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<TokenSourceError>())
        .is_some_and(TokenSourceError::is_no_source)
}

/// Number of retry attempts after the initial token resolution attempt.
const TOKEN_RESOLUTION_RETRIES: usize = 3;

/// Resolve a bearer token, retrying transient mint failures.
///
/// Token sources own their cache/refresh policy: every attempt asks for a
/// usable token via `token()`, and the provider decides whether to reuse or
/// re-mint. Token sources can mark clear configuration/auth failures as
/// non-retryable so we fail fast instead of repeating a known-bad request.
pub async fn token_with_retry(provider: &TokenProvider) -> Result<String> {
    for attempt in 0..=TOKEN_RESOLUTION_RETRIES {
        match provider.token().await {
            Ok(token) => return Ok(token),
            Err(e) if is_non_retryable_token_error(&e) => return Err(e),
            Err(e) if attempt < TOKEN_RESOLUTION_RETRIES => {
                warn!(
                    "Token resolution failed, retrying ({}/{}): {:?}",
                    attempt + 1,
                    TOKEN_RESOLUTION_RETRIES,
                    e
                );
            }
            Err(e) => return Err(e),
        }
    }

    unreachable!("token retry loop always returns");
}

/// Resolve the configured token provider and then a bearer token with the
/// shared retry policy. Use for one-shot backend requests.
pub async fn resolve_token_with_retry(http: &reqwest::Client, config: &Config) -> Result<String> {
    let provider = resolve_token_provider(http, config)?;
    token_with_retry(&provider).await
}

/// A token cached alongside its decoded `exp` (seconds since epoch). `None`
/// expiry means the token is opaque (not a decodable JWT) and must be re-minted
/// on every use to stay safe.
#[derive(Clone)]
struct CachedToken {
    value: String,
    exp: Option<i64>,
    minted_at: Instant,
    minted_at_secs: i64,
}

impl CachedToken {
    fn new(value: String) -> Self {
        let exp = read_token_exp(&value);
        Self {
            value,
            exp,
            minted_at: Instant::now(),
            minted_at_secs: Utc::now().timestamp(),
        }
    }

    fn is_fresh(&self, now_secs: i64) -> bool {
        let Some(exp) = self.exp else {
            return false;
        };
        if exp - EXPIRY_SKEW_SECONDS <= now_secs {
            return false;
        }

        let lifetime_secs = exp.saturating_sub(self.minted_at_secs);
        if lifetime_secs <= 0 {
            return false;
        }
        let elapsed_secs = now_secs.saturating_sub(self.minted_at_secs);
        elapsed_secs.saturating_mul(REFRESH_FRACTION_DENOMINATOR as i64)
            < lifetime_secs.saturating_mul(REFRESH_FRACTION_NUMERATOR as i64)
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
            let message = format!(
                "Failed to mint GitHub Actions OIDC token (audience '{}'): {} {}. \
                 Ensure the workflow grants 'id-token: write' permission.",
                self.audience, status, body
            );
            if status.is_server_error()
                || status == reqwest::StatusCode::REQUEST_TIMEOUT
                || status == reqwest::StatusCode::TOO_MANY_REQUESTS
            {
                return Err(TokenSourceError::retryable(message).into());
            }
            return Err(TokenSourceError::non_retryable(message).into());
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
            return Err(TokenSourceError::non_retryable(
                "GitHub Actions OIDC endpoint returned an empty token",
            )
            .into());
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

    fn describe(&self) -> &'static str {
        "GitHub Actions OIDC"
    }
}

/// Default TTL for opaque (non-JWT) tokens produced by `RISE_TOKEN_COMMAND`.
/// Overridable via `RISE_TOKEN_COMMAND_TTL` (seconds).
const COMMAND_TOKEN_TTL_DEFAULT_SECS: u64 = 10 * 60;
/// Default maximum runtime for `RISE_TOKEN_COMMAND`.
/// Overridable via `RISE_TOKEN_COMMAND_TIMEOUT` (seconds).
const COMMAND_TOKEN_TIMEOUT_DEFAULT_SECS: u64 = 10;

/// Runs a user-supplied shell command and uses its trimmed stdout as the
/// bearer token. Generic escape hatch for any CI / identity system.
///
/// If the command outputs a JWT, its `exp` claim governs freshness. For opaque
/// tokens (no decodable `exp`), the command is re-run before `ttl` elapses
/// (default 10 minutes, overridable via `RISE_TOKEN_COMMAND_TTL`).
pub struct CommandToken {
    command: String,
    ttl: Duration,
    timeout: Duration,
    cache: Mutex<Option<CachedToken>>,
}

impl CommandToken {
    pub fn new(command: String, ttl: Duration, timeout: Duration) -> Self {
        Self {
            command,
            ttl,
            timeout,
            cache: Mutex::new(None),
        }
    }

    fn is_cached_fresh(&self, cached: &CachedToken, now_ts: i64) -> bool {
        // JWT output: trust the embedded exp (more precise than wall-clock TTL).
        if cached.exp.is_some() {
            return cached.is_fresh(now_ts);
        }
        // Opaque output: refresh before the configured TTL is reached. The
        // external command should return tokens valid beyond this threshold.
        cached
            .minted_at
            .elapsed()
            .saturating_mul(REFRESH_FRACTION_DENOMINATOR)
            < self.ttl.saturating_mul(REFRESH_FRACTION_NUMERATOR)
    }

    async fn run(&self) -> Result<CachedToken> {
        let child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&self.command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("Failed to spawn RISE_TOKEN_COMMAND")?;

        let output = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(output) => output.context("Failed to run RISE_TOKEN_COMMAND")?,
            Err(_) => {
                return Err(TokenSourceError::retryable(format!(
                    "RISE_TOKEN_COMMAND timed out after {} seconds",
                    self.timeout.as_secs()
                ))
                .into());
            }
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(TokenSourceError::retryable(format!(
                "RISE_TOKEN_COMMAND failed (exit {}): {}",
                output
                    .status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string()),
                stderr.trim()
            ))
            .into());
        }
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if token.is_empty() {
            return Err(TokenSourceError::non_retryable(
                "RISE_TOKEN_COMMAND produced no token on stdout",
            )
            .into());
        }
        Ok(CachedToken::new(token))
    }
}

#[async_trait::async_trait]
impl TokenSource for CommandToken {
    async fn token(&self) -> Result<String> {
        let mut guard = self.cache.lock().await;
        let now_ts = Utc::now().timestamp();
        if let Some(cached) = guard.as_ref() {
            if self.is_cached_fresh(cached, now_ts) {
                return Ok(cached.value.clone());
            }
        }
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
    /// How long to cache an opaque (non-JWT) token from `RISE_TOKEN_COMMAND`
    /// before re-running the command. JWT tokens use their `exp` claim instead.
    pub rise_token_command_ttl: Duration,
    /// Maximum runtime for `RISE_TOKEN_COMMAND`.
    pub rise_token_command_timeout: Duration,
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
            inputs.rise_token_command_timeout,
        )));
    }
    // 3. GitHub Actions OIDC auto-detection.
    if let (Some(url), Some(req_token)) = (
        inputs.gha_request_url.filter(|s| !s.is_empty()),
        inputs.gha_request_token.filter(|s| !s.is_empty()),
    ) {
        let audience = inputs.audience.filter(|a| !a.is_empty()).ok_or_else(|| {
            anyhow!(
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
    Err(TokenSourceError::NoSource("Not authenticated. Run 'rise login' first.".to_string()).into())
}

/// Read real env + config and build the token provider.
pub fn resolve_token_provider(http: &reqwest::Client, config: &Config) -> Result<TokenProvider> {
    let command_ttl_secs = std::env::var("RISE_TOKEN_COMMAND_TTL")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(COMMAND_TOKEN_TTL_DEFAULT_SECS);
    let command_timeout_secs = std::env::var("RISE_TOKEN_COMMAND_TIMEOUT")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(COMMAND_TOKEN_TIMEOUT_DEFAULT_SECS);
    let inputs = ProviderInputs {
        rise_token: std::env::var("RISE_TOKEN").ok(),
        rise_token_command: std::env::var("RISE_TOKEN_COMMAND").ok(),
        rise_token_command_ttl: Duration::from_secs(command_ttl_secs),
        rise_token_command_timeout: Duration::from_secs(command_timeout_secs),
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
            rise_token_command_timeout: Duration::from_secs(COMMAND_TOKEN_TIMEOUT_DEFAULT_SECS),
            gha_request_url: None,
            gha_request_token: None,
            audience: None,
            stored_token: None,
            backend_url: "https://rise.example".to_string(),
        }
    }

    #[tokio::test]
    async fn static_token_returns_value() {
        let s = StaticToken::new("abc".to_string());
        assert_eq!(s.token().await.unwrap(), "abc");
    }

    #[test]
    fn cached_token_freshness_respects_skew_and_lifetime_fraction() {
        let now = 1_000_000;
        let fresh = CachedToken {
            value: "x".into(),
            exp: Some(now + 300),
            minted_at: Instant::now(),
            minted_at_secs: now,
        };
        assert!(fresh.is_fresh(now));
        let near = CachedToken {
            value: "x".into(),
            exp: Some(now + EXPIRY_SKEW_SECONDS - 10),
            minted_at: Instant::now(),
            minted_at_secs: now,
        };
        assert!(!near.is_fresh(now));
        let two_thirds_elapsed = CachedToken {
            value: "x".into(),
            exp: Some(now + 100),
            minted_at: Instant::now() - Duration::from_secs(200),
            minted_at_secs: now - 200,
        };
        assert!(!two_thirds_elapsed.is_fresh(now));
        let opaque = CachedToken {
            value: "x".into(),
            exp: None,
            minted_at: Instant::now(),
            minted_at_secs: now,
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

    #[tokio::test]
    async fn selected_provider_resolves_token_with_retry_helper() {
        let mut i = base_inputs();
        i.rise_token = Some("tok".into());
        let p = select_token_provider(&client(), i).unwrap();
        assert_eq!(token_with_retry(&p).await.unwrap(), "tok");
    }

    #[test]
    fn no_source_errors() {
        let err = match select_token_provider(&client(), base_inputs()) {
            Ok(_) => panic!("expected an error when no token source is available"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("Not authenticated"));
        let err = match select_token_provider(&client(), base_inputs()) {
            Ok(_) => panic!("expected an error when no token source is available"),
            Err(e) => e,
        };
        assert!(is_no_token_source_error(&err));
    }

    mod command_token_ttl {
        use super::*;

        fn opaque_command_token(ttl: Duration) -> CommandToken {
            CommandToken::new(
                "echo opaque-key".to_string(),
                ttl,
                Duration::from_secs(COMMAND_TOKEN_TIMEOUT_DEFAULT_SECS),
            )
        }

        #[test]
        fn opaque_token_is_fresh_before_two_thirds_of_ttl() {
            let ct = opaque_command_token(Duration::from_secs(600));
            let cached = CachedToken::new("opaque-key".to_string());
            assert!(cached.exp.is_none(), "plain string should have no exp");
            // Fresh immediately after minting.
            assert!(ct.is_cached_fresh(&cached, Utc::now().timestamp()));
        }

        #[test]
        fn opaque_token_is_stale_after_two_thirds_of_ttl() {
            let ct = opaque_command_token(Duration::from_secs(90));
            let mut cached = CachedToken::new("opaque-key".to_string());
            // Simulate minted 61 seconds ago (> 2/3 of 90s).
            cached.minted_at = Instant::now() - Duration::from_secs(61);
            assert!(!ct.is_cached_fresh(&cached, Utc::now().timestamp()));
        }

        #[test]
        fn jwt_token_uses_exp_not_ttl() {
            // Very short TTL but the JWT exp is far in the future — should be fresh.
            let ct = opaque_command_token(Duration::from_millis(1));
            let exp = Utc::now().timestamp() + 3600;
            let cached = CachedToken::new(jwt_with_exp(exp));
            assert!(cached.exp.is_some());
            assert!(ct.is_cached_fresh(&cached, Utc::now().timestamp()));
        }

        #[test]
        fn jwt_token_stale_when_exp_near() {
            // Long TTL but JWT is near expiry — should be stale.
            let ct = opaque_command_token(Duration::from_secs(3600));
            let exp = Utc::now().timestamp() + EXPIRY_SKEW_SECONDS - 10; // within skew window
            let cached = CachedToken::new(jwt_with_exp(exp));
            assert!(!ct.is_cached_fresh(&cached, Utc::now().timestamp()));
        }

        #[tokio::test]
        async fn command_times_out() {
            let ct = CommandToken::new(
                "sleep 5; echo never".to_string(),
                Duration::from_secs(600),
                Duration::from_millis(50),
            );

            let err = ct.token().await.unwrap_err().to_string();

            assert!(err.contains("RISE_TOKEN_COMMAND timed out"));
        }
    }
}
