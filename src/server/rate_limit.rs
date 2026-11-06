use axum::{
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use moka::future::Cache;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::settings::{OAuthRateLimitSettings, WorkloadTokenRateLimitSettings};

/// Rate limiter for OAuth endpoints with four independent limits:
/// - Per-project: keyed by project name (configurable, default 500 req/60s)
/// - Per-IP: keyed by client IP (configurable, default 50 req/60s)
/// - Per-session: keyed by `rise_jwt` cookie hash (configurable, default 20 req/60s)
/// - Global: shared across all requests (configurable, default 2000 req/60s)
pub struct OAuthRateLimiter {
    project_limiter: Arc<Cache<String, Arc<AtomicU32>>>,
    ip_limiter: Arc<Cache<String, Arc<AtomicU32>>>,
    session_limiter: Arc<Cache<String, Arc<AtomicU32>>>,
    global_limiter: Arc<Cache<String, Arc<AtomicU32>>>,
    per_project_max: u32,
    per_project_window_secs: u64,
    per_ip_max: u32,
    per_ip_window_secs: u64,
    per_session_max: u32,
    per_session_window_secs: u64,
    global_max: u32,
    global_window_secs: u64,
}

impl OAuthRateLimiter {
    const PROJECT_MAX_CAPACITY: u64 = 10_000;
    const IP_MAX_CAPACITY: u64 = 50_000;
    const SESSION_MAX_CAPACITY: u64 = 10_000;
    const GLOBAL_MAX_CAPACITY: u64 = 1;

    pub fn new(settings: &OAuthRateLimitSettings) -> Self {
        let project_limiter = Arc::new(
            Cache::builder()
                .time_to_live(Duration::from_secs(settings.per_project_window_secs))
                .max_capacity(Self::PROJECT_MAX_CAPACITY)
                .build(),
        );
        let ip_limiter = Arc::new(
            Cache::builder()
                .time_to_live(Duration::from_secs(settings.per_ip_window_secs))
                .max_capacity(Self::IP_MAX_CAPACITY)
                .build(),
        );
        let session_limiter = Arc::new(
            Cache::builder()
                .time_to_live(Duration::from_secs(settings.per_session_window_secs))
                .max_capacity(Self::SESSION_MAX_CAPACITY)
                .build(),
        );
        let global_limiter = Arc::new(
            Cache::builder()
                .time_to_live(Duration::from_secs(settings.global_window_secs))
                .max_capacity(Self::GLOBAL_MAX_CAPACITY)
                .build(),
        );
        Self {
            project_limiter,
            ip_limiter,
            session_limiter,
            global_limiter,
            per_project_max: settings.per_project_max,
            per_project_window_secs: settings.per_project_window_secs,
            per_ip_max: settings.per_ip_max,
            per_ip_window_secs: settings.per_ip_window_secs,
            per_session_max: settings.per_session_max,
            per_session_window_secs: settings.per_session_window_secs,
            global_max: settings.global_max,
            global_window_secs: settings.global_window_secs,
        }
    }

    /// Atomically increment counters and check limits.
    ///
    /// Uses `AtomicU32` counters stored in the cache so concurrent increments are never lost.
    /// The cache's `time_to_live` acts as a fixed window: a counter entry is created (with TTL)
    /// on first access and expires naturally, resetting the count. Subsequent increments within
    /// the window use the existing entry without refreshing its TTL.
    ///
    /// Returns `Ok(())` if within limits after incrementing, `Err(retry_after_secs)` if exceeded.
    pub async fn increment_and_check(
        &self,
        ip: &str,
        session_key: Option<&str>,
        project: &str,
    ) -> Result<(), u64> {
        // Global
        let global_counter = self
            .global_limiter
            .entry_by_ref("global")
            .or_insert_with(std::future::ready(Arc::new(AtomicU32::new(0))))
            .await
            .into_value();
        let global_count = global_counter.fetch_add(1, Ordering::Relaxed) + 1;
        if global_count > self.global_max {
            return Err(self.global_window_secs);
        }

        // Per-project
        let project_counter = self
            .project_limiter
            .entry_by_ref(project)
            .or_insert_with(std::future::ready(Arc::new(AtomicU32::new(0))))
            .await
            .into_value();
        let project_count = project_counter.fetch_add(1, Ordering::Relaxed) + 1;
        if project_count > self.per_project_max {
            return Err(self.per_project_window_secs);
        }

        // Per-IP
        let ip_counter = self
            .ip_limiter
            .entry_by_ref(ip)
            .or_insert_with(std::future::ready(Arc::new(AtomicU32::new(0))))
            .await
            .into_value();
        let ip_count = ip_counter.fetch_add(1, Ordering::Relaxed) + 1;
        if ip_count > self.per_ip_max {
            return Err(self.per_ip_window_secs);
        }

        // Per-session
        if let Some(key) = session_key {
            let session_counter = self
                .session_limiter
                .entry_by_ref(key)
                .or_insert_with(std::future::ready(Arc::new(AtomicU32::new(0))))
                .await
                .into_value();
            let session_count = session_counter.fetch_add(1, Ordering::Relaxed) + 1;
            if session_count > self.per_session_max {
                return Err(self.per_session_window_secs);
            }
        }

        Ok(())
    }
}

/// One fixed-window counter per key, with the same semantics as the tiers of
/// [`OAuthRateLimiter`].
struct WindowCounter {
    counters: Cache<String, Arc<AtomicU32>>,
    max: u32,
    window_secs: u64,
}

impl WindowCounter {
    fn new(max: u32, window_secs: u64, capacity: u64) -> Self {
        Self {
            counters: Cache::builder()
                .time_to_live(Duration::from_secs(window_secs))
                .max_capacity(capacity)
                .build(),
            max,
            window_secs,
        }
    }

    /// Count one request against `key`: `Err(retry_after_secs)` once over the limit.
    async fn hit(&self, key: &str) -> Result<(), u64> {
        let counter = self
            .counters
            .entry_by_ref(key)
            .or_insert_with(std::future::ready(Arc::new(AtomicU32::new(0))))
            .await
            .into_value();
        if counter.fetch_add(1, Ordering::Relaxed) + 1 > self.max {
            Err(self.window_secs)
        } else {
            Ok(())
        }
    }
}

/// Rate limiter for the workload identity token-exchange endpoint. See
/// [`WorkloadTokenRateLimitSettings`] for why it keys on the deployment for a
/// valid credential and on the address only for a rejected one.
pub struct WorkloadTokenRateLimiter {
    per_deployment: WindowCounter,
    rejected_per_ip: WindowCounter,
}

impl WorkloadTokenRateLimiter {
    const DEPLOYMENT_MAX_CAPACITY: u64 = 50_000;
    const IP_MAX_CAPACITY: u64 = 50_000;

    pub fn new(settings: &WorkloadTokenRateLimitSettings) -> Self {
        Self {
            per_deployment: WindowCounter::new(
                settings.per_deployment_max,
                settings.per_deployment_window_secs,
                Self::DEPLOYMENT_MAX_CAPACITY,
            ),
            rejected_per_ip: WindowCounter::new(
                settings.rejected_per_ip_max,
                settings.rejected_per_ip_window_secs,
                Self::IP_MAX_CAPACITY,
            ),
        }
    }

    /// Count a request that presented `deployment`'s valid credential.
    pub async fn authenticated(&self, deployment: &str) -> Result<(), u64> {
        self.per_deployment.hit(deployment).await
    }

    /// Count a request from `ip` whose credential matched no deployment.
    pub async fn rejected(&self, ip: &str) -> Result<(), u64> {
        self.rejected_per_ip.hit(ip).await
    }
}

/// Budget for the generic resource API's `policy-audit` listing.
///
/// One run reads every binding, Role, Group and owner-label setter in its
/// scope, so it is the most expensive read the API serves. Each caller gets a
/// small per-window budget, counted before authorization so a refused caller
/// cannot spin on the decision either; the replica-wide budget counts only runs
/// that passed authorization, so refused callers cannot spend it for everyone.
pub struct PolicyAuditRateLimiter {
    per_actor: WindowCounter,
    authorized: WindowCounter,
}

impl PolicyAuditRateLimiter {
    const WINDOW_SECS: u64 = 60;
    const PER_ACTOR_MAX: u32 = 6;
    const AUTHORIZED_MAX: u32 = 30;
    const ACTOR_MAX_CAPACITY: u64 = 10_000;

    pub fn new() -> Self {
        Self {
            per_actor: WindowCounter::new(
                Self::PER_ACTOR_MAX,
                Self::WINDOW_SECS,
                Self::ACTOR_MAX_CAPACITY,
            ),
            authorized: WindowCounter::new(Self::AUTHORIZED_MAX, Self::WINDOW_SECS, 1),
        }
    }

    /// Count one request by `actor`: `Err(retry_after_secs)` once over its budget.
    pub async fn requested(&self, actor: &str) -> Result<(), u64> {
        self.per_actor.hit(actor).await
    }

    /// Count one authorized run against this replica's budget.
    pub async fn authorized(&self) -> Result<(), u64> {
        self.authorized.hit("policy-audit").await
    }
}

impl Default for PolicyAuditRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract a session fingerprint from the `rise_jwt` cookie for rate limiting.
///
/// Returns a SHA-256 hash of the full cookie value prefixed with `"session:"`, or `None` if the
/// cookie is absent. Hashing ensures unique keys per-user (a prefix-based approach would collide
/// because all HS256 JWTs share the same base64url-encoded header).
pub fn extract_session_key(headers: &HeaderMap) -> Option<String> {
    let cookie_str = headers.get("cookie")?.to_str().ok()?;

    for pair in cookie_str.split(';') {
        let pair = pair.trim();
        if let Some(value) = pair.strip_prefix("rise_jwt=") {
            if !value.is_empty() {
                let hash = Sha256::digest(value.as_bytes());
                let mut key = String::with_capacity("session:".len() + 64);
                key.push_str("session:");
                for b in hash.iter() {
                    use std::fmt::Write;
                    let _ = write!(key, "{b:02x}");
                }
                return Some(key);
            }
        }
    }
    None
}

/// Build a `429 Too Many Requests` response with a `Retry-After` header.
pub fn rate_limit_response(retry_after: u64) -> Response {
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        "Rate limit exceeded. Please try again later.",
    )
        .into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(&retry_after.to_string()) {
        response
            .headers_mut()
            .insert(axum::http::header::RETRY_AFTER, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workload_limiter(max: u32) -> WorkloadTokenRateLimiter {
        WorkloadTokenRateLimiter::new(&WorkloadTokenRateLimitSettings {
            per_deployment_max: max,
            per_deployment_window_secs: 60,
            rejected_per_ip_max: max,
            rejected_per_ip_window_secs: 60,
        })
    }

    /// Every task of an install may share one address (a NAT gateway, or
    /// "unknown" with no proxy headers). Valid credentials must therefore be
    /// budgeted per deployment, never per address.
    #[tokio::test]
    async fn valid_credentials_are_budgeted_per_deployment_not_per_address() {
        let limiter = workload_limiter(2);
        for _ in 0..2 {
            assert!(limiter.authenticated("dep-a").await.is_ok());
        }
        assert_eq!(limiter.authenticated("dep-a").await, Err(60));
        // Another deployment behind the same address is untouched...
        assert!(limiter.authenticated("dep-b").await.is_ok());
        // ...and so is the address's budget for rejected credentials.
        assert!(limiter.rejected("unknown").await.is_ok());
    }

    #[tokio::test]
    async fn rejected_credentials_are_budgeted_per_address() {
        let limiter = workload_limiter(2);
        for _ in 0..2 {
            assert!(limiter.rejected("203.0.113.7").await.is_ok());
        }
        assert_eq!(limiter.rejected("203.0.113.7").await, Err(60));
        assert!(limiter.rejected("203.0.113.8").await.is_ok());
        // Guessing from an address never spends a deployment's budget.
        assert!(limiter.authenticated("dep-a").await.is_ok());
    }
    use axum::http::{HeaderMap, HeaderValue};

    fn test_settings() -> OAuthRateLimitSettings {
        OAuthRateLimitSettings {
            per_project_max: 10,
            per_project_window_secs: 300,
            per_ip_max: 10,
            per_ip_window_secs: 300,
            per_session_max: 5,
            per_session_window_secs: 300,
            global_max: 1000,
            global_window_secs: 60,
        }
    }

    #[test]
    fn test_extract_session_key_present() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "cookie",
            HeaderValue::from_str("rise_jwt=some-token-value; other=value").unwrap(),
        );
        let key = extract_session_key(&headers).unwrap();
        assert!(key.starts_with("session:"));
        // SHA-256 hex digest is 64 chars
        assert_eq!(key.len(), "session:".len() + 64);
    }

    #[test]
    fn test_extract_session_key_different_tokens_produce_different_keys() {
        let mut headers_a = HeaderMap::new();
        headers_a.insert("cookie", HeaderValue::from_static("rise_jwt=token-a"));
        let mut headers_b = HeaderMap::new();
        headers_b.insert("cookie", HeaderValue::from_static("rise_jwt=token-b"));
        let key_a = extract_session_key(&headers_a).unwrap();
        let key_b = extract_session_key(&headers_b).unwrap();
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn test_extract_session_key_absent() {
        let mut headers = HeaderMap::new();
        headers.insert("cookie", HeaderValue::from_static("other=value"));
        assert!(extract_session_key(&headers).is_none());
    }

    #[tokio::test]
    async fn test_rate_limiter_allows_within_limit() {
        let limiter = OAuthRateLimiter::new(&test_settings());
        assert!(limiter
            .increment_and_check("1.2.3.4", None, "my-project")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_rate_limiter_blocks_at_project_limit() {
        let settings = test_settings();
        let limiter = OAuthRateLimiter::new(&settings);
        let project = "my-project";
        // Use different IPs so we only hit the project limit, not the IP limit
        for i in 0..settings.per_project_max {
            let ip = format!("10.0.0.{i}");
            let _ = limiter.increment_and_check(&ip, None, project).await;
        }
        // Next request should be blocked by project limit
        let result = limiter.increment_and_check("10.0.1.0", None, project).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), settings.per_project_window_secs);
    }

    #[tokio::test]
    async fn test_rate_limiter_blocks_at_ip_limit() {
        let settings = test_settings();
        let limiter = OAuthRateLimiter::new(&settings);
        let ip = "10.0.0.1";
        // Use different projects so we only hit the IP limit, not the project limit
        for i in 0..settings.per_ip_max {
            let project = format!("project-{i}");
            let _ = limiter.increment_and_check(ip, None, &project).await;
        }
        // Next request should be blocked by IP limit
        let result = limiter
            .increment_and_check(ip, None, "another-project")
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), settings.per_ip_window_secs);
    }

    #[tokio::test]
    async fn test_rate_limiter_different_projects_independent() {
        let settings = test_settings();
        let limiter = OAuthRateLimiter::new(&settings);
        // Use different IPs per request so we don't hit the IP limit
        // Exhaust project-a's limit
        for i in 0..settings.per_project_max {
            let ip = format!("10.0.0.{i}");
            let _ = limiter.increment_and_check(&ip, None, "project-a").await;
        }
        // project-a should be blocked
        assert!(limiter
            .increment_and_check("10.0.1.0", None, "project-a")
            .await
            .is_err());
        // project-b should still be allowed
        assert!(limiter
            .increment_and_check("10.0.1.1", None, "project-b")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_rate_limiter_blocks_at_session_limit() {
        let settings = test_settings();
        let limiter = OAuthRateLimiter::new(&settings);
        let session = "session:abc123";
        // Use different IPs and projects so we only hit the session limit
        for i in 0..settings.per_session_max {
            let ip = format!("10.0.0.{i}");
            let project = format!("proj-{i}");
            let _ = limiter
                .increment_and_check(&ip, Some(session), &project)
                .await;
        }
        // Next request should be blocked by session limit
        let result = limiter
            .increment_and_check("10.0.1.0", Some(session), "proj-new")
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), settings.per_session_window_secs);
    }
}
