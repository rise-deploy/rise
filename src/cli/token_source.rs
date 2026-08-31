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
use std::future::Future;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::warn;

use crate::config::Config;
use crate::login::token_utils::{log_token_debug, read_token_exp};

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

/// Base delay between token-resolution retries. Grows linearly with the attempt
/// number (1×, 2×, 3×) so a momentarily-flaky mint endpoint gets a little
/// breathing room instead of being hammered back-to-back.
const TOKEN_RETRY_BASE_BACKOFF: Duration = Duration::from_millis(500);

/// Default maximum runtime for a single GitHub Actions OIDC mint request.
const GHA_OIDC_MINT_TIMEOUT_DEFAULT_SECS: u64 = 10;

/// Resolve a bearer token, retrying transient mint failures.
///
/// Token sources own their cache/refresh policy: every attempt asks for a
/// usable token via `token()`, and the provider decides whether to reuse or
/// re-mint. Token sources can mark clear configuration/auth failures as
/// non-retryable so we fail fast instead of repeating a known-bad request.
pub async fn token_with_retry(provider: &TokenProvider) -> Result<String> {
    for attempt in 0..=TOKEN_RESOLUTION_RETRIES {
        match provider.token().await {
            Ok(token) => {
                // Surface the resolved token's claims at debug so operators can
                // confirm what identity a command (e.g. `rise deploy` from CI)
                // is presenting — the OIDC token in RISE_TOKEN, a minted GitHub
                // Actions OIDC token, etc. Signature is never logged.
                log_token_debug(&token, provider.describe());
                return Ok(token);
            }
            Err(e) if is_non_retryable_token_error(&e) => return Err(e),
            Err(e) if attempt < TOKEN_RESOLUTION_RETRIES => {
                let backoff = TOKEN_RETRY_BASE_BACKOFF * (attempt as u32 + 1);
                warn!(
                    "Token resolution failed (attempt {}/{}), retrying in {:?}: {:?}",
                    attempt + 1,
                    TOKEN_RESOLUTION_RETRIES + 1,
                    backoff,
                    e
                );
                tokio::time::sleep(backoff).await;
            }
            Err(e) => return Err(e),
        }
    }

    unreachable!("token retry loop always returns");
}

/// Resolve the configured token provider and then a bearer token with the
/// shared retry policy. Use for one-shot backend requests. Exchange (when
/// `RISE_IDENTITY` is set) is handled by [`resolve_token_provider`].
pub async fn resolve_token_with_retry(http: &reqwest::Client, config: &Config) -> Result<String> {
    let provider = resolve_token_provider(http, config)?;
    token_with_retry(&provider).await
}

/// A token cached alongside the lifetime derived from its JWT `exp`. The
/// wall-clock `exp` is converted into a [`Duration`] relative to `minted_at`
/// once, at mint time, so all subsequent freshness math runs off a single
/// monotonic [`Instant`] clock. `lifetime` is `None` for an opaque (non-JWT)
/// token, which must be re-minted on every use to stay safe; a JWT already past
/// its `exp` at mint time yields `Some(Duration::ZERO)` so it's still
/// recognisable as a JWT (distinct from opaque) while never counting as fresh.
#[derive(Clone)]
struct CachedToken {
    value: String,
    minted_at: Instant,
    lifetime: Option<Duration>,
}

impl CachedToken {
    fn new(value: String) -> Self {
        let lifetime = read_token_exp(&value).map(|exp| {
            let remaining = exp.saturating_sub(Utc::now().timestamp());
            Duration::from_secs(remaining.max(0) as u64)
        });
        Self {
            value,
            minted_at: Instant::now(),
            lifetime,
        }
    }

    /// Whether the token decoded as a JWT (has an `exp`), as opposed to an
    /// opaque value. Used to decide whether `exp`-based or TTL-based freshness
    /// applies.
    fn is_jwt(&self) -> bool {
        self.lifetime.is_some()
    }

    fn is_fresh(&self) -> bool {
        let Some(lifetime) = self.lifetime else {
            return false;
        };
        let elapsed = self.minted_at.elapsed();
        // Within the skew window of expiry?
        if lifetime.saturating_sub(elapsed) <= Duration::from_secs(EXPIRY_SKEW_SECONDS as u64) {
            return false;
        }
        // Past two thirds of the lifetime?
        elapsed.saturating_mul(REFRESH_FRACTION_DENOMINATOR)
            < lifetime.saturating_mul(REFRESH_FRACTION_NUMERATOR)
    }
}

/// A fixed token (from `RISE_TOKEN` or the stored login config). Never expires
/// from the CLI's perspective; `refresh` returns the same value. `source` is a
/// short label naming where the token came from, surfaced in debug logs.
pub struct StaticToken {
    value: String,
    source: &'static str,
}

impl StaticToken {
    pub fn new(value: String, source: &'static str) -> Self {
        Self { value, source }
    }
}

#[async_trait::async_trait]
impl TokenSource for StaticToken {
    async fn token(&self) -> Result<String> {
        Ok(self.value.clone())
    }
    fn describe(&self) -> &'static str {
        self.source
    }
}

/// Mints GitHub Actions OIDC ID tokens on demand via the runner's token
/// endpoint (`ACTIONS_ID_TOKEN_REQUEST_URL` / `ACTIONS_ID_TOKEN_REQUEST_TOKEN`).
pub struct GithubActionsOidc {
    http: reqwest::Client,
    request_url: String,
    request_token: String,
    audience: String,
    mint_timeout: Duration,
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
            mint_timeout: Duration::from_secs(GHA_OIDC_MINT_TIMEOUT_DEFAULT_SECS),
            cache: Mutex::new(None),
        }
    }

    async fn mint(&self) -> Result<CachedToken> {
        timeout_gha_oidc_mint(self.mint_timeout, self.mint_once()).await
    }

    async fn mint_once(&self) -> Result<CachedToken> {
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

async fn timeout_gha_oidc_mint<T, F>(timeout: Duration, mint: F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    match tokio::time::timeout(timeout, mint).await {
        Ok(result) => result,
        Err(_) => Err(gha_oidc_mint_timeout_error(timeout)),
    }
}

fn gha_oidc_mint_timeout_error(timeout: Duration) -> anyhow::Error {
    TokenSourceError::retryable(format!(
        "GitHub Actions OIDC token mint timed out after {} seconds",
        timeout.as_secs()
    ))
    .into()
}

#[async_trait::async_trait]
impl TokenSource for GithubActionsOidc {
    async fn token(&self) -> Result<String> {
        let mut guard = self.cache.lock().await;
        if let Some(cached) = guard.as_ref() {
            if cached.is_fresh() {
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

/// Runs a user-supplied shell command and uses its trimmed stdout as the bearer
/// token. Generic escape hatch for any CI / identity system. Intended to print
/// an **external OIDC token**: when `RISE_IDENTITY` is set the output is exchanged
/// for a Rise access token (see [`ExchangingTokenSource`]); otherwise it is sent
/// as-is (the legacy raw-token path).
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

    fn is_cached_fresh(&self, cached: &CachedToken) -> bool {
        // JWT output: trust the embedded exp (more precise than wall-clock TTL).
        if cached.is_jwt() {
            return cached.is_fresh();
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
        if let Some(cached) = guard.as_ref() {
            if self.is_cached_fresh(cached) {
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

/// RFC 8693 grant type / subject-token type for the auth token-exchange
/// endpoint (`POST /api/v1/auth/token`). These mirror the server-side
/// constants in `server::auth::exchange::models`, which live behind the
/// `backend` feature and so can't be shared with the CLI build.
const GRANT_TYPE_TOKEN_EXCHANGE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const SUBJECT_TOKEN_TYPE_JWT: &str = "urn:ietf:params:oauth:token-type:jwt";

/// Maximum runtime for a single token-exchange round-trip (the server fetches
/// the source issuer's JWKS, so allow more headroom than a local mint).
const TOKEN_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

/// Decorates a base [`TokenSource`] so its token is exchanged for a short-lived,
/// Rise-signed access token at `POST /api/v1/auth/token` before use, assuming a
/// given identity (a service account's email).
///
/// The wrap is applied by [`resolve_token_provider`] only when `RISE_IDENTITY` is
/// set — the explicit, channel-agnostic signal that the caller wants to exchange
/// its token (from any source) for that identity. So this decorator always
/// exchanges and never inspects the token's contents.
///
/// Nested freshness: the inner source owns its own OIDC mint/refresh cache, so
/// the inner token is only re-minted when stale; the exchanged access token is
/// cached here and re-exchanged only once it nears its own `exp`. A still-fresh
/// access token is returned without even consulting the inner source.
pub struct ExchangingTokenSource {
    inner: TokenProvider,
    http: reqwest::Client,
    /// Rise backend base URL — used to build the exchange endpoint URL.
    backend_url: String,
    /// The identity to assume: a service account's synthetic-user email. The
    /// minted access token is bound to that SA (and its project).
    identity: String,
    cache: Mutex<Option<CachedToken>>,
}

impl ExchangingTokenSource {
    fn new(
        inner: TokenProvider,
        http: reqwest::Client,
        backend_url: String,
        identity: String,
    ) -> Self {
        Self {
            inner,
            http,
            backend_url,
            identity,
            cache: Mutex::new(None),
        }
    }

    async fn exchange(&self, subject_token: &str) -> Result<String> {
        // Bound the whole round-trip: a hung endpoint must not block the deploy
        // indefinitely (the CLI client carries no default timeout). A timeout is
        // transient, so it's retryable.
        match tokio::time::timeout(TOKEN_EXCHANGE_TIMEOUT, self.exchange_once(subject_token)).await
        {
            Ok(result) => result,
            Err(_) => Err(TokenSourceError::retryable(format!(
                "Token exchange timed out after {} seconds",
                TOKEN_EXCHANGE_TIMEOUT.as_secs()
            ))
            .into()),
        }
    }

    async fn exchange_once(&self, subject_token: &str) -> Result<String> {
        let url = format!(
            "{}/api/v1/auth/token",
            self.backend_url.trim_end_matches('/')
        );
        let response = self
            .http
            .post(&url)
            .json(&build_exchange_body(subject_token, &self.identity))
            .send()
            .await
            .map_err(|e| {
                TokenSourceError::retryable(format!("Token exchange request failed: {e}"))
            })?;
        let status = response.status();
        // A failed body read on an otherwise-OK response is a transient network
        // error, not a malformed payload — keep it retryable rather than letting
        // an empty body fall through to a non-retryable parse failure.
        let body = response.text().await.map_err(|e| {
            TokenSourceError::retryable(format!("Failed to read token-exchange response body: {e}"))
        })?;
        parse_exchange_response(status, &body)
    }
}

#[async_trait::async_trait]
impl TokenSource for ExchangingTokenSource {
    async fn token(&self) -> Result<String> {
        // Hold the cache lock across the whole resolution (as GithubActionsOidc
        // and CommandToken do) so concurrent callers serialize: a second caller
        // waits, then sees the access token the first one just exchanged instead
        // of racing it into a duplicate exchange (extra requests, extra audit
        // entries, distinct `jti` for one source token).
        let mut guard = self.cache.lock().await;

        // A still-fresh exchanged access token is reused directly; it outlives
        // inner OIDC rotations, so we needn't consult the inner source at all.
        if let Some(cached) = guard.as_ref() {
            if cached.is_fresh() {
                return Ok(cached.value.clone());
            }
        }

        // The wrap is only applied when an exchange was explicitly requested
        // (RISE_IDENTITY set), so the inner token is always exchanged — no
        // content inspection.
        let subject = self.inner.token().await?;
        let access = self.exchange(&subject).await?;
        let cached = CachedToken::new(access);
        let value = cached.value.clone();
        *guard = Some(cached);
        Ok(value)
    }

    /// Reports the underlying source, not the exchange wrapper: the exchange is
    /// always in play in this CLI, so what an operator wants in the logs is
    /// which identity is being presented (GitHub Actions OIDC, stored login, …).
    fn describe(&self) -> &'static str {
        self.inner.describe()
    }
}

/// Wrap `inner` so its token is exchanged for a Rise access token assuming
/// `identity` before use. See [`ExchangingTokenSource`]; applied by
/// [`resolve_token_provider`] when `RISE_IDENTITY` is set.
fn with_token_exchange(
    inner: TokenProvider,
    http: reqwest::Client,
    backend_url: String,
    identity: String,
) -> TokenProvider {
    Arc::new(ExchangingTokenSource::new(
        inner,
        http,
        backend_url,
        identity,
    ))
}

/// Build the RFC 8693 request body for the exchange endpoint.
fn build_exchange_body(subject_token: &str, identity: &str) -> serde_json::Value {
    serde_json::json!({
        "grant_type": GRANT_TYPE_TOKEN_EXCHANGE,
        "subject_token": subject_token,
        "subject_token_type": SUBJECT_TOKEN_TYPE_JWT,
        "identity": identity,
    })
}

/// Parse the exchange endpoint's response into the minted access token, mapping
/// the HTTP status onto retryable vs. non-retryable token errors so
/// [`token_with_retry`] only retries transient failures (`5xx`, `429`).
fn parse_exchange_response(status: reqwest::StatusCode, body: &str) -> Result<String> {
    if status.is_success() {
        #[derive(serde::Deserialize)]
        struct ExchangeOk {
            access_token: String,
        }
        // A 200 with an unparseable body won't parse on retry — fail fast.
        let parsed: ExchangeOk = serde_json::from_str(body).map_err(|e| {
            TokenSourceError::non_retryable(format!("Failed to parse token-exchange response: {e}"))
        })?;
        if parsed.access_token.trim().is_empty() {
            return Err(TokenSourceError::non_retryable(
                "Token-exchange endpoint returned an empty access token",
            )
            .into());
        }
        return Ok(parsed.access_token);
    }

    let detail = parse_oauth_error(body).unwrap_or_else(|| truncate_detail(body));
    let message = format!("Token exchange failed ({status}): {detail}");
    if status.is_server_error()
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
    {
        Err(TokenSourceError::retryable(message).into())
    } else {
        Err(TokenSourceError::non_retryable(message).into())
    }
}

/// Extract a readable `error: description` from an OAuth/RFC 8693 error body.
fn parse_oauth_error(body: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct OAuthErr {
        error: String,
        error_description: Option<String>,
    }
    let parsed: OAuthErr = serde_json::from_str(body).ok()?;
    Some(match parsed.error_description {
        Some(desc) => format!("{}: {}", parsed.error, desc),
        None => parsed.error,
    })
}

/// Bound a non-OAuth error body to a sane length for the error message (a
/// proxy/gateway can return a large HTML page). Char-safe truncation.
fn truncate_detail(body: &str) -> String {
    const MAX: usize = 300;
    let trimmed = body.trim();
    if trimmed.chars().count() > MAX {
        format!("{}…", trimmed.chars().take(MAX).collect::<String>())
    } else {
        trimmed.to_string()
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
    /// Resolved backend URL — enriches the "no audience" error and is the
    /// exchange endpoint's base when `identity` is set.
    pub backend_url: String,
    /// `RISE_IDENTITY`: the service-account identity (synthetic-user email) to
    /// assume. When set, the selected token is exchanged for it; when unset, the
    /// token is used as-is.
    pub identity: Option<String>,
}

/// Pure precedence: pick a token source from already-resolved inputs, and — when
/// `identity` is set — layer the token exchange on top (channel-agnostic).
pub fn select_token_provider(
    http: &reqwest::Client,
    inputs: ProviderInputs,
) -> Result<TokenProvider> {
    // Capture the exchange context before the precedence consumes `inputs`.
    let identity = inputs.identity.clone().filter(|s| !s.trim().is_empty());
    let backend_url = inputs.backend_url.clone();

    let base: TokenProvider = if let Some(token) = inputs.rise_token.filter(|t| !t.is_empty()) {
        // 1. Explicit RISE_TOKEN wins (backward compatible).
        Arc::new(StaticToken::new(token, "RISE_TOKEN environment variable"))
    } else if let Some(cmd) = inputs.rise_token_command.filter(|c| !c.is_empty()) {
        // 2. Generic command escape hatch.
        Arc::new(CommandToken::new(
            cmd,
            inputs.rise_token_command_ttl,
            inputs.rise_token_command_timeout,
        ))
    } else if let (Some(url), Some(req_token)) = (
        inputs.gha_request_url.filter(|s| !s.is_empty()),
        inputs.gha_request_token.filter(|s| !s.is_empty()),
    ) {
        // 3. GitHub Actions OIDC auto-detection.
        let audience = inputs.audience.filter(|a| !a.is_empty()).ok_or_else(|| {
            anyhow!(
                "GitHub Actions OIDC detected but no audience configured. \
                 Set RISE_GHA_AUDIENCE (recommended: the Rise server URL, e.g. {}).",
                backend_url
            )
        })?;
        Arc::new(GithubActionsOidc::new(
            http.clone(),
            url,
            req_token,
            audience,
        ))
    } else if let Some(token) = inputs.stored_token.filter(|t| !t.is_empty()) {
        // 4. Stored login token.
        Arc::new(StaticToken::new(token, "stored login token"))
    } else {
        // 5. Nothing.
        let message = match Config::active_profile().ok().flatten() {
            Some(profile) => format!(
                "Not authenticated for profile '{profile}'. Run 'rise login --profile {profile}' first."
            ),
            None => "Not authenticated. Run 'rise login' first.".to_string(),
        };
        return Err(TokenSourceError::NoSource(message).into());
    };

    // `RISE_IDENTITY` set → exchange the resolved token for that identity,
    // regardless of which source produced it. Unset → use the token as-is.
    Ok(match identity {
        Some(identity) => with_token_exchange(base, http.clone(), backend_url, identity),
        None => base,
    })
}

/// Read real env + config and build the token provider for a command.
///
/// This is the single entry point every command uses to obtain a token source.
/// It selects the base source by precedence (`RISE_TOKEN`, then
/// `RISE_TOKEN_COMMAND`, then GitHub Actions OIDC, then the stored login). If
/// `RISE_IDENTITY` is set, the selected token is exchanged for that identity (a
/// service account, by email) regardless of source; otherwise it is used as-is.
/// Exchange is thus explicit and channel-agnostic — never inferred from the
/// token's contents.
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
        identity: std::env::var("RISE_IDENTITY").ok(),
    };
    select_token_provider(http, inputs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose, Engine as _};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

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
            identity: None,
        }
    }

    #[tokio::test]
    async fn static_token_returns_value() {
        let s = StaticToken::new("abc".to_string(), "static token");
        assert_eq!(s.token().await.unwrap(), "abc");
    }

    /// Build a cached token as if it were minted `elapsed` ago with the given
    /// total `lifetime`, bypassing wall-clock decoding so freshness boundaries
    /// can be exercised deterministically.
    fn aged_token(lifetime: Option<Duration>, elapsed: Duration) -> CachedToken {
        CachedToken {
            value: "x".into(),
            minted_at: Instant::now() - elapsed,
            lifetime,
        }
    }

    #[test]
    fn cached_token_freshness_respects_skew_and_lifetime_fraction() {
        // Just minted, plenty of lifetime left → fresh.
        let fresh = aged_token(Some(Duration::from_secs(300)), Duration::ZERO);
        assert!(fresh.is_fresh());
        // Within the skew window of expiry → stale.
        let near = aged_token(
            Some(Duration::from_secs(EXPIRY_SKEW_SECONDS as u64 - 10)),
            Duration::ZERO,
        );
        assert!(!near.is_fresh());
        // 2/3 of the lifetime elapsed → stale.
        let two_thirds_elapsed =
            aged_token(Some(Duration::from_secs(300)), Duration::from_secs(200));
        assert!(!two_thirds_elapsed.is_fresh());
        // Opaque (no lifetime) → never fresh.
        let opaque = aged_token(None, Duration::ZERO);
        assert!(!opaque.is_fresh());
    }

    #[test]
    fn cached_token_decodes_exp_from_jwt() {
        // A JWT with an exp far in the future yields a large positive lifetime.
        let c = CachedToken::new(jwt_with_exp(4_102_444_800));
        assert!(c.is_jwt());
        assert!(c.lifetime.unwrap() > Duration::from_secs(0));
        // An opaque (non-JWT) value has no lifetime.
        let opaque = CachedToken::new("not-a-jwt".to_string());
        assert!(!opaque.is_jwt());
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
        assert_eq!(p.describe(), "RISE_TOKEN environment variable");
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
        assert_eq!(p.describe(), "stored login token");
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

    #[tokio::test(start_paused = true)]
    async fn gha_oidc_mint_timeout_is_retryable() {
        let err = match timeout_gha_oidc_mint(
            Duration::from_secs(GHA_OIDC_MINT_TIMEOUT_DEFAULT_SECS),
            std::future::pending::<Result<CachedToken>>(),
        )
        .await
        {
            Ok(_) => panic!("expected pending OIDC mint to time out"),
            Err(e) => e,
        };

        assert!(err
            .to_string()
            .contains("GitHub Actions OIDC token mint timed out"));
        assert!(err.chain().any(|cause| matches!(
            cause.downcast_ref::<TokenSourceError>(),
            Some(TokenSourceError::Retryable(_))
        )));
        assert!(!is_non_retryable_token_error(&err));
    }

    struct RetryAfterOidcTimeoutSource {
        attempts: AtomicUsize,
        succeed_on_attempt: usize,
    }

    #[async_trait::async_trait]
    impl TokenSource for RetryAfterOidcTimeoutSource {
        async fn token(&self) -> Result<String> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt >= self.succeed_on_attempt {
                Ok(format!("token-{attempt}"))
            } else {
                Err(gha_oidc_mint_timeout_error(Duration::from_secs(
                    GHA_OIDC_MINT_TIMEOUT_DEFAULT_SECS,
                )))
            }
        }

        fn describe(&self) -> &'static str {
            "test OIDC timeout source"
        }
    }

    #[tokio::test(start_paused = true)]
    async fn token_with_retry_retries_gha_oidc_timeout() {
        let source = Arc::new(RetryAfterOidcTimeoutSource {
            attempts: AtomicUsize::new(0),
            succeed_on_attempt: 2,
        });
        let provider: TokenProvider = source.clone();

        let token = token_with_retry(&provider).await.unwrap();

        assert_eq!(token, "token-2");
        assert_eq!(source.attempts.load(Ordering::SeqCst), 2);
    }

    mod token_exchange {
        use super::*;

        #[tokio::test]
        async fn no_identity_passes_token_through() {
            // Regression (e2e-docker): with RISE_IDENTITY unset, the token is used
            // as-is — no exchange, regardless of source or the token's contents.
            let mut inputs = base_inputs();
            inputs.rise_token = Some("a-rise-token".into());
            let provider = select_token_provider(&client(), inputs).unwrap();
            assert_eq!(provider.token().await.unwrap(), "a-rise-token");
        }

        #[tokio::test]
        async fn identity_set_wraps_and_exchanges() {
            // With RISE_IDENTITY set, the selected token is wrapped and exchanged
            // (any source). Point at an unroutable backend: a wrapped provider
            // tries the exchange and fails to reach it, rather than returning the
            // bearer as-is.
            let mut inputs = base_inputs();
            inputs.rise_token = Some("a-rise-token".into());
            inputs.identity = Some("demo+1@sa.rise.local".into());
            inputs.backend_url = "http://127.0.0.1:1".into();
            let provider = select_token_provider(&client(), inputs).unwrap();
            let err = provider.token().await.unwrap_err();
            assert!(
                err.to_string().to_lowercase().contains("token exchange"),
                "expected an exchange attempt, got: {err}"
            );
        }

        #[tokio::test]
        async fn blank_identity_does_not_exchange() {
            // A blank RISE_IDENTITY is treated as unset (no exchange).
            let mut inputs = base_inputs();
            inputs.rise_token = Some("a-rise-token".into());
            inputs.identity = Some("   ".into());
            let provider = select_token_provider(&client(), inputs).unwrap();
            assert_eq!(provider.token().await.unwrap(), "a-rise-token");
        }

        #[test]
        fn describe_delegates_to_inner_source() {
            let inner: TokenProvider =
                Arc::new(StaticToken::new("x".into(), "GitHub Actions OIDC"));
            let src = ExchangingTokenSource::new(
                inner,
                client(),
                "https://rise.example.com".to_string(),
                "demo+1@sa.rise.local".to_string(),
            );
            assert_eq!(src.describe(), "GitHub Actions OIDC");
        }

        #[test]
        fn rfc8693_constants_match_spec() {
            // Guard the CLI's copy of the RFC 8693 URNs against a typo drifting
            // from the server (which has its own copy behind the backend feature).
            assert_eq!(
                GRANT_TYPE_TOKEN_EXCHANGE,
                "urn:ietf:params:oauth:grant-type:token-exchange"
            );
            assert_eq!(
                SUBJECT_TOKEN_TYPE_JWT,
                "urn:ietf:params:oauth:token-type:jwt"
            );
        }

        #[test]
        fn build_exchange_body_uses_rfc8693_fields() {
            let body = build_exchange_body("the-subject", "demo+1@sa.rise.local");
            assert_eq!(body["grant_type"], GRANT_TYPE_TOKEN_EXCHANGE);
            assert_eq!(body["subject_token"], "the-subject");
            assert_eq!(body["subject_token_type"], SUBJECT_TOKEN_TYPE_JWT);
            assert_eq!(body["identity"], "demo+1@sa.rise.local");
        }

        #[test]
        fn parse_exchange_response_success_returns_access_token() {
            let token = parse_exchange_response(
                reqwest::StatusCode::OK,
                r#"{"access_token":"rise-access","token_type":"Bearer","issued_token_type":"urn:ietf:params:oauth:token-type:jwt","expires_in":600}"#,
            )
            .unwrap();
            assert_eq!(token, "rise-access");
        }

        #[test]
        fn parse_exchange_response_400_is_non_retryable_with_detail() {
            let err = parse_exchange_response(
                reqwest::StatusCode::BAD_REQUEST,
                r#"{"error":"invalid_grant","error_description":"no matching service account"}"#,
            )
            .unwrap_err();
            assert!(is_non_retryable_token_error(&err));
            let msg = err.to_string();
            assert!(msg.contains("invalid_grant"));
            assert!(msg.contains("no matching service account"));
        }

        #[test]
        fn parse_exchange_response_503_is_retryable() {
            let err = parse_exchange_response(
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                r#"{"error":"temporarily_unavailable"}"#,
            )
            .unwrap_err();
            assert!(!is_non_retryable_token_error(&err));
        }

        #[test]
        fn parse_exchange_response_malformed_200_is_non_retryable() {
            // A 200 with a non-JSON body won't parse on retry — must fail fast.
            let err =
                parse_exchange_response(reqwest::StatusCode::OK, "<html>oops</html>").unwrap_err();
            assert!(is_non_retryable_token_error(&err));
        }

        #[test]
        fn parse_exchange_response_empty_token_is_non_retryable() {
            let err = parse_exchange_response(reqwest::StatusCode::OK, r#"{"access_token":""}"#)
                .unwrap_err();
            assert!(is_non_retryable_token_error(&err));
        }
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
            assert!(!cached.is_jwt(), "plain string should have no exp");
            // Fresh immediately after minting.
            assert!(ct.is_cached_fresh(&cached));
        }

        #[test]
        fn opaque_token_is_stale_after_two_thirds_of_ttl() {
            let ct = opaque_command_token(Duration::from_secs(90));
            let mut cached = CachedToken::new("opaque-key".to_string());
            // Simulate minted 61 seconds ago (> 2/3 of 90s).
            cached.minted_at = Instant::now() - Duration::from_secs(61);
            assert!(!ct.is_cached_fresh(&cached));
        }

        #[test]
        fn jwt_token_uses_exp_not_ttl() {
            // Very short TTL but the JWT exp is far in the future — should be fresh.
            let ct = opaque_command_token(Duration::from_millis(1));
            let exp = Utc::now().timestamp() + 3600;
            let cached = CachedToken::new(jwt_with_exp(exp));
            assert!(cached.is_jwt());
            assert!(ct.is_cached_fresh(&cached));
        }

        #[test]
        fn jwt_token_stale_when_exp_near() {
            // Long TTL but JWT is near expiry — should be stale.
            let ct = opaque_command_token(Duration::from_secs(3600));
            let exp = Utc::now().timestamp() + EXPIRY_SKEW_SECONDS - 10; // within skew window
            let cached = CachedToken::new(jwt_with_exp(exp));
            assert!(!ct.is_cached_fresh(&cached));
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
