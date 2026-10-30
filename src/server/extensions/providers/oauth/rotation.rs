//! Rotating refresh-token handles (`rrt2.`).
//!
//! In `rotating` mode the app's refresh token is single use. Every refresh
//! consumes the presented handle and returns a successor in the same *family*
//! (the chain of handles descending from one login); the upstream refresh
//! token is kept encrypted in PostgreSQL and never leaves Rise.
//!
//! A handle is `rrt2.` plus 32 random bytes, base64url. It carries no data and
//! only its SHA-256 is stored. Everything stored for a handle is encrypted
//! under a context bound to the project, the extension and that handle's hash.
//!
//! Refreshing, in short (the SQL is in `db::oauth_refresh_tokens`):
//! 1. One request atomically *claims* the handle and calls the upstream.
//! 2. On success it records the successor and the exact response it returns.
//!    Until the grace period is over, the same handle returns that same
//!    response, so parallel tabs and retried requests agree on one successor.
//! 3. A handle presented after its grace period is reuse: the whole family is
//!    revoked.
//!
//! The upstream call is a parameter ([`Rotation::refresh`] takes a closure),
//! so the state machine runs without an HTTP server.

use super::models::{OAuth2TokenResponse, RefreshTokenSettings};
use super::refresh_handle::{replace_refresh_token, split_refresh_token};
use crate::db::oauth_refresh_tokens::{self as store, HandleState, RotationOutcome};
use crate::server::encryption::{EncryptionContext, EncryptionProvider};
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::Rng;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::future::Future;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};
use uuid::Uuid;

/// Scheme prefix of the server-side-state handle.
pub const ROTATING_HANDLE_PREFIX: &str = "rrt2.";

/// How long a request waits for a concurrent refresh of the same handle to
/// record its successor before giving up with a retryable error.
const WAIT_FOR_CONCURRENT_REFRESH: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Upper bound for one upstream refresh call. It is well inside the claim
/// lease, so a claim cannot be taken over while its holder is still waiting.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether `token` is a `rotating`-mode handle.
pub fn is_rotating_handle(token: &str) -> bool {
    token.starts_with(ROTATING_HANDLE_PREFIX)
}

fn new_handle() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    format!("{ROTATING_HANDLE_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
}

fn hash_handle(handle: &str) -> [u8; 32] {
    Sha256::digest(handle.as_bytes()).into()
}

/// Which stored value of a handle an encryption context protects.
#[derive(Clone, Copy)]
enum Slot {
    UpstreamToken,
    Response,
}

fn context(project: Uuid, extension: &str, handle_hash: &[u8], slot: Slot) -> EncryptionContext {
    let slot = match slot {
        Slot::UpstreamToken => "upstream",
        Slot::Response => "response",
    };
    EncryptionContext::extension(
        project,
        "oauth",
        extension,
        &format!(
            "/tokens/refresh/{}/{slot}",
            URL_SAFE_NO_PAD.encode(handle_hash)
        ),
    )
}

/// What the upstream returned for a refresh.
#[derive(Debug)]
pub struct Refreshed {
    /// The upstream's new refresh token, if it rotated its own.
    pub new_upstream_refresh_token: Option<String>,
    /// The response for the app. Its `refresh_token` is replaced by the
    /// successor handle.
    pub response: OAuth2TokenResponse,
}

/// Why the upstream refresh failed.
#[derive(Debug)]
pub enum UpstreamError {
    /// The upstream rejected the refresh token for good (`invalid_grant`).
    InvalidGrant,
    /// A 2xx answer that is not a usable token response. The token may still
    /// be good.
    MalformedResponse(anyhow::Error),
    /// Anything else (network, timeout, upstream 4xx/5xx). The token may still
    /// be good.
    Failed(anyhow::Error),
}

/// Why a rotating-mode request did not produce a token response.
#[derive(Debug)]
pub enum RotationError {
    /// The handle is unknown, expired, revoked, or was reused.
    InvalidGrant,
    /// A concurrent refresh of the same handle did not finish in time.
    TemporarilyUnavailable,
    /// The upstream refresh failed; the handle stays usable.
    Upstream(anyhow::Error),
    /// The upstream answered 2xx with something that is not a token response;
    /// the handle stays usable.
    UpstreamMalformed(anyhow::Error),
    /// The store or the encryption provider failed.
    Internal(anyhow::Error),
}

fn internal(error: impl Into<anyhow::Error>) -> RotationError {
    RotationError::Internal(error.into())
}

/// Rotating-mode refresh-token handling for one project extension.
pub struct Rotation<'a> {
    pub pool: &'a PgPool,
    pub encryption: &'a dyn EncryptionProvider,
    pub project: Uuid,
    pub extension: &'a str,
    pub settings: RefreshTokenSettings,
    /// How long to wait for a concurrent refresh of the same handle.
    pub wait: Duration,
    /// Upper bound for one upstream refresh call.
    pub upstream_timeout: Duration,
}

impl<'a> Rotation<'a> {
    pub fn new(
        pool: &'a PgPool,
        encryption: &'a dyn EncryptionProvider,
        project: Uuid,
        extension: &'a str,
        settings: RefreshTokenSettings,
    ) -> Self {
        Self {
            pool,
            encryption,
            project,
            extension,
            settings,
            wait: WAIT_FOR_CONCURRENT_REFRESH,
            upstream_timeout: UPSTREAM_TIMEOUT,
        }
    }

    /// Start a family for `upstream_refresh_token` and return its first handle.
    async fn start_family(&self, upstream_refresh_token: &str) -> Result<(String, Uuid)> {
        let handle = new_handle();
        let hash = hash_handle(&handle);
        let encrypted = self
            .encryption
            .encrypt(
                upstream_refresh_token,
                &context(self.project, self.extension, &hash, Slot::UpstreamToken),
            )
            .await?;
        let family = store::create_family(
            self.pool,
            self.project,
            self.extension,
            self.settings.max_lifetime(),
            &hash,
            &encrypted,
        )
        .await?;
        Ok((handle, family))
    }

    /// Replace the upstream `refresh_token` of a code-grant response by the
    /// first handle of a new family.
    ///
    /// The same body rules as the sealed mode apply: only a 2xx JSON object
    /// with a non-empty string `refresh_token` is rewritten, everything else is
    /// returned untouched.
    pub async fn issue_in_token_response(&self, status: u16, body: String) -> Result<String> {
        let Some((mut members, upstream)) = split_refresh_token(status, &body) else {
            return Ok(body);
        };
        let (handle, family) = self.start_family(&upstream).await?;
        info!(
            family_id = %family,
            project_id = %self.project,
            extension = self.extension,
            outcome = "family_started",
            "rotating refresh token family issued"
        );
        replace_refresh_token(&mut members, handle)
    }

    /// Refresh with a handle previously issued by this mode.
    ///
    /// Returns the JSON token response for the app. `upstream` is called with
    /// the upstream refresh token when this request wins the claim on the
    /// handle, and not otherwise.
    pub async fn refresh<F, Fut>(&self, handle: &str, upstream: F) -> Result<String, RotationError>
    where
        F: FnOnce(String) -> Fut,
        Fut: Future<Output = Result<Refreshed, UpstreamError>>,
    {
        let hash = hash_handle(handle);
        let claimant = Uuid::new_v4();
        let deadline = Instant::now() + self.wait;
        let mut upstream = Some(upstream);
        loop {
            let claimed = store::claim(
                self.pool,
                &hash,
                self.project,
                self.extension,
                claimant,
                store::CLAIM_LEASE,
            )
            .await
            .map_err(internal)?;
            if let Some(claimed) = claimed {
                let upstream = upstream.take().expect("the claim is won at most once");
                return self.rotate(&hash, claimant, claimed, upstream).await;
            }

            match store::state(self.pool, &hash, self.project, self.extension)
                .await
                .map_err(internal)?
            {
                HandleState::Unknown | HandleState::FamilyEnded => {
                    return Err(RotationError::InvalidGrant)
                }
                HandleState::Rotated {
                    within_grace: true,
                    response_encrypted,
                    family_id,
                } => {
                    let encrypted = response_encrypted
                        .ok_or_else(|| internal(anyhow!("recorded refresh response is missing")))?;
                    let body = self
                        .encryption
                        .decrypt(
                            &encrypted,
                            &context(self.project, self.extension, &hash, Slot::Response),
                        )
                        .await
                        .map_err(|e| self.unreadable(family_id, e))?;
                    info!(
                        family_id = %family_id,
                        project_id = %self.project,
                        extension = self.extension,
                        outcome = "grace_replay",
                        "rotating refresh token replayed within grace period"
                    );
                    return Ok(body);
                }
                HandleState::Rotated { family_id, .. } => {
                    self.revoke(family_id, "reuse_detected").await;
                    return Err(RotationError::InvalidGrant);
                }
                HandleState::InFlight | HandleState::Available => {
                    if Instant::now() >= deadline {
                        warn!(
                            project_id = %self.project,
                            extension = self.extension,
                            outcome = "concurrent_refresh_timeout",
                            "concurrent refresh of a rotating refresh token did not finish in time"
                        );
                        return Err(RotationError::TemporarilyUnavailable);
                    }
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
        }
    }

    /// The winner of a claim: refresh upstream and record the successor.
    async fn rotate<F, Fut>(
        &self,
        hash: &[u8; 32],
        claimant: Uuid,
        claimed: store::ClaimedHandle,
        upstream: F,
    ) -> Result<String, RotationError>
    where
        F: FnOnce(String) -> Fut,
        Fut: Future<Output = Result<Refreshed, UpstreamError>>,
    {
        let family_id = claimed.family_id;
        let upstream_token = match self
            .encryption
            .decrypt(
                &claimed.upstream_token_encrypted,
                &context(self.project, self.extension, hash, Slot::UpstreamToken),
            )
            .await
        {
            Ok(token) => token,
            Err(e) => {
                self.release(hash, claimant).await;
                return Err(self.unreadable(family_id, e));
            }
        };

        let refreshed = match self.call_upstream(upstream, upstream_token.clone()).await {
            Ok(refreshed) => refreshed,
            Err(UpstreamError::InvalidGrant) => {
                self.revoke(family_id, "upstream_invalid_grant").await;
                return Err(RotationError::InvalidGrant);
            }
            Err(UpstreamError::Failed(e)) => {
                self.release(hash, claimant).await;
                return Err(RotationError::Upstream(e));
            }
            Err(UpstreamError::MalformedResponse(e)) => {
                self.release(hash, claimant).await;
                return Err(RotationError::UpstreamMalformed(e));
            }
        };

        let committed = self
            .record_successor(hash, claimant, family_id, upstream_token, refreshed)
            .await;
        match committed {
            Ok((RotationOutcome::Recorded, body)) => {
                info!(
                    family_id = %family_id,
                    project_id = %self.project,
                    extension = self.extension,
                    outcome = "rotated",
                    "rotating refresh token rotated"
                );
                Ok(body)
            }
            Ok((RotationOutcome::FamilyEnded, _)) => Err(RotationError::InvalidGrant),
            Ok((RotationOutcome::ClaimLost, _)) => {
                warn!(
                    family_id = %family_id,
                    project_id = %self.project,
                    extension = self.extension,
                    outcome = "claim_lost",
                    "claim on a rotating refresh token expired during the upstream request"
                );
                Err(RotationError::TemporarilyUnavailable)
            }
            Err(e) => {
                self.release(hash, claimant).await;
                Err(internal(e))
            }
        }
    }

    async fn record_successor(
        &self,
        hash: &[u8; 32],
        claimant: Uuid,
        family_id: Uuid,
        upstream_token: String,
        refreshed: Refreshed,
    ) -> Result<(RotationOutcome, String)> {
        // The upstream's new token if it rotated its own, otherwise the one it
        // just accepted, which stays valid.
        let next_upstream = match refreshed.new_upstream_refresh_token {
            Some(token) if !token.is_empty() => token,
            _ => upstream_token,
        };
        let successor = new_handle();
        let successor_hash = hash_handle(&successor);
        let mut response = refreshed.response;
        response.refresh_token = Some(successor);
        let body =
            serde_json::to_string(&response).context("Failed to serialize token response")?;

        let upstream_encrypted = self
            .encryption
            .encrypt(
                &next_upstream,
                &context(
                    self.project,
                    self.extension,
                    &successor_hash,
                    Slot::UpstreamToken,
                ),
            )
            .await?;
        let response_encrypted = self
            .encryption
            .encrypt(
                &body,
                &context(self.project, self.extension, hash, Slot::Response),
            )
            .await?;
        let outcome = store::record_rotation(
            self.pool,
            &store::Rotation {
                claimed_handle: hash,
                claimant,
                family_id,
                grace: self.settings.grace_period(),
                successor_handle: &successor_hash,
                successor_upstream_token_encrypted: &upstream_encrypted,
                response_encrypted: &response_encrypted,
            },
        )
        .await?;
        Ok((outcome, body))
    }

    /// Refresh with an upstream token that is not a `rotating` handle (an
    /// `rrt1.` handle or a raw token, already unsealed by the caller) and start
    /// a family from the result. This is how an extension moves from `sealed`
    /// to `rotating` without signing anyone out.
    pub async fn refresh_unsealed<F, Fut>(
        &self,
        upstream_refresh_token: String,
        upstream: F,
    ) -> Result<String, RotationError>
    where
        F: FnOnce(String) -> Fut,
        Fut: Future<Output = Result<Refreshed, UpstreamError>>,
    {
        let refreshed = match self
            .call_upstream(upstream, upstream_refresh_token.clone())
            .await
        {
            Ok(refreshed) => refreshed,
            Err(UpstreamError::InvalidGrant) => return Err(RotationError::InvalidGrant),
            Err(UpstreamError::Failed(e)) => return Err(RotationError::Upstream(e)),
            Err(UpstreamError::MalformedResponse(e)) => {
                return Err(RotationError::UpstreamMalformed(e))
            }
        };
        let next_upstream = match refreshed.new_upstream_refresh_token {
            Some(token) if !token.is_empty() => token,
            _ => upstream_refresh_token,
        };
        let (handle, family) = self.start_family(&next_upstream).await.map_err(internal)?;
        info!(
            family_id = %family,
            project_id = %self.project,
            extension = self.extension,
            outcome = "family_started_from_sealed",
            "rotating refresh token family started from a sealed handle"
        );
        let mut response = refreshed.response;
        response.refresh_token = Some(handle);
        serde_json::to_string(&response)
            .context("Failed to serialize token response")
            .map_err(internal)
    }

    async fn call_upstream<F, Fut>(
        &self,
        upstream: F,
        upstream_refresh_token: String,
    ) -> Result<Refreshed, UpstreamError>
    where
        F: FnOnce(String) -> Fut,
        Fut: Future<Output = Result<Refreshed, UpstreamError>>,
    {
        tokio::time::timeout(self.upstream_timeout, upstream(upstream_refresh_token))
            .await
            .unwrap_or_else(|_| {
                Err(UpstreamError::Failed(anyhow!(
                    "upstream refresh timed out after {:?}",
                    self.upstream_timeout
                )))
            })
    }

    /// A stored value that does not decrypt for its handle was tampered with
    /// (or moved between rows): the handle is not usable.
    fn unreadable(&self, family_id: Uuid, error: anyhow::Error) -> RotationError {
        error!(
            security_event = "refresh_token_record_unreadable",
            family_id = %family_id,
            project_id = %self.project,
            extension = self.extension,
            "stored rotating refresh token record did not decrypt: {:?}",
            error
        );
        RotationError::InvalidGrant
    }

    async fn revoke(&self, family_id: Uuid, reason: &'static str) {
        match store::revoke_family(self.pool, family_id).await {
            Ok(_) => warn!(
                security_event = "refresh_token_family_revoked",
                reason,
                family_id = %family_id,
                project_id = %self.project,
                extension = self.extension,
                "rotating refresh token family revoked"
            ),
            Err(e) => error!(
                security_event = "refresh_token_family_revoke_failed",
                reason,
                family_id = %family_id,
                project_id = %self.project,
                extension = self.extension,
                "failed to revoke rotating refresh token family: {:?}",
                e
            ),
        }
    }

    async fn release(&self, hash: &[u8; 32], claimant: Uuid) {
        if let Err(e) = store::release_claim(self.pool, hash, claimant).await {
            error!(
                project_id = %self.project,
                extension = self.extension,
                "failed to release claim on a rotating refresh token (it expires on its own): {:?}",
                e
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{extensions, projects, users, ProjectStatus};
    use crate::server::encryption::providers::local::LocalEncryptionProvider;
    use crate::server::extensions::providers::oauth::refresh_handle;
    use serde_json::{json, Value};
    use std::sync::Arc;

    fn encryption() -> LocalEncryptionProvider {
        LocalEncryptionProvider::new(&base64::engine::general_purpose::STANDARD.encode([9u8; 32]))
            .unwrap()
    }

    async fn project(pool: &PgPool, name: &str, extensions_: &[&str]) -> Uuid {
        let user = users::create(pool, &format!("{name}@example.com"))
            .await
            .unwrap();
        let project = projects::create(
            pool,
            name,
            ProjectStatus::Stopped,
            "default".into(),
            Some(user.id),
            None,
            None,
        )
        .await
        .unwrap();
        for extension in extensions_ {
            extensions::create(pool, project.id, extension, "oauth", &json!({}))
                .await
                .unwrap();
        }
        project.id
    }

    fn settings() -> RefreshTokenSettings {
        RefreshTokenSettings {
            mode: super::super::models::RefreshTokenMode::Rotating,
            grace_period_seconds: Some(30),
            max_lifetime_seconds: Some(3600),
        }
    }

    fn rotation<'a>(
        pool: &'a PgPool,
        encryption: &'a LocalEncryptionProvider,
        project: Uuid,
        extension: &'a str,
    ) -> Rotation<'a> {
        let mut rotation = Rotation::new(pool, encryption, project, extension, settings());
        rotation.wait = Duration::from_millis(400);
        rotation
    }

    fn refreshed(access_token: &str, new_upstream: Option<&str>) -> Refreshed {
        Refreshed {
            new_upstream_refresh_token: new_upstream.map(str::to_string),
            response: OAuth2TokenResponse {
                access_token: access_token.into(),
                token_type: "Bearer".into(),
                expires_in: Some(3600),
                refresh_token: None,
                scope: None,
                id_token: None,
            },
        }
    }

    /// Records the upstream refresh tokens it is called with.
    #[derive(Clone, Default)]
    struct FakeUpstream {
        calls: Arc<std::sync::Mutex<Vec<String>>>,
        delay: Duration,
    }

    impl FakeUpstream {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn answer(
            &self,
            reply: Result<Refreshed, UpstreamError>,
        ) -> impl FnOnce(
            String,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<Refreshed, UpstreamError>> + Send>,
        > {
            let this = self.clone();
            move |token| {
                Box::pin(async move {
                    this.calls.lock().unwrap().push(token);
                    tokio::time::sleep(this.delay).await;
                    reply
                })
            }
        }
    }

    fn token(body: &str) -> (Value, String) {
        let value: Value = serde_json::from_str(body).unwrap();
        let handle = value["refresh_token"].as_str().unwrap().to_string();
        (value, handle)
    }

    /// Log in: issue a family for `upstream_rt` and return its first handle.
    async fn login(rotation: &Rotation<'_>, upstream_rt: &str) -> String {
        let body = rotation
            .issue_in_token_response(
                200,
                json!({"access_token": "at0", "token_type": "Bearer", "refresh_token": upstream_rt})
                    .to_string(),
            )
            .await
            .unwrap();
        token(&body).1
    }

    async fn expire_grace(pool: &PgPool) {
        sqlx::query("UPDATE oauth_refresh_tokens SET grace_until = NOW() - INTERVAL '1 second' WHERE rotated_at IS NOT NULL")
            .execute(pool)
            .await
            .unwrap();
    }

    #[sqlx::test]
    async fn code_grant_issues_a_stored_handle_without_leaking_the_upstream_token(pool: PgPool) {
        let project = project(&pool, "issue", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let body = rotation
            .issue_in_token_response(
                200,
                r#"{"access_token":"at","token_type":"Bearer","refresh_token":"upstream-rt","scope":"a b","extra":[1]}"#.into(),
            )
            .await
            .unwrap();
        let (value, handle) = token(&body);
        assert!(is_rotating_handle(&handle));
        assert_eq!(handle.len(), "rrt2.".len() + 43);
        assert!(!body.contains("upstream-rt"));
        assert_eq!(value["scope"], "a b");
        assert_eq!(value["extra"], json!([1]));

        // Only the hash of the handle is stored, and the stored token is ciphertext.
        let (hash, stored): (Vec<u8>, String) = sqlx::query_as(
            "SELECT handle_hash, upstream_token_encrypted FROM oauth_refresh_tokens",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(hash, hash_handle(&handle).to_vec());
        assert!(!stored.contains("upstream-rt"));
        let expires_in_range: bool = sqlx::query_scalar(
            "SELECT expires_at BETWEEN NOW() + INTERVAL '3590 seconds' AND NOW() + INTERVAL '3600 seconds' FROM oauth_refresh_families",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            expires_in_range,
            "family lifetime follows max_lifetime_seconds"
        );

        // Responses without a usable refresh token and failures pass through.
        for (status, body) in [
            (200, r#"{"access_token":"at"}"#),
            (200, r#"{"access_token":"at","refresh_token":""}"#),
            (400, r#"{"error":"invalid_grant","refresh_token":"rt"}"#),
            (200, "not json"),
        ] {
            assert_eq!(
                rotation
                    .issue_in_token_response(status, body.into())
                    .await
                    .unwrap(),
                body
            );
        }
        let families: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oauth_refresh_families")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(families, 1);
    }

    #[sqlx::test]
    async fn rotation_without_upstream_rotation_keeps_the_upstream_token(pool: PgPool) {
        let project = project(&pool, "plain", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let first = login(&rotation, "upstream-1").await;

        let upstream = FakeUpstream::default();
        let body = rotation
            .refresh(&first, upstream.answer(Ok(refreshed("at1", None))))
            .await
            .unwrap();
        let (value, second) = token(&body);
        assert_eq!(value["access_token"], "at1");
        assert!(is_rotating_handle(&second));
        assert_ne!(
            second, first,
            "a successor is issued even if upstream did not rotate"
        );
        assert!(!body.contains("upstream-1"));
        assert_eq!(upstream.calls(), ["upstream-1"]);

        // The successor still wraps the same upstream token.
        let body = rotation
            .refresh(&second, upstream.answer(Ok(refreshed("at2", None))))
            .await
            .unwrap();
        assert_ne!(token(&body).1, second);
        assert_eq!(upstream.calls(), ["upstream-1", "upstream-1"]);
    }

    #[sqlx::test]
    async fn rotation_with_upstream_rotation_wraps_the_new_upstream_token(pool: PgPool) {
        let project = project(&pool, "upstream-rotates", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let first = login(&rotation, "upstream-1").await;

        let upstream = FakeUpstream::default();
        let body = rotation
            .refresh(
                &first,
                upstream.answer(Ok(refreshed("at1", Some("upstream-2")))),
            )
            .await
            .unwrap();
        assert!(!body.contains("upstream-2"));
        let second = token(&body).1;
        let body = rotation
            .refresh(&second, upstream.answer(Ok(refreshed("at2", Some("")))))
            .await
            .unwrap();
        let third = token(&body).1;
        // An empty upstream token counts as "not rotated".
        rotation
            .refresh(&third, upstream.answer(Ok(refreshed("at3", None))))
            .await
            .unwrap();
        assert_eq!(upstream.calls(), ["upstream-1", "upstream-2", "upstream-2"]);
    }

    #[sqlx::test]
    async fn a_duplicate_within_grace_gets_the_same_successor_without_calling_upstream(
        pool: PgPool,
    ) {
        let project = project(&pool, "dup", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let first = login(&rotation, "upstream-1").await;
        let upstream = FakeUpstream::default();

        let a = rotation
            .refresh(&first, upstream.answer(Ok(refreshed("at1", None))))
            .await
            .unwrap();
        let b = rotation
            .refresh(&first, upstream.answer(Ok(refreshed("never", None))))
            .await
            .unwrap();
        assert_eq!(a, b, "the exact response is replayed");
        assert_eq!(upstream.calls().len(), 1);

        // The replayed successor is a working handle, and spent handles stay
        // replayable for the whole grace window.
        let (_, second) = token(&a);
        rotation
            .refresh(&second, upstream.answer(Ok(refreshed("at2", None))))
            .await
            .unwrap();
        assert_eq!(
            rotation
                .refresh(&first, upstream.answer(Ok(refreshed("never", None))))
                .await
                .unwrap(),
            a
        );
    }

    #[sqlx::test]
    async fn concurrent_requests_agree_on_one_successor_and_one_upstream_call(pool: PgPool) {
        let project = project(&pool, "concurrent", &["login"]).await;
        let provider = encryption();
        let mut rotation = rotation(&pool, &provider, project, "login");
        rotation.wait = Duration::from_secs(5);
        let first = login(&rotation, "upstream-1").await;
        let upstream = FakeUpstream {
            delay: Duration::from_millis(400),
            ..Default::default()
        };

        let attempts = (0..4).map(|n| {
            rotation.refresh(
                &first,
                upstream.answer(Ok(refreshed(&format!("at{n}"), None))),
            )
        });
        let bodies: Vec<String> = futures::future::join_all(attempts)
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();
        assert!(bodies.windows(2).all(|pair| pair[0] == pair[1]));
        assert_eq!(upstream.calls().len(), 1);
    }

    #[sqlx::test]
    async fn a_request_that_waits_too_long_for_the_winner_gets_a_retryable_error(pool: PgPool) {
        let project = project(&pool, "slow", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let first = login(&rotation, "upstream-1").await;
        // Another request holds the claim and never finishes.
        crate::db::oauth_refresh_tokens::claim(
            &pool,
            &hash_handle(&first),
            project,
            "login",
            Uuid::new_v4(),
            crate::db::oauth_refresh_tokens::CLAIM_LEASE,
        )
        .await
        .unwrap()
        .unwrap();

        let upstream = FakeUpstream::default();
        let result = rotation
            .refresh(&first, upstream.answer(Ok(refreshed("never", None))))
            .await;
        assert!(matches!(result, Err(RotationError::TemporarilyUnavailable)));
        assert!(upstream.calls().is_empty());
    }

    #[sqlx::test]
    async fn reuse_after_the_grace_period_revokes_the_whole_family(pool: PgPool) {
        let project = project(&pool, "reuse", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let first = login(&rotation, "upstream-1").await;
        let upstream = FakeUpstream::default();
        let second = token(
            &rotation
                .refresh(&first, upstream.answer(Ok(refreshed("at1", None))))
                .await
                .unwrap(),
        )
        .1;
        let third = token(
            &rotation
                .refresh(&second, upstream.answer(Ok(refreshed("at2", None))))
                .await
                .unwrap(),
        )
        .1;
        expire_grace(&pool).await;

        // Replaying the oldest handle is reuse.
        assert!(matches!(
            rotation
                .refresh(&first, upstream.answer(Ok(refreshed("never", None))))
                .await,
            Err(RotationError::InvalidGrant)
        ));
        // It ended the login: the current handle is dead as well.
        assert!(matches!(
            rotation
                .refresh(&third, upstream.answer(Ok(refreshed("never", None))))
                .await,
            Err(RotationError::InvalidGrant)
        ));
        assert_eq!(upstream.calls().len(), 2, "upstream was not called again");
        let revoked: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM oauth_refresh_families WHERE revoked_at IS NOT NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(revoked, 1);
    }

    #[sqlx::test]
    async fn the_family_lifetime_is_absolute(pool: PgPool) {
        let project = project(&pool, "lifetime", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let first = login(&rotation, "upstream-1").await;
        let upstream = FakeUpstream::default();
        let second = token(
            &rotation
                .refresh(&first, upstream.answer(Ok(refreshed("at1", None))))
                .await
                .unwrap(),
        )
        .1;
        // Refreshing does not extend the family: it ends when it was set to.
        let extended: bool = sqlx::query_scalar(
            "SELECT expires_at <= NOW() + INTERVAL '3600 seconds' FROM oauth_refresh_families",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(extended);

        sqlx::query("UPDATE oauth_refresh_families SET expires_at = NOW() - INTERVAL '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            rotation
                .refresh(&second, upstream.answer(Ok(refreshed("never", None))))
                .await,
            Err(RotationError::InvalidGrant)
        ));
        assert_eq!(upstream.calls().len(), 1);
    }

    #[sqlx::test]
    async fn an_upstream_failure_releases_the_claim_for_a_retry(pool: PgPool) {
        let project = project(&pool, "failure", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let first = login(&rotation, "upstream-1").await;
        let upstream = FakeUpstream::default();

        let result = rotation
            .refresh(
                &first,
                upstream.answer(Err(UpstreamError::Failed(anyhow!("503 from upstream")))),
            )
            .await;
        assert!(matches!(result, Err(RotationError::Upstream(_))));

        // Same handle, immediately usable again, without waiting for a lease.
        let body = rotation
            .refresh(&first, upstream.answer(Ok(refreshed("at1", None))))
            .await
            .unwrap();
        assert_eq!(token(&body).0["access_token"], "at1");
        assert_eq!(upstream.calls(), ["upstream-1", "upstream-1"]);
    }

    #[sqlx::test]
    async fn an_upstream_invalid_grant_revokes_the_family(pool: PgPool) {
        let project = project(&pool, "dead", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let first = login(&rotation, "upstream-1").await;
        let upstream = FakeUpstream::default();
        let second = token(
            &rotation
                .refresh(&first, upstream.answer(Ok(refreshed("at1", None))))
                .await
                .unwrap(),
        )
        .1;

        assert!(matches!(
            rotation
                .refresh(&second, upstream.answer(Err(UpstreamError::InvalidGrant)))
                .await,
            Err(RotationError::InvalidGrant)
        ));
        for handle in [&first, &second] {
            assert!(matches!(
                rotation
                    .refresh(handle, upstream.answer(Ok(refreshed("never", None))))
                    .await,
                Err(RotationError::InvalidGrant)
            ));
        }
        assert_eq!(upstream.calls().len(), 2);
    }

    #[sqlx::test]
    async fn handles_do_not_cross_extensions_or_projects_and_foreign_use_revokes_nothing(
        pool: PgPool,
    ) {
        let project_a = project(&pool, "scope-a", &["login", "other"]).await;
        let project_b = project(&pool, "scope-b", &["login"]).await;
        let provider = encryption();
        let rotation_a = rotation(&pool, &provider, project_a, "login");
        let first = login(&rotation_a, "upstream-1").await;
        let upstream = FakeUpstream::default();

        for foreign in [
            rotation(&pool, &provider, project_a, "other"),
            rotation(&pool, &provider, project_b, "login"),
            rotation(&pool, &provider, project_a, "missing"),
        ] {
            assert!(matches!(
                foreign
                    .refresh(&first, upstream.answer(Ok(refreshed("never", None))))
                    .await,
                Err(RotationError::InvalidGrant)
            ));
        }
        assert!(upstream.calls().is_empty());

        // The owner can still use it: a foreign presenter cannot end the login.
        rotation_a
            .refresh(&first, upstream.answer(Ok(refreshed("at1", None))))
            .await
            .unwrap();
        assert_eq!(upstream.calls(), ["upstream-1"]);
    }

    #[sqlx::test]
    async fn unknown_and_malformed_handles_are_invalid_grant(pool: PgPool) {
        let project = project(&pool, "unknown", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let upstream = FakeUpstream::default();
        for handle in [new_handle().as_str(), "rrt2.", "rrt2.!!!", "rrt2.AAAA"] {
            assert!(matches!(
                rotation
                    .refresh(handle, upstream.answer(Ok(refreshed("never", None))))
                    .await,
                Err(RotationError::InvalidGrant)
            ));
        }
        assert!(upstream.calls().is_empty());
    }

    #[sqlx::test]
    async fn an_rrt1_handle_or_raw_token_starts_a_new_family(pool: PgPool) {
        let project = project(&pool, "migrate", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let upstream = FakeUpstream::default();

        // A sealed handle issued while the extension was in sealed mode.
        let sealed = refresh_handle::seal(&provider, project, "login", "upstream-sealed", 1)
            .await
            .unwrap();
        let presented = refresh_handle::present(&provider, project, "login", &sealed)
            .await
            .unwrap();
        let body = rotation
            .refresh_unsealed(
                presented.upstream_token().to_string(),
                upstream.answer(Ok(refreshed("at1", None))),
            )
            .await
            .unwrap();
        let (value, handle) = token(&body);
        assert_eq!(value["access_token"], "at1");
        assert!(is_rotating_handle(&handle));
        assert!(!body.contains("upstream-sealed"));
        assert_eq!(upstream.calls(), ["upstream-sealed"]);

        // The new handle is a normal member of a new family wrapping the same upstream token.
        let body = rotation
            .refresh(
                &handle,
                upstream.answer(Ok(refreshed("at2", Some("upstream-new")))),
            )
            .await
            .unwrap();
        assert_ne!(token(&body).1, handle);
        assert_eq!(upstream.calls(), ["upstream-sealed", "upstream-sealed"]);

        // A raw pre-handle token migrates the same way, taking the upstream's
        // rotated token when there is one.
        let presented = refresh_handle::present(&provider, project, "login", "raw-upstream")
            .await
            .unwrap();
        let body = rotation
            .refresh_unsealed(
                presented.upstream_token().to_string(),
                upstream.answer(Ok(refreshed("at3", Some("raw-rotated")))),
            )
            .await
            .unwrap();
        let handle = token(&body).1;
        rotation
            .refresh(&handle, upstream.answer(Ok(refreshed("at4", None))))
            .await
            .unwrap();
        assert_eq!(upstream.calls()[2..], ["raw-upstream", "raw-rotated"]);

        let families: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oauth_refresh_families")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(families, 2);
    }

    #[sqlx::test]
    async fn a_failed_migration_refresh_creates_nothing(pool: PgPool) {
        let project = project(&pool, "migrate-fail", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let upstream = FakeUpstream::default();
        for failure in [
            UpstreamError::InvalidGrant,
            UpstreamError::Failed(anyhow!("down")),
        ] {
            assert!(rotation
                .refresh_unsealed("upstream".into(), upstream.answer(Err(failure)))
                .await
                .is_err());
        }
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oauth_refresh_families")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[sqlx::test]
    async fn a_malformed_upstream_answer_releases_the_claim(pool: PgPool) {
        let project = project(&pool, "malformed", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let first = login(&rotation, "upstream-1").await;
        let upstream = FakeUpstream::default();
        let result = rotation
            .refresh(
                &first,
                upstream.answer(Err(UpstreamError::MalformedResponse(anyhow!("not json")))),
            )
            .await;
        assert!(matches!(result, Err(RotationError::UpstreamMalformed(_))));
        rotation
            .refresh(&first, upstream.answer(Ok(refreshed("at1", None))))
            .await
            .unwrap();
        assert_eq!(upstream.calls().len(), 2);
    }

    #[sqlx::test]
    async fn an_upstream_call_that_times_out_releases_the_claim(pool: PgPool) {
        let project = project(&pool, "timeout", &["login"]).await;
        let provider = encryption();
        let mut rotation = rotation(&pool, &provider, project, "login");
        rotation.upstream_timeout = Duration::from_millis(100);
        let first = login(&rotation, "upstream-1").await;
        let slow = FakeUpstream {
            delay: Duration::from_millis(500),
            ..Default::default()
        };
        let result = rotation
            .refresh(&first, slow.answer(Ok(refreshed("late", None))))
            .await;
        assert!(matches!(result, Err(RotationError::Upstream(_))));
        let fast = FakeUpstream::default();
        rotation
            .refresh(&first, fast.answer(Ok(refreshed("at1", None))))
            .await
            .unwrap();
    }

    #[sqlx::test]
    async fn a_stale_claim_is_taken_over_and_the_late_claimant_cannot_overwrite_the_result(
        pool: PgPool,
    ) {
        let project = project(&pool, "lease", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let first = login(&rotation, "upstream-1").await;
        let slow = FakeUpstream {
            delay: Duration::from_millis(600),
            ..Default::default()
        };
        let fast = FakeUpstream::default();

        let original = rotation.refresh(&first, slow.answer(Ok(refreshed("original", None))));
        let takeover = async {
            // The original claimant is frozen past its lease.
            tokio::time::sleep(Duration::from_millis(150)).await;
            sqlx::query("UPDATE oauth_refresh_tokens SET claim_expires_at = NOW() - INTERVAL '1 second' WHERE claimed_by IS NOT NULL")
                .execute(&pool)
                .await
                .unwrap();
            rotation
                .refresh(&first, fast.answer(Ok(refreshed("takeover", None))))
                .await
        };
        let (original, takeover) = tokio::join!(original, takeover);

        let takeover = takeover.unwrap();
        assert_eq!(token(&takeover).0["access_token"], "takeover");
        assert!(matches!(
            original,
            Err(RotationError::TemporarilyUnavailable)
        ));
        // The takeover's response is what the handle replays, and only its
        // successor exists.
        let replay = rotation
            .refresh(&first, fast.answer(Ok(refreshed("never", None))))
            .await
            .unwrap();
        assert_eq!(replay, takeover);
        assert_eq!(slow.calls().len(), 1);
        assert_eq!(fast.calls().len(), 1);
        let handles: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oauth_refresh_tokens")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(handles, 2);
    }

    #[sqlx::test]
    async fn the_sealed_mode_does_not_accept_rrt2_handles(pool: PgPool) {
        // `present` is what a sealed-mode extension runs on every presented token.
        let project = project(&pool, "sealed", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let handle = login(&rotation, "upstream-1").await;
        assert!(refresh_handle::is_handle(&handle));
        assert!(
            refresh_handle::present(&provider, project, "login", &handle)
                .await
                .is_err()
        );
    }

    #[sqlx::test]
    async fn stored_values_are_bound_to_their_handle_and_unreadable_ones_are_invalid_grant(
        pool: PgPool,
    ) {
        // Moving a ciphertext to another row must not let it decrypt there.
        let project = project(&pool, "bound", &["login"]).await;
        let provider = encryption();
        let rotation = rotation(&pool, &provider, project, "login");
        let a = login(&rotation, "upstream-a").await;
        let b = login(&rotation, "upstream-b").await;
        sqlx::query("UPDATE oauth_refresh_tokens SET upstream_token_encrypted = (SELECT upstream_token_encrypted FROM oauth_refresh_tokens WHERE handle_hash = $1) WHERE handle_hash = $2")
            .bind(hash_handle(&a).to_vec())
            .bind(hash_handle(&b).to_vec())
            .execute(&pool)
            .await
            .unwrap();
        let upstream = FakeUpstream::default();
        assert!(matches!(
            rotation
                .refresh(&b, upstream.answer(Ok(refreshed("never", None))))
                .await,
            Err(RotationError::InvalidGrant)
        ));
        assert!(upstream.calls().is_empty());
        // The failed attempt did not leave the handle claimed.
        assert!(matches!(
            crate::db::oauth_refresh_tokens::state(&pool, &hash_handle(&b), project, "login")
                .await
                .unwrap(),
            HandleState::Available
        ));
    }
}
